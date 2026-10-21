//! `cpu.profile` over `perf_event_open`, with call stacks.
//!
//! This is the source that makes the stack machinery mean something. Every
//! other collector answers "how much"; this one answers "where", and the answer
//! is only worth having if the frames under the leaf are really its callers.
//! So the unwinder's own account of what it could walk travels with the numbers
//! as a degradation rather than being left to be noticed in a flame graph that
//! looks plausible.
//!
//! The clock is `PERF_COUNT_SW_CPU_CLOCK`, whose period counts nanoseconds of
//! *on-cpu* time. A task that is asleep produces no samples, which is what an
//! on-cpu profile means: the count of samples on a stack is proportional to the
//! cpu time spent there, and a request for 199 Hz asks for one sample per
//! 5.025 ms of cpu, not one per 5.025 ms of wall clock.
//!
//! **It follows every thread**, the ones the target already has and the ones
//! it starts, and the processes it forks: one event per task, each carrying
//! its own period across every cpu ([`TaskSampler`]), and the tasks the target
//! starts found on each tick. Until the eleventh wave it sampled the one task
//! it was opened on, and `omn latency` read every other thread's on-cpu time as
//! nothing. A forked child is another address space, so each process met is
//! symbolized against its own mappings.
//!
//! Not inherited, deliberately. An inherited event would follow a task from
//! its first instruction, but the kernel maps one only pinned to a cpu, and a
//! task sampled by one event per cpu loses the unfinished part of a period on
//! every cpu it ran on: five to nine milliseconds of a 400 ms program, where a
//! time decomposition states one period as this row's resolution. A task found
//! on a tick has its cpu time until then read from its `schedstat`, and the run
//! says how much cpu went unsampled that way.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration as StdDuration;

use perf_event_open_sys::bindings as perf;

use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::emit::STACK_ID_ATTR;
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::perf::{Callchain, Record, SampleSpec, TaskSampler, ThreadSampler};
use crate::request::{Mode, Target};
use crate::stack::unwind::Debuginfod;
use crate::stack::{StackChoice, Unwinder};

const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// A sample with a callchain is a word per frame rather than a fixed record, so
/// the same burst needs far more room than it would without stacks.
///
/// Per task: each task has an event and a ring of its own, and one task yields
/// at most a sample per period between drains. The target's own task keeps
/// the 256 pages the profile had when it followed that task alone: when the
/// rings became one per thread they were cut to 32 each, and a single-threaded
/// program sampled at 100 kHz went from losing no sample to losing about 6400
/// of 22000, every one of them in the one ring it has.
const RING_PAGES: usize = 32;
const ROOT_RING_PAGES: usize = 256;

/// A frequency in hertz, as a period of cpu nanoseconds.
fn period_for(hz: u64) -> u64 {
    (1_000_000_000 / hz).max(1)
}

/// What a profile samples on: the kernel's cpu clock for `cpu.profile`, or the
/// counter of the counting source whose `sample` mode this serves --
/// `cpu.cycles -m sample@997hz -e stack` is `perf record`'s default, and
/// `cpu.branch_misses` sampled is `perf record -e branch-misses` (perfrecord
/// 2 and 8). One collector for all of them, because what differs is one
/// `perf_event_attr` and what a sample weighs; the rings, the unwinding and
/// the symbols are the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sampled {
    pub source: &'static str,
    pub backend: &'static str,
    /// `perf_event_attr.type` and `.config`.
    kind: u32,
    config: u64,
    /// The event's name, as a sample's `event` attribute carries it.
    pub name: &'static str,
    weight: Weight,
}

/// What one sample stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Weight {
    /// A period of cpu nanoseconds: `1e9 / hz` of on-cpu time each.
    CpuTime,
    /// A fixed number of cycles: the cpu's highest frequency over the rate,
    /// so a sample per so many cycles whatever the clock does. The kernel's
    /// frequency mode instead when that frequency is not published.
    Cycles,
    /// The kernel's frequency mode: about `hz` samples a second, each a
    /// period the kernel adjusts and this does not see.
    Adjusted,
}

const fn hardware(config: u32) -> (u32, u64) {
    (perf::PERF_TYPE_HARDWARE, config as u64)
}

const fn software(config: u32) -> (u32, u64) {
    (perf::PERF_TYPE_SOFTWARE, config as u64)
}

/// A `PERF_TYPE_HW_CACHE` event: which cache, which operation and which
/// result, a byte each, as `perf list` spells `L1-dcache-load-misses`.
const fn hw_cache(cache: u32, op: u32, result: u32) -> (u32, u64) {
    (
        perf::PERF_TYPE_HW_CACHE,
        cache as u64 | (op as u64) << 8 | (result as u64) << 16,
    )
}

const fn sampled(
    source: &'static str,
    backend: &'static str,
    (kind, config): (u32, u64),
    name: &'static str,
    weight: Weight,
) -> Sampled {
    Sampled {
        source,
        backend,
        kind,
        config,
        name,
        weight,
    }
}

/// Every (source, backend) this collector samples for.
pub const SAMPLED: &[Sampled] = &[
    sampled(
        "cpu.profile",
        "pmu:sw",
        software(perf::PERF_COUNT_SW_CPU_CLOCK),
        "cpu-clock",
        Weight::CpuTime,
    ),
    sampled(
        "cpu.cycles",
        "pmu",
        hardware(perf::PERF_COUNT_HW_CPU_CYCLES),
        "cycles",
        Weight::Cycles,
    ),
    sampled(
        "cpu.cycles",
        "pmu:sw",
        software(perf::PERF_COUNT_SW_CPU_CLOCK),
        "cpu-clock",
        Weight::CpuTime,
    ),
    sampled(
        "cpu.instructions",
        "pmu",
        hardware(perf::PERF_COUNT_HW_INSTRUCTIONS),
        "instructions",
        Weight::Adjusted,
    ),
    sampled(
        "cpu.branches",
        "pmu",
        hardware(perf::PERF_COUNT_HW_BRANCH_INSTRUCTIONS),
        "branches",
        Weight::Adjusted,
    ),
    sampled(
        "cpu.branch_misses",
        "pmu",
        hardware(perf::PERF_COUNT_HW_BRANCH_MISSES),
        "branch-misses",
        Weight::Adjusted,
    ),
    sampled(
        "cpu.cache_misses",
        "pmu",
        hardware(perf::PERF_COUNT_HW_CACHE_MISSES),
        "cache-misses",
        Weight::Adjusted,
    ),
    sampled(
        "cpu.context_switches",
        "pmu:sw",
        software(perf::PERF_COUNT_SW_CONTEXT_SWITCHES),
        "context-switches",
        Weight::Adjusted,
    ),
    // `mem.cache_*`'s `sample` mode: a profile on the miss, `perf record -e
    // L1-dcache-load-misses`, where the loads beside it are what a counted
    // window divides by and a profile has no use for.
    sampled(
        "mem.cache_l1",
        "pmu",
        hw_cache(
            perf::PERF_COUNT_HW_CACHE_L1D,
            perf::PERF_COUNT_HW_CACHE_OP_READ,
            perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
        ),
        "L1-dcache-load-misses",
        Weight::Adjusted,
    ),
    sampled(
        "mem.cache_llc",
        "pmu",
        hw_cache(
            perf::PERF_COUNT_HW_CACHE_LL,
            perf::PERF_COUNT_HW_CACHE_OP_READ,
            perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
        ),
        "LLC-load-misses",
        Weight::Adjusted,
    ),
    sampled(
        "mem.cache_llc",
        "pmu:generic",
        hardware(perf::PERF_COUNT_HW_CACHE_MISSES),
        "cache-misses",
        Weight::Adjusted,
    ),
    sampled(
        "mem.cache_tlb",
        "pmu",
        hw_cache(
            perf::PERF_COUNT_HW_CACHE_DTLB,
            perf::PERF_COUNT_HW_CACHE_OP_READ,
            perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
        ),
        "dTLB-load-misses",
        Weight::Adjusted,
    ),
];

/// A frame as the instruction it is at: `function+0xoff`, or the object and
/// its offset for a frame nothing named, or the bare address.
fn instruction(frame: &crate::stack::Frame) -> String {
    match (&frame.symbol, &frame.module) {
        (Some(symbol), _) => format!("{symbol}+{:#x}", frame.offset),
        (None, Some(module)) => format!(
            "{}+{:#x}",
            module.rsplit('/').next().unwrap_or(module),
            frame.offset
        ),
        (None, None) => format!("{:#x}", frame.addr),
    }
}

