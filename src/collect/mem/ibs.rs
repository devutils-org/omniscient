//! `mem.latency` and `mem.sharing` over AMD instruction-based sampling.
//!
//! A counting PMU says how many loads missed; it cannot say which load, where
//! it went, or how long it waited, because the counter overflows some cycles
//! after the instruction that caused it and the record is written about
//! whatever was retiring then. IBS works the other way round. It tags one
//! micro-op in every so many, follows that op through the pipeline, and when it
//! retires writes down what happened to it: the linear address it accessed,
//! which level of the hierarchy answered, whether the line came from another
//! core's cache in modified state, and -- for a load that missed the L1 data
//! cache -- the cycles until its data arrived. The kernel decodes those
//! registers into the vendor-neutral `data_src` and `weight` words that Intel's
//! PEBS fills too, so this collector reads nothing AMD-specific beyond the
//! unit's type number and its two format bits.
//!
//! One unit, two questions, and they keep different samples:
//!
//! - **`mem.latency`** keeps every sampled load the hardware measured a latency
//!   for. On family 17h that is a load that missed L1d: a hit carries no
//!   latency at all, and bucketing it as zero would put the loads the hardware
//!   did not time at the fast end of the distribution. They are counted by the
//!   level that answered them, and the count travels with the numbers.
//! - **`mem.sharing`** keeps every sampled access whose line was answered by
//!   another core's cache holding it modified -- `HITM`, the signal two cores
//!   writing one line leave, whether they write the same bytes or not.
//!
//! Following a program's threads needs one event per cpu with `inherit`, since
//! the kernel refuses a ring to an inherited event over every cpu at once; each
//! cpu then has one ring, and every task's event on that cpu writes into it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration as StdDuration;

use super::cgroup::Cgroup;
use crate::collect::task::Comms;
use crate::collect::{not_built, Collector, Losses, Prepared, SampleError};
use crate::emit::STACK_ID_ATTR;
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::perf::{
    online_cpus, Callchain, IbsOp, MemLevel, MemOp, MemoryAccess, Rate, Record, Sample, SampleSpec,
    SampledEvent, Scope,
};
use crate::request::{KeyKind, Mode, Target};
use crate::stack::Unwinder;

/// How often the rings are drained. A tagged op is a record, and at the rates
/// a latency distribution needs, a ring left alone for long overflows.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// Data pages per cpu. One ring per cpu, so the whole run maps this times the
/// number of cpus the target can run on.
const RING_PAGES: usize = 64;

/// The line size, when the kernel does not say.
const DEFAULT_LINE: u64 = 64;

/// Which of the two questions a collector answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Question {
    Latency,
    Sharing,
}

impl Question {
    fn source(self) -> &'static str {
        match self {
            Question::Latency => "mem.latency",
            Question::Sharing => "mem.sharing",
        }
    }

    /// The series key when the request names none: a latency distribution is
    /// asked about per task, false sharing about a line.
    fn default_keys(self) -> [KeyKind; 3] {
        match self {
            Question::Latency => [KeyKind::Task, KeyKind::Mem, KeyKind::Cpu],
            Question::Sharing => [KeyKind::Mem, KeyKind::Task, KeyKind::Cpu],
        }
    }
}

/// The keys an event carries, in the order the request asked for them.
///
/// The first key is the series an emit groups by, so a request for `--key cpu`
/// gets one distribution per cpu. The keys it did not name follow, so a join
/// can still find the address on an event whose series is the task.
fn key_order(question: Question, requested: &[KeyKind]) -> Vec<KeyKind> {
    let mut order: Vec<KeyKind> = requested
        .iter()
        .copied()
        .filter(|kind| question.default_keys().contains(kind))
        .collect();
    for kind in question.default_keys() {
        if !order.contains(&kind) {
            order.push(kind);
        }
    }
    order
}

