//! `net.request`: one event per request a server declared, by state.
//!
//! `src/bpf/request.rs` holds the programs and the reasoning. What this file
//! decides is what a program has to declare, which probes are the request's,
//! and what the event says: the request is the series, by the id the server
//! chose, and the four states its serving task was in add up to its life.
//!
//! **Why the event is published at the end.** The states are only known once
//! the request is over, and a request's rows in a decomposition are read off
//! one event per request. The birth is seen too -- the kernel publishes it --
//! and is used here to say which requests began and never ended, rather than
//! to make an event of its own that half of every emit would have to skip.
//!
//! **And why it can be published later than the end.** A request whose probes
//! name its socket has two more rows, `net_rx` and `net_tx`
//! (`src/bpf/request_net.rs`), and the second of them runs past the end: a
//! send returns when the response is queued, and it has left the stack only
//! when the last byte is transmitted. When it had not by the end, the kernel
//! says so on the end's record and publishes a drain record when it has; the
//! request is held here until then. Its event is still stamped at its end.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use crate::bpf::usdt::{self, Site};
use crate::bpf::{request_key, RequestCounts, RequestProgram, RequestRecord};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// The probes a program declares its requests with.
pub const START: &str = "request__start";
pub const DONE: &str = "request__done";

const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);
/// One record per request and per birth, 128 bytes with its header.
const RING_BYTES: usize = 256 * 1024;

pub struct Prepare {
    pub mode: Mode,
    /// The request's own target: at attach time a launched program has not
    /// execed, and its pid says nothing about what it will run.
    pub target: Target,
    pub filter: Option<FilterExpr>,
    /// Whether the backend follows the socket a probe names: `bpf:usdt` does,
    /// `bpf:usdt:threads` is the fallback for a kernel whose socket functions
    /// cannot be traced.
    pub sockets: bool,
}

/// The executable a target runs, which is where its probes are looked for.
fn executable(target: &Target, pid: i32) -> Result<PathBuf, String> {
    match target {
        Target::Launch(argv) => {
            let program = argv
                .first()
                .ok_or("a launch with no program names nothing to put a probe in")?;
            if program.contains('/') {
                return Ok(PathBuf::from(program));
            }
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|dir| dir.join(program))
                .find(|candidate| candidate.is_file())
                .ok_or_else(|| format!("`{program}` is not on the PATH"))
        }
        _ => std::fs::read_link(format!("/proc/{pid}/exe"))
            .map_err(|error| format!("cannot read what process {pid} runs: {error}")),
    }
}

/// The start and done sites an object declares, or a refusal that names what
/// it does declare.
fn request_sites(path: &Path) -> Result<(Vec<Site>, Vec<Site>), String> {
    let object = usdt::Object::read(path)?;
    let sites = usdt::sites(&object)?;
    let named = |name: &str| -> Vec<Site> {
        sites
            .iter()
            .filter(|site| site.name == name)
            .cloned()
            .collect()
    };
    let (starts, ends) = (named(START), named(DONE));
    if starts.is_empty() || ends.is_empty() {
        let carried: Vec<String> = sites.iter().map(Site::qualified).collect();
        return Err(format!(
            "{} declares no request: `net.request` follows a request from a `{START}(id)` probe \
             to a `{DONE}(id)` probe the program fires itself, any provider, and this one {}. \
             Nothing is inferred from the connection instead -- see `docs/scenarios/vertical.md`",
            path.display(),
            if carried.is_empty() {
                "carries no USDT probe at all".to_owned()
            } else {
                format!("carries {}", carried.join(", "))
            }
        ));
    }
    Ok((starts, ends))
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot follow a request: its life is the gap between the server's two \
                 probes, and its states are only known at the end",
                self.mode
            ));
        }
        let pid = match target {
            Target::Pid(pids) => match pids[..] {
                [pid] => pid,
                _ => {
                    return Err(format!(
                        "`net.request` follows one process per run, and this target names {}",
                        pids.len()
                    ))
                }
            },
            other => {
                return Err(format!(
                    "`net.request` puts its probes in one process, and a {} target names none",
                    other.kind()
                ))
            }
        };
        let path = executable(&self.target, pid)?;
        let (starts, ends) = request_sites(&path)?;
        let declares_socket = starts.iter().chain(&ends).any(|site| site.args.len() >= 2);
        // Loaded here, before the run's window opens, as every other eBPF
        // source here loads: an fentry attach patches kernel text, and six of
        // them cost this process about a hundred milliseconds of system time
        // that is the attach's, not the measurement's.
        let program = RequestProgram::load(&starts, &ends, pid, RING_BYTES, self.sockets)?;
        let mut collector = RequestCollector::new(path, self.sockets, declares_socket, self.filter);
        collector.program = Some(program);
        Ok(Box::new(collector))
    }
}