/// The event a (source, backend) samples on, or nothing when this collector
/// does not sample for it.
pub fn sampled_for(source: &str, backend: &str) -> Option<Sampled> {
    SAMPLED
        .iter()
        .find(|sampled| sampled.source == source && sampled.backend == backend)
        .copied()
}

/// The cpu's highest frequency in hertz, as the kernel publishes it.
fn max_frequency_hz() -> Option<u64> {
    let khz: u64 = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (khz > 0).then_some(khz * 1_000)
}

use crate::perf::parent_of;

pub struct Prepare {
    pub mode: Mode,
    /// The event sampled, and the source and backend it serves.
    pub sampled: Sampled,
    /// How the stacks are unwound, as [`crate::stack::choose`] decided.
    pub choice: StackChoice,
    /// Where a stripped file's names are fetched from, when a caller has a
    /// client of its own; `None` takes the one `choice` names, which is
    /// `--symbols`.
    pub debuginfod: Option<std::sync::Arc<Debuginfod>>,
}

impl Prepare {
    fn collector(&self, pid: i32, whole_process: bool) -> ProfileCollector {
        let mut collector =
            ProfileCollector::new(pid, self.mode, self.choice.callchain(), whole_process);
        collector.sampled = self.sampled;
        // A client handed in wins; otherwise the choice's own, which is what
        // `--symbols debuginfod` asked for (`StackChoice::debuginfod`).
        collector.debuginfod = self.debuginfod.clone().or_else(|| self.choice.debuginfod());
        if pid > 0 {
            collector.unwinders.insert(
                pid as u32,
                Unwinder::new(pid, collector.callchain, collector.debuginfod.clone()),
            );
        }
        collector.choice = Some(self.choice.clone());
        collector
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        // A machine or a set of cores is one cpu-clock event per cpu, over
        // every task that runs there. The unfinished-period loss that keeps a
        // process profile off inherited per-cpu events does not arise: the
        // cpu's clock runs whoever is on it, so no period is left unfinished
        // by a task moving on.
        let cpus = match target {
            Target::System => Some(
                crate::perf::online_cpus()
                    .map_err(|error| format!("cannot tell which cpus are online: {error}"))?,
            ),
            Target::Cpu(set) => Some(set.cpus().to_vec()),
            _ => None,
        };
        if let Some(cpus) = cpus {
            let mut collector = self.collector(-1, false);
            collector.cpus = Some(cpus);
            return Ok(Box::new(collector));
        }
        // A cgroup is the same shape, each cpu's event filtered by the kernel
        // to the cgroup's tasks (`cgroup.rs`).
        if let Target::Cgroup(path) = target {
            let cgroup = super::cgroup::Cgroup::resolve(path)?;
            let mut collector = self.collector(-1, false);
            collector.cpus = Some(
                crate::perf::online_cpus()
                    .map_err(|error| format!("cannot tell which cpus are online: {error}"))?,
            );
            collector.cgroup = Some(cgroup);
            return Ok(Box::new(collector));
        }
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "profiling a {} target is not built: a profile follows a process, a core, a \
                 cgroup or the whole machine, and this target names none of them",
                target.kind()
            )
        })?;
        // A `--tid` target is that task and what it starts. Every other
        // target that names a process is every thread it has.
        Ok(Box::new(
            self.collector(pid, !matches!(target, Target::Tid(_))),
        ))
    }
}

/// What the samples come from: a process's tasks, each with its own event, or
/// every task on a set of cpus, one event per cpu.
enum Sampler {
    Tasks(TaskSampler),
    Cpus(ThreadSampler),
}

impl Sampler {
    fn drain(&mut self, handle: impl FnMut(Record)) {
        match self {
            Sampler::Tasks(sampler) => sampler.drain(handle),
            Sampler::Cpus(sampler) => sampler.drain(handle),
        }
    }

    fn disable(&mut self) -> std::io::Result<()> {
        match self {
            Sampler::Tasks(sampler) => sampler.disable(),
            Sampler::Cpus(sampler) => sampler.disable(),
        }
    }

    fn undecoded(&self) -> u64 {
        match self {
            Sampler::Tasks(sampler) => sampler.undecoded(),
            Sampler::Cpus(sampler) => sampler.undecoded(),
        }
    }

    fn throttled(&self) -> u64 {
        match self {
            Sampler::Tasks(sampler) => sampler.throttled(),
            Sampler::Cpus(sampler) => sampler.throttled(),
        }
    }
}

/// When a followed process's mappings are re-read: in the drains that hold
/// samples of it, once each, and in no other.
///
/// - Before a drain: a process the last drain held samples of, as every
///   process used to be on every tick, and one no drain has held samples of
///   yet. The maps are then those of the interval the samples come from,
///   which matters across an `execve`: a launched program's first samples are
///   this tool's own code in the child calling `execve`, and maps read after
///   the drain name them against the new image and lose them (82 to 266
///   frames in no mapping at 100 kHz, against 7 to 54).
/// - After a drain, before any of its stacks is interned: a process the
///   drain holds samples of and that was not read before it.
/// - Never twice for one drain's samples: a process read after a drain, or
///   whose unwinder was built in it (which reads its maps), is not read again
///   before the next one. Without that, a process sampled now and then -- most
///   of a machine, in `--cpus` mode -- was read after the drain that held it
///   and again before the next, which holds nothing of it.
///
/// What this does not save: a process no drain has held samples of is read
/// before every drain until one does -- a launched program that only sleeps,
/// or one that lives in system calls under a user-only profile until its
/// first sample in user mode, costs what it did under the old rule.
#[derive(Debug, Default)]
struct MapReads {
    /// The processes the last drain held samples of.
    recent: BTreeSet<u32>,
    /// The processes whose maps were read after the last drain, or while it
    /// was processed.
    fresh: BTreeSet<u32>,
}

impl MapReads {
    /// Whether `pid` is read before this drain; `known` says whether a drain
    /// has held samples of it.
    fn before_drain(&self, pid: u32, known: bool) -> bool {
        !known || (self.recent.contains(&pid) && !self.fresh.contains(&pid))
    }

    /// What is read after the drain that held samples of `sampled`, `before`
    /// being what was read before it and `followed` saying which processes
    /// have an unwinder to read with.
    fn after_drain(
        &mut self,
        sampled: BTreeSet<u32>,
        before: &BTreeSet<u32>,
        followed: impl Fn(u32) -> bool,
    ) -> Vec<u32> {
        let now: Vec<u32> = sampled
            .difference(before)
            .copied()
            .filter(|pid| followed(*pid))
            .collect();
        self.fresh = now.iter().copied().collect();
        self.recent = sampled;
        now
    }

    /// `pid`'s maps were read while this drain was processed.
    fn read_now(&mut self, pid: u32) {
        self.fresh.insert(pid);
    }
}

