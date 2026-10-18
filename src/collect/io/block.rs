//! `io.latency`, `io.queue` and `io.size` over the block layer's raw
//! tracepoints.
//!
//! Three sources, one program shape, and the difference between them is which
//! two moments of a request's life are subtracted. `src/bpf/io.rs` holds the
//! programs and says why; this file is the part that belongs to the family:
//! what an event carries, what is refused, and what is counted as lost.
//!
//! **These are the first sources in the project to produce `blk:rq@open_ts`.**
//! `scoped_eq` is one of the four match kinds the join engine implements and
//! was the only one nothing produced, so an `io.queue` and an `io.latency` run
//! over the same workload can now be joined into the two halves of one
//! request's wait. The scope is the kernel's own `request.start_time_ns` rather
//! than anything this tool observed, which is what lets two collectors that
//! never met agree on it; where the kernel stamped none, the event says so and
//! the collector reports how many.
//!
//! **What a process target does and does not reach.** A read is submitted in
//! the reading task's own context, so the task is readable at the insert and at
//! the issue. A write is not: it leaves through writeback in a kworker minutes
//! later, and the process that wrote it is gone from every context the block
//! layer has. The catalog's caveat says so, and a run with a process target
//! repeats it in its own output rather than leaving a reader to wonder why a
//! program that wrote a gigabyte produced four events.
//!
//! **A request submitted while serving a request carries `req:<id>`.** The
//! program that sees the submitting task current -- the issue, or the insert
//! for `io.queue` -- reads the id the request's birth wrote into that task's
//! storage (`src/bpf/request.rs`), and the event carries it as a key. Nothing
//! here infers it from the file, the descriptor or the timing: it is the value
//! the task held, which is why a decomposition read per `req` can call the
//! block row IDENTITY. A write that leaves through writeback carries none, for
//! the same reason it carries no submitter.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use crate::bpf::{pidns, BlockEvent, BlockProgram, Stage, Submitter};
use crate::clock::monotonic_ns;
use crate::collect::{not_built, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Direction, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained.
///
/// A block completion is thousands of times rarer than a context switch, but
/// it is bursty in exactly the way that fills a ring: a queue of depth 1024
/// draining at once is a thousand records in a few microseconds. Draining at
/// ten milliseconds is cheap and what it cannot absorb is counted.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// The ring the programs write into. Each record is 104 bytes plus an 8-byte
/// header, so this holds about two and a half thousand requests between drains.
const RING_BYTES: usize = 256 * 1024;

/// Which of the three sources a collector is serving.
///
/// The stage decides the measurement and the source id decides what the events
/// are called; they are one choice, so they are made in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    /// `io.latency`: issue to completion, what the device did.
    Latency,
    /// `io.queue`: insert to issue, time on a software queue.
    Queue,
    /// `io.size`: the length of a request at the issue.
    Size,
}

impl Measure {
    fn stage(self) -> Stage {
        match self {
            Measure::Latency => Stage::Latency,
            Measure::Queue => Stage::Queue,
            Measure::Size => Stage::Size,
        }
    }

    fn source_id(self) -> &'static str {
        match self {
            Measure::Latency => "io.latency",
            Measure::Queue => "io.queue",
            Measure::Size => "io.size",
        }
    }

    /// The attribute holding the measurement, which is what every emit reads.
    fn value_attr(self) -> &'static str {
        match self {
            Measure::Latency => "latency_ns",
            Measure::Queue => "queue_ns",
            Measure::Size => "bytes",
        }
    }

    /// What the number is, in a sentence a refusal or a note can use.
    fn what(self) -> &'static str {
        match self {
            Measure::Latency => {
                "the time between a request being issued to the driver and the \
                                 device completing it"
            }
            Measure::Queue => {
                "the time a request spent on a software queue before the driver \
                               was handed it"
            }
            Measure::Size => "the length of a request at the moment it was issued",
        }
    }
}

/// Holds the request until the target exists.
pub struct Prepare {
    pub measure: Measure,
    pub mode: Mode,
    /// The request's predicate, which this collector applies itself.
    ///
    /// The resolver credits any backend whose id begins with `bpf` with pushing
    /// a filter into the kernel and then leaves its events alone. That credit
    /// comes from the backend's *name*, so a collector that ignores the
    /// predicate returns everything and nothing anywhere says the filter did
    /// not run. Applying it here is what makes the resolver's claim true of the
    /// numbers; what it is not yet true of is the cost, which the run says.
    pub filter: Option<FilterExpr>,
    /// Count the latencies into a log2 histogram in the kernel rather than
    /// publishing a record per request: see [`histogram_is_enough`].
    pub in_kernel_histogram: bool,
}

