//! `net.tcp.lifetime` over a raw tracepoint: one event per closed connection.
//!
//! The measurement is the gap between a socket entering `TCP_ESTABLISHED` and
//! the same socket entering `TCP_CLOSE`, with what crossed it: payload bytes
//! each way and segments each way, read off the socket at the moment it closes.
//! The program is in `src/bpf/net_lifetime.rs`.
//!
//! **A connection has no process**, and that is the family's own rule rather
//! than a shortcoming here. Both ends of a TCP connection reach
//! `TCP_ESTABLISHED` in softirq context -- the passive end when the final ACK
//! arrives, the active end when the SYN-ACK does -- so the task that is current
//! at that moment is whatever the interrupt happened to land on. The task this
//! collector reports is the one current at the *close*, which is the owning
//! process when an application closed the connection and is not when the kernel
//! closed it from a timer. That is stated on the event and in the run rather
//! than left to be assumed, and it is why a process target keeps only the
//! connections whose owner the program saw: an `at_close` one is a counted
//! loss under `--pid`, never placed by the task it interrupted.
//!
//! **The cookie is the join key.** `net.tcp.rtt` keys its series on the same
//! number, read through `sock_diag`, so a `sock:cookie` join between the two
//! needs no translation. The kernel generates a cookie the first time something
//! asks for one, so a connection that opened and closed without anything ever
//! dumping it has none: those events carry the socket's inode and no socket
//! key, because a key of zero would join every anonymous connection to every
//! other.

use std::time::Duration as StdDuration;

use crate::bpf::{TcpLifetime, TcpLifetimeProgram};
use crate::collect::{not_built, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained. Connections close in bursts -- a load
/// generator finishing, a pool being recycled -- and draining often is cheap.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(20);

/// The ring the program writes into. Each record is 104 bytes plus an 8-byte
/// header, so this holds about two thousand connections between drains.
const RING_BYTES: usize = 256 * 1024;

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// The request's predicate, which this collector applies itself: the
    /// resolver credits every `bpf` backend with filtering in the kernel and
    /// then leaves its events alone.
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot measure a connection's lifetime: it is the gap between two \
                 state changes, and a sample that catches one of them without the other has no \
                 duration to report",
                self.mode
            ));
        }
        let mut thread = None;
        let tree = match target {
            Target::System => None,
            // A launch arrives here as its child's pid.
            Target::Pid(pids) if !pids.is_empty() => Some(Tree::new(pids)),
            // One thread: the connections it connected or closed first, by
            // the kernel's id of it, which is what the program records.
            Target::Tid(tids) if !tids.is_empty() => {
                thread = Some(crate::bpf::pidns::kernel_tid(tids[0] as u32)?);
                None
            }
            other => {
                return Err(not_built(format!(
                    "a {} target cannot be served here: a connection is attributed to the \
                     process that connected or closed it, and that is a process tree, not \
                     a {}. Use --system or --pid.",
                    other.kind(),
                    other.kind()
                )))
            }
        };
        let mut collector = LifetimeCollector::new(self.filter);
        collector.tree = tree;
        collector.thread = thread;
        Ok(Box::new(collector))
    }
}

pub struct LifetimeCollector {
    program: Option<TcpLifetimeProgram>,
    /// Runs of the programs the kernel skipped, as last read.
    skipped: u64,
    losses: Losses,
    notes: Vec<String>,
    /// The program's counters are cumulative, so what is new is the difference.
    dropped_open: u64,
    dropped_ring: u64,
    /// Connections seen, and how many of them the kernel had never given a
    /// cookie -- which is what decides whether they can be joined at all.
    closed: u64,
    without_cookie: u64,
    without_inode: u64,
    /// Connections still established when the run ended.
    still_open: usize,
    predicate: Option<FilterExpr>,
    filtered_in: u64,
    filtered_out: u64,
    /// A process target: the tree whose connections are kept.
    tree: Option<Tree>,
    /// A thread target, by its kernel id.
    thread: Option<u32>,
    /// Connections of other processes, left out under a process target.
    outside: u64,
    /// Connections whose task had exited before it could be placed in or
    /// out of the tree.
    unplaced: u64,
    /// Connections no process was seen acting on, which a process target
    /// can neither keep nor leave out.
    unowned: u64,
}

/// The process tree a `--pid` or launch target follows: the roots and every
/// descendant seen, refreshed at each drain through
/// `/proc/<pid>/task/<tid>/children`, and an unknown pid placed by walking
/// its parents while it is still there to walk.
struct Tree {
    known: std::collections::BTreeSet<u32>,
}