pub struct ProfileCollector {
    pid: i32,
    mode: Mode,
    /// The event sampled; the cpu clock unless a counting source's `sample`
    /// mode asked for its own counter.
    sampled: Sampled,
    /// The period a fixed-period event was opened with, in its own unit, or
    /// `None` for the kernel's frequency mode: what the notes say a sample
    /// weighs.
    period: Option<u64>,
    /// Each sampled task's name, re-read at a bounded rate: the `comm` the
    /// catalog declares as a filter field (perfrecord 1).
    comms: crate::collect::task::Comms,
    callchain: Callchain,
    event: Option<Sampler>,
    /// The cpus sampled, for a machine or a core target. `None` follows `pid`.
    cpus: Option<Vec<u32>>,
    /// The cgroup whose tasks are sampled on `cpus`, for a cgroup target.
    cgroup: Option<super::cgroup::Cgroup>,
    /// Throttled periods already reported; see `Sampler::throttled`.
    throttled: u64,
    /// Samples a cpu took while it ran nothing: the idle task is not a
    /// program, and its stacks are not on-cpu work, so they are counted here
    /// and left out.
    idle: u64,
    /// Which processes have their mappings re-read around each drain; see
    /// [`MapReads`] and [`ProfileCollector::drain_into`].
    map_reads: MapReads,
    /// How many times a process's mappings were re-read, all drains
    /// together: what the tests hold the rule to.
    #[cfg_attr(not(test), allow(dead_code))]
    refreshes: u64,
    /// How many drains held no sample with a stack.
    #[cfg_attr(not(test), allow(dead_code))]
    idle_drains: u64,
    /// Every process a sample came from.
    processes: BTreeSet<u32>,
    /// One per process samples arrived from, by pid, re-read in the drains
    /// that hold samples of it ([`MapReads`]). A symbolizer reads its process's `/proc/<pid>/maps`, and those are gone
    /// the moment it exits -- but for a launched program they are also not yet
    /// the target's when this is built, because the child is still a fork of
    /// this process waiting at the gate. See [`Unwinder::refresh_symbols`].
    ///
    /// More than one because the events are inherited across `fork`, and a
    /// child that execs is another program at other addresses: its frames
    /// symbolized against the target's maps would name functions it never ran.
    unwinders: BTreeMap<u32, Unwinder>,
    /// Each unwinder numbers its own stacks from zero, so the id an event
    /// carries is this run's: `(pid, the unwinder's id)` to the run's id, and
    /// the run's ids in order.
    stack_ids: BTreeMap<(u32, u64), u64>,
    stack_origins: Vec<(u32, u64)>,
    /// The file each sampled task's process executed, by the key its events
    /// carry (`task:<tid>`), read at the task's first sample with a stack,
    /// while it is alive: after the run `/proc/<pid>/exe` is gone.
    executables: BTreeMap<String, String>,
    /// The process of each sampled task with a stack, by the same key: its
    /// unwinder's executable, re-read after an `execve`, is what the
    /// provenance names, `executables` what was read first.
    task_processes: BTreeMap<String, u32>,
    losses: Losses,
    /// Whether every thread of `pid` is followed rather than `pid` alone.
    whole_process: bool,
    /// The tasks samples actually arrived from.
    sampled_tasks: BTreeSet<i32>,
    /// How many tasks had an event from the open, read at `start` and kept,
    /// because `stop` closes the events and the notes are gathered after it.
    at_open: usize,
    /// Tasks found on a tick after they began, and the cpu they had used by
    /// then: time no sample of this run covers.
    found_late: usize,
    unsampled_ns: u64,
    /// The same per task (`task:<tid>`), carried to the results' `fields`
    /// ([`crate::join::UNSAMPLED_NS`]) so that the task's on-cpu row adds it:
    /// `make`'s second compile, found on a late tick, had a second of cpu in
    /// no row while the run's notes said so.
    unsampled_by_task: BTreeMap<i32, u64>,
    /// Each sampled process's parent, read from its `/proc/<pid>/stat` the
    /// first time a sample of it arrives, while it runs, and carried on every
    /// sample as `ppid`: a compile that only computes never blocks, so the
    /// scheduler sources gave `make`'s busiest `cc1` no parent, and both ways
    /// of reading a job took every busy task for it.
    parents: BTreeMap<u32, Option<u64>>,
    unwinding: bool,
    /// How the stacks are unwound and why, when a chooser decided it.
    choice: Option<StackChoice>,
    debuginfod: Option<std::sync::Arc<Debuginfod>>,
    /// What each ring lost, by the key its ring belongs to: the task a
    /// per-task ring follows, the cpu a per-cpu ring is pinned to -- the
    /// samples it dropped and the periods the kernel throttled it. Read from
    /// the sampler at each drain, because `stop` closes it.
    ring_losses: Vec<(KeyValue, RingLost)>,
}

impl ProfileCollector {
    pub fn new(pid: i32, mode: Mode, callchain: Callchain, whole_process: bool) -> Self {
        let mut unwinders = BTreeMap::new();
        if pid > 0 {
            unwinders.insert(pid as u32, Unwinder::for_process(pid));
        }
        Self {
            pid,
            mode,
            sampled: SAMPLED[0],
            period: None,
            comms: crate::collect::task::Comms::new(),
            callchain,
            event: None,
            cpus: None,
            cgroup: None,
            throttled: 0,
            idle: 0,
            map_reads: MapReads::default(),
            refreshes: 0,
            idle_drains: 0,
            processes: BTreeSet::new(),
            unwinders,
            stack_ids: BTreeMap::new(),
            stack_origins: Vec::new(),
            executables: BTreeMap::new(),
            task_processes: BTreeMap::new(),
            losses: Losses::default(),
            whole_process,
            sampled_tasks: BTreeSet::new(),
            at_open: 0,
            found_late: 0,
            unsampled_ns: 0,
            unsampled_by_task: BTreeMap::new(),
            parents: BTreeMap::new(),
            unwinding: true,
            choice: None,
            debuginfod: None,
            ring_losses: Vec::new(),
        }
    }

    /// The event this profile opens, sampling every `period` ns of cpu.
    ///
    /// The paranoia refuses the kernel to the samples, not only to the
    /// stacks: with no stack at all (`--stack none`) there is no callchain
    /// whose kernel half drops it, so a user-only choice says so itself.
    fn spec(&self, hz: u64) -> SampleSpec {
        let mut spec = SampleSpec::software(self.sampled.config, period_for(hz), self.callchain);
        spec.kind = self.sampled.kind;
        spec.rate = match self.rate_period(hz) {
            Some(period) => crate::perf::Rate::Period(period),
            None => crate::perf::Rate::Hz(hz),
        };
        spec.exclude_kernel = self.choice.as_ref().is_some_and(|choice| choice.user_only);
        spec
    }

    /// The fixed period a rate of `hz` is for this event, or `None` for the
    /// kernel's frequency mode.
    fn rate_period(&self, hz: u64) -> Option<u64> {
        match self.sampled.weight {
            Weight::CpuTime => Some(period_for(hz)),
            Weight::Cycles => max_frequency_hz().map(|max| (max / hz.max(1)).max(1)),
            Weight::Adjusted => None,
        }
    }

