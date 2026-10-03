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
use crate::perf::{Callchain, Found, Record, SampleSpec, TaskSampler, ThreadSampler};
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

/// The cpu time a task had used when it was found, from the first field of
/// its `schedstat`: nanoseconds on a cpu, the kernel's own account.
fn cpu_before(task: Found) -> u64 {
    std::fs::read_to_string(format!("/proc/{}/task/{}/schedstat", task.tgid, task.tid))
        .ok()
        .and_then(|line| line.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

pub struct Prepare {
    pub mode: Mode,
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

pub struct ProfileCollector {
    id: CollectorId,
    pid: i32,
    mode: Mode,
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
    /// Mapping reads: how many times a process's mappings were re-read
    /// because a drain held samples of it, and how many drains held none and
    /// read nothing. See [`ProfileCollector::drain_into`].
    #[cfg_attr(not(test), allow(dead_code))]
    refreshes: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    idle_drains: u64,
    /// Every process a sample came from.
    processes: BTreeSet<u32>,
    /// One per process samples arrived from, by pid, re-read every tick. A
    /// symbolizer reads its process's `/proc/<pid>/maps`, and those are gone
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
    unwinding: bool,
    /// How the stacks are unwound and why, when a chooser decided it.
    choice: Option<StackChoice>,
    debuginfod: Option<std::sync::Arc<Debuginfod>>,
    /// What each ring lost, by the key its ring belongs to: the task a
    /// per-task ring follows, the cpu a per-cpu ring is pinned to. Read from
    /// the sampler at each drain, because `stop` closes it.
    ring_losses: Vec<(KeyValue, u64)>,
}

impl ProfileCollector {
    pub fn new(pid: i32, mode: Mode, callchain: Callchain, whole_process: bool) -> Self {
        let mut unwinders = BTreeMap::new();
        if pid > 0 {
            unwinders.insert(pid as u32, Unwinder::for_process(pid));
        }
        Self {
            id: CollectorId::new("cpu.profile/pmu:sw"),
            pid,
            mode,
            callchain,
            event: None,
            cpus: None,
            cgroup: None,
            throttled: 0,
            idle: 0,
            refreshes: 0,
            idle_drains: 0,
            processes: BTreeSet::new(),
            unwinders,
            stack_ids: BTreeMap::new(),
            stack_origins: Vec::new(),
            losses: Losses::default(),
            whole_process,
            sampled_tasks: BTreeSet::new(),
            at_open: 0,
            found_late: 0,
            unsampled_ns: 0,
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
    fn spec(&self, period: u64) -> SampleSpec {
        let mut spec =
            SampleSpec::software(perf::PERF_COUNT_SW_CPU_CLOCK as u64, period, self.callchain);
        spec.exclude_kernel = self.choice.as_ref().is_some_and(|choice| choice.user_only);
        spec
    }

    /// Reads what the rings hold and turns each sample into an event.
    ///
    /// The records are taken out of the rings first, and only the processes
    /// they hold samples of have their mappings re-read before any stack is
    /// interned. Re-reading every process's maps on every tick -- twice for a
    /// stack mode with CFI, and its executable's link once more -- was most
    /// of what a profile cost a program that is mostly off the cpu: 0.25 to
    /// 0.4 ms a tick of a debug build, 25 to 40 ms of this tool's cpu a
    /// second of wall clock whatever the program did, against `sqlite3`'s
    /// fsync-bound commits that burn 200 ms a second. A user-only profile
    /// (`perf_event_paranoid` 2) delivers no sample while the task is in the
    /// kernel, so a program that lives in system calls left nearly every
    /// tick empty and still paid for every read. The maps are still read in
    /// the tick that interns the stacks, while the process is alive.
    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(event) = self.event.as_mut() else {
            return;
        };
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
        for pid in &sampled {
            if let Some(unwinder) = self.unwinders.get_mut(pid) {
                unwinder.refresh_symbols();
                self.refreshes += 1;
            }
        }
        let id = self.id.clone();
        let unwinders = &mut self.unwinders;
        let stack_ids = &mut self.stack_ids;
        let stack_origins = &mut self.stack_origins;
        let seen = &mut self.sampled_tasks;
        let processes = &mut self.processes;
        let idle = &mut self.idle;
        let mut index = seq;
        let cgroup_key = self.cgroup.as_ref().map(|cgroup| cgroup.key());
        let callchain = self.callchain;
        let debuginfod = &self.debuginfod;
        let mut dropped = 0u64;
        let mut undecoded = 0u64;
        for record in records {
            match record {
                Record::Sample(sample) if sample.pid == 0 && sample.tid == 0 => *idle += 1,
                Record::Sample(sample) => {
                    seen.insert(sample.tid as i32);
                    processes.insert(sample.pid);
                    let mut attrs = vec![Attr::uint("count", 1), Attr::uint("ip", sample.ip)];
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
                                Unwinder::new(sample.pid as i32, callchain, debuginfod.clone())
                            })
                            .record(&sample)
                    });
                    let stack = stack.flatten();
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
                        kind: "cpu.profile".to_owned(),
                        keys,
                        attrs,
                        collector_id: id.clone(),
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
            Sampler::Tasks(sampler) => sampler
                .lost_by_task()
                .into_iter()
                .map(|(tid, lost)| (KeyValue::task(tid), lost))
                .collect(),
            Sampler::Cpus(sampler) => sampler
                .lost_by_cpu()
                .into_iter()
                .map(|(cpu, lost)| (KeyValue::cpu(cpu), lost))
                .collect(),
        };
        let throttles = event.throttled() - self.throttled;
        self.throttled += throttles;
        if dropped > 0 {
            self.losses.dropped += dropped;
            self.losses.note(
                "the profile ring overflowed: samples were taken and never read, so the stacks \
                 here are a biased subset -- the busy phases are the ones that overflow",
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
            self.losses.note(
                "the kernel throttled profile sampling: an unknown number of samples in each \
                 throttled period were not taken (see kernel.perf_event_max_sample_rate)",
            );
        }
        self.losses.undecoded += undecoded;
    }
}

impl Collector for ProfileCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        // Before the window, not inside a drain: see
        // `stack::prime_kernel_symbols`. A profile whose callchain has a
        // kernel half walks into the kernel on almost any workload, and the
        // runner's round is serial. A user-only one (`perf_event_paranoid` 2)
        // never sees a kernel frame, and the table would be most of a second
        // of this process's cpu before the window for nothing.
        if self.callchain.kernel() {
            crate::stack::prime_kernel_symbols();
        }
        let period = match self.mode {
            Mode::Sample { freq } => period_for(freq.as_hz()),
            other => {
                return Err(format!(
                    "`{other}` mode does not produce a profile: a profile is a sample rate, so \
                     ask for `--mode sample@99hz`"
                ))
            }
        };

        let spec = self.spec(period);
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
            format!(
                "perf_event_open for a cpu-clock profile of pid {} was refused ({error}); \
                 check /proc/sys/kernel/perf_event_paranoid",
                self.pid
            )
        })?;
        event.enable().map_err(|error| error.to_string())?;
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
            for task in event.discover() {
                self.found_late += 1;
                self.unsampled_ns += cpu_before(task);
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

    /// Each ring is one task's, or one cpu's, so what it dropped is that
    /// key's series' loss and no other's.
    fn losses_by_ring(&self) -> Vec<crate::collect::RingLoss> {
        self.ring_losses
            .iter()
            .map(|(key, lost)| {
                let mut losses = Losses {
                    dropped: *lost,
                    ..Losses::default()
                };
                losses.note(format!(
                    "the ring of {key} overflowed: {lost} samples were taken and never read"
                ));
                crate::collect::RingLoss {
                    key: key.clone(),
                    losses,
                }
            })
            .collect()
    }

    fn facts(&self) -> Vec<String> {
        let mut facts: Vec<String> = Vec::new();
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

    fn notes(&self) -> Vec<String> {
        // Each process's unwinder speaks for its own samples; the same
        // sentence from two of them is said once.
        let mut notes: Vec<String> = Vec::new();
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
                    choice,
                    debuginfod: None,
                };
                let spec = prepare.collector(1, false).spec(1_000_000);
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

    /// Bytes this thread has read through system calls so far, by the
    /// kernel's own account (`/proc/thread-self/io`, `rchar`).
    fn thread_rchar() -> u64 {
        std::fs::read_to_string("/proc/thread-self/io")
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
    /// a kernel half must, before the window rather than inside a drain.
    ///
    /// Ground truth is the kernel's count of the bytes the starting thread
    /// read, against the size of `/proc/kallsyms` read here. The table is read
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
                    choice,
                    debuginfod: None,
                };
                let mut collector = prepare.collector(pid, false);
                let before = thread_rchar();
                let started = collector.start();
                let read = thread_rchar() - before;
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

        let all: Vec<&String> = table.values().flatten().collect();
        assert!(
            all.iter().any(|frame| frame.contains("omn")
                || frame.contains("profile")
                || frame.contains("main")),
            "no frame of the test binary was symbolized: {:?}",
            &all[..all.len().min(12)]
        );
    }

    /// A tick whose drain holds no sample of a process reads nothing of it.
    ///
    /// The mappings were re-read for every process on every tick, and that
    /// was most of what profiling a program that waits on its disk cost
    /// (`drain_into`). A sleeping child produces no sample, so ten ticks over
    /// it read its maps no time at all; a burning one is read once in each
    /// drain that held its samples, and in no other.
    #[test]
    fn a_tick_with_no_sample_of_a_process_does_not_reread_its_mappings() {
        let ticks = 10;
        let mut sleeper = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("sleep runs");
        // Both halves where the kernel allows them, the user half where it
        // does not (`perf_event_paranoid` 2): the path the CI's cell takes.
        let collector = |pid: u32| {
            let mut choice = StackChoice::of(crate::request::StackMode::Fp);
            choice.user_only = !crate::perf::kernel_callchain_allowed();
            Prepare {
                mode: "sample@997hz".parse().unwrap(),
                choice,
                debuginfod: None,
            }
            .collector(pid as i32, true)
        };
        let mut idle = collector(sleeper.id());
        if let Err(error) = idle.start() {
            let _ = sleeper.kill();
            let _ = sleeper.wait();
            eprintln!("skipped: this machine refuses unprivileged perf sampling ({error})");
            return;
        }
        for seq in 0..ticks {
            std::thread::sleep(StdDuration::from_millis(10));
            idle.sample(seq).expect("a tick");
        }
        idle.stop();
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        // `stop` drains once more.
        assert_eq!(
            (idle.refreshes, idle.idle_drains),
            (0, ticks + 1),
            "a process that slept through {ticks} ticks and the stop's drain: (mapping reads, \
             drains that held no sample)"
        );

        let mut burner = std::process::Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .spawn()
            .expect("sh runs");
        let mut busy = collector(burner.id());
        busy.start()
            .expect("a sleeping child was profiled a moment ago");
        let mut events = 0;
        for seq in 0..ticks {
            std::thread::sleep(StdDuration::from_millis(10));
            events += busy.sample(seq).expect("a tick").len();
        }
        events += busy.stop().len();
        let _ = burner.kill();
        let _ = burner.wait();
        assert!(
            events > 0 && busy.refreshes > 0 && busy.refreshes + busy.idle_drains == ticks + 1,
            "{events} samples of a child burning through {ticks} ticks and the stop's drain: \
             its mappings were read {} times, and {} drains held no sample",
            busy.refreshes,
            busy.idle_drains
        );
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
}
