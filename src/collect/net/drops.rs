//! `net.drops` over a raw tracepoint: every packet the kernel threw away.
//!
//! This is the source that makes the rest of the family honest. `net.iface`
//! reports the frames a *device* lost and can see nothing that happens after
//! the driver hands a packet up; `net.qdisc` reports what a queueing discipline
//! held or dropped on the way out. Everything in between -- a checksum that did
//! not match, a socket that was not listening, a buffer that was full, a
//! filter that said no -- is `kfree_skb`, and since 5.17 it says which.
//!
//! The program is in `src/bpf/net_drops.rs`. This file is the part that belongs
//! to the family: what an event looks like, what is refused and what is counted
//! as lost.
//!
//! **The reason is the kernel's own name for it**, read out of this kernel's
//! BTF: `enum skb_drop_reason` grows every release and has been reorganised,
//! so a table compiled in here would be a property of the kernel this was
//! built against, and a mislabelled reason does not read as wrong -- it sends
//! a reader to fix a different fault. `reason` is the name without its
//! `SKB_DROP_REASON_` prefix (`NO_SOCKET`), and `reason_code` the number
//! beside it; a value the enum does not name -- a subsystem's own reasons sit
//! in the upper bits -- is carried as its number in both.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::Duration as StdDuration;

use crate::bpf::{DropProgram, PacketDrop};
use crate::collect::kernel::{KernelCounts, Lost, UserFilter};
use crate::collect::{not_built, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained.
///
/// Drops are bursty in exactly the way that fills a ring: a port scan, a flood,
/// a link that starts erroring produce thousands in a millisecond. Draining
/// often is cheap, and what it cannot absorb is counted rather than hidden.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// The ring the program writes into. Each record is 48 bytes plus an 8-byte
/// header, so this holds about nine thousand drops between drains.
const RING_BYTES: usize = 512 * 1024;

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// The request's predicate, which this collector applies itself: the
    /// resolver credits every `bpf` backend with filtering in the kernel and
    /// then leaves its events alone.
    pub filter: Option<FilterExpr>,
    /// Whether the request asked for `stack`: the kernel stack of each drop,
    /// interned in the kernel and symbolised through `/proc/kallsyms`.
    pub wants_stacks: bool,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot observe drops: a drop is an instant, and sampling one in a \
                 hundred of them would report a fraction of the packets the machine lost as \
                 though it were all of them",
                self.mode
            ));
        }
        if !matches!(target, Target::System) {
            return Err(not_built(format!(
                "a {} target cannot be served here: a dropped packet is dropped because nothing \
                 took delivery of it, so most drops belong to no process at all -- use --system",
                target.kind()
            )));
        }
        let mut collector = DropCollector::new(self.mode, self.filter);
        collector.wants_stacks = self.wants_stacks;
        Ok(Box::new(collector))
    }
}

pub struct DropCollector {
    /// `trace` is one event per drop; `count` is the single measurement of one
    /// window, totalled per series.
    mode: Mode,
    program: Option<DropProgram>,
    /// The program's loss counters, as last read (`collect::kernel`).
    counts: KernelCounts,
    losses: Losses,
    notes: Vec<String>,
    /// The program's counter is cumulative, so what is new is the difference.
    dropped: u64,
    /// The totals a `count` run accumulates, by what keys a series.
    counted: BTreeMap<(u64, u32, i32), Counted>,
    /// Drops seen with no socket, which is what makes `protocol` unanswerable.
    without_socket: u64,
    seen: u64,
    /// Whether this kernel's tracepoint says which socket a received packet
    /// would have gone to.
    receive_socket: bool,
    predicate: UserFilter,
    /// Stacks asked: each drop carries its kernel stack's id, and `function`,
    /// the symbol `location` is in.
    wants_stacks: bool,
    /// Kernel frames only, so the process it is built for does not matter.
    symbolizer: Option<crate::stack::Symbolizer>,
    /// Every stack the run resolved, root first, by the kernel's id.
    stacks: BTreeMap<u64, Vec<String>>,
    /// Drops whose stack the kernel could not walk, or whose id it evicted.
    unwalkable: u64,
    evicted_stacks: u64,
}

/// One `(socket, reason)` pair's drops over a counted window.
struct Counted {
    drops: u64,
    bytes: u64,
    last: PacketDrop,
}

impl DropCollector {
    fn new(mode: Mode, predicate: Option<FilterExpr>) -> Self {
        Self {
            mode,
            program: None,
            losses: Losses::default(),
            notes: Vec::new(),
            dropped: 0,
            counts: KernelCounts::default(),
            counted: BTreeMap::new(),
            without_socket: 0,
            seen: 0,
            receive_socket: true,
            predicate: UserFilter::new(predicate, "drops"),
            wants_stacks: false,
            symbolizer: None,
            stacks: BTreeMap::new(),
            unwalkable: 0,
            evicted_stacks: 0,
        }
    }