    /// Reads what the rings hold and turns each sample into an event.
    ///
    /// Only the processes being sampled have their mappings re-read, by the
    /// rule [`MapReads`] states. Re-reading every process's maps on every
    /// tick -- twice for a stack mode with CFI, and its executable's link
    /// once more -- was most of what a profile cost a program that is mostly
    /// off the cpu: 0.25 to 0.4 ms a tick of a debug build, whatever the
    /// program did, against `sqlite3`'s fsync-bound commits that burn 200 ms a
    /// second. A user-only profile (`perf_event_paranoid` 2) delivers no
    /// sample while the task is in the kernel, so a program that lives in
    /// system calls left nearly every tick empty and still paid for every
    /// read.
    ///
    /// The records are gathered before any is processed, because which
    /// processes they hold decides what is read before their stacks are
    /// interned. The price is memory: a tick holds the rings' whole contents
    /// at once, which with copied user stacks (`dwarf`) on per-cpu rings at a
    /// high frequency is every stack copy the rings held, where reading them
    /// one at a time held one.
    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(event) = self.event.as_mut() else {
            return;
        };
        let mut before = BTreeSet::new();
        for (pid, unwinder) in self.unwinders.iter_mut() {
            if self
                .map_reads
                .before_drain(*pid, self.processes.contains(pid))
            {
                unwinder.refresh_symbols();
                self.refreshes += 1;
                before.insert(*pid);
            }
        }
        let mut records = Vec::new();
        event.drain(|record| records.push(record));
        let sampled: BTreeSet<u32> = records
            .iter()
            .filter_map(|record| match record {
                Record::Sample(sample)
                    if !(sample.pid == 0 && sample.tid == 0)
                        && (sample.callchain.is_some() || sample.user_regs.is_some()) =>
                {
                    Some(sample.pid)
                }
                _ => None,
            })
            .collect();
        if sampled.is_empty() {
            self.idle_drains += 1;
        }
        let unwinders = &self.unwinders;
        for pid in self
            .map_reads
            .after_drain(sampled, &before, |pid| unwinders.contains_key(&pid))
        {
            if let Some(unwinder) = self.unwinders.get_mut(&pid) {
                unwinder.refresh_symbols();
                self.refreshes += 1;
            }
        }
        let unwinders = &mut self.unwinders;
        let stack_ids = &mut self.stack_ids;
        let stack_origins = &mut self.stack_origins;
        let executables = &mut self.executables;
        let task_processes = &mut self.task_processes;
        let seen = &mut self.sampled_tasks;
        let processes = &mut self.processes;
        let map_reads = &mut self.map_reads;
        let idle = &mut self.idle;
        let mut index = seq;
        let cgroup_key = self.cgroup.as_ref().map(|cgroup| cgroup.key());
        let callchain = self.callchain;
        let debuginfod = &self.debuginfod;
        let comms = &mut self.comms;
        let parents = &mut self.parents;
        let kind = self.sampled.source;
        let mut dropped = 0u64;
        let mut undecoded = 0u64;
        for record in records {
            match record {
                Record::Sample(sample) if sample.pid == 0 && sample.tid == 0 => *idle += 1,
                Record::Sample(sample) => {
                    seen.insert(sample.tid as i32);
                    processes.insert(sample.pid);
                    // A sample in the kernel's frequency mode stands for the
                    // period it was taken at, which the kernel adjusts from
                    // one to the next: weighed by it, a share is perf's, a
                    // share of the event and not of the samples.
                    let mut attrs = vec![
                        Attr::uint("count", sample.period.unwrap_or(1).max(1)),
                        Attr::uint("ip", sample.ip),
                        // The process beside the thread the key names, so a
                        // profile of a machine or a cgroup can be read per
                        // program without a join.
                        Attr::uint("pid", u64::from(sample.pid)),
                    ];
                    if let Some(period) = sample.period {
                        attrs.push(Attr::uint("period", period));
                    }
                    if let Some(ppid) = *parents
                        .entry(sample.pid)
                        .or_insert_with(|| parent_of(sample.pid))
                    {
                        attrs.push(Attr::uint("ppid", ppid));
                    }
                    if let Some(comm) = comms.get(sample.tid as i32) {
                        attrs.push(Attr::text("comm", comm));
                    }
                    // Symbolized here, while the target is alive, and stored once:
                    // the event carries the identity and the table travels beside
                    // the results.
                    // Only a sample with a stack needs its process's mappings:
                    // over a machine, reading every process's maps for samples
                    // that carry one address each would cost more than they do.
                    let carries_stack = sample.callchain.is_some() || sample.user_regs.is_some();
                    let stack = carries_stack.then(|| {
                        unwinders
                            .entry(sample.pid)
                            .or_insert_with(|| {
                                // It reads the maps as it is built.
                                map_reads.read_now(sample.pid);
                                Unwinder::new(sample.pid as i32, callchain, debuginfod.clone())
                            })
                            .record(&sample)
                    });
                    let stack = stack.flatten();
                    // The leaf's instruction, for `--keys insn`: the frame the
                    // sample's own ip is in, as `func+0xoff`.
                    if let Some(leaf) = stack.and_then(|stack| {
                        unwinders
                            .get(&sample.pid)?
                            .table()
                            .get(stack)?
                            .frames
                            .first()
                            .map(instruction)
                    }) {
                        attrs.push(Attr::text("insn", leaf));
                    }
                    if stack.is_some() {
                        let task = KeyValue::task(sample.tid as i32).to_string();
                        task_processes.entry(task.clone()).or_insert(sample.pid);
                        if let std::collections::btree_map::Entry::Vacant(slot) =
                            executables.entry(task)
                        {
                            if let Some(file) = crate::stack::executable_of(sample.pid as i32) {
                                slot.insert(file);
                            }
                        }
                    }
                    if let Some(stack) = stack {
                        let run_id = *stack_ids.entry((sample.pid, stack.0)).or_insert_with(|| {
                            stack_origins.push((sample.pid, stack.0));
                            stack_origins.len() as u64 - 1
                        });
                        attrs.push(Attr::uint(STACK_ID_ATTR, run_id));
                    }
                    let mut keys = vec![KeyValue::task(sample.tid as i32)];
                    if let Some(cpu) = sample.cpu {
                        keys.push(KeyValue::cpu(cpu));
                        attrs.push(Attr::uint("cpu", u64::from(cpu)));
                    }
                    keys.extend(cgroup_key.clone());
                    events.push(Event {
                        ts_ns: sample.time_ns,
                        ts_err_ns: 0,
                        kind: kind.to_owned(),
                        keys,
                        attrs,
                        collector_id: CollectorId::unstamped(),
                        seq: index,
                    });
                    index += 1;
                }
                Record::Lost { count } => dropped += count,
                // Counted by the sampler itself; see `SampledEvent::throttled`.
                Record::Throttle { .. } => {}
                Record::Other { .. } => undecoded += 1,
            }
        }
        self.ring_losses = match &*event {
            Sampler::Tasks(sampler) => {
                RingLost::merge(sampler.lost_by_task(), sampler.throttled_by_task())
                    .into_iter()
                    .map(|(tid, lost)| (KeyValue::task(tid), lost))
                    .collect()
            }
            Sampler::Cpus(sampler) => {
                RingLost::merge(sampler.lost_by_cpu(), sampler.throttled_by_cpu())
                    .into_iter()
                    .map(|(cpu, lost)| (KeyValue::cpu(cpu), lost))
                    .collect()
            }
        };
        let throttles = event.throttled() - self.throttled;
        self.throttled += throttles;
        if dropped > 0 {
            self.losses.dropped += dropped;
            self.losses.note_times(
                "the profile ring overflowed: samples were taken and never read, so the stacks \
                 here are a biased subset -- the busy phases are the ones that overflow",
                dropped,
            );
        }
        // A throttle is the kernel declining to deliver for a while, not a
        // number of samples the ring reported losing. How many samples were not
        // taken inside that window is not knowable from userspace, so counting
        // one throttle as one drop states a precision nobody has -- and leaves
        // `losses.throttled`, the field a consumer filters on, at zero on a run
        // that was throttled twenty times.
        if throttles > 0 {
            self.losses.throttled += throttles;
            self.losses.note_times(
                "the kernel throttled profile sampling: an unknown number of samples in each \
                 throttled period were not taken (see kernel.perf_event_max_sample_rate)",
                throttles,
            );
        }
        self.losses.undecoded += undecoded;
    }
}

/// One ring's losses: the samples it dropped and the periods the kernel
/// throttled it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RingLost {
    dropped: u64,
    throttled: u64,
}

impl RingLost {
    /// The sampler's two per-ring accounts, by the key each ring belongs to,
    /// as one list: a ring that only dropped, only was throttled, or both.
    fn merge<K: Ord + Copy>(dropped: Vec<(K, u64)>, throttled: Vec<(K, u64)>) -> Vec<(K, Self)> {
        let mut rings: BTreeMap<K, Self> = BTreeMap::new();
        for (key, count) in dropped {
            rings.entry(key).or_default().dropped += count;
        }
        for (key, count) in throttled {
            rings.entry(key).or_default().throttled += count;
        }
        rings.into_iter().collect()
    }
}

impl Collector for ProfileCollector {
    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        // Started now, read beside the window rather than before it: see
        // `stack::prime_kernel_symbols_aside`. A profile whose callchain has a
        // kernel half walks into the kernel on almost any workload; read here
        // on the runner's thread, the table held the window shut for 1.0 to
        // 1.5 s of a debug build, and `--pid` attached that late -- what the
        // process did meanwhile was in no sample. A user-only one
        // (`perf_event_paranoid` 2) never sees a kernel frame, and the table
        // would be most of a second of this process's cpu for nothing.
        if self.callchain.kernel() {
            crate::stack::prime_kernel_symbols_aside();
        }
        let hz = match self.mode {
            Mode::Sample { freq } => freq.as_hz(),
            other => {
                return Err(format!(
                    "`{other}` mode does not produce a profile: a profile is a sample rate, so \
                     ask for `--mode sample@99hz`"
                ))
            }
        };