pub struct RequestCollector {
    id: CollectorId,
    object: PathBuf,
    program: Option<RequestProgram>,
    losses: Losses,
    predicate: Option<FilterExpr>,
    counts: RequestCounts,
    skipped: u64,
    /// Requests born and not yet ended, by id, with the thread and the birth.
    open: BTreeMap<u64, (u32, u64)>,
    /// Ended requests whose response had not left the stack, by id and end.
    awaiting: BTreeMap<(u64, u64), RequestRecord>,
    /// Drains that arrived before their request's end record, which the
    /// transmit program can publish first: the end arms the socket, and the
    /// last byte can leave on another cpu before the end has published.
    drains: BTreeMap<(u64, u64), u64>,
    net: NetCounts,
    ended: u64,
    mismatched: u64,
    anomalies: u64,
    filtered_out: u64,
    /// Whether the socket programs were attached, kept past the program's end
    /// so that the run's account of itself can say.
    follows_sockets: bool,
    /// Whether any probe names a socket.
    declares_socket: bool,
    /// Whether this backend follows it.
    sockets: bool,
}

/// What was followed of the requests' sockets, for the run's account of itself.
#[derive(Debug, Default)]
struct NetCounts {
    /// Ended requests whose probes named a socket.
    declared: u64,
    /// Of those, the ones with both rows measured.
    both: u64,
    /// The reasons a row was not, with how many requests each.
    rx_unknown: BTreeMap<&'static str, u64>,
    tx_unknown: BTreeMap<&'static str, u64>,
    /// Responses still in the stack when the run ended.
    undrained: u64,
    /// Drain records that matched no ended request.
    orphan_drains: u64,
}

/// The network rows of one request, when its probes named a socket.
#[derive(Debug, Clone, Copy, PartialEq)]
struct NetRows {
    /// From the first byte to the birth.
    rx_queued: Option<u64>,
    /// From the end to the last byte leaving.
    tx_drain: Option<u64>,
}

fn net_rows(record: &RequestRecord) -> NetRows {
    let rx_queued = (record.rx_unknown().is_none() && record.first_byte_ns != 0)
        .then(|| record.born_ns.saturating_sub(record.first_byte_ns));
    let tx_drain = (record.tx_unknown().is_none() && record.drained_ns != 0)
        .then(|| record.drained_ns.saturating_sub(record.done_ns));
    NetRows {
        rx_queued,
        tx_drain,
    }
}