    /// The frames behind a drop's stack, once per distinct id; the id to put
    /// on the event, or `None` when there is none to point at.
    fn resolve_stack(&mut self, dropped: &PacketDrop) -> Option<u32> {
        if !self.wants_stacks {
            return None;
        }
        let Some(id) = dropped.stack_id() else {
            self.unwalkable += 1;
            return None;
        };
        if self.stacks.contains_key(&u64::from(id)) {
            return Some(id);
        }
        let program = self.program.as_ref()?;
        let Some(addresses) = program.stack(id) else {
            self.evicted_stacks += 1;
            return None;
        };
        let truncated = addresses.len() >= program.stack_depth();
        let symbolizer = self.symbolizer.as_mut()?;
        let stack = symbolizer.symbolize(&crate::stack::RawStack {
            kernel: addresses,
            user: Vec::new(),
            elsewhere: Vec::new(),
        });
        let mut frames: Vec<String> = stack
            .frames
            .iter()
            .rev()
            .map(crate::stack::Frame::label)
            .collect();
        // The leaf frames are this tool's: the tracepoint's trampoline and the
        // program itself. What the drop's path was ends at the free.
        while frames.last().is_some_and(|frame| {
            frame.starts_with("bpf_prog_") || frame.starts_with("bpf_trace_run")
        }) {
            frames.pop();
        }
        if truncated {
            frames.insert(0, "[truncated]".to_owned());
        }
        self.stacks.insert(u64::from(id), frames);
        Some(id)
    }

    /// The symbol `location` is in, from the same kernel table.
    fn function(&mut self, location: u64) -> Option<String> {
        let symbolizer = self.symbolizer.as_mut()?;
        let stack = symbolizer.symbolize(&crate::stack::RawStack {
            kernel: vec![location],
            user: Vec::new(),
            elsewhere: Vec::new(),
        });
        let frame = stack.frames.first()?;
        frame.symbol.as_ref()?;
        Some(frame.label())
    }

    fn counting(&self) -> bool {
        self.mode == Mode::Count
    }

    fn event(&self, dropped: &PacketDrop, drops: u64, bytes: u64, seq: u64) -> Event {
        Event {
            ts_ns: dropped.ts_ns,
            ts_err_ns: 0,
            kind: "net.drops".to_owned(),
            // The socket is the series when there is one. A cookie of zero is
            // not a socket: the kernel generates cookies lazily, so a socket
            // nothing has ever dumped has none, and keying such an event on
            // zero would join every anonymous drop to every other.
            keys: match dropped.cookie {
                0 => Vec::new(),
                cookie => vec![KeyValue::Sock { cookie }],
            },
            attrs: attributes(dropped, drops, bytes),
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
        program.drain(|dropped| batch.push(dropped), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the drop ring was shorter than one drop");
        }

        let mut index = 0;
        for dropped in batch {
            let mut candidate = self.event(&dropped, 1, u64::from(dropped.len), seq + index);
            if !self.predicate.keep(&candidate, 1) {
                continue;
            }
            self.seen += 1;
            if dropped.protocol().is_none() {
                self.without_socket += 1;
            }
            if self.wants_stacks {
                if let Some(function) = self.function(dropped.location) {
                    candidate.attrs.push(Attr::text("function", function));
                }
                if let Some(id) = self.resolve_stack(&dropped) {
                    candidate
                        .attrs
                        .push(Attr::uint(crate::emit::STACK_ID_ATTR, u64::from(id)));
                }
            }
            if self.counting() {
                let held = self
                    .counted
                    .entry((dropped.cookie, dropped.reason, dropped.stack))
                    .or_insert(Counted {
                        drops: 0,
                        bytes: 0,
                        last: dropped,
                    });
                held.drops += 1;
                held.bytes += u64::from(dropped.len);
                held.last = dropped;
                continue;
            }
            events.push(candidate);
            index += 1;
        }
    }

    /// Reads the program's own drop counter, which is the only place a packet
    /// the ring could not hold is recorded.
    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        self.counts
            .absorb(&mut self.losses, [Lost::skipped_runs(program.skipped())]);
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note_times(
                "the drop ring was full: the machine lost packets faster than they were drained, \
                 so this run's own count of them is short",
                dropped - self.dropped,
            );
            self.dropped = dropped;
        }
    }
}