/// Whether a request of `io.latency` is answered whole by a histogram the
/// kernel builds, one row per `(device, log2 bucket)`.
///
/// Only when a log2 histogram is everything it asks for: one `hist` emit, in
/// `trace`, with no filter and no key but the device. Anything else needs the
/// requests one at a time -- `raw`, `topk` and a linear histogram read each
/// value, `summary` its exact minimum and maximum, a filter each request's
/// fields, and a key other than `dev` (above all `blk`, which the `scoped_eq`
/// join reads) is something a row keyed by device does not carry.
///
/// The one thing this cannot see is a session's join: a join names sources, not
/// requests, and reaches no collector. The validator can, and refuses a join on
/// any key but `dev` over a request this answers (exit 2, naming the emit that
/// would carry the key): `validate::check_joins`.
pub fn histogram_is_enough(request: &crate::request::Request) -> bool {
    use crate::request::{Emit, HistScale, KeyKind};
    request.mode == Mode::Trace
        && request.filter.is_none()
        && matches!(request.emit.as_slice(), [Emit::Hist(HistScale::Log2)])
        && request.keys.iter().all(|key| *key == KeyKind::Dev)
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let measure = self.measure;
        match self.mode {
            Mode::Trace => {}
            // Every request, aggregated, rather than every request reported.
            // The ring still carries one record per request -- there is no
            // other way to know a request ended -- and what changes is that
            // the run answers with one number instead of thousands.
            Mode::Count if measure == Measure::Latency => {}
            other => {
                return Err(format!(
                    "`{other}` mode cannot measure {}: it is the gap between two events, and a \
                     sample that catches one of them without the other has no duration to \
                     report",
                    measure.what()
                ))
            }
        }

        let submitter = match target {
            Target::System => Submitter::Any,
            Target::Pid(pids) => {
                Submitter::Process(pidns::kernel_tgid(first(pids, "pid")? as u32)?)
            }
            Target::Tid(tids) => Submitter::Thread(pidns::kernel_tid(first(tids, "tid")? as u32)?),
            Target::Cgroup(_) | Target::Container(_) => {
                return Err(not_built(format!(
                    "`{}` can be scoped to a cgroup -- the catalog says so -- but this build \
                     filters by thread group and thread id only: a cgroup filter needs \
                     `bpf_get_current_cgroup_id` and the request is written down in \
                     `docs/open-findings.md`",
                    measure.source_id()
                )))
            }
            other => {
                return Err(format!(
                    "a {} target names no task, and a block request is attributed to the task \
                     that submitted it; use --system to watch every device on the machine",
                    other.kind()
                ))
            }
        };

        let mut collector = BlockCollector::new(measure, self.mode, submitter, self.filter);
        collector.in_kernel_histogram = self.in_kernel_histogram && measure == Measure::Latency;
        Ok(Box::new(collector))
    }
}

fn first(pids: &[i32], kind: &str) -> Result<i32, String> {
    pids.first().copied().ok_or_else(|| {
        format!("a {kind} target with no {kind} in it names nothing to attribute a request to")
    })
}

pub struct BlockCollector {
    id: CollectorId,
    measure: Measure,
    mode: Mode,
    submitter: Submitter,
    program: Option<BlockProgram>,
    losses: Losses,
    /// The programs' counters are cumulative, so what is new is the difference.
    dropped_inflight: u64,
    dropped_ring: u64,
    skipped: u64,
    /// Requests seen closing that this run never saw open. Not a loss.
    unpaired: u64,
    /// `io.queue` only: requests the block layer issued directly, without ever
    /// putting them on a software queue. Their queueing time is zero, and that
    /// is a measurement rather than a gap.
    direct: u64,
    /// `io.queue` only: issues of requests that already existed when the
    /// programs attached, so whether they queued is genuinely unknown.
    predating: u64,
    /// Device names by the major:minor the block layer reports, refreshed when
    /// a pair turns up that is not in the map.
    names: BTreeMap<(u32, u32), String>,
    /// Requests whose scope this run had to invent, because the kernel put no
    /// `start_time_ns` on them.
    own_scope: u64,
    /// Requests that belonged to no partition, so carry no device.
    no_device: u64,
    events: u64,
    /// Requests still open when the run ended, read while the map was still
    /// there: `notes` is asked after `stop`, and by then it is closed.
    /// Requests opened and not closed, and whether that is a lower bound; or
    /// why it is not known.
    still_in_flight: Result<(usize, bool), String>,
    /// The `count` mode accumulation: how many requests, and their total.
    counted: u64,
    counted_total: u64,
    /// When the run started, which is the timestamp a counted total carries.
    opened_ns: u64,
    predicate: Option<FilterExpr>,
    filtered_out: u64,
    filtered_in: u64,
    /// Events that carried a request identity.
    carried_request: u64,
    /// Why no event can carry one, when the kernel gave the programs no
    /// task-storage map to read it from.
    no_request_identity: Option<String>,
    /// Whether this kernel's completion reports a negative errno rather than a
    /// `blk_status_t` (5.15), which decides what the attribute is called.
    status_is_errno: bool,
    /// Whether the programs count into a kernel-side histogram.
    in_kernel_histogram: bool,
    /// What the histogram held at the end: the requests it counted, and the
    /// rows they were reported as.
    histogram_requests: u64,
    histogram_rows: u64,
    /// Why the histogram's read may have missed a completion, when it may.
    histogram_caveat: Option<String>,
}

impl BlockCollector {
    pub fn new(
        measure: Measure,
        mode: Mode,
        submitter: Submitter,
        predicate: Option<FilterExpr>,
    ) -> Self {
        Self {
            id: CollectorId::new(format!("{}/bpf:raw_tp", measure.source_id())),
            measure,
            mode,
            submitter,
            program: None,
            losses: Losses::default(),
            dropped_inflight: 0,
            dropped_ring: 0,
            skipped: 0,
            unpaired: 0,
            direct: 0,
            predating: 0,
            names: BTreeMap::new(),
            own_scope: 0,
            no_device: 0,
            events: 0,
            still_in_flight: Ok((0, false)),
            counted: 0,
            counted_total: 0,
            opened_ns: 0,
            predicate,
            filtered_out: 0,
            filtered_in: 0,
            carried_request: 0,
            no_request_identity: None,
            status_is_errno: false,
            in_kernel_histogram: false,
            histogram_requests: 0,
            histogram_rows: 0,
            histogram_caveat: None,
        }
    }