pub struct Prepare {
    pub question: Question,
    pub mode: Mode,
    pub keys: Vec<KeyKind>,
    pub callchain: Callchain,
    /// The request's predicate. The runner applies it to the events; it is
    /// carried here too so that the loads this collector counts but does not
    /// emit are counted over the same selection, rather than over everything
    /// the interpreter around a workload did.
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let ibs = IbsOp::probe().ok_or_else(|| {
            format!(
                "this machine publishes no `{}` unit under {}, so there is no op sampling to \
                 read: it is an AMD feature, from family 10h on",
                IbsOp::PMU,
                crate::perf::PMU_DEVICES
            )
        })?;
        let rate = match self.mode {
            Mode::Sample { freq } => Rate::Hz(freq.as_hz()),
            other => {
                return Err(format!(
                    "`{other}` mode does not sample ops: ask for `--mode sample@4999hz`"
                ))
            }
        };
        let cpus =
            online_cpus().map_err(|error| format!("cannot list the online cpus: {error}"))?;
        let (reach, pid) = Reach::of(target, &cpus)?;
        if self.callchain != Callchain::Off && pid.is_none() {
            return Err(not_built(format!(
                "a stack is symbolized through one process's mappings, and symbolizing the many \
                 processes a {} target samples is not built; ask for `--emit topk` or `raw`, or \
                 name a process",
                target.kind()
            )));
        }
        let mut collector = IbsCollector::new(
            self.question,
            ibs,
            rate,
            reach,
            pid,
            &self.keys,
            self.callchain,
        );
        collector.filter = self.filter;
        Ok(Box::new(collector))
    }
}

/// What the events are opened over, decided at attach and opened at start.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reach {
    /// These processes, and every thread they have or start.
    Processes { pids: Vec<i32>, cpus: Vec<u32> },
    /// These threads and nothing they start.
    Threads { tids: Vec<i32>, cpus: Vec<u32> },
    /// Everything that runs on these cpus.
    Cpus(Vec<u32>),
    /// Every task of a cgroup, on every cpu.
    Cgroup { cgroup: Cgroup, cpus: Vec<u32> },
}

impl Reach {
    fn of(target: &Target, online: &[u32]) -> Result<(Self, Option<i32>), String> {
        let cpus = online.to_vec();
        Ok(match target {
            // A launched program arrives here as the pid of the child held at
            // the gate, which has one thread and has started nothing yet: its
            // events, inherited, follow everything it goes on to start.
            Target::Pid(pids) => (
                Reach::Processes {
                    pids: pids.clone(),
                    cpus,
                },
                pids.first().copied(),
            ),
            Target::Tid(tids) => (
                Reach::Threads {
                    tids: tids.clone(),
                    cpus,
                },
                tids.first()
                    .and_then(|tid| crate::collect::task::tgid_of(*tid)),
            ),
            Target::System => (Reach::Cpus(cpus), None),
            Target::Cpu(set) => {
                if let Some(absent) = set.cpus().iter().find(|cpu| !online.contains(cpu)) {
                    return Err(format!(
                        "cpu {absent} is not online, so there is nothing running on it to sample"
                    ));
                }
                (Reach::Cpus(set.cpus().to_vec()), None)
            }
            Target::Cgroup(_) => (
                Reach::Cgroup {
                    cgroup: Cgroup::resolve(target)?,
                    cpus,
                },
                None,
            ),
            Target::Container(_) => {
                return Err(not_built(
                    "a container has to be resolved to its cgroup first, which is not built \
                     yet; pass `--cgroup` with the path",
                ))
            }
            other => {
                return Err(format!(
                    "op sampling follows tasks or cpus, and a {} target names neither",
                    other.kind()
                ))
            }
        })
    }