        let spec = self.spec(hz);
        self.period = self.rate_period(hz);
        if let (Some(cpus), Some(cgroup)) = (&self.cpus, &self.cgroup) {
            let event =
                ThreadSampler::open_cgroup(&spec, &cgroup.directory, cpus, ROOT_RING_PAGES)?;
            event.enable().map_err(|error| error.to_string())?;
            self.unwinding = event.callchain() != Callchain::Off;
            self.event = Some(Sampler::Cpus(event));
            return Ok(());
        }
        if let Some(cpus) = &self.cpus {
            let event = ThreadSampler::open_cpus(&spec, cpus, ROOT_RING_PAGES)?;
            event.enable().map_err(|error| error.to_string())?;
            self.unwinding = event.callchain() != Callchain::Off;
            self.event = Some(Sampler::Cpus(event));
            return Ok(());
        }
        let mut event = TaskSampler::open(
            &spec,
            self.pid,
            self.whole_process,
            ROOT_RING_PAGES,
            RING_PAGES,
        )
        .map_err(|error| {
            if crate::perf::is_ring_too_large(&error) {
                return crate::perf::task_ring_budget_refusal(
                    &format!("a {} profile", self.sampled.name),
                    self.pid,
                    &error,
                );
            }
            format!(
                "perf_event_open for a {} profile of pid {} was refused ({error}); \
                 check /proc/sys/kernel/perf_event_paranoid",
                self.sampled.name, self.pid
            )
        })?;
        event.enable().map_err(|error| error.to_string())?;
        // The tasks the target starts are found on a thread of their own from
        // here, not on the drain's ticks, which a large program's symbols
        // make late.
        event.discover_in_background();
        // Read while the events are open and kept: `stop` closes them, and
        // the notes are gathered afterwards.
        self.at_open = event.at_open();
        self.unwinding = event.callchain() != Callchain::Off;
        self.event = Some(Sampler::Tasks(event));
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        // The mappings are re-read inside the drain, for the processes this
        // tick's samples came from (`drain_into`).
        if let Some(Sampler::Tasks(event)) = self.event.as_mut() {
            for found in event.discover() {
                // Named and parented now, while it runs: its samples may be
                // decoded after it is gone.
                if let Some(comm) = &found.comm {
                    self.comms.prime(found.task.tid, comm);
                }
                if let Some(ppid) = found.ppid {
                    self.parents.insert(found.task.tgid as u32, Some(ppid));
                }
                let before = found.cpu_before_ns;
                self.found_late += 1;
                self.unsampled_ns += before;
                if before > 0 {
                    *self.unsampled_by_task.entry(found.task.tid).or_default() += before;
                }
            }
        }
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        if let Some(event) = self.event.as_mut() {
            let _ = event.disable();
        }
        self.drain_into(&mut trailing, u64::MAX / 2);
        // The perf map outlives its writer, and a runtime that buffers it
        // flushes at exit: the lines written after the last tick name frames
        // interned before them.
        for unwinder in self.unwinders.values_mut() {
            unwinder.finish();
        }
        if let Some(event) = self.event.as_ref() {
            self.losses.undecoded += event.undecoded();
        }
        self.event = None;
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    /// Each ring is one task's, or one cpu's, so what it dropped and the
    /// periods the kernel throttled it are that key's series' loss and no
    /// other's.
    ///
    /// The throttles are carried too, not only the drops: `emit.rs` places a
    /// run's losses per ring only when the rings account for all of them, so
    /// a profile the kernel throttled -- any 50 kHz profile on a loaded
    /// machine -- marked every series with the whole run's loss while its
    /// rings said only whose drops they were.
    fn losses_by_ring(&self) -> Vec<crate::collect::RingLoss> {
        self.ring_losses
            .iter()
            .map(|(key, lost)| {
                let mut losses = Losses {
                    dropped: lost.dropped,
                    throttled: lost.throttled,
                    ..Losses::default()
                };
                if lost.dropped > 0 {
                    losses.note(format!(
                        "the ring of {key} overflowed: {} samples were taken and never read",
                        lost.dropped
                    ));
                }
                if lost.throttled > 0 {
                    losses.note(format!(
                        "the kernel throttled the sampling of {key} {} times: an unknown number \
                         of samples in each throttled period were not taken",
                        lost.throttled
                    ));
                }
                crate::collect::RingLoss {
                    key: key.clone(),
                    losses,
                }
            })
            .collect()
    }

    /// Each task found on a late tick, with the cpu it had used by then:
    /// `unsampled_ns:task:<tid>`, read by a decomposition's `samples` row.
    fn fields(&self) -> crate::emit::Fields {
        self.unsampled_by_task
            .iter()
            .map(|(tid, ns)| (format!("{}task:{tid}", crate::join::UNSAMPLED_NS), *ns))
            .collect()
    }

    fn facts(&self) -> Vec<String> {
        let mut facts: Vec<String> = Vec::new();
        if let (Weight::Cycles, Some(period)) = (self.sampled.weight, self.period) {
            facts.push(format!(
                "sampled on `cycles`, one sample every {period} cycles (the cpu's highest \
                 frequency over the rate asked for): a sample's share is a share of cycles, \
                 not of time, and a phase at a reduced clock has fewer of them"
            ));
        }
        if let Some(choice) = &self.choice {
            facts.push(describe_choice(choice));
        }
        for fact in self.unwinders.values().flat_map(Unwinder::facts) {
            if !facts.contains(&fact) {
                facts.push(fact);
            }
        }
        facts
    }

    /// Root first, which is the order [`crate::emit::Results::stacks`] stores
    /// and the `folded` format writes.
    fn stacks(&self) -> BTreeMap<u64, Vec<String>> {
        self.stack_origins
            .iter()
            .enumerate()
            .filter_map(|(run_id, (pid, local))| {
                let stack = self
                    .unwinders
                    .get(pid)?
                    .table()
                    .get(crate::stack::StackId(*local))?;
                let frames = stack
                    .frames
                    .iter()
                    .rev()
                    .map(crate::stack::Frame::label)
                    .collect();
                Some((run_id as u64, frames))
            })
            .collect()
    }

    /// Who named each frame of [`Collector::stacks`], under the same ids, and
    /// the file each sampled task executed.
    fn provenance(&self) -> crate::stack::Provenance {
        let mut named_by = BTreeMap::new();
        let mut objects = BTreeMap::new();
        for (run_id, (pid, local)) in self.stack_origins.iter().enumerate() {
            let Some(stack) = self
                .unwinders
                .get(pid)
                .and_then(|unwinder| unwinder.table().get(crate::stack::StackId(*local)))
            else {
                continue;
            };
            named_by.insert(run_id as u64, stack.named_by_root_first());
            objects.insert(run_id as u64, stack.objects_root_first());
        }
        // The unwinders' own: the perf map each read, with the file's device
        // and inode, and the file each process executes as last read -- after
        // an `execve` the new one.
        let mut perf_maps: Vec<crate::stack::PerfMapFile> = self
            .unwinders
            .values()
            .filter_map(Unwinder::perf_map_file)
            .collect();
        perf_maps.sort();
        perf_maps.dedup();
        let mut executables = self.executables.clone();
        for (task, pid) in &self.task_processes {
            if let Some(file) = self.unwinders.get(pid).and_then(Unwinder::executable) {
                executables.insert(task.clone(), file.to_owned());
            }
        }
        crate::stack::Provenance {
            named_by,
            perf_maps,
            executables,
            objects,
        }
    }