/// The event for one ended request.
fn event(id: &CollectorId, record: &RequestRecord, seq: u64) -> Event {
    let total = record.done_ns.saturating_sub(record.born_ns);
    let mut attrs = vec![
        Attr::uint("total_ns", total),
        Attr::uint("oncpu_ns", record.oncpu_ns),
        Attr::uint("runq_ns", record.runq_ns),
        Attr::uint("sleep_ns", record.sleep_ns),
        Attr::uint("disk_ns", record.disk_ns),
        Attr::uint("born_ns", record.born_ns),
        Attr::uint("blocks", u64::from(record.blocks)),
        Attr::uint("preempted", u64::from(record.preempted)),
        Attr::uint("holder_ns", record.holder_ns),
        Attr::uint("holder_tid", u64::from(record.holder_tid)),
        Attr::uint("tid", u64::from(record.tid)),
        Attr::uint("pid", u64::from(record.tgid)),
        Attr::text("comm", record.comm()),
        Attr::uint("count", 1),
    ];
    if record.anomalies > 0 {
        // Stretches whose end the programs could not place: their time is in
        // no state, so the four do not add up to the total by that much.
        attrs.push(Attr::uint("anomalies", u64::from(record.anomalies)));
    }
    if record.flags & crate::bpf::ID_MISMATCH != 0 {
        attrs.push(Attr::text("declared", request_key(record.declared_id)));
    }
    let mut keys = vec![
        KeyValue::Req {
            id: request_key(record.id),
        },
        KeyValue::task(record.tid as i32),
    ];
    if record.cookie != 0 {
        // The rows below are read by this key, not by the request's id:
        // nothing on the receive or the transmit path runs in the serving
        // thread, and what ties them to the request is the socket.
        keys.push(KeyValue::Sock {
            cookie: record.cookie,
        });
        attrs.push(Attr::uint("rx_wait_ns", record.rx_wait_ns));
        attrs.push(Attr::uint("tx_wait_ns", record.tx_wait_ns));
        let rows = net_rows(record);
        if let Some(queued) = rows.rx_queued {
            attrs.push(Attr::uint("first_byte_ns", record.first_byte_ns));
            attrs.push(Attr::uint("rx_queued_ns", queued));
            attrs.push(Attr::uint("net_rx_ns", queued + record.rx_wait_ns));
        }
        if let Some(drain) = rows.tx_drain {
            attrs.push(Attr::uint("drained_ns", record.drained_ns));
            attrs.push(Attr::uint("tx_drain_ns", drain));
            attrs.push(Attr::uint("net_tx_ns", record.tx_wait_ns + drain));
        }
        if let (Some(queued), Some(drain)) = (rows.rx_queued, rows.tx_drain) {
            // First byte in to last byte out: the whole the six rows tile.
            attrs.push(Attr::uint("span_ns", queued + total + drain));
        }
    }
    Event {
        ts_ns: record.done_ns,
        ts_err_ns: 0,
        kind: "net.request".to_owned(),
        keys,
        attrs,
        collector_id: id.clone(),
        seq,
    }
}