/// The IANA protocol numbers this build names.
///
/// Unlike a kernel enum these are an assigned-numbers registry: 6 has been TCP
/// since 1981 and cannot be renumbered, so naming them here is not the kind of
/// constant that goes stale. Anything else is reported as its number.
fn protocol_name(protocol: Option<u32>) -> String {
    match protocol {
        // Not a protocol this run could not identify -- a drop that had no
        // socket to read one from, which is most receive-side drops and every
        // drop of a packet nothing was listening for. Named rather than left
        // out so that a predicate over `protocol` can select exactly them.
        None => "no-socket".to_owned(),
        Some(1) => "icmp".to_owned(),
        Some(6) => "tcp".to_owned(),
        Some(17) => "udp".to_owned(),
        Some(58) => "icmpv6".to_owned(),
        Some(132) => "sctp".to_owned(),
        Some(other) => format!("proto {other}"),
    }
}

/// The name this kernel gives a drop reason, without the enum's prefix, or the
/// number when it gives none.
///
/// Looked up once per value and kept: a drop storm is thousands of events a
/// millisecond and a handful of distinct reasons, and the enum is walked
/// linearly.
fn reason_name(value: u32) -> String {
    static NAMES: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    let names = NAMES.get_or_init(Default::default);
    let mut names = names
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    names
        .entry(value)
        .or_insert_with(|| {
            crate::bpf::kernel_btf()
                .ok()
                .and_then(|btf| btf.enum_variant("skb_drop_reason", i64::from(value)))
                .map(|name| {
                    name.strip_prefix("SKB_DROP_REASON_")
                        .unwrap_or(name)
                        .to_owned()
                })
                .unwrap_or_else(|| value.to_string())
        })
        .clone()
}

/// What an event carries beside its value.
///
/// `reason` and `protocol` are here because the catalog entry declares them as
/// filter fields: a predicate over a field no event carries is false
/// everywhere, which empties the series and looks exactly like a machine that
/// dropped nothing.
fn attributes(dropped: &PacketDrop, drops: u64, bytes: u64) -> Vec<Attr> {
    let mut attrs = vec![
        Attr::uint("drops", drops),
        // The kernel's own name for `enum skb_drop_reason`'s value, read from
        // this kernel's BTF, and the value beside it.
        Attr::text("reason", reason_name(dropped.reason)),
        Attr::uint("reason_code", u64::from(dropped.reason)),
        Attr::text("protocol", protocol_name(dropped.protocol())),
        Attr::uint("bytes", bytes),
        // Where in the kernel the packet died. Not symbolised: this is a kernel
        // address and resolving it needs `/proc/kallsyms`, which is a different
        // privilege and a different question.
        Attr::uint("location", dropped.location),
    ];
    if dropped.family != 0 {
        attrs.push(Attr::uint("family", u64::from(dropped.family)));
    }
    if dropped.cookie != 0 {
        attrs.push(Attr::uint("cookie", dropped.cookie));
    }
    if dropped.inode != 0 {
        // The socket's inode, which is what `/proc/<pid>/fd` shows as
        // `socket:[n]`. Carried beside the cookie because a socket has an inode
        // from the moment it exists and gets a cookie only when something asks.
        attrs.push(Attr::uint("inode", dropped.inode));
    }
    attrs
}