    fn notes(&self) -> Vec<String> {
        // Each process's unwinder speaks for its own samples; the same
        // sentence from two of them is said once.
        let mut notes: Vec<String> = Vec::new();
        match (self.sampled.weight, self.period) {
            (Weight::CpuTime, _) | (Weight::Cycles, Some(_)) => {}
            (_, _) => notes.push(format!(
                "sampled on `{}` in the kernel's frequency mode: about the rate asked for, each \
                 sample's `count` the period the kernel chose for it, so a share is one of \
                 `{}`, as perf's is -- the sum is an estimate of the count, which `--mode \
                 count` reads exactly",
                self.sampled.name, self.sampled.name
            )),
        }
        for note in self.unwinders.values().flat_map(Unwinder::notes) {
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
        if let Some(cpus) = &self.cpus {
            notes.push(format!(
                "every task on {} cpu{} was sampled, one event per cpu; samples came from {} \
                 task{} in {} process{}, and {} landed in the idle task and are not in the \
                 profile",
                cpus.len(),
                if cpus.len() == 1 { "" } else { "s" },
                self.sampled_tasks.len(),
                if self.sampled_tasks.len() == 1 {
                    ""
                } else {
                    "s"
                },
                self.processes.len(),
                if self.processes.len() == 1 { "" } else { "es" },
                self.idle,
            ));
        }
        if self.at_open > 1 {
            notes.push(format!(
                "process {} was already running {} threads when the profile began, and each has \
                 an event of its own; samples came from {} task{}",
                self.pid,
                self.at_open,
                self.sampled_tasks.len(),
                if self.sampled_tasks.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        if self.found_late > 0 {
            // Time, not a count of samples: how many samples it would have
            // made is a guess at the phase of a period nobody was timing.
            notes.push(format!(
                "{} task{} the target started {} found on a tick after {} began, and had used \
                 {:.2} ms of cpu by then, by the kernel's own account: that time is in no \
                 sample here. A task that started and ended between two ticks is in none at \
                 all",
                self.found_late,
                if self.found_late == 1 { "" } else { "s" },
                if self.found_late == 1 { "was" } else { "were" },
                if self.found_late == 1 { "it" } else { "they" },
                self.unsampled_ns as f64 / 1e6,
            ));
        }
        let processes = self
            .unwinders
            .iter()
            .filter(|(pid, unwinder)| **pid != self.pid as u32 && !unwinder.table().is_empty())
            .count();
        if processes > 0 && self.pid > 0 {
            notes.push(format!(
                "{processes} process{} the target started {} sampled too, each symbolized \
                 against its own mappings",
                if processes == 1 { "" } else { "es" },
                if processes == 1 { "was" } else { "were" },
            ));
        }
        if let Some(choice) = &self.choice {
            for degradation in &choice.degradations {
                let note = format!("stacks unwound by `{}`: {degradation}", choice.mode);
                if !notes.contains(&note) {
                    notes.push(note);
                }
            }
        }
        if !self.unwinding {
            notes.push(
                "no unwind mode was asked for, so each sample is one instruction pointer and \
                 there are no stacks to aggregate"
                    .to_owned(),
            );
        }
        notes
    }
}

/// The unwind mode a profile used and how it came to it, in one line.
fn describe_choice(choice: &StackChoice) -> String {
    let how = if choice.forced {
        "forced by `--stack`".to_owned()
    } else {
        let passed: Vec<String> = choice
            .considered
            .iter()
            .filter_map(|considered| match &considered.verdict {
                crate::resolve::Verdict::Rejected(reasons) => Some(format!(
                    "{} ({})",
                    considered.mode,
                    reasons.first().cloned().unwrap_or_default()
                )),
                crate::resolve::Verdict::Unimplemented => {
                    Some(format!("{} (served here, not built)", considered.mode))
                }
                _ => None,
            })
            .collect();
        if passed.is_empty() {
            "chosen by `--stack auto`".to_owned()
        } else {
            format!("chosen by `--stack auto` over {}", passed.join(", "))
        }
    };
    format!(
        "stacks unwound by `{}`{}, {how}",
        choice.mode,
        match (choice.user_only, choice.mode) {
            (false, _) => "",
            (true, crate::request::StackMode::None) => ", user-mode samples only",
            (true, _) => ", user half only",
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process's parent is read past its command, whatever the command
    /// holds: spaces and parentheses included.
    #[test]
    fn a_process_s_parent_is_read_past_its_command() {
        assert_eq!(
            crate::perf::parent_in_stat("678750 (cc1) R 678616 678613 0"),
            Some(678616)
        );
        assert_eq!(
            crate::perf::parent_in_stat("42 (a (b) c) S 7 42 42"),
            Some(7)
        );
        assert_eq!(crate::perf::parent_in_stat("2 (kthreadd) S 0 0 0"), None);
        assert_eq!(
            parent_of(std::process::id()),
            Some(std::os::unix::process::parent_id() as u64)
        );
    }

    /// At `perf_event_paranoid` 2 the kernel refuses an unprivileged sampler
    /// that includes it, stack or no stack: a user-only choice excludes the
    /// kernel from the event for every mode, `none` included, where no
    /// callchain is asked whose kernel half would have dropped it.
    #[test]
    fn a_user_only_choice_excludes_the_kernel_even_without_a_stack() {
        use crate::request::StackMode;
        for mode in [StackMode::None, StackMode::Fp, StackMode::Dwarf] {
            for user_only in [false, true] {
                let mut choice = StackChoice::of(mode);
                choice.user_only = user_only;
                let prepare = Prepare {
                    mode: "sample@99hz".parse().unwrap(),
                    sampled: SAMPLED[0],
                    choice,
                    debuginfod: None,
                };
                let spec = prepare.collector(1, false).spec(997);
                assert_eq!(spec.exclude_kernel, user_only, "{mode:?}");
                assert_eq!(
                    spec.callchain.kernel(),
                    !user_only && mode != StackMode::None
                );
            }
        }
    }

    /// Set in the copy of this test binary that
    /// [`a_profile_reads_the_kernels_symbols_only_when_its_stacks_have_a_kernel_half`]
    /// runs, which is a process whose kernel symbol table nothing has read yet.
    const PRIME_CHILD: &str = "OMN_PROFILE_PRIME_CHILD";

    /// Bytes this process has read through system calls so far, by the
    /// kernel's own account (`/proc/self/io`, `rchar`), its threads included.
    fn process_rchar() -> u64 {
        std::fs::read_to_string("/proc/self/io")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("rchar:"))
                    .and_then(|value| value.trim().parse().ok())
            })
            .expect("this thread's io accounting is readable")
    }

    /// A profile whose stacks are user-only (`perf_event_paranoid` 2) never
    /// sees a kernel frame, so it must not pay for the kernel's symbol table
    /// before its window -- the better part of a second of a debug build's
    /// cpu, measured at 978 ms for a one-second profile. One whose stacks have
    /// a kernel half must, from its start -- on a thread of its own, so the
    /// window opens at once -- rather than leave it to a drain.
    ///
    /// Ground truth is the kernel's count of the bytes this process read,
    /// against the size of `/proc/kallsyms` read here. The table is read
    /// once per process, so the two starts run, user-only first, in a fresh
    /// copy of this test binary where nothing has read it.
    #[test]
    fn a_profile_reads_the_kernels_symbols_only_when_its_stacks_have_a_kernel_half() {
        use crate::request::StackMode;
        if std::env::var_os(PRIME_CHILD).is_some() {
            let pid = std::process::id() as i32;
            for (label, user_only) in [("user-only", true), ("both-halves", false)] {
                let mut choice = StackChoice::of(StackMode::Fp);
                choice.user_only = user_only;
                let prepare = Prepare {
                    mode: "sample@99hz".parse().unwrap(),
                    sampled: SAMPLED[0],
                    choice,
                    debuginfod: None,
                };
                let mut collector = prepare.collector(pid, false);
                let before = process_rchar();
                let started = collector.start();
                // The read runs aside: waited for here, never by the start.
                let asked = std::time::Instant::now();
                while !user_only
                    && !crate::stack::kernel_symbols_read()
                    && asked.elapsed() < std::time::Duration::from_secs(60)
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                let read = process_rchar() - before;
                // The table is primed before the event is opened, so what
                // was read is known whether or not the kernel then refused
                // the event (`perf_event_paranoid` 4).
                match started {
                    Ok(()) => println!("prime: {label} {read}"),
                    Err(error) => println!("prime: {label} {read} refused {error}"),
                }
                let _ = collector.stop();
            }
            return;
        }
        let kallsyms = match std::fs::read("/proc/kallsyms") {
            Ok(bytes) if bytes.len() > 1 << 20 => bytes.len() as u64,
            Ok(bytes) => {
                eprintln!(
                    "skipped: /proc/kallsyms is {} bytes here, too small for its read to be \
                     told from the rest of a start",
                    bytes.len()
                );
                return;
            }
            Err(error) => {
                eprintln!("skipped: /proc/kallsyms is not readable here: {error}");
                return;
            }
        };
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "collect::cpu::profile::tests::\
                 a_profile_reads_the_kernels_symbols_only_when_its_stacks_have_a_kernel_half",
                "--test-threads",
                "1",
                "--nocapture",
            ])
            .env(PRIME_CHILD, "1")
            .output()
            .expect("this test binary runs");
        let said = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(output.status.success(), "the child failed: {said}");
        let answer = |label: &str| -> (u64, Option<String>) {
            let line = said
                .lines()
                .find_map(|line| {
                    // libtest prints the test's name on the same line as the
                    // first thing it writes.
                    let marker = format!("prime: {label} ");
                    line.find(&marker).map(|at| &line[at + marker.len()..])
                })
                .unwrap_or_else(|| panic!("the child said nothing of {label}: {said}"));
            let (read, refused) = match line.split_once(" refused ") {
                Some((read, error)) => (read, Some(error.to_owned())),
                None => (line, None),
            };
            (read.trim().parse().expect("a byte count"), refused)
        };
        let paranoid = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
            .map(|text| text.trim().to_owned())
            .unwrap_or_else(|_| "unreadable".to_owned());
        let (user_only, refused) = answer("user-only");
        if let Some(error) = &refused {
            eprintln!(
                "perf_event_paranoid is {paranoid}, which refuses this process perf sampling \
                 ({error}); the reads are held all the same, since they come before the open"
            );
        }
        assert!(
            user_only < kallsyms / 4,
            "a user-only profile read {user_only} bytes as it started, and /proc/kallsyms is \
             {kallsyms}: it read the kernel's symbols for frames it will never see"
        );
        let (read, _) = answer("both-halves");
        // `kptr_restrict` zeroes every address or none, and a zeroed table is
        // known from its first line: priming it reads that far and no further.
        let zeroed = std::fs::read_to_string("/proc/kallsyms")
            .ok()
            .and_then(|text| {
                let first = text.split_whitespace().next()?.to_owned();
                u64::from_str_radix(&first, 16).ok()
            })
            == Some(0);
        if zeroed {
            assert!(
                read > 0 && read < kallsyms / 4,
                "a profile whose stacks have a kernel half, on a machine whose /proc/kallsyms \
                 is zeroed, read {read} bytes of its {kallsyms} as it started: its first line \
                 says the table names nothing"
            );
            return;
        }
        assert!(
            read >= kallsyms * 9 / 10,
            "a profile whose stacks have a kernel half read {read} bytes as it started, and \
             /proc/kallsyms is {kallsyms}: the table is left to the first drain"
        );
    }

    /// The frequency the caller asked for is the frequency the kernel is told,
    /// expressed in the unit this event counts in.
    #[test]
    fn a_frequency_becomes_a_period_of_cpu_nanoseconds() {
        assert_eq!(period_for(1000), 1_000_000);
        assert_eq!(period_for(99), 10_101_010);
        // A frequency higher than a sample per nanosecond is not expressible;
        // it clamps rather than dividing to zero, which the kernel refuses.
        assert_eq!(period_for(4_000_000_000), 1);
    }

    /// Every event this collector samples on serves a source the catalog
    /// declares, on a backend of its chain, with `sample` among its modes: a
    /// row here no request can reach is dead, and a request the catalog
    /// promises with no row here is refused at start.
    #[test]
    fn every_sampled_event_serves_a_declared_sample_mode() {
        let catalog = crate::catalog::Catalog::embedded().expect("the embedded catalog");
        for sampled in SAMPLED {
            let id: crate::request::SourceId = sampled.source.parse().expect("an id");
            let entry = catalog.get(&id).expect(sampled.source);
            assert!(
                entry.backend(sampled.backend).is_some(),
                "{} has no backend {}",
                sampled.source,
                sampled.backend
            );
            assert!(
                entry.modes.contains(&crate::request::ModeKind::Sample),
                "{} does not declare sample",
                sampled.source
            );
        }
        assert_eq!(sampled_for("cpu.profile", "pmu:sw"), Some(SAMPLED[0]));
        assert_eq!(sampled_for("cpu.profile", "pmu"), None);
    }

    /// A cycles profile (perfrecord 2) against a cycles counter over the same
    /// spin: one sample per fixed period of cycles, so the samples times the
    /// period are the cycles the counter counted, within the few periods the
    /// edges and the user-only paranoia cost. Each sample names its process
    /// and its task's name.
    #[test]
    fn a_cycles_profile_samples_one_period_of_cycles_at_a_time() {
        let Some(sampled) = sampled_for("cpu.cycles", "pmu") else {
            panic!("no cycles row");
        };
        let mut collector = ProfileCollector::new(
            0,
            Mode::Sample {
                freq: crate::scalar::Frequency::from_hz(997),
            },
            Callchain::Off,
            false,
        );
        collector.sampled = sampled;
        let Ok(counter) = crate::perf::CounterGroup::open_user_mode(
            &[crate::perf::Counter::hardware(
                "cycles",
                perf::PERF_COUNT_HW_CPU_CYCLES,
            )],
            crate::perf::Scope::Process(0),
        ) else {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        };
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses a cycles sampling event");
            return;
        }
        let Some(period) = collector.period else {
            eprintln!("skipped: this machine publishes no highest frequency");
            return;
        };
        counter.enable().expect("enable");
        let deadline = std::time::Instant::now() + StdDuration::from_millis(400);
        let mut spin = 0u64;
        while std::time::Instant::now() < deadline {
            spin = spin.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(spin);
        let cycles = counter.read_final().expect("read")[0].value as f64;
        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());

        let expected = cycles / period as f64;
        let got = events.len() as f64;
        assert!(
            (got - expected).abs() <= expected * 0.1 + 3.0,
            "{got} samples of one per {period} cycles over {cycles} cycles counted"
        );
        let pid = u64::from(std::process::id());
        let event = &events[0];
        assert_eq!(event.kind, "cpu.cycles");
        assert_eq!(event.attr("pid").and_then(|v| v.as_f64()), Some(pid as f64));
        assert!(event.attr("comm").is_some(), "no comm on {event:?}");
        assert!(collector.facts().iter().any(|fact| fact.contains("cycles")));
    }