impl RequestCollector {
    fn new(
        object: PathBuf,
        sockets: bool,
        declares_socket: bool,
        predicate: Option<FilterExpr>,
    ) -> Self {
        RequestCollector {
            id: CollectorId::new(if sockets {
                "net.request/bpf:usdt"
            } else {
                "net.request/bpf:usdt:threads"
            }),
            sockets,
            program: None,
            losses: Losses::default(),
            predicate,
            counts: RequestCounts::default(),
            skipped: 0,
            open: BTreeMap::new(),
            awaiting: BTreeMap::new(),
            drains: BTreeMap::new(),
            net: NetCounts::default(),
            ended: 0,
            mismatched: 0,
            anomalies: 0,
            filtered_out: 0,
            follows_sockets: sockets,
            declares_socket,
            object,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let mut records = Vec::new();
        let mut undecoded = 0;
        program.drain(|record| records.push(record), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the request ring was shorter than one request");
        }
        self.absorb(records, events, seq);
    }

    /// What the ring carried, in the order it carried it: births, ends and
    /// drains, a drain possibly before the end it belongs to.
    fn absorb(&mut self, records: Vec<RequestRecord>, events: &mut Vec<Event>, seq: u64) {
        for record in records {
            if record.is_birth() {
                self.open.insert(record.id, (record.tid, record.born_ns));
                continue;
            }
            if record.is_drain() {
                let key = (record.id, record.done_ns);
                match self.awaiting.remove(&key) {
                    Some(mut ended) => {
                        ended.drained_ns = record.drained_ns;
                        self.publish(ended, events, seq);
                    }
                    None => {
                        self.drains.insert(key, record.drained_ns);
                    }
                }
                continue;
            }
            self.open.remove(&record.id);
            self.ended += 1;
            if record.flags & crate::bpf::ID_MISMATCH != 0 {
                self.mismatched += 1;
            }
            self.anomalies += u64::from(record.anomalies);
            let mut record = record;
            if record.tx_pending() {
                let key = (record.id, record.done_ns);
                match self.drains.remove(&key) {
                    Some(drained) => record.drained_ns = drained,
                    None => {
                        self.awaiting.insert(key, record);
                        continue;
                    }
                }
            }
            self.publish(record, events, seq);
        }
    }

    /// The end of the run: a request still waiting for its drain is published
    /// without one.
    fn flush(&mut self, events: &mut Vec<Event>) {
        let held = std::mem::take(&mut self.awaiting);
        for record in held.into_values() {
            self.publish(record, events, u64::MAX / 2);
        }
        self.net.orphan_drains += self.drains.len() as u64;
        self.drains.clear();
    }

    /// Makes the event of one ended request, whole or as whole as it will get.
    fn publish(&mut self, record: RequestRecord, events: &mut Vec<Event>, seq: u64) {
        if record.declared_socket() {
            self.net.declared += 1;
            if let Some(why) = record.rx_unknown() {
                *self.net.rx_unknown.entry(why).or_default() += 1;
            } else if record.first_byte_ns == 0 {
                *self
                    .net
                    .rx_unknown
                    .entry("its first byte was not seen")
                    .or_default() += 1;
            }
            if let Some(why) = record.tx_unknown() {
                *self.net.tx_unknown.entry(why).or_default() += 1;
            } else if record.drained_ns == 0 {
                self.net.undrained += 1;
            }
            let rows = net_rows(&record);
            if rows.rx_queued.is_some() && rows.tx_drain.is_some() {
                self.net.both += 1;
            }
        }
        let event = event(&self.id, &record, seq + events.len() as u64);
        if let Some(predicate) = &self.predicate {
            if !crate::filter::matches(predicate, &event) {
                self.filtered_out += 1;
                return;
            }
        }
        events.push(event);
    }

    fn absorb_counters(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let counts = program.counts();
        let grew = |now: u64, before: u64| now.saturating_sub(before);
        let ring = grew(counts.ring_full, self.counts.ring_full);
        if ring > 0 {
            self.losses.dropped += ring;
            self.losses
                .note("the request ring was full: requests ended faster than they were drained");
        }
        let storage = grew(counts.no_storage, self.counts.no_storage);
        if storage > 0 {
            self.losses.dropped += storage;
            self.losses.note(
                "a request was born in a task the kernel would not give storage to, so it was \
                 never followed",
            );
        }
        let sleepers = grew(counts.sleepers_full, self.counts.sleepers_full);
        if sleepers > 0 {
            self.losses.dropped += sleepers;
            self.losses.note(
                "more serving tasks were asleep at once than the sleeper map holds, so a \
                 sleep's wakeup was not seen and the wait after it is counted asleep",
            );
        }
        self.counts = counts;
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note(
                "the kernel skipped a run of these programs on a cpu where one was already \
                 running: a switch nobody saw is a stretch whose state is unknown",
            );
            self.skipped = skipped;
        }
    }
}

impl RequestCollector {
    /// Whether the socket programs are attached.
    fn attached(&self) -> bool {
        match &self.program {
            Some(program) => program.follows_sockets(),
            None => self.follows_sockets,
        }
    }