impl Tree {
    fn new(roots: &[i32]) -> Self {
        let mut tree = Self {
            known: roots.iter().map(|pid| *pid as u32).collect(),
        };
        tree.refresh();
        tree
    }

    fn refresh(&mut self) {
        let mut pending: Vec<u32> = self.known.iter().copied().collect();
        while let Some(pid) = pending.pop() {
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
                continue;
            };
            for task in tasks.flatten() {
                let Ok(text) = std::fs::read_to_string(task.path().join("children")) else {
                    continue;
                };
                for child in text.split_whitespace().filter_map(|word| word.parse().ok()) {
                    if self.known.insert(child) {
                        pending.push(child);
                    }
                }
            }
        }
    }

    /// Whether `pid` is in the tree: `Some(true)` or `Some(false)`, or
    /// `None` when it has exited and was never seen.
    fn holds(&mut self, pid: u32) -> Option<bool> {
        if self.known.contains(&pid) {
            return Some(true);
        }
        let mut at = pid;
        for _ in 0..64 {
            let stat = std::fs::read_to_string(format!("/proc/{at}/stat")).ok()?;
            // The parent is the second field after the parenthesised comm.
            let parent: u32 = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            if self.known.contains(&parent) {
                self.known.insert(pid);
                return Some(true);
            }
            if parent <= 1 {
                return Some(false);
            }
            at = parent;
        }
        Some(false)
    }
}

impl LifetimeCollector {
    fn new(predicate: Option<FilterExpr>) -> Self {
        Self {
            program: None,
            losses: Losses::default(),
            notes: Vec::new(),
            dropped_open: 0,
            dropped_ring: 0,
            skipped: 0,
            closed: 0,
            without_cookie: 0,
            without_inode: 0,
            still_open: 0,
            predicate,
            filtered_in: 0,
            filtered_out: 0,
            tree: None,
            thread: None,
            outside: 0,
            unplaced: 0,
            unowned: 0,
        }
    }

    fn event(&self, closed: &TcpLifetime, seq: u64) -> Event {
        Event {
            ts_ns: closed.ts_ns,
            ts_err_ns: 0,
            kind: "net.tcp.lifetime".to_owned(),
            keys: match closed.cookie {
                0 => Vec::new(),
                cookie => vec![KeyValue::Sock { cookie }],
            },
            attrs: attributes(closed),
            collector_id: CollectorId::unstamped(),
            seq,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let mut undecoded = 0;
        let mut batch = Vec::new();
        program.drain(|closed| batch.push(closed), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the connection ring was shorter than one connection");
        }

        if let Some(tree) = self.tree.as_mut() {
            tree.refresh();
        }
        let mut index = 0;
        for closed in batch {
            if !self.in_target(&closed) {
                continue;
            }
            let candidate = self.event(&closed, seq + index);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &candidate) {
                    self.filtered_out += 1;
                    continue;
                }
                self.filtered_in += 1;
            }
            self.closed += 1;
            if closed.cookie == 0 {
                self.without_cookie += 1;
            }
            if closed.inode == 0 {
                self.without_inode += 1;
            }
            events.push(candidate);
            index += 1;
        }
    }

    /// Whether a closed connection belongs to the target, counting the ones
    /// that do not and the ones that cannot be told.
    ///
    /// Only an `owner` pid places a connection. An `at_close` one is the task
    /// the close interrupted (a peer's reset, a timer, a connection opened
    /// before the program attached), which says nothing about who held the
    /// socket: sorting it in or out of the tree by that task would drop the
    /// target's connections and keep strangers'. So under a process target it
    /// is a counted loss, as a connection whose owner had exited is.
    fn in_target(&mut self, closed: &TcpLifetime) -> bool {
        if self.tree.is_none() && self.thread.is_none() {
            return true;
        }
        if closed.owned == 0 {
            self.unowned += 1;
            self.losses.dropped += 1;
            self.losses.note_times(
                "a connection closed with no process seen acting on it (a reset, a timer, or \
                 one opened before the program attached), so it could be the target's or \
                 another process's and was left out",
                1,
            );
            return false;
        }
        if let Some(thread) = self.thread {
            if closed.tid == thread {
                return true;
            }
            self.outside += 1;
            return false;
        }
        let Some(tree) = self.tree.as_mut() else {
            return true;
        };
        // The program wrote the kernel's id, and the tree is read from
        // `/proc` in this namespace's: the same process under two numbers
        // inside a container, and nothing kept until this.
        let pid = crate::bpf::pidns::ours(closed.pid);
        match pid.and_then(|pid| tree.holds(pid)) {
            Some(true) => true,
            Some(false) => {
                self.outside += 1;
                false
            }
            None => {
                // Possibly the target's: a loss, not a connection known to be
                // someone else's.
                self.unplaced += 1;
                self.losses.dropped += 1;
                self.losses.note_times(
                    "a connection's process had exited before it could be placed in or out of \
                     the target's tree",
                    1,
                );
                false
            }
        }
    }

    /// Reads the program's own counters, which are the only place a connection
    /// it could not record is counted.
    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note_times(
                "the kernel skipped a run of these programs because one was already running on \
                 its cpu, so a connection went unseen; the count is the machine's, and an upper bound \
                 on this target's", skipped - self.skipped);
            self.skipped = skipped;
        }
        let (open, ring) = program.dropped();
        if open > self.dropped_open {
            self.losses.dropped += open - self.dropped_open;
            self.losses.note_times(
                "more connections were established at once than the open map holds, so their \
                 lifetimes were never timed",
                open - self.dropped_open,
            );
            self.dropped_open = open;
        }
        if ring > self.dropped_ring {
            self.losses.dropped += ring - self.dropped_ring;
            self.losses.note_times(
                "the connection ring was full: connections closed faster than they were drained",
                ring - self.dropped_ring,
            );
            self.dropped_ring = ring;
        }
    }
}