    /// The whole chain in one test: burn cpu, sample it, and check that what
    /// comes out is stacks with names on them rather than bare addresses.
    ///
    /// The frames asserted are the ones nothing here can fake: the test binary
    /// is the target, so its own symbols must appear. A collector that returned
    /// plausible ids and an empty table passes every self-consistency check and
    /// fails this.
    #[test]
    fn a_profile_of_this_process_names_its_own_frames() {
        let mut collector = ProfileCollector::new(
            0,
            Mode::Sample {
                freq: crate::scalar::Frequency::from_hz(997),
            },
            Callchain::BOTH_HALVES,
            true,
        );
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses unprivileged perf sampling");
            return;
        }

        // Cpu time, not wall time: this event counts the former, so a sleep
        // here would produce no samples at all.
        let deadline = std::time::Instant::now() + StdDuration::from_millis(300);
        let mut spin = 0u64;
        while std::time::Instant::now() < deadline {
            spin = spin.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        assert!(spin != 1, "the loop must not be optimised away");

        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());

        assert!(
            !events.is_empty(),
            "300ms of cpu at 997hz produced no samples at all"
        );

        let table = collector.stacks();
        if table.is_empty() {
            // A kernel that refuses callchains to an unprivileged task gives
            // samples without them. That is a real configuration, not a bug
            // here, and it is distinguishable from a broken unwinder by the
            // events carrying no stack identity either.
            assert!(
                events
                    .iter()
                    .all(|event| event.attr(STACK_ID_ATTR).is_none()),
                "events name stacks that the table does not have"
            );
            eprintln!("skipped: this kernel gave no callchains");
            return;
        }

        let with_stacks = events
            .iter()
            .filter(|event| event.attr(STACK_ID_ATTR).is_some())
            .count();
        assert!(
            with_stacks * 2 > events.len(),
            "only {with_stacks} of {} samples carried a stack",
            events.len()
        );

        // Every identity an event names is in the table, which is the contract
        // the side table exists to keep.
        for event in &events {
            if let Some(crate::event::AttrValue::Uint(id)) = event.attr(STACK_ID_ATTR) {
                assert!(
                    table.contains_key(id),
                    "event names stack {id}, absent from the table"
                );
            }
        }

        // Who named each frame travels beside the labels, frame for frame,
        // and the test binary is the file its own task executed.
        let provenance = collector.provenance();
        for (id, labels) in &table {
            assert_eq!(
                provenance.named_by.get(id).map(Vec::len),
                Some(labels.len()),
                "stack {id}'s namers do not line up with its labels"
            );
        }
        let this_binary = crate::stack::executable_of(0).expect("this test's own executable");
        assert!(
            provenance
                .executables
                .values()
                .any(|file| *file == this_binary),
            "no sampled task names this test binary: {:?}",
            provenance.executables
        );