    /// The kernel-side histogram's rows, as events: one per `(device, log2
    /// bucket)`, weighed by the requests it counted, with the bucket's low
    /// bound as the latency -- the value the emit layer's log2 bucketing maps
    /// back to exactly that bucket -- and the exact total beside it.
    fn histogram_events(&mut self, rows: Vec<crate::bpf::HistRow>, seq: u64) -> Vec<Event> {
        let measure = self.measure;
        let mut events = Vec::with_capacity(rows.len());
        for (index, row) in rows.into_iter().enumerate() {
            if row.count == 0 {
                continue;
            }
            let (major, minor) = (row.major(), row.minor());
            if row.dev == 0 {
                self.no_device += row.count;
            }
            let name = self.device_name(major, minor);
            self.histogram_requests += row.count;
            self.histogram_rows += 1;
            events.push(Event {
                // A row is the whole run's, so it is stamped when the run
                // opened, like every other kernel-built histogram's first row.
                ts_ns: self.opened_ns,
                ts_err_ns: 0,
                kind: measure.source_id().to_owned(),
                keys: vec![KeyValue::dev(major, minor)],
                attrs: vec![
                    Attr::uint(measure.value_attr(), row.bucket_low_ns()),
                    Attr::uint("count", row.count),
                    Attr::uint(crate::emit::WEIGHT_ATTR, row.count),
                    Attr::uint("total_ns", row.total_ns),
                    Attr::uint("bucket", u64::from(row.bucket)),
                    Attr::text("dev", name),
                    Attr::uint("dev_major", u64::from(major)),
                    Attr::uint("dev_minor", u64::from(minor)),
                ],
                collector_id: self.id.clone(),
                seq: seq + index as u64,
            });
        }
        events
    }

    /// The name of a device, or the pair itself when nothing names it.
    ///
    /// The block layer reports the *partition* a request went to, so this is
    /// `nvme0n1p6` where `/proc/diskstats` sums into `nvme0n1` as well. The two
    /// are not interchangeable and are not made so here.
    fn device_name(&mut self, major: u32, minor: u32) -> String {
        if let Some(name) = self.names.get(&(major, minor)) {
            return name.clone();
        }
        self.names = super::device_names();
        self.names
            .get(&(major, minor))
            .cloned()
            .unwrap_or_else(|| format!("{major}:{minor}"))
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let mut undecoded = 0;
        let mut records = Vec::new();
        program.drain(|record| records.push(record), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the block ring was shorter than one request");
        }

        for (index, record) in records.into_iter().enumerate() {
            // A close with no open carries no interval, and the same record
            // stands for two opposite facts. The kernel's own creation stamp
            // separates them: a request created after the programs attached
            // would have been seen at its insert had there been one, so its
            // absence means it was issued directly and queued for no time. One
            // created before them is simply older than the measurement.
            if record.had_no_open() {
                let created_while_watching =
                    record.scope_is_the_kernels() && record.open_ts >= self.opened_ns;
                if created_while_watching {
                    self.direct += 1;
                } else {
                    self.predating += 1;
                    continue;
                }
            }
            if record.dev == 0 {
                self.no_device += 1;
            }
            if !record.scope_is_the_kernels() {
                self.own_scope += 1;
            }
            let event = self.event(&record, seq + index as u64);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += 1;
                    continue;
                }
                self.filtered_in += 1;
            }
            if self.mode == Mode::Count {
                self.counted += 1;
                self.counted_total += record.delta_ns;
                continue;
            }
            self.events += 1;
            if record.req != 0 {
                self.carried_request += 1;
            }
            events.push(event);
        }
    }

    fn event(&mut self, record: &BlockEvent, seq: u64) -> Event {
        let (major, minor) = (record.major(), record.minor());
        let name = self.device_name(major, minor);
        let measure = self.measure;

        let mut attrs = vec![
            // The value. Which attribute holds it is what `value_attr` says,
            // and the emit layer reads that rather than guessing.
            Attr::uint(measure.value_attr(), value_of(measure, record)),
            Attr::uint("count", 1),
            // The device as a name, because that is what the `dev` filter field
            // means everywhere else in this family, and as the pair, because a
            // name is a symlink away from meaning something else.
            Attr::text("dev", name),
            Attr::uint("dev_major", u64::from(major)),
            Attr::uint("dev_minor", u64::from(minor)),
            // The kernel's own `blk_fill_rwbs` letters, and the flags they were
            // built from, so a mapping this reader got wrong is visible rather
            // than only wrong.
            Attr::text("rwbs", rwbs(record.cmd_flags)),
            Attr::uint("cmd_flags", u64::from(record.cmd_flags)),
            Attr::uint("bytes", u64::from(record.bytes)),
            Attr::uint("sector", record.sector),
            Attr::text("comm", record.comm()),
            Attr::uint("pid", u64::from(record.tgid)),
            Attr::uint("tid", u64::from(record.tid)),
            Attr::uint("cpu", u64::from(record.cpu)),
            // Which life of the request pointer this is, and where it came
            // from. A scope this run invented will not agree with another
            // collector's, so it is on the event rather than only in a note.
            Attr::uint("open_ts", record.open_ts),
            Attr::text(
                "scope",
                if record.scope_is_the_kernels() {
                    "kernel"
                } else {
                    "run"
                },
            ),
        ];
        if measure == Measure::Queue {
            // Whether this request touched a software queue at all. A zero
            // from a direct issue and a zero from a queue that drained
            // instantly are the same number and not the same fact.
            attrs.push(Attr::text(
                "queued",
                if record.had_no_open() {
                    "direct"
                } else {
                    "scheduler"
                },
            ));
        }
        if measure == Measure::Latency {
            // A failed I/O's latency is a different measurement from a
            // successful one's -- an error can come back in microseconds -- so
            // the status travels rather than being folded in.
            //
            // 5.15 hands the completion a negative errno where later kernels
            // hand it the `blk_status_t` it came from. Both are zero on
            // success; neither number is the other's, so the attribute says
            // which one it is rather than carrying `-5` as a status.
            let name = if self.status_is_errno {
                "errno"
            } else {
                "status"
            };
            attrs.push(Attr::int(name, i64::from(record.status)));
            attrs.push(Attr::uint("done_bytes", u64::from(record.done_bytes)));
        }

        Event {
            ts_ns: record.ts_ns,
            ts_err_ns: 0,
            kind: measure.source_id().to_owned(),
            keys: vec![
                // The device is the series. A latency averaged over an NVMe and
                // a spinning disk describes neither, and recovering one from
                // the other is not something any later layer can do.
                KeyValue::dev(major, minor),
                // The request, scoped by the life of its pointer. This is the
                // key the `scoped_eq` join reads, and the reason it is scoped
                // is that the pointer is reissued within milliseconds.
                KeyValue::Blk {
                    ptr: record.rq,
                    open_ts: record.open_ts,
                },
                // The submitting thread, captured where it was still the
                // current one.
                KeyValue::task(record.tid as i32),
            ]
            .into_iter()
            // The request that thread was serving, read from its own storage
            // at the same moment. Absent, not zero, when it was serving none.
            .chain((record.req != 0).then(|| KeyValue::Req {
                id: crate::bpf::request_key(record.req),
            }))
            // The operation, from the low byte of the flags `rwbs` is built
            // from: what `--keys dir` splits on, with no filter.
            .chain(direction(record.cmd_flags).map(KeyValue::dir))
            .collect(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }

    /// Reads the programs' own counters of what they could not record.
    fn absorb_counters(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let (inflight, ring) = program.dropped();
        if inflight > self.dropped_inflight {
            self.losses.dropped += inflight - self.dropped_inflight;
            self.losses.note_times(
                "more requests were in flight at once than the in-flight map holds, so they were \
                 never timed",
                inflight - self.dropped_inflight,
            );
            self.dropped_inflight = inflight;
        }
        if ring > self.dropped_ring {
            self.losses.dropped += ring - self.dropped_ring;
            self.losses
                .note("the block ring was full: requests completed faster than they were drained");
            self.dropped_ring = ring;
        }
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note_times(
                "the kernel skipped a run of these programs because an interrupt re-entered one \
                 on its own cpu, so a request went unseen; the count is the machine's, and an \
                 upper bound on this target's",
                skipped - self.skipped,
            );
            self.skipped = skipped;
        }
        let unfollowed = program.unfollowed();
        if unfollowed > 0 {
            self.losses.note(format!(
                "{unfollowed} process{} the target started {} not followed: the map of the \
                 processes it started was full, so {} requests are in no event",
                if unfollowed == 1 { "" } else { "es" },
                if unfollowed == 1 { "was" } else { "were" },
                if unfollowed == 1 { "its" } else { "their" },
            ));
        }
        self.unpaired = program.unpaired();
    }
}