/// What an event carries beside its value: both ends of the connection, what
/// crossed it, and the task current at the close.
fn attributes(closed: &TcpLifetime) -> Vec<Attr> {
    let mut attrs = vec![
        Attr::uint("duration_ns", closed.duration_ns),
        // Payload bytes, which is what `bytes_sent` counts in the kernel: not
        // segments, not wire bytes, and not retransmissions.
        Attr::uint("bytes_sent", closed.bytes_sent),
        Attr::uint("bytes_received", closed.bytes_received),
        Attr::uint("segs_out", u64::from(closed.segs_out)),
        Attr::uint("segs_in", u64::from(closed.segs_in)),
        Attr::uint("family", u64::from(closed.family)),
        Attr::uint("sport", u64::from(closed.local_port())),
        // The connection's owner when the program saw it act on the socket
        // (`owner`), else the task current at the close (`at_close`), which
        // the run's notes qualify: it is whoever the close interrupted.
        Attr::text("comm", closed.comm()),
        Attr::uint("pid", u64::from(closed.pid)),
        Attr::uint("tid", u64::from(closed.tid)),
        Attr::text(
            "attributed",
            if closed.owned != 0 {
                "owner"
            } else {
                "at_close"
            },
        ),
    ];
    if closed.inode != 0 {
        // Zero is not an inode. A socket accepted from a listen queue has none
        // until `accept()` gives it a file, which is after it is established,
        // so the passive end of a connection usually has no inode here.
        attrs.push(Attr::uint("inode", closed.inode));
    }
    if let Some(address) = closed.local_address() {
        attrs.push(Attr::text("saddr", address));
    }
    if let Some(address) = closed.remote_address() {
        // The address alone, as `saddr` is: the port is its own field, and an
        // address with a port inside it is one no equality can match.
        attrs.push(Attr::text("daddr", address));
        attrs.push(Attr::uint("dport", u64::from(closed.remote_port())));
    }
    if closed.cookie != 0 {
        attrs.push(Attr::uint("cookie", closed.cookie));
    }
    attrs
}