    /// The per-cpu scopes to open, grouped by cpu: the first of each group owns
    /// the ring and the rest follow into it.
    ///
    /// `directory` is the cgroup's descriptor, opened by the caller and held
    /// until every event is open.
    fn scopes(&self, directory: Option<i32>) -> Result<Vec<(u32, Vec<Scope>)>, String> {
        let per_cpu = |cpus: &[u32], tasks: &[i32]| -> Vec<(u32, Vec<Scope>)> {
            cpus.iter()
                .map(|&cpu| {
                    (
                        cpu,
                        tasks
                            .iter()
                            .map(|&pid| Scope::TaskOnCpu { pid, cpu })
                            .collect(),
                    )
                })
                .collect()
        };
        Ok(match self {
            // A process that is already running has threads the inherited
            // event of its leader will never see, because `inherit` follows
            // what a task starts after the event exists. So every thread it
            // has now gets an event of its own, and each of them follows
            // whatever that thread starts.
            Reach::Processes { pids, cpus } => {
                let mut tasks = Vec::new();
                for pid in pids {
                    tasks.extend(threads_of(*pid)?);
                }
                per_cpu(cpus, &tasks)
            }
            Reach::Threads { tids, cpus } => per_cpu(cpus, tids),
            Reach::Cpus(cpus) => cpus
                .iter()
                .map(|&cpu| (cpu, vec![Scope::Cpu(cpu)]))
                .collect(),
            Reach::Cgroup { cpus, .. } => {
                let fd = directory.expect("a cgroup reach is opened with its directory");
                cpus.iter()
                    .map(|&cpu| (cpu, vec![Scope::CgroupOnCpu { fd, cpu }]))
                    .collect()
            }
        })
    }

    fn inherits(&self) -> bool {
        matches!(self, Reach::Processes { .. })
    }
}

/// The threads a process has right now.
fn threads_of(pid: i32) -> Result<Vec<i32>, String> {
    let directory = format!("/proc/{pid}/task");
    let entries = std::fs::read_dir(&directory)
        .map_err(|error| format!("cannot list {directory}: {error}"))?;
    let mut tids: Vec<i32> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    tids.sort_unstable();
    Ok(tids)
}

/// The coherency unit, as the kernel describes the first cache of cpu 0.
fn line_size() -> u64 {
    std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cache/index0/coherency_line_size")
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .filter(|size: &u64| size.is_power_of_two())
        .unwrap_or(DEFAULT_LINE)
}

/// What the unit tagged, before either question kept anything.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Tally {
    /// Every sample the rings delivered.
    ops: u64,
    /// Samples the hardware described as a load.
    loads: u64,
    /// Loads by the level that answered, whether or not a latency came with
    /// them. This is where the L1 hits `mem.latency` cannot bucket are counted.
    loads_by_level: BTreeMap<MemLevel, u64>,
    /// Loads that carried a latency, which are the ones `mem.latency` keeps.
    timed: u64,
    /// Accesses answered by a line another core held modified.
    hitm: u64,
}

pub struct IbsCollector {
    id: CollectorId,
    question: Question,
    ibs: IbsOp,
    rate: Rate,
    reach: Reach,
    keys: Vec<KeyKind>,
    callchain: Callchain,
    line: u64,
    rings: Vec<SampledEvent>,
    comms: Comms,
    unwinder: Option<Unwinder>,
    losses: Losses,
    tally: Tally,
    /// Which tasks took a `HITM` on each line, for the note that says which
    /// lines more than one task fought over.
    contenders: BTreeMap<u64, (u64, BTreeSet<u32>)>,
    /// How many events were opened, and on how many cpus.
    opened: (usize, usize),
    filter: Option<FilterExpr>,
}