/// The measurement this source takes off a record.
fn value_of(measure: Measure, record: &BlockEvent) -> u64 {
    match measure {
        Measure::Latency | Measure::Queue => record.delta_ns,
        Measure::Size => u64::from(record.bytes),
    }
}

/// The direction of a request's operation, or none for one that is neither a
/// read, a write, a discard nor a flush (a driver-private or zone operation).
/// The same low byte, and the same grouping, as [`rwbs`]'s first letter.
fn direction(cmd_flags: u32) -> Option<Direction> {
    match cmd_flags & 0xff {
        0 => Some(Direction::Read),
        // REQ_OP_WRITE and REQ_OP_WRITE_ZEROES.
        1 | 9 => Some(Direction::Write),
        // REQ_OP_DISCARD and REQ_OP_SECURE_ERASE.
        3 | 5 => Some(Direction::Discard),
        2 => Some(Direction::Flush),
        _ => None,
    }
}

/// The kernel's own letters for an operation, as `blk_fill_rwbs` writes them.
///
/// Deliberately the same definition rather than a better one: `rwbs` is a
/// filter field the catalog declares, and someone filtering on it is filtering
/// on what `blktrace` and every block tracepoint in tracefs already call these.
/// The flags are carried raw beside it so a letter this function got wrong can
/// be seen to be wrong.
fn rwbs(cmd_flags: u32) -> String {
    /// `REQ_OP_BITS` is 8, so the operation is the low byte and the flags are
    /// everything above it.
    const OP_MASK: u32 = 0xff;
    const REQ_OP_READ: u32 = 0;
    const REQ_OP_WRITE: u32 = 1;
    const REQ_OP_FLUSH: u32 = 2;
    const REQ_OP_DISCARD: u32 = 3;
    const REQ_OP_SECURE_ERASE: u32 = 5;
    const REQ_OP_WRITE_ZEROES: u32 = 9;
    const REQ_SYNC: u32 = 1 << 11;
    const REQ_META: u32 = 1 << 12;
    const REQ_FUA: u32 = 1 << 17;
    const REQ_PREFLUSH: u32 = 1 << 18;
    const REQ_RAHEAD: u32 = 1 << 19;

    let mut letters = String::new();
    match cmd_flags & OP_MASK {
        REQ_OP_WRITE | REQ_OP_WRITE_ZEROES => letters.push('W'),
        REQ_OP_DISCARD => letters.push('D'),
        REQ_OP_SECURE_ERASE => letters.push_str("DE"),
        REQ_OP_FLUSH => letters.push('F'),
        REQ_OP_READ => letters.push('R'),
        // Not a read, a write or anything else with a letter: a driver-private
        // or zone operation. `N` is what the kernel prints too.
        _ => letters.push('N'),
    }
    if cmd_flags & REQ_FUA != 0 {
        letters.push('F');
    }
    if cmd_flags & REQ_RAHEAD != 0 {
        letters.push('A');
    }
    if cmd_flags & REQ_SYNC != 0 {
        letters.push('S');
    }
    if cmd_flags & REQ_META != 0 {
        letters.push('M');
    }
    if cmd_flags & REQ_PREFLUSH != 0 {
        letters.push('P');
    }
    letters
}