impl Collector for DropCollector {
    fn value_attr(&self) -> &'static str {
        "drops"
    }

    fn start(&mut self) -> Result<(), String> {
        let program = DropProgram::load(RING_BYTES, self.wants_stacks)?;
        if self.wants_stacks {
            // The kernel's table, read before the window rather than inside
            // the first drain (`stack::prime_kernel_symbols`).
            crate::stack::prime_kernel_symbols();
            let symbolizer = crate::stack::Symbolizer::for_process(std::process::id() as i32);
            if !symbolizer.kernel_symbols_available() {
                self.notes.push(
                    "/proc/kallsyms gives no addresses to this process (kptr_restrict), so the \
                     stacks' frames are addresses rather than names"
                        .to_owned(),
                );
            }
            self.symbolizer = Some(symbolizer);
        }
        self.receive_socket = program.has_receive_socket();
        self.program = Some(program);
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
        self.program = None;
        if self.counting() {
            let seq = u64::MAX / 2;
            let counted = std::mem::take(&mut self.counted);
            for (index, held) in counted.into_values().enumerate() {
                trailing.push(self.event(&held.last, held.drops, held.bytes, seq + index as u64));
            }
        }
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn stacks(&self) -> BTreeMap<u64, Vec<String>> {
        self.stacks.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        if self.unwalkable > 0 || self.evicted_stacks > 0 {
            notes.push(format!(
                "{} drops carry no stack: the kernel could not walk {} of them and had evicted \
                 the stack of {} from its map",
                self.unwalkable + self.evicted_stacks,
                self.unwalkable,
                self.evicted_stacks
            ));
        }
        if !self.receive_socket {
            notes.push(
                "this kernel's `kfree_skb` does not say which socket a received packet would \
                 have gone to, so a receive-side drop here carries no socket at all"
                    .to_owned(),
            );
        }
        if self.without_socket > 0 {
            notes.push(format!(
                "{} of {} drops had no socket to read a protocol from and are reported as \
                 `no-socket`; a packet nothing was listening for is the commonest case and is \
                 exactly the one a protocol cannot be recovered for without reading the packet \
                 itself",
                self.without_socket, self.seen,
            ));
        }

        notes.push(
            "`reason` is the running kernel's own name for its `enum skb_drop_reason` value, \
             read from its BTF, and `reason_code` the number beside it"
                .to_owned(),
        );
        notes.extend(self.predicate.note());
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dropped() -> PacketDrop {
        // Built through the decoder rather than by naming private fields, which
        // is the same path a real record takes.
        let mut bytes = [0u8; 56];
        bytes[0..8].copy_from_slice(&1_000u64.to_ne_bytes());
        bytes[8..12].copy_from_slice(&4u32.to_ne_bytes());
        bytes[12..16].copy_from_slice(&17u32.to_ne_bytes());
        bytes[16..20].copy_from_slice(&142u32.to_ne_bytes());
        bytes[20..24].copy_from_slice(&2u32.to_ne_bytes());
        bytes[24..32].copy_from_slice(&4242u64.to_ne_bytes());
        bytes[32..40].copy_from_slice(&99u64.to_ne_bytes());
        bytes[40..48].copy_from_slice(&0xffff_ffff_8000_0000u64.to_ne_bytes());
        PacketDrop::from_bytes(&bytes).expect("a full record")
    }

    /// A mode that cannot mean anything here is refused with the reason.
    #[test]
    fn a_sampled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Sample {
                freq: "997hz".parse().expect("a frequency"),
            },
            filter: None,
            wants_stacks: false,
        });
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a sampled mode must be refused");
        };
        assert!(error.contains("cannot observe drops"), "{error}");
    }

    /// A process target is refused, and the refusal says why rather than
    /// filtering to a process that would see almost nothing: a packet is
    /// dropped precisely because no process took delivery of it.
    #[test]
    fn a_process_target_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
            wants_stacks: false,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("a pid names nothing most drops belong to");
        };
        assert!(error.contains("belong to no process"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// Every filter field the catalog offers must be on the events.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&dropped(), 1, 142);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"net.drops".parse().expect("a source id"))
            .expect("net.drops is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty(), "the entry declares filter fields");
        for field in declared {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// The reason is named by this kernel's own BTF, and the name is the one
    /// the kernel's source spells: `SKB_DROP_REASON_NO_SOCKET` is found by its
    /// name, turned back into its number, and that number named again.
    #[test]
    fn a_drop_reason_is_named_by_the_running_kernel() {
        let Ok(btf) = crate::bpf::kernel_btf() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let Some(no_socket) = (0..1024).find(|value| {
            btf.enum_variant("skb_drop_reason", *value) == Some("SKB_DROP_REASON_NO_SOCKET")
        }) else {
            panic!("this kernel's `enum skb_drop_reason` has no NO_SOCKET below 1024");
        };
        assert_eq!(reason_name(no_socket as u32), "NO_SOCKET");
        // A value the enum does not name is its number, not a guess.
        assert_eq!(reason_name(0xdead_0000), (0xdead_0000u32).to_string());
    }

    /// A drop with no socket says so under the `protocol` field rather than
    /// carrying no field at all, so that a predicate can select exactly those
    /// and a predicate over a real protocol excludes them.
    #[test]
    fn a_drop_with_no_socket_is_selectable_by_the_protocol_field() {
        assert_eq!(protocol_name(None), "no-socket");
        assert_eq!(protocol_name(Some(6)), "tcp");
        assert_eq!(protocol_name(Some(17)), "udp");
        assert_eq!(protocol_name(Some(41)), "proto 41");
    }

    /// A cookie of zero is not a socket, and keying an event on it would join
    /// every anonymous drop on the machine to every other.
    #[test]
    fn a_drop_whose_socket_has_no_cookie_carries_no_socket_key() {
        let collector = DropCollector::new(Mode::Trace, None);
        let mut anonymous = dropped();
        anonymous.cookie = 0;
        assert!(collector.event(&anonymous, 1, 0, 0).keys.is_empty());
        assert_eq!(
            collector.event(&dropped(), 1, 0, 0).keys,
            vec![KeyValue::Sock { cookie: 4242 }]
        );
    }

    /// The run says whose name the reason is: the kernel's, from its BTF, so a
    /// reader looks for it in the kernel's headers rather than in this tool.
    #[test]
    fn the_run_says_the_reason_is_the_kernels_own_name() {
        let collector = DropCollector::new(Mode::Trace, None);
        let notes = collector.notes().join(" ");
        assert!(notes.contains("skb_drop_reason"), "{notes}");
    }
}