impl IbsCollector {
    fn new(
        question: Question,
        ibs: IbsOp,
        rate: Rate,
        reach: Reach,
        pid: Option<i32>,
        keys: &[KeyKind],
        callchain: Callchain,
    ) -> Self {
        Self {
            id: CollectorId::new(format!("{}/pmu:ibs", question.source())),
            question,
            ibs,
            rate,
            reach,
            keys: key_order(question, keys),
            callchain,
            line: line_size(),
            rings: Vec::new(),
            comms: Comms::new(),
            unwinder: match (callchain, pid) {
                (Callchain::Off, _) | (_, None) => None,
                (_, Some(pid)) => Some(Unwinder::for_process(pid)),
            },
            losses: Losses::default(),
            tally: Tally::default(),
            contenders: BTreeMap::new(),
            opened: (0, 0),
            filter: None,
        }
    }

    fn spec(&self) -> SampleSpec {
        SampleSpec {
            callchain: self.callchain,
            // The unit tags ops in the kernel too, and a load the kernel makes
            // on the program's behalf is not the program's. Left out when the
            // unit can filter by privilege; kept, and said, when it cannot.
            ..self.ibs.spec(self.rate, true, self.reach.inherits())
        }
    }

    fn open(&mut self) -> Result<(), String> {
        let spec = self.spec();
        // Held open until every event exists: the kernel resolves the
        // descriptor at `perf_event_open` and needs nothing from it afterwards.
        let directory = match &self.reach {
            Reach::Cgroup { cgroup, .. } => {
                Some(std::fs::File::open(&cgroup.directory).map_err(|error| {
                    format!("cannot open {}: {error}", cgroup.directory.display())
                })?)
            }
            _ => None,
        };
        let fd = directory.as_ref().map(std::os::fd::AsRawFd::as_raw_fd);
        let mut opened = 0;
        for (cpu, scopes) in self.reach.scopes(fd)? {
            let mut scopes = scopes.into_iter();
            let Some(first) = scopes.next() else { continue };
            let refused = |error: std::io::Error| refusal(cpu, &error, self.rate);
            let mut ring = match SampledEvent::open(&spec, first, RING_PAGES) {
                Ok(ring) => ring,
                // A thread that exited between the listing and the open has
                // nothing left to sample; every other refusal is the run's.
                Err(error) if gone(&error) && matches!(first, Scope::TaskOnCpu { .. }) => continue,
                Err(error) => return Err(refused(error)),
            };
            opened += 1;
            for scope in scopes {
                match ring.follow(&spec, scope) {
                    Ok(()) => opened += 1,
                    Err(error) if gone(&error) => {}
                    Err(error) => return Err(refused(error)),
                }
            }
            self.rings.push(ring);
        }
        if self.rings.is_empty() {
            return Err(
                "every task of the target exited before its events could be opened".to_owned(),
            );
        }
        self.opened = (opened, self.rings.len());
        Ok(())
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let mut samples = Vec::new();
        let mut dropped = 0u64;
        let mut throttles = 0u64;
        let mut undecoded = 0u64;
        for ring in &mut self.rings {
            ring.drain(|record| match record {
                Record::Sample(sample) => samples.push(sample),
                Record::Lost { count } => dropped += count,
                Record::Throttle { throttled } => throttles += u64::from(throttled),
                Record::Other { .. } => undecoded += 1,
            });
        }
        // Rings are per cpu and each is in order; the run's stream is by time.
        samples.sort_by_key(|sample| sample.time_ns);
        let mut index = seq;
        for sample in samples {
            if let Some(event) = self.keep(&sample, index) {
                events.push(event);
                index += 1;
            }
        }
        if dropped > 0 {
            self.losses.dropped += dropped;
            self.losses.note(
                "an op-sampling ring overflowed: ops were tagged and never read, and the busy \
                 phases are the ones that overflow, so what is left under-represents them",
            );
        }
        if throttles > 0 {
            self.losses.throttled += throttles;
            self.losses.note(
                "the kernel throttled op sampling: an unknown number of ops in each throttled \
                 period were not tagged (see kernel.perf_event_max_sample_rate)",
            );
        }
        self.losses.undecoded += undecoded;
    }