impl Collector for BlockCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        self.measure.value_attr()
    }

    fn start(&mut self) -> Result<(), String> {
        let program = if self.in_kernel_histogram {
            BlockProgram::load_histogram(self.measure.stage(), self.submitter)?
        } else {
            BlockProgram::load(self.measure.stage(), self.submitter, RING_BYTES)?
        };
        self.no_request_identity = program.no_request_identity.clone();
        self.status_is_errno = program.status_is_errno;
        self.program = Some(program);
        self.names = super::device_names();
        self.opened_ns = monotonic_ns();
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
        // Read before the map is closed. A request still in flight has a real
        // interval that is in no event, because it has no end yet.
        if let Some(program) = self.program.as_ref() {
            self.still_in_flight = program.still_in_flight();
        }
        // The histogram is read once, here: the closing program is detached
        // first, so the read is of rows nothing adds to any more.
        if let Some(program) = self.program.as_mut().filter(|p| p.is_histogram()) {
            match program.read_histogram() {
                Ok((rows, unwaited)) => {
                    if let Some(caveat) = unwaited {
                        self.histogram_caveat = Some(caveat);
                    }
                    let events = self.histogram_events(rows, u64::MAX / 2);
                    self.events += events.len() as u64;
                    trailing.extend(events);
                }
                Err(reason) => {
                    // Every request the rows held is lost, and how many that
                    // was is not knowable without the read that failed.
                    self.losses.failed_reads += 1;
                    self.losses.note_times(reason, 1);
                }
            }
            self.absorb_counters();
        }

        // A counted run has no series: its single event is the whole window,
        // and it is produced here because this is the first moment the total
        // is complete.
        if self.mode == Mode::Count {
            trailing.push(Event {
                ts_ns: monotonic_ns(),
                ts_err_ns: 0,
                kind: self.measure.source_id().to_owned(),
                keys: Vec::new(),
                attrs: vec![
                    Attr::uint(self.measure.value_attr(), self.counted_total),
                    Attr::uint("requests", self.counted),
                    Attr::uint("window_ns", monotonic_ns().saturating_sub(self.opened_ns)),
                ],
                collector_id: self.id.clone(),
                seq: u64::MAX / 2,
            });
        }

        // Detached, not dropped: the run reads what the programs spent from
        // their descriptors after every collector has stopped.
        if let Some(program) = self.program.as_mut() {
            program.detach();
        }
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn facts(&self) -> Vec<String> {
        if !self.in_kernel_histogram {
            return Vec::new();
        }
        // How the answer was obtained, which is not a concession: the buckets
        // are the ones the emit layer builds, and the total is exact.
        vec![format!(
            "the histogram was built in the kernel: {} requests counted into {} rows of \
             (device, log2 bucket), read once at the end, and no record per request crossed \
             to userspace; `latency_ns` on a row is its bucket's low bound and `total_ns` the \
             exact sum. Rows carry `dev` and no `blk`, `task` or `req` key -- ask for another \
             emit beside `hist` to have the requests one at a time",
            self.histogram_requests, self.histogram_rows,
        )]
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();

        if let Some(caveat) = &self.histogram_caveat {
            notes.push(caveat.clone());
        }

        if self.mode == Mode::Count {
            notes.push(format!(
                "{} requests, {} ns in total: a counted run reports how long the {} held them \
                 altogether, which is not a latency -- the distribution is what `-m trace` gives",
                self.counted,
                self.counted_total,
                match self.measure {
                    Measure::Queue => "software queue",
                    _ => "device",
                },
            ));
        }

        // The caveat the catalog carries, said by the run that it applies to.
        // A process target sees the reads it submitted and almost none of its
        // writes, and four events from a program that wrote a gigabyte is a
        // number somebody will otherwise try to explain.
        if self.submitter != Submitter::Any {
            notes.push(
                "a process target sees the requests submitted in its own context or in that of \
                 a process it started, which is reads; a write leaves through writeback in a kworker minutes later and carries \
                 no submitter at all, so it is absent here rather than counted as zero"
                    .to_owned(),
            );
        }

        if self.own_scope > 0 {
            notes.push(format!(
                "{} of the requests carried no `start_time_ns` -- the kernel keeps no statistics \
                 for their queue -- so their `blk:rq@open_ts` is scoped by this run's own first \
                 sighting and will not match another collector's; each such event says `scope = \
                 run`",
                self.own_scope
            ));
        }

        if self.no_device > 0 {
            notes.push(format!(
                "{} of the requests belonged to no partition, which a passthrough request does \
                 not, and are reported under device 0:0",
                self.no_device
            ));
        }

        if self.unpaired > 0 {
            let because = if self.submitter == Submitter::Any {
                "they were already in flight when the programs attached"
            } else {
                "they belong to another process, or were already in flight when the programs \
                 attached"
            };
            notes.push(format!(
                "{} {} events had no opening this run recorded and were not measured: {because}. \
                 Nothing was dropped -- there was nothing to subtract from",
                self.unpaired,
                self.measure.stage().closes_at(),
            ));
        }

        if self.direct > 0 {
            notes.push(format!(
                "{} of the requests were issued directly, without ever being put on a software \
                 queue: `block_rq_insert` never fired for them, and the kernel created them \
                 after these programs attached, so the absence is the measurement. They are in \
                 the distribution at zero, and each says `queued = direct`. A queue running the \
                 `none` scheduler -- the default for NVMe -- issues nearly everything this way",
                self.direct
            ));
        }
        if self.predating > 0 {
            notes.push(format!(
                "{} requests were issued that the kernel had created before these programs \
                 attached; whether they queued is not knowable and they are in no event",
                self.predating
            ));
        }

        match &self.still_in_flight {
            Ok((0, false)) => {}
            Ok((count, truncated)) => notes.push(format!(
                "{}{count} request{} still in flight when the run ended; the interval it is in \
                 has no end yet and is in no event",
                if *truncated { "at least " } else { "" },
                if *count == 1 && !truncated {
                    " was"
                } else {
                    "s were"
                }
            )),
            Err(why) => notes.push(format!(
                "how many requests were still in flight when the run ended is not known ({why}); \
                 their intervals have no end yet and are in no event"
            )),
        }

        if self.status_is_errno && self.measure == Measure::Latency {
            notes.push(
                "this kernel's `block_rq_complete` reports a negative errno rather than a \
                 `blk_status_t`, so each event carries `errno` where later kernels carry `status`"
                    .to_owned(),
            );
        }

        if let Some(reason) = &self.no_request_identity {
            notes.push(format!(
                "no event carries `req`: the request identity is read from the submitting task's \
                 storage, and this kernel gave the programs no task-storage map ({reason})"
            ));
        } else if self.carried_request > 0 {
            notes.push(format!(
                "{} of the {} requests were submitted by a task serving a request and carry \
                 `req:<id>`, read from that task's own storage at the {} -- a propagated \
                 identity, not an inference from the file or the timing",
                self.carried_request,
                self.events,
                self.measure.stage().opens_at().unwrap_or("issue"),
            ));
        }

        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} requests and removed {}; it ran here, over the drained \
                 records, not in the kernel, so the removed ones were carried up before they \
                 were dropped",
                self.filtered_in, self.filtered_out,
            ));
        }

        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> BlockEvent {
        // Built through the decoder rather than by naming fields, because the
        // fields are private to the layer that writes them: this is the same
        // bytes the ring carries.
        let mut bytes = [0u8; 104];
        bytes[0..8].copy_from_slice(&1_000_000u64.to_ne_bytes()); // ts_ns
        bytes[8..16].copy_from_slice(&0xffff_0000u64.to_ne_bytes()); // rq
        bytes[16..24].copy_from_slice(&900_000u64.to_ne_bytes()); // open_ts
        bytes[24..32].copy_from_slice(&100_000u64.to_ne_bytes()); // delta_ns
        bytes[32..40].copy_from_slice(&2048u64.to_ne_bytes()); // sector
        bytes[40..44].copy_from_slice(&((259u32 << 20) | 6).to_ne_bytes()); // dev
        bytes[44..48].copy_from_slice(&131_072u32.to_ne_bytes()); // bytes
        bytes[48..52].copy_from_slice(&1u32.to_ne_bytes()); // cmd_flags: write
        bytes[56..60].copy_from_slice(&4242u32.to_ne_bytes()); // tid
        bytes[60..64].copy_from_slice(&4200u32.to_ne_bytes()); // tgid
        bytes[64..68].copy_from_slice(&3u32.to_ne_bytes()); // cpu
        bytes[80..84].copy_from_slice(b"dd\0\0");
        BlockEvent::from_bytes(&bytes).expect("a full record")
    }

    fn collector(measure: Measure) -> BlockCollector {
        BlockCollector::new(measure, Mode::Trace, Submitter::Any, None)
    }

    /// `dir` and `rwbs` read the same low byte and must agree: a request
    /// keyed `dir:read` whose letters say `W` is a split nobody could trust.
    #[test]
    fn the_direction_key_agrees_with_the_kernels_letters() {
        for op in 0u32..=255 {
            let letters = rwbs(op | (1 << 11));
            let expected = match letters.chars().next() {
                Some('R') => Some(Direction::Read),
                Some('W') => Some(Direction::Write),
                Some('D') => Some(Direction::Discard),
                Some('F') => Some(Direction::Flush),
                _ => None,
            };
            assert_eq!(direction(op), expected, "op {op}: `{letters}`");
        }
        let event = collector(Measure::Latency).event(&record(), 0);
        assert!(
            event.keys.contains(&KeyValue::dir(Direction::Write)),
            "{:?}",
            event.keys
        );
    }

    /// Every filter field the catalog offers must be on the events, or a
    /// predicate over it is false everywhere and silently empties the series
    /// -- which looks exactly like a machine that did no I/O.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        for measure in [Measure::Latency, Measure::Queue, Measure::Size] {
            let event = collector(measure).event(&record(), 0);
            let declared = &catalog
                .get(&measure.source_id().parse().expect("a source id"))
                .expect("the source is in the catalog")
                .filter_fields;
            assert!(
                !declared.is_empty(),
                "{} declares filter fields",
                measure.source_id()
            );
            for field in declared {
                assert!(
                    event.attrs.iter().any(|attr| attr.name == *field),
                    "`{}` offers a `{field}` filter and no event carries one",
                    measure.source_id()
                );
            }
        }
    }

    /// The value attribute a collector names has to be on its own events, or
    /// every emit reads a number that is not there.
    #[test]
    fn the_value_attribute_is_the_one_the_events_carry() {
        for (measure, expected) in [
            (Measure::Latency, 100_000.0),
            (Measure::Queue, 100_000.0),
            (Measure::Size, 131_072.0),
        ] {
            let mut collector = collector(measure);
            let attr = collector.value_attr();
            let event = collector.event(&record(), 0);
            assert_eq!(
                event.attr(attr).and_then(|value| value.as_f64()),
                Some(expected),
                "{}",
                measure.source_id()
            );
        }
    }

    /// The key the `scoped_eq` join reads. Both halves matter: the pointer is
    /// the identity and the timestamp is which life of it this is, and a key
    /// carrying only the first attributes one request's latency to another.
    #[test]
    fn an_event_carries_the_scoped_block_key_the_join_needs() {
        let event = collector(Measure::Latency).event(&record(), 0);
        let key = event
            .key(crate::request::KeyKind::Blk)
            .expect("a blk key")
            .clone();
        assert_eq!(
            key,
            KeyValue::Blk {
                ptr: 0xffff_0000,
                open_ts: 900_000
            }
        );
        assert_eq!(key.to_string(), "blk:0xffff0000@900000");
    }

    /// A request submitted while its task served a request carries that
    /// request's id as a `req` key, spelled the way `net.request` spells it;
    /// one submitted while serving none carries no `req` key at all rather
    /// than a zero that every such request would share.
    #[test]
    fn a_request_identity_is_a_key_when_the_task_carried_one_and_absent_otherwise() {
        let bare = collector(Measure::Latency).event(&record(), 0);
        assert_eq!(bare.key(crate::request::KeyKind::Req), None);

        let mut bytes = [0u8; 104];
        bytes[40..44].copy_from_slice(&((259u32 << 20) | 6).to_ne_bytes());
        bytes[96..104].copy_from_slice(&0xa3f1u64.to_ne_bytes());
        let tagged = BlockEvent::from_bytes(&bytes).expect("a full record");
        let event = collector(Measure::Latency).event(&tagged, 0);
        assert_eq!(
            event
                .key(crate::request::KeyKind::Req)
                .map(ToString::to_string),
            Some("req:a3f1".to_owned())
        );
    }

    /// The device is the first key and therefore the series. A latency
    /// averaged over an NVMe and a spinning disk describes neither of them,
    /// and no later layer can split one back apart.
    #[test]
    fn the_device_is_the_series_and_carries_the_pair_not_only_the_name() {
        let event = collector(Measure::Latency).event(&record(), 0);
        assert_eq!(event.keys.first(), Some(&KeyValue::dev(259, 6)));
        assert_eq!(
            event.attr("dev_major").and_then(|value| value.as_f64()),
            Some(259.0)
        );
    }

    /// `rwbs` is the kernel's own `blk_fill_rwbs`, because that is what
    /// somebody filtering on it has seen everywhere else.
    #[test]
    fn the_operation_letters_are_the_ones_the_kernel_prints() {
        assert_eq!(rwbs(0), "R");
        assert_eq!(rwbs(1), "W");
        assert_eq!(rwbs(2), "F");
        assert_eq!(rwbs(3), "D");
        // A synchronous, metadata write with a forced unit access.
        assert_eq!(rwbs(1 | (1 << 11) | (1 << 12) | (1 << 17)), "WFSM");
        // A readahead read.
        assert_eq!(rwbs(1 << 19), "RA");
        // An operation with no letter of its own is `N`, as the kernel writes
        // it, rather than being reported as a read.
        assert_eq!(rwbs(34), "N");
    }

    /// A scope this run invented will not match another collector's, so it has
    /// to be visible on the event and not only in a note.
    #[test]
    fn an_invented_scope_is_named_on_the_event_that_carries_it() {
        let mut record = record();
        assert_eq!(
            collector(Measure::Latency)
                .event(&record, 0)
                .attr("scope")
                .map(ToString::to_string),
            Some("kernel".to_owned())
        );
        record.flags = crate::bpf::SCOPE_IS_OURS;
        assert_eq!(
            collector(Measure::Latency)
                .event(&record, 0)
                .attr("scope")
                .map(ToString::to_string),
            Some("run".to_owned())
        );
    }

    /// A mode that cannot mean anything here is refused with the reason. A
    /// sampled latency is a latency whose beginning or end was not seen, and
    /// that is not a short duration, it is not a duration.
    #[test]
    fn a_sampled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            measure: Measure::Latency,
            mode: Mode::Sample {
                freq: "997hz".parse().expect("a frequency"),
            },
            filter: None,
            in_kernel_histogram: false,
        });
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a sampled mode must be refused");
        };
        assert!(error.contains("gap between two events"), "{error}");
    }

    /// `count` is declared by `io.latency` and by neither of the others, and
    /// the build must agree with the catalog about that rather than serving a
    /// mode nobody promised or refusing one that was.
    #[test]
    fn the_modes_served_are_the_modes_the_catalog_declares() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        for measure in [Measure::Latency, Measure::Queue, Measure::Size] {
            let declared = &catalog
                .get(&measure.source_id().parse().expect("a source id"))
                .expect("the source is in the catalog")
                .modes;
            let counted = Box::new(Prepare {
                measure,
                mode: Mode::Count,
                filter: None,
                in_kernel_histogram: false,
            })
            .attach(&Target::System);
            assert_eq!(
                counted.is_ok(),
                declared.contains(&crate::request::ModeKind::Count),
                "`{}` and its catalog entry disagree about `count`",
                measure.source_id()
            );
        }
    }

    /// A target this collector cannot scope to is refused as a gap in the
    /// build, not as a machine that cannot serve it: exit 3 rather than 1,
    /// because no other machine would answer differently.
    #[test]
    fn a_cgroup_target_is_refused_as_something_this_build_has_not_written() {
        let prepared = Box::new(Prepare {
            measure: Measure::Latency,
            mode: Mode::Trace,
            filter: None,
            in_kernel_histogram: false,
        });
        let Err(error) = prepared.attach(&Target::Cgroup("/sys/fs/cgroup".into())) else {
            panic!("a cgroup target must be refused");
        };
        assert!(error.contains("cgroup"), "{error}");
        assert!(error.contains("open-findings"), "{error}");
    }

    /// The caveat the catalog carries has to reach the run it applies to. Four
    /// events from a program that wrote a gigabyte is a number somebody will
    /// try to explain, and the explanation is that writes have no submitter.
    #[test]
    fn a_process_target_says_that_its_writes_are_not_here() {
        let machine_wide = BlockCollector::new(Measure::Latency, Mode::Trace, Submitter::Any, None);
        assert!(
            !machine_wide
                .notes()
                .iter()
                .any(|note| note.contains("kworker")),
            "a machine-wide run sees the kworker's writes like any other"
        );
        let targeted =
            BlockCollector::new(Measure::Latency, Mode::Trace, Submitter::Process(1), None);
        let notes = targeted.notes().join(" ");
        assert!(notes.contains("kworker"), "{notes}");
        assert!(
            notes.contains("absent here rather than counted as zero"),
            "{notes}"
        );
    }

    /// A close with no open is not a loss, and must not be counted as one: a
    /// loss is something the tool had and could not keep.
    #[test]
    fn an_unpaired_completion_is_reported_and_is_not_a_loss() {
        let mut collector = collector(Measure::Latency);
        collector.unpaired = 12;
        let notes = collector.notes().join(" ");
        assert!(notes.contains("12 block_rq_complete events"), "{notes}");
        assert!(notes.contains("Nothing was dropped"), "{notes}");
        assert!(
            collector.losses().is_clean(),
            "an unpaired close loses nothing"
        );
    }

    /// The kernel-side histogram answers a request only when a log2 histogram
    /// is all it asks for: every other emit, a filter, or a key a row keyed by
    /// device does not carry needs the requests one at a time.
    #[test]
    fn only_a_request_for_a_log2_histogram_alone_is_answered_in_the_kernel() {
        use crate::request::{Emit, HistScale, KeyKind, Request};
        let request = |emit: Vec<Emit>, keys: Vec<KeyKind>, filter: Option<&str>| Request {
            source: "io.latency".parse().expect("an id"),
            target: Target::System,
            mode: Mode::Trace,
            emit,
            keys,
            filter: filter.map(|text| text.parse().expect("a filter")),
            duration: None,
            budget: None,
            stack: None,
            symbols: None,
            backend: None,
            label: None,
            window: None,
            value: None,
            sum: Vec::new(),
        };
        let log2 = Emit::Hist(HistScale::Log2);
        assert!(histogram_is_enough(&request(vec![log2], vec![], None)));
        assert!(histogram_is_enough(&request(
            vec![log2],
            vec![KeyKind::Dev],
            None
        )));
        for (emit, keys, filter) in [
            (vec![log2, Emit::Summary], vec![], None),
            (vec![log2, Emit::Raw], vec![], None),
            (
                vec![Emit::Hist(HistScale::Linear { width: 1000 })],
                vec![],
                None,
            ),
            (vec![log2], vec![KeyKind::Blk], None),
            (vec![log2], vec![KeyKind::Task], None),
            (vec![log2], vec![], Some("bytes > 4096")),
        ] {
            let asked = request(emit, keys, filter);
            assert!(
                !histogram_is_enough(&asked),
                "{asked:?} needs the requests one at a time"
            );
        }
        let mut counted = request(vec![log2], vec![], None);
        counted.mode = Mode::Count;
        assert!(!histogram_is_enough(&counted));
    }

    /// A row becomes one event weighed by the requests it counted, at its
    /// bucket's low bound -- the value the emit layer's log2 bucketing puts
    /// back in that bucket -- with the exact total beside it, and keyed by the
    /// device and nothing else.
    #[test]
    fn a_histogram_row_is_one_weighted_event_in_its_own_bucket() {
        let mut collector = collector(Measure::Latency);
        let row = crate::bpf::HistRow {
            dev: (259 << 20) | 6,
            bucket: 15,
            count: 40,
            total_ns: 40 * 20_000,
        };
        let events = collector.histogram_events(vec![row], 0);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        let value = |name: &str| {
            event
                .attrs
                .iter()
                .find(|attr| attr.name == name)
                .unwrap_or_else(|| panic!("no `{name}`"))
                .value
                .clone()
        };
        assert_eq!(value("latency_ns"), crate::event::AttrValue::Uint(1 << 14));
        assert_eq!(
            value(crate::emit::WEIGHT_ATTR),
            crate::event::AttrValue::Uint(40)
        );
        assert_eq!(value("total_ns"), crate::event::AttrValue::Uint(800_000));
        assert_eq!(event.keys, vec![KeyValue::dev(259, 6)]);
        assert_eq!(collector.histogram_requests, 40);
    }
}