        let all: Vec<&String> = table.values().flatten().collect();
        assert!(
            all.iter().any(|frame| frame.contains("omn")
                || frame.contains("profile")
                || frame.contains("main")),
            "no frame of the test binary was symbolized: {:?}",
            &all[..all.len().min(12)]
        );
    }

    /// A tick whose drain holds no sample of a process reads nothing of it,
    /// once a drain has held samples of it.
    ///
    /// The mappings were re-read for every process on every tick, and that
    /// was most of what profiling a program that waits on its disk cost
    /// (`drain_into`). A child that burns, then `exec`s `sleep`: until its
    /// first samples, and in each drain that holds samples of it or follows
    /// one that did, it is read once; in the drains of its sleep, never. The
    /// old rule read it once a drain, every drain.
    #[test]
    fn a_tick_with_no_sample_of_a_process_does_not_reread_its_mappings() {
        use std::io::Write;
        let ticks: u64 = 80;
        // It waits for a line before it burns: opening a profile with a
        // kernel half reads the kernel's symbols first, which takes longer
        // than the burn.
        let mut child = std::process::Command::new("sh")
            .args([
                "-c",
                "read go; i=0; while [ $i -lt 100000 ]; do i=$((i+1)); done; exec sleep 5",
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("sh runs");
        // Both halves where the kernel allows them, the user half where it
        // does not (`perf_event_paranoid` 2): the path the CI's cell takes.
        let mut choice = StackChoice::of(crate::request::StackMode::Fp);
        choice.user_only = !crate::perf::kernel_callchain_allowed();
        let mut collector = Prepare {
            mode: "sample@997hz".parse().unwrap(),
            sampled: SAMPLED[0],
            choice,
            debuginfod: None,
        }
        .collector(child.id() as i32, true);
        // Skipped only where the kernel refuses a sampled event with a stack
        // of either shape; a `start` that fails where one opens is this
        // collector's defect, not the machine's, and fails the test.
        if let Err(error) = crate::perf::SampledEvent::callchain_supported() {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("skipped: this machine refuses unprivileged perf sampling ({error})");
            return;
        }
        if let Err(error) = collector.start() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("a sampled event with a stack opens here, and the profile did not: {error}");
        }
        let _ = child.stdin.take().expect("a pipe").write_all(b"go\n");
        let mut before_first = None;
        let mut sampled = 0;
        for seq in 0..ticks {
            std::thread::sleep(StdDuration::from_millis(10));
            if !collector.sample(seq).expect("a tick").is_empty() {
                before_first.get_or_insert(seq);
                sampled += 1;
            }
        }
        collector.stop();
        let _ = child.kill();
        let _ = child.wait();
        let said = format!(
            "{ticks} ticks and the stop's drain over a child that burns and then sleeps: {} \
             drains held no sample, {sampled} held samples (the first at tick {before_first:?}), \
             and the mappings were read {} times",
            collector.idle_drains, collector.refreshes
        );
        eprintln!("{said}");
        let Some(first) = before_first else {
            panic!("the child's burn was never sampled: {said}");
        };
        assert!(collector.idle_drains >= 10, "the child never slept: {said}");
        assert!(
            collector.refreshes <= first + 1 + 2 * sampled && collector.refreshes + 10 <= ticks + 1,
            "{said}"
        );
    }

    /// Over a machine (`--cpus`), a process is read once for each drain that
    /// holds samples of it, and never for one that holds none.
    ///
    /// Machine-wide, most processes are sampled now and then. The rule that
    /// read a process after the drain that held it and again before the next
    /// one doubled what a machine-wide profile read; the rule before it read
    /// only the last drain's processes, once, before the next drain. This
    /// drives [`MapReads`] the way `drain_into` does, without a sampler: no
    /// process is read more often than the drains holding it, a process held
    /// by consecutive drains is read once a drain, and a process built during
    /// a drain is not read again before the next.
    #[test]
    fn over_a_machine_a_process_is_read_once_for_each_drain_that_holds_it() {
        // Three known processes and one first seen in drain 2, by drain.
        let held: [&[u32]; 10] = [
            &[10, 20],
            &[],
            &[30],
            &[10, 40],
            &[10],
            &[10],
            &[],
            &[20, 40],
            &[],
            &[30],
        ];
        let mut rule = MapReads::default();
        let mut followed: BTreeSet<u32> = [10, 20, 30].into_iter().collect();
        let mut reads: BTreeMap<u32, usize> = BTreeMap::new();
        for drain in held {
            let mut before = BTreeSet::new();
            for pid in &followed {
                if rule.before_drain(*pid, true) {
                    *reads.entry(*pid).or_default() += 1;
                    before.insert(*pid);
                }
            }
            let sampled: BTreeSet<u32> = drain.iter().copied().collect();
            for pid in rule.after_drain(sampled.clone(), &before, |pid| followed.contains(&pid)) {
                *reads.entry(pid).or_default() += 1;
            }
            for pid in sampled {
                if followed.insert(pid) {
                    // Built while the drain is processed: its maps read then.
                    rule.read_now(pid);
                    *reads.entry(pid).or_default() += 1;
                }
            }
        }
        for pid in [10, 20, 30, 40] {
            let holding = held.iter().filter(|drain| drain.contains(&pid)).count();
            assert_eq!(
                reads.get(&pid).copied().unwrap_or(0),
                holding,
                "process {pid}: held by {holding} drains, read {reads:?}"
            );
        }
    }

    /// A profile follows a process, a core, a cgroup or the machine. A target
    /// that is none of them -- a container, which nothing here resolves to its
    /// cgroup -- is refused by name, rather than served this process's own
    /// profile.
    #[test]
    fn a_target_that_is_not_a_process_a_core_a_cgroup_or_the_machine_is_refused_by_name() {
        let prepare = Box::new(Prepare {
            mode: Mode::Sample {
                freq: Mode::DEFAULT_SAMPLE_FREQ,
            },
            sampled: SAMPLED[0],
            choice: StackChoice::of(crate::request::StackMode::Fp),
            debuginfod: None,
        });
        let Err(error) = prepare.attach(&Target::Container("web".into())) else {
            panic!("a container target names no process, no cpu and no cgroup");
        };
        assert!(error.contains("container"), "{error}");
        assert!(error.contains("not built"), "{error}");
    }

    /// A cgroup that is not there is refused as that, at the attach, before
    /// any event is opened.
    #[test]
    fn a_cgroup_that_does_not_exist_is_refused_as_absent() {
        let prepare = Box::new(Prepare {
            mode: Mode::Sample {
                freq: Mode::DEFAULT_SAMPLE_FREQ,
            },
            sampled: SAMPLED[0],
            choice: StackChoice::of(crate::request::StackMode::None),
            debuginfod: None,
        });
        let Err(error) = prepare.attach(&Target::Cgroup("/omn-no-such-cgroup".into())) else {
            panic!("a cgroup that does not exist cannot be profiled");
        };
        assert!(error.contains("not a cgroup v2 directory"), "{error}");
    }

    /// The machine is one cpu-clock event per online cpu, over every task:
    /// this thread, burning its own cpu, is found in it for about the time it
    /// burned. Opening a cpu needs `perf_event_paranoid` at 0 or `CAP_PERFMON`,
    /// which a unit test binary never carries, so the refusal is accepted when
    /// it names them -- `tests/scenario_cpu.rs` checks the same through the
    /// binary that does.
    #[test]
    fn a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned() {
        let prepare = Box::new(Prepare {
            mode: Mode::Sample {
                freq: crate::scalar::Frequency::from_hz(997),
            },
            sampled: SAMPLED[0],
            choice: StackChoice::of(crate::request::StackMode::None),
            debuginfod: None,
        });
        let mut collector = prepare
            .attach(&Target::System)
            .expect("the machine is a target");
        if let Err(error) = collector.start() {
            assert!(error.contains("perf_event_paranoid"), "{error}");
            eprintln!("skipped: {error}");
            return;
        }
        // SAFETY: a read-only query of this thread's own id.
        let tid = unsafe { libc::gettid() };
        let cpu_ns = || {
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `now` is a valid timespec for the kernel to write into.
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
            now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
        };
        let before = cpu_ns();
        let mut spin = 0u64;
        while cpu_ns() - before < 200_000_000 {
            spin = spin.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        assert!(spin != 1, "the loop must not be optimised away");
        let burned = (cpu_ns() - before) as f64;
        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());
        let mine = events
            .iter()
            .filter(|event| event.key(crate::request::KeyKind::Task) == Some(&KeyValue::task(tid)))
            .count() as f64;
        let expected = 997.0 * burned / 1e9;
        assert!(
            (mine - expected).abs() - 1.0 < 0.3 * expected,
            "this thread burned {:.0} ms of cpu, about {expected:.0} samples at 997 Hz, and the \
             machine's profile holds {mine} of it",
            burned / 1e6
        );
        assert!(
            events
                .iter()
                .all(|event| event.key(crate::request::KeyKind::Cpu).is_some()),
            "every sample of a per-cpu event names its cpu"
        );
        assert!(
            collector
                .notes()
                .iter()
                .any(|note| note.contains("one event per cpu")),
            "{:?}",
            collector.notes()
        );
    }

    /// Every other mode is refused by name with the one that works, rather than
    /// silently profiling at a default rate nobody asked for.
    #[test]
    fn a_mode_that_is_not_sampling_is_refused_with_the_one_that_is() {
        let error = ProfileCollector::new(0, Mode::Trace, Callchain::BOTH_HALVES, true)
            .start()
            .unwrap_err();
        assert!(error.contains("--mode sample@99hz"), "{error}");
    }

    /// A ring the kernel throttled and that dropped nothing is still one of
    /// the rings that lost something, and one that did both carries both:
    /// the per-ring account sums to the run's on both fields, which is what
    /// `emit.rs` needs to place a throttled profile's losses per task.
    #[test]
    fn each_ring_carries_its_throttles_beside_its_drops() {
        let rings = RingLost::merge(vec![(7, 40), (9, 3)], vec![(7, 2), (11, 5)]);
        assert_eq!(
            rings,
            vec![
                (
                    7,
                    RingLost {
                        dropped: 40,
                        throttled: 2
                    }
                ),
                (
                    9,
                    RingLost {
                        dropped: 3,
                        throttled: 0
                    }
                ),
                (
                    11,
                    RingLost {
                        dropped: 0,
                        throttled: 5
                    }
                ),
            ]
        );
    }
}