    /// Counts a sample and returns the event it is, when this question keeps it.
    ///
    /// Every memory access is made into an event before either question looks
    /// at it, because the tally below is taken over the accesses the request's
    /// filter keeps: an L1 hit is emitted by `mem.latency` without a latency,
    /// and the count of them is only worth reading if it is a count of the ones
    /// asked about.
    fn keep(&mut self, sample: &Sample, seq: u64) -> Option<Event> {
        self.tally.ops += 1;
        let access = sample.access?;
        let op = access.source.op();
        let hitm = access.source.hitm();
        if op == MemOp::None && !hitm {
            return None;
        }
        let event = self.event_of(sample, access, seq);
        if let Some(predicate) = &self.filter {
            if !crate::filter::matches(predicate, &event) {
                return None;
            }
        }

        let level = access.source.level();
        if op == MemOp::Load {
            self.tally.loads += 1;
            *self.tally.loads_by_level.entry(level).or_insert(0) += 1;
            if access.latency_cycles > 0 {
                self.tally.timed += 1;
            }
        }
        if hitm {
            self.tally.hitm += 1;
        }
        // Every sampled load, timed or not. A load this cpu did not time -- an
        // L1 hit, on family 17h -- is an event with its level and no
        // `latency_cycles`, which the emits count as `unmeasured` beside the
        // distribution instead of leaving it to a sentence.
        let kept = match self.question {
            Question::Latency => op == MemOp::Load,
            Question::Sharing => hitm,
        };
        if !kept {
            return None;
        }
        if self.question == Question::Sharing {
            let line = sample.addr & !(self.line - 1);
            let entry = self.contenders.entry(line).or_insert((0, BTreeSet::new()));
            entry.0 += 1;
            entry.1.insert(sample.tid);
        }
        let mut event = event;
        if let Some(stack) = self
            .unwinder
            .as_mut()
            .and_then(|unwinder| unwinder.record(sample))
        {
            event.attrs.push(Attr::uint(STACK_ID_ATTR, stack.0));
        }
        Some(event)
    }