    /// What the run could say about the requests' network rows.
    fn socket_notes(&self) -> Vec<String> {
        let plural = |n: u64| if n == 1 { "" } else { "s" };
        if !self.declares_socket {
            return vec![format!(
                "the probes name no socket, so no request has `net_rx` or `net_tx`, and a sleep \
                 on the socket is counted in `sleep_ns`: `{START}(id, fd)` and `{DONE}(id, fd)`, \
                 with the descriptor the request was received and answered on, is what follows \
                 it"
            )];
        }
        if !self.sockets {
            return vec![format!(
                "the probes name a socket, and this backend does not follow one: no request has \
                 `net_rx` or `net_tx`, and a sleep on the socket is counted in `sleep_ns`. `{}` \
                 is the backend that does, on a kernel that can trace {}",
                "bpf:usdt",
                RequestProgram::socket_functions().join(", ")
            )];
        }
        if !self.attached() {
            return vec![
                "the probes name a socket and the socket programs are not attached: no request \
                 has `net_rx` or `net_tx`, and a sleep on the socket is counted in `sleep_ns`"
                    .to_owned(),
            ];
        }
        let mut notes = vec![format!(
            "{} of {} request{} whose probes named a socket have both network rows, read by the \
             socket's cookie rather than the request's id -- `net_rx` from its first byte \
             reaching the socket, over `{}` and `{}`, and `net_tx` to its last byte leaving, \
             over `{}` and `{}` -- which is KEY evidence, not the IDENTITY of the rows read off \
             the serving thread",
            self.net.both,
            self.net.declared,
            plural(self.net.declared),
            RequestProgram::socket_functions()[0],
            RequestProgram::socket_functions()[2],
            RequestProgram::socket_functions()[3],
            RequestProgram::socket_functions()[1],
        )];
        for (why, count) in &self.net.rx_unknown {
            notes.push(format!(
                "{count} request{} {} no `net_rx`: {why}",
                plural(*count),
                if *count == 1 { "has" } else { "have" },
            ));
        }
        for (why, count) in &self.net.tx_unknown {
            notes.push(format!(
                "{count} request{} {} no `net_tx`: {why}",
                plural(*count),
                if *count == 1 { "has" } else { "have" },
            ));
        }
        if self.net.undrained > 0 {
            notes.push(format!(
                "{} response{} had not left the stack when the run ended, so {} no `net_tx` \
                 rather than a short one",
                self.net.undrained,
                plural(self.net.undrained),
                if self.net.undrained == 1 {
                    "its request has"
                } else {
                    "their requests have"
                },
            ));
        }
        if self.net.orphan_drains > 0 {
            notes.push(format!(
                "{} drain record{} matched no ended request",
                self.net.orphan_drains,
                plural(self.net.orphan_drains),
            ));
        }
        notes
    }
}