impl Collector for LifetimeCollector {
    fn value_attr(&self) -> &'static str {
        "duration_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(TcpLifetimeProgram::load(RING_BYTES)?);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_drops();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        self.absorb_drops();
        if let Some(program) = self.program.as_ref() {
            self.still_open = program.still_open();
        }
        self.program = None;
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        notes.push(
            "`comm` and `pid` are the connection's owner where `attributed` is `owner`: the \
             process that connected, or that closed it first. Where it is `at_close`, the \
             program saw no process act on the socket and they are the task current at \
             TCP_CLOSE, which the kernel's timer or a reset makes whoever was interrupted"
                .to_owned(),
        );
        if self.tree.is_some() || self.thread.is_some() {
            notes.push(format!(
                "{} connections of processes outside the target were left out, by the process \
                 each is attributed to",
                self.outside
            ));
        }
        if self.unplaced > 0 {
            notes.push(format!(
                "{} connections were left out because their process had exited before it \
                 could be placed in or out of the target's tree: a child that lived less than \
                 one drain ({} ms) is not seen",
                self.unplaced,
                DRAIN_INTERVAL.as_millis()
            ));
        }
        if self.unowned > 0 {
            notes.push(format!(
                "{} connections were left out, as a loss, because no process was seen acting \
                 on them: the pid current at their close is whoever the reset or timer \
                 interrupted, which places them neither in nor out of the target's tree",
                self.unowned
            ));
        }
        if self.without_inode > 0 {
            notes.push(format!(
                "{} of {} connections carry no inode: a socket taken off a listen queue has \
                 no file, and therefore no inode, until `accept()` gives it one -- which is \
                 after it is established and is where this reads",
                self.without_inode, self.closed,
            ));
        }
        if self.without_cookie > 0 {
            notes.push(format!(
                "{} of {} connections carry no socket key: the kernel generates a socket cookie \
                 the first time something asks for one, and nothing ever asked for theirs, so \
                 they cannot be joined to `net.tcp.rtt` -- their inode is on the event instead",
                self.without_cookie, self.closed,
            ));
        }
        if self.still_open > 0 {
            // Not a loss. A connection open when the run ended has a real
            // lifetime with no end yet, so it is in no event.
            notes.push(format!(
                "{} connection{} still established when the run ended; their lifetimes have no \
                 end yet and are in no event",
                self.still_open,
                if self.still_open == 1 {
                    " was"
                } else {
                    "s were"
                },
            ));
        }
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} connections and removed {}; it ran here, over the drained \
                 records, not in the kernel",
                self.filtered_in, self.filtered_out,
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed() -> TcpLifetime {
        let mut bytes = [0u8; 128];
        bytes[0..8].copy_from_slice(&1_000_000u64.to_ne_bytes());
        bytes[8..16].copy_from_slice(&300_000_000u64.to_ne_bytes());
        bytes[16..24].copy_from_slice(&4242u64.to_ne_bytes());
        bytes[24..32].copy_from_slice(&909u64.to_ne_bytes());
        bytes[32..40].copy_from_slice(&4096u64.to_ne_bytes());
        bytes[40..48].copy_from_slice(&8192u64.to_ne_bytes());
        bytes[48..52].copy_from_slice(&7u32.to_ne_bytes());
        bytes[52..56].copy_from_slice(&9u32.to_ne_bytes());
        bytes[56..60].copy_from_slice(&[127, 0, 0, 1]);
        bytes[72..76].copy_from_slice(&2u32.to_ne_bytes());
        bytes[76..80].copy_from_slice(&u32::from(8080u16.to_be()).to_ne_bytes());
        bytes[80..84].copy_from_slice(&11u32.to_ne_bytes());
        bytes[84..88].copy_from_slice(&12u32.to_ne_bytes());
        bytes[88..92].copy_from_slice(b"curl");
        bytes[104..108].copy_from_slice(&[127, 0, 0, 2]);
        bytes[120..124].copy_from_slice(&u32::from(443u16.to_be()).to_ne_bytes());
        TcpLifetime::from_bytes(&bytes).expect("a full record")
    }

    /// A mode that cannot mean anything here is refused with the reason.
    #[test]
    fn a_sampled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Count,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a counted mode must be refused");
        };
        assert!(error.contains("gap between two"), "{error}");
    }

    /// A process target is refused, and the refusal says what the tracepoint
    /// cannot do rather than pretending the filter would be meaningful.
    #[test]
    fn a_process_target_is_served_and_a_cgroup_target_is_refused() {
        let prepared = || {
            Box::new(Prepare {
                mode: Mode::Trace,
                filter: None,
            })
        };
        assert!(prepared().attach(&Target::Pid(vec![1])).is_ok());
        let Err(error) = prepared().attach(&Target::Cgroup("/".into())) else {
            panic!("a connection is attributed to a process tree");
        };
        assert!(error.contains("--pid"), "{error}");
    }

    /// The tree holds its root and what descends from it, and not this
    /// test's parent.
    #[test]
    fn a_tree_holds_its_descendants_and_not_its_parent() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let me = std::process::id();
        let mut tree = Tree::new(&[me as i32]);
        assert_eq!(tree.holds(child.id()), Some(true));
        let parent = std::os::unix::process::parent_id();
        assert_eq!(tree.holds(parent), Some(false));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// Every filter field the catalog declares is on the event: a predicate
    /// over a field no event carries is false everywhere and empties the
    /// series silently.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&closed());
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"net.tcp.lifetime".parse().expect("a source id"))
            .expect("net.tcp.lifetime is in the catalog")
            .filter_fields;
        for field in declared {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
        assert!(declared.iter().any(|field| field == "daddr"));
    }

    /// The record's numbers reach the event unchanged, and the port is in the
    /// order a person reads.
    #[test]
    fn the_events_numbers_are_the_records_numbers() {
        let attributes = attributes(&closed());
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .map(|attr| attr.value.clone())
                .unwrap_or_else(|| panic!("no `{name}` attribute"))
        };
        assert_eq!(of("duration_ns").as_f64(), Some(300_000_000.0));
        assert_eq!(of("bytes_sent").as_f64(), Some(4096.0));
        assert_eq!(of("bytes_received").as_f64(), Some(8192.0));
        assert_eq!(of("sport").as_f64(), Some(8080.0));
        assert_eq!(of("saddr").to_string(), "127.0.0.1");
        assert_eq!(of("daddr").to_string(), "127.0.0.2");
        assert_eq!(of("dport").as_f64(), Some(443.0));
        assert_eq!(of("comm").to_string(), "curl");
    }

    /// A cookie of zero is not a socket: the kernel assigns one lazily, so a
    /// connection nothing ever dumped has none, and a key of zero would join
    /// every such connection to every other.
    #[test]
    fn a_connection_the_kernel_never_named_carries_no_socket_key() {
        let collector = LifetimeCollector::new(None);
        let mut anonymous = closed();
        anonymous.cookie = 0;
        assert!(collector.event(&anonymous, 0).keys.is_empty());
        assert_eq!(
            collector.event(&closed(), 0).keys,
            vec![KeyValue::Sock { cookie: 4242 }]
        );
        assert!(
            !attributes(&anonymous)
                .iter()
                .any(|attr| attr.name == "cookie"),
            "a cookie of zero must not be reported as a cookie"
        );
    }

    /// Under a process target only an owner places a connection: an
    /// `at_close` record whose interrupted task happens to be the target is
    /// not kept, one whose task is a stranger is not counted as outside, and
    /// both are a loss. An owned one is placed by its pid.
    #[test]
    fn an_unowned_connection_under_a_process_target_is_a_loss_not_a_placement() {
        let me = std::process::id();
        // The program writes the kernel's ids, which inside a container are
        // not the ones this test reads its own pid as.
        let kernel = |pid: u32| crate::bpf::pidns::kernel_tgid(pid).expect("a running process");
        let parent = kernel(std::os::unix::process::parent_id());
        let mut collector = LifetimeCollector::new(None);
        collector.tree = Some(Tree::new(&[me as i32]));

        let mut inside_by_chance = closed();
        inside_by_chance.pid = kernel(me);
        inside_by_chance.owned = 0;
        assert!(!collector.in_target(&inside_by_chance));

        let mut stranger_by_chance = closed();
        stranger_by_chance.pid = parent;
        stranger_by_chance.owned = 0;
        assert!(!collector.in_target(&stranger_by_chance));

        assert_eq!(collector.unowned, 2);
        assert_eq!(collector.outside, 0);
        assert_eq!(collector.losses().dropped, 2);
        assert!(collector
            .notes()
            .join(" ")
            .contains("2 connections were left out, as a loss"));

        let mut owned = closed();
        owned.pid = kernel(me);
        owned.owned = 1;
        assert!(collector.in_target(&owned));
        owned.pid = parent;
        assert!(!collector.in_target(&owned));
        assert_eq!(collector.outside, 1);

        // Machine-wide, nothing is placed and nothing is lost.
        let mut system = LifetimeCollector::new(None);
        assert!(system.in_target(&inside_by_chance));
        assert!(system.losses().is_clean());
    }

    /// A thread target keeps the connections that thread owns, by the
    /// kernel's id, and leaves its siblings' out.
    #[test]
    fn a_thread_target_keeps_its_own_connections_only() {
        let me = std::process::id() as i32;
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        assert!(prepared.attach(&Target::Tid(vec![me])).is_ok());
        let mut collector = LifetimeCollector::new(None);
        collector.thread = Some(4242);
        let mut own = closed();
        own.tid = 4242;
        own.owned = 1;
        assert!(collector.in_target(&own));
        own.tid = 4243;
        assert!(!collector.in_target(&own));
        assert_eq!(collector.outside, 1);
        own.owned = 0;
        assert!(!collector.in_target(&own));
        assert_eq!(collector.unowned, 1);
    }

    /// A connection still open at the end of the run is a fact about the
    /// window, not a loss: its lifetime has no end yet.
    #[test]
    fn a_connection_still_open_at_the_end_is_a_note_and_not_a_loss() {
        let mut collector = LifetimeCollector::new(None);
        collector.still_open = 3;
        let notes = collector.notes().join(" ");
        assert!(
            notes.contains("3 connections were still established"),
            "{notes}"
        );
        assert!(collector.losses().is_clean());
    }
}