    fn event_of(&mut self, sample: &Sample, access: MemoryAccess, seq: u64) -> Event {
        let line = sample.addr & !(self.line - 1);
        let keys = self
            .keys
            .iter()
            .filter_map(|kind| match kind {
                KeyKind::Task => Some(KeyValue::task(sample.tid as i32)),
                // A latency is about one address, and a one-byte range is what
                // an interval join asks an allocation to contain. Sharing is
                // about the line, which is the unit two cores fight over.
                KeyKind::Mem => Some(match self.question {
                    Question::Latency => KeyValue::Mem {
                        base: sample.addr,
                        len: 1,
                    },
                    Question::Sharing => KeyValue::Mem {
                        base: line,
                        len: self.line,
                    },
                }),
                KeyKind::Cpu => sample.cpu.map(KeyValue::cpu),
                _ => None,
            })
            .collect();
        let mut attrs = Vec::with_capacity(8);
        match self.question {
            // Absent rather than zero when the unit did not time the load: a
            // zero would be a measurement of an instant access.
            Question::Latency if access.latency_cycles > 0 => attrs.push(Attr::uint(
                "latency_cycles",
                u64::from(access.latency_cycles),
            )),
            Question::Latency => {}
            Question::Sharing => attrs.push(Attr::uint("count", 1)),
        }
        attrs.extend([
            Attr::text("level", access.source.level().as_str()),
            Attr::uint("addr", sample.addr),
            Attr::uint("line", line),
            Attr::uint("ip", sample.ip),
            Attr::uint("tlb_miss", u64::from(access.source.tlb_miss())),
        ]);
        if self.question == Question::Sharing {
            attrs.push(Attr::uint(
                "latency_cycles",
                u64::from(access.latency_cycles),
            ));
        }
        if let Some(cpu) = sample.cpu {
            attrs.push(Attr::uint("cpu", u64::from(cpu)));
        }
        if let Some(comm) = self.comms.get(sample.tid as i32) {
            attrs.push(Attr::text("comm", comm));
        }
        Event {
            ts_ns: sample.time_ns,
            ts_err_ns: 0,
            kind: self.question.source().to_owned(),
            keys,
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }
}

/// Whether an open failed because the task it named has already gone.
fn gone(error: &std::io::Error) -> bool {
    error.to_string().contains("No such process")
}

/// A refusal in the reader's terms: which privilege, or which limit.
fn refusal(cpu: u32, error: &std::io::Error, rate: Rate) -> String {
    let text = error.to_string();
    if text.contains("Invalid argument") {
        if let Rate::Hz(hz) = rate {
            let limit = std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
                .map(|text| text.trim().to_owned())
                .unwrap_or_else(|_| "unknown".to_owned());
            return format!(
                "the kernel refused op sampling at {hz} Hz on cpu {cpu} ({text}); this machine \
                 allows at most {limit} samples a second (kernel.perf_event_max_sample_rate)"
            );
        }
    }
    format!(
        "the kernel refused op sampling on cpu {cpu} ({text}); following a program's threads \
         needs an event per cpu, which needs CAP_PERFMON or kernel.perf_event_paranoid at 1 or \
         below"
    )
}

impl Collector for IbsCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        match self.question {
            Question::Latency => "latency_cycles",
            Question::Sharing => "count",
        }
    }

    fn start(&mut self) -> Result<(), String> {
        if self.callchain.kernel() {
            crate::stack::prime_kernel_symbols();
        }
        self.open()?;
        for ring in &self.rings {
            ring.enable().map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if let Some(unwinder) = self.unwinder.as_mut() {
            unwinder.refresh_symbols();
        }
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        for ring in &self.rings {
            let _ = ring.disable();
        }
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        // The perf map's lines flushed after the last tick (see
        // `Unwinder::finish`).
        if let Some(unwinder) = self.unwinder.as_mut() {
            unwinder.finish();
        }
        for ring in &self.rings {
            self.losses.undecoded += ring.undecoded();
        }
        self.rings.clear();
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn stacks(&self) -> BTreeMap<u64, Vec<String>> {
        let Some(unwinder) = self.unwinder.as_ref() else {
            return BTreeMap::new();
        };
        unwinder
            .table()
            .iter()
            .map(|(id, stack)| {
                let frames = stack
                    .frames
                    .iter()
                    .rev()
                    .map(crate::stack::Frame::label)
                    .collect();
                (id.0, frames)
            })
            .collect()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        let tally = &self.tally;
        let (events, cpus) = self.opened;
        notes.push(format!(
            "IBS tagged {} ops over {events} event{} on {cpus} cpu{}, one in every so many {}: {} \
             were loads{}, {} of them timed, and {} were answered by a line another core held \
             modified",
            tally.ops,
            if events == 1 { "" } else { "s" },
            if cpus == 1 { "" } else { "s" },
            if self.ibs.counts_ops() {
                "dispatched ops"
            } else {
                "cycles (this unit cannot count ops, so ops around a long stall are \
                 under-represented)"
            },
            tally.loads,
            if self.filter.is_some() {
                " the filter kept"
            } else {
                ""
            },
            tally.timed,
            tally.hitm,
        ));
        if !self.ibs.can_exclude_kernel() {
            notes.push(
                "this unit has no software filter, so accesses made in kernel mode are included"
                    .to_owned(),
            );
        }
        match self.question {
            Question::Latency => {
                let untimed = tally.loads - tally.timed;
                if untimed > 0 {
                    let levels = tally
                        .loads_by_level
                        .iter()
                        .map(|(level, count)| format!("{} {count}", level.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    notes.push(format!(
                        "{untimed} of {} sampled loads{} carry no latency and are counted as \
                         unmeasured rather than bucketed: this cpu times a load only when it \
                         misses L1d, so the distribution is of the loads that missed; sampled \
                         loads by level: {levels}",
                        tally.loads,
                        if self.filter.is_some() {
                            " the filter kept"
                        } else {
                            ""
                        },
                    ));
                }
                notes.push(
                    "latencies are core cycles from the miss to the data, not nanoseconds: the \
                     core clock moves with load and temperature, and a cycle count is what the \
                     hardware measured"
                        .to_owned(),
                );
            }
            Question::Sharing => {
                let mut contested: Vec<_> = self
                    .contenders
                    .iter()
                    .filter(|(_, (_, tasks))| tasks.len() > 1)
                    .collect();
                contested
                    .sort_by(|left, right| right.1 .0.cmp(&left.1 .0).then(left.0.cmp(right.0)));
                if contested.is_empty() {
                    notes.push(
                        "no line was answered modified to more than one task: a HITM taken by a \
                         single task is a line it shares with something this run does not \
                         follow"
                            .to_owned(),
                    );
                } else {
                    let lines = contested
                        .iter()
                        .take(5)
                        .map(|(line, (count, tasks))| {
                            format!("{line:#x} ({count} from {} tasks)", tasks.len())
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    notes.push(format!(
                        "{} line{} answered modified to more than one task: {lines}",
                        contested.len(),
                        if contested.len() == 1 {
                            " was"
                        } else {
                            "s were"
                        },
                    ));
                }
            }
        }
        if let Some(unwinder) = self.unwinder.as_ref() {
            notes.extend(unwinder.notes());
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_series_key_is_the_one_asked_for_and_the_rest_follow() {
        assert_eq!(
            key_order(Question::Latency, &[]),
            vec![KeyKind::Task, KeyKind::Mem, KeyKind::Cpu]
        );
        assert_eq!(
            key_order(Question::Sharing, &[]),
            vec![KeyKind::Mem, KeyKind::Task, KeyKind::Cpu]
        );
        assert_eq!(
            key_order(Question::Latency, &[KeyKind::Cpu]),
            vec![KeyKind::Cpu, KeyKind::Task, KeyKind::Mem]
        );
    }

    /// A child with a known number of threads, because this test binary's own
    /// come and go with the tests running beside this one.
    #[test]
    fn a_running_process_is_followed_through_every_thread_it_has() {
        let Ok(mut child) = std::process::Command::new("python3")
            .args([
                "-c",
                "import threading, time\n\
                 for _ in range(3): threading.Thread(target=time.sleep, args=(10,), daemon=True).start()\n\
                 time.sleep(10)",
            ])
            .spawn()
        else {
            eprintln!("skipped: no python3 to start a process with known threads");
            return;
        };
        let pid = child.id() as i32;
        let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
        while threads_of(pid).map_or(0, |tids| tids.len()) < 4 {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never started its threads"
            );
            std::thread::sleep(StdDuration::from_millis(10));
        }

        let (reach, followed) = Reach::of(&Target::Pid(vec![pid]), &[0, 1]).expect("reach");
        let scopes = reach.scopes(None);
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(followed, Some(pid));
        assert!(
            reach.inherits(),
            "a process is followed into what it starts"
        );
        let scopes = scopes.expect("scopes");
        assert_eq!(scopes.len(), 2, "one group per cpu");
        for (cpu, group) in scopes {
            assert_eq!(
                group.len(),
                4,
                "one event per thread on cpu {cpu}: {group:?}"
            );
            assert!(
                group.contains(&Scope::TaskOnCpu { pid, cpu }),
                "the leader is followed too"
            );
        }
    }

    #[test]
    fn a_cpu_that_is_not_online_is_refused_by_name() {
        let target = Target::Cpu("7".parse().expect("a cpu list"));
        let refusal = Reach::of(&target, &[0, 1]).expect_err("cpu 7 is not online");
        assert!(refusal.contains("cpu 7"), "{refusal}");
    }
}