impl Collector for RequestCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "total_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_counters();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        self.absorb_counters();
        if let Some(program) = &self.program {
            self.follows_sockets = program.follows_sockets();
        }
        // The programs stay attached until the collector is dropped, after the
        // run has read its own cost: unpatching six fentry trampolines costs
        // this process some 60 ms of system time, which is the detach's and
        // not the measurement's -- and every other source here detaches at
        // the drop too.
        //
        // A response still in the stack at the end of the run: the request is
        // whole but for its `net_tx`, which is left out rather than cut short.
        self.flush(&mut trailing);
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = vec![format!(
            "{} request{} born and {} ended in {}, followed by the id the program declared at \
             `{START}` and carried in the serving task's own storage until `{DONE}`; a block \
             request submitted while serving one carries the id as `req:<id>` in `io.latency`",
            self.counts.births,
            if self.counts.births == 1 {
                " was"
            } else {
                "s were"
            },
            self.ended,
            self.object.display(),
        )];
        if !self.open.is_empty() {
            let named: Vec<String> = self
                .open
                .iter()
                .take(5)
                .map(|(id, (tid, _))| format!("req:{} in task {tid}", request_key(*id)))
                .collect();
            notes.push(format!(
                "{} request{} never ended in the run ({}{}); a request with no end has no total \
                 and is in no event",
                self.open.len(),
                if self.open.len() == 1 { "" } else { "s" },
                named.join(", "),
                if self.open.len() > named.len() {
                    ", ..."
                } else {
                    ""
                },
            ));
        }
        if self.counts.unborn > 0 {
            notes.push(format!(
                "{} `{DONE}` hit{} a task carrying no request: begun before the probes \
                 attached, or never declared at `{START}`",
                self.counts.unborn,
                if self.counts.unborn == 1 { "" } else { "s" },
            ));
        }
        if self.counts.restarted > 0 {
            notes.push(format!(
                "{} `{START}` hit{} a task still carrying a request that never ended; the old one \
                 was replaced and is among those that never ended",
                self.counts.restarted,
                if self.counts.restarted == 1 { "" } else { "s" },
            ));
        }
        if self.mismatched > 0 {
            notes.push(format!(
                "{} request{} ended under a different id from the one {} born with; each event \
                 carries the id it was born with as the key and the other as `declared`",
                self.mismatched,
                if self.mismatched == 1 { "" } else { "s" },
                if self.mismatched == 1 {
                    "it was"
                } else {
                    "they were"
                },
            ));
        }
        if self.anomalies > 0 {
            notes.push(format!(
                "{} off-cpu stretch{} could not be placed -- the cpu switched without the \
                 programs seeing it -- so {} time is in no state and the states add up to less \
                 than the total",
                self.anomalies,
                if self.anomalies == 1 { "" } else { "es" },
                if self.anomalies == 1 { "its" } else { "their" },
            ));
        }
        if self.filtered_out > 0 {
            notes.push(format!(
                "the filter removed {} requests, here over the drained records rather than in \
                 the kernel",
                self.filtered_out
            ));
        }
        notes.extend(self.socket_notes());
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(flags: u32) -> RequestRecord {
        with_socket(flags, 0, [0; 4], 0)
    }

    /// A record with the network half filled: the cookie, the first byte, the
    /// two waits, the drain and the flags, at the offsets the kernel writes.
    fn with_socket(flags: u32, cookie: u64, net: [u64; 4], net_flags: u32) -> RequestRecord {
        let mut bytes = [0u8; 168];
        let put = |bytes: &mut [u8; 168], at: usize, value: u64| {
            bytes[at..at + 8].copy_from_slice(&value.to_ne_bytes())
        };
        put(&mut bytes, 0, 0xa3f1);
        put(&mut bytes, 8, 1_000_000);
        put(&mut bytes, 16, 43_000_000);
        put(&mut bytes, 24, 2_000_000);
        put(&mut bytes, 32, 500_000);
        put(&mut bytes, 40, 40_000_000);
        put(&mut bytes, 48, 0);
        bytes[64..68].copy_from_slice(&8820u32.to_ne_bytes());
        bytes[72..76].copy_from_slice(&8821u32.to_ne_bytes());
        bytes[84..88].copy_from_slice(&flags.to_ne_bytes());
        put(&mut bytes, 88, 0xbeef);
        put(&mut bytes, 120, cookie);
        for (slot, value) in net.iter().enumerate() {
            put(&mut bytes, 128 + slot * 8, *value);
        }
        bytes[160..164].copy_from_slice(&net_flags.to_ne_bytes());
        RequestRecord::from_bytes(&bytes).expect("a full record")
    }

    const DECLARED: u32 = 1;
    const RX_UNSEEN: u32 = 4;
    const TX_PENDING: u32 = 8;
    const DRAIN: u32 = 4;

    fn collector() -> RequestCollector {
        RequestCollector::new(PathBuf::from("/srv"), true, true, None)
    }

    fn value(event: &Event, name: &str) -> Option<f64> {
        event.attr(name).and_then(|value| value.as_f64())
    }

    /// The span is the first byte to the last byte out, and the six rows tile
    /// it: `net_rx` and `net_tx` are each a stretch outside the life plus the
    /// socket's waits inside it, which are not in `sleep_ns`.
    #[test]
    fn a_followed_socket_adds_both_rows_and_the_span_they_tile() {
        // Born at 1 ms, ended at 43 ms; first byte at 0.4 ms, drained at 45 ms.
        let record = with_socket(0, 77, [400_000, 1_000, 2_000, 45_000_000], DECLARED);
        let event = event(&CollectorId::new("t"), &record, 0);
        assert!(event.keys.contains(&KeyValue::Sock { cookie: 77 }));
        assert_eq!(value(&event, "rx_queued_ns"), Some(600_000.0));
        assert_eq!(value(&event, "net_rx_ns"), Some(601_000.0));
        assert_eq!(value(&event, "tx_drain_ns"), Some(2_000_000.0));
        assert_eq!(value(&event, "net_tx_ns"), Some(2_002_000.0));
        assert_eq!(value(&event, "span_ns"), Some(44_600_000.0));
    }

    /// A first byte that was not seen is no `net_rx`, and no span: a row the
    /// programs could not measure is absent, not zero.
    #[test]
    fn an_unseen_first_byte_is_no_row_rather_than_a_zero() {
        let record = with_socket(0, 77, [0, 0, 0, 45_000_000], DECLARED | RX_UNSEEN);
        let event = event(&CollectorId::new("t"), &record, 0);
        assert_eq!(value(&event, "net_rx_ns"), None);
        assert_eq!(value(&event, "span_ns"), None);
        assert_eq!(value(&event, "net_tx_ns"), Some(2_000_000.0));
    }

    /// An end whose response had not left waits for its drain, whichever of
    /// the two the ring carries first, and is published once, whole.
    #[test]
    fn an_end_and_its_drain_are_joined_in_either_order() {
        let ended = with_socket(0, 77, [400_000, 0, 5_000, 0], DECLARED | TX_PENDING);
        let mut drain = with_socket(DRAIN, 77, [0, 0, 0, 51_000_000], DECLARED);
        drain.done_ns = ended.done_ns;
        for order in [vec![ended, drain], vec![drain, ended]] {
            let mut collector = collector();
            let mut events = Vec::new();
            collector.absorb(order, &mut events, 0);
            assert_eq!(events.len(), 1);
            assert_eq!(value(&events[0], "tx_drain_ns"), Some(8_000_000.0));
            assert_eq!(value(&events[0], "net_tx_ns"), Some(8_005_000.0));
            assert_eq!(events[0].ts_ns, 43_000_000, "stamped at its end");
            assert!(collector.awaiting.is_empty() && collector.drains.is_empty());
        }
    }

    /// A response still in the stack when the run ends: the request is
    /// published without `net_tx`, and the run counts it.
    #[test]
    fn a_response_that_never_left_is_published_without_net_tx_and_counted() {
        let ended = with_socket(0, 77, [400_000, 0, 5_000, 0], DECLARED | TX_PENDING);
        let mut collector = collector();
        let mut events = Vec::new();
        collector.absorb(vec![ended], &mut events, 0);
        assert!(events.is_empty(), "held for its drain");
        collector.flush(&mut events);
        assert_eq!(events.len(), 1);
        assert_eq!(value(&events[0], "net_tx_ns"), None);
        assert_eq!(value(&events[0], "net_rx_ns"), Some(600_000.0));
        assert!(collector
            .socket_notes()
            .iter()
            .any(|note| note.starts_with("1 response had not left the stack")));
    }

    /// The request is the series and its thread the second key; the total is
    /// the end less the birth, and the states are carried as the kernel added
    /// them, not rescaled to fit it.
    #[test]
    fn an_ended_request_is_keyed_by_its_id_and_carries_its_states() {
        let event = event(&CollectorId::new("t"), &record(0), 0);
        assert_eq!(event.keys[0].to_string(), "req:a3f1");
        assert_eq!(event.keys[1], KeyValue::task(8820));
        let value = |name: &str| event.attr(name).and_then(|value| value.as_f64());
        assert_eq!(value("total_ns"), Some(42_000_000.0));
        assert_eq!(value("sleep_ns"), Some(40_000_000.0));
        assert_eq!(value("holder_tid"), Some(8821.0));
        assert_eq!(
            event.ts_ns, 43_000_000,
            "an ended request is stamped at its end"
        );
        assert!(event.attr("declared").is_none());
    }

    /// An end under another id says so, rather than folding the request into
    /// either.
    #[test]
    fn an_end_under_another_id_carries_both() {
        let event = event(&CollectorId::new("t"), &record(crate::bpf::ID_MISMATCH), 0);
        assert_eq!(event.keys[0].to_string(), "req:a3f1");
        assert_eq!(
            event.attr("declared").map(ToString::to_string),
            Some("beef".to_owned())
        );
    }

    /// Every filter field the catalog declares is on the event.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let entry = catalog
            .get(&"net.request".parse().expect("an id"))
            .expect("net.request is in the catalog");
        let event = event(&CollectorId::new("t"), &record(0), 0);
        for field in &entry.filter_fields {
            assert!(
                event.attr(field).is_some(),
                "`{field}` is declared and absent"
            );
        }
    }

    /// A program with no request probes is refused with what it does carry,
    /// and the refusal names the two probes it would need.
    #[test]
    fn a_program_that_declares_no_request_is_refused_with_what_it_carries() {
        let error = request_sites(Path::new("/bin/sh")).expect_err("no request probes");
        assert!(error.contains(START) && error.contains(DONE), "{error}");
        assert!(error.contains("/bin/sh"), "{error}");
    }
}
