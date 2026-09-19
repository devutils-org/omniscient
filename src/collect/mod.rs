//! Collectors: where events come from.
//!
//! A collector turns one resolved backend into a stream of events. It knows its
//! backend and nothing else -- not the emit, not the output format, not the
//! other collectors. Everything above it works on `Event`, which is what makes
//! adding a source an additive change.
//!
//! Two shapes live behind the same trait. A *polling* collector reads a value
//! whenever it is asked, at the interval the request names. A *streaming* one
//! has the kernel push events into a buffer and is drained often enough that
//! the buffer does not overflow. The runner asks which it is dealing with
//! rather than assuming, because draining a ring once a second loses events and
//! polling procfs every ten milliseconds is just expensive.
//!
//! Loss accounting is part of the interface rather than an afterthought. A
//! collection that loses three percent of its events without saying so produces
//! wrong numbers that look right, and by the time anyone notices the run is
//! gone. So a `Collection` always carries what was lost and why, and the count
//! travels all the way to the user-visible output.

mod cpu;
pub mod gpu;
mod io;
mod launch;
pub mod mem;
mod net;
mod power;
mod proc;
mod runtime;
mod sched;
mod task;

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use serde::{Deserialize, Serialize};

use crate::clock::monotonic_ns;
use crate::event::{CollectorId, Event};
use crate::request::{Mode, Target};
use crate::resolve::{FilterPlacement, ResolvedRequest};

pub use mem::heap::find_shim;

/// Why a collection could not start, or could not continue.
#[derive(Debug, thiserror::Error)]
pub enum CollectError {
    #[error("no collector is written for `{source_id}` on backend `{backend}` yet")]
    NoCollector { source_id: String, backend: String },
    #[error("`{source_id}` cannot be collected for a {target} target yet")]
    UnsupportedTarget { source_id: String, target: String },
    #[error(
        "a poll needs to know when to stop: pass --duration, or launch a program with `-- prog`"
    )]
    NoStopCondition,
    #[error("cannot launch {program}: {error}")]
    Launch { program: String, error: String },
    #[error("cannot start collecting `{source_id}`: {reason}")]
    Start { source_id: String, reason: String },
    #[error(
        "one run observes one target, but this session asks for `{first}` and `{second}`; \
         use one session per target"
    )]
    MixedTargets { first: String, second: String },
    #[error(
        "two collectors disagree about `{variable}` in the launched program's environment \
         (`{first}` and `{second}`)"
    )]
    ConflictingEnvironment {
        variable: String,
        first: String,
        second: String,
    },
}

/// What a run produced, and what it lost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub events: Vec<Event>,
    /// The attribute holding the measured value, named by the collector rather
    /// than guessed from the unit: a source may carry several numbers.
    pub value_attr: String,
    pub losses: Losses,
    /// Wall time actually observed, which is not the requested duration when a
    /// launched program exits early or a target disappears.
    pub observed_ns: u64,
    /// Why the run stopped.
    pub stopped_because: String,
    /// The frames behind each stack identity this run saw, root first.
    ///
    /// Carried here because a stack is interned by the collector that captured
    /// it and rendered by the layer that aggregates it, and neither should have
    /// to know about the other's types. The events name ids; this says what
    /// they were.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stacks: BTreeMap<u64, Vec<String>>,
    /// What this collector had to give up, discovered while running rather than
    /// while resolving: a counter the machine would only open for user mode, a
    /// group the hardware had to time-share. The resolver cannot know these
    /// until the event is opened, and they change what the number means, so
    /// they travel with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// What the request's filter removed, when it had to run here. `None` when
    /// the request carried no filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered: Option<Filtered>,
    /// The backends that produced these events, in the order they served.
    ///
    /// One entry, the resolver's choice, unless a budget moved the collection
    /// onto a cheaper backend part way through; then the series is two
    /// mechanisms end to end and `notes` says when it changed. Recorded here
    /// because the resolved request is written down before the run and cannot
    /// know.
    #[serde(default)]
    pub served_by: Vec<String>,
    /// What this collection cost, measured rather than declared.
    #[serde(default)]
    pub overhead: Overhead,
}

/// What a filter the resolver could not push down did to a collection.
///
/// Events a filter removed are not losses: they were asked to go. But they were
/// collected, paid for, and are absent from every number that follows, so the
/// count belongs beside the results for the same reason the loss count does --
/// a series that quietly holds three of forty events looks exactly like a
/// series of three.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filtered {
    /// The predicate, as the request wrote it.
    pub predicate: String,
    pub kept: u64,
    pub dropped: u64,
    /// Fields the predicate names that no event carried. A predicate over a
    /// field nothing produces is false everywhere, which empties the series and
    /// looks exactly like a workload that did nothing -- so the reason is
    /// recorded rather than left to be guessed at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unseen_fields: Vec<String>,
}

/// Everything that did not make it into `events`.
///
/// Zero everywhere is the only reading that means "these numbers are complete".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Losses {
    /// Events the kernel had nowhere to put. Reported by the ring itself.
    pub dropped: u64,
    /// Reads that returned nothing.
    pub failed_reads: u64,
    /// Ticks that started late enough to shift the series.
    pub late_ticks: u64,
    /// Records the reader did not know how to decode. Counted rather than
    /// skipped, because a record shape nobody handles is a gap in the data.
    pub undecoded: u64,
    /// Periods in which the kernel throttled the event because it was costing
    /// too much.
    ///
    /// Deliberately not folded into `dropped`. A drop count is a number of
    /// events the ring reported losing; a throttle is the kernel declining to
    /// deliver anything for a while, and how many events happened inside that
    /// window is not knowable from userspace. Counting one throttle as one
    /// drop claims a precision that does not exist, and is the reason a
    /// throttled run of `sched.switches` under-reported its own loss rather
    /// than saying it could not tell.
    ///
    /// `kernel.perf_event_max_sample_rate` is what decides this, and a run
    /// with a non-zero count here is not comparable to one without.
    pub throttled: u64,
    /// The reasons behind the counts above, with how often each occurred.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reasons: BTreeMap<String, u64>,
}

impl Losses {
    pub fn is_clean(&self) -> bool {
        self.dropped == 0
            && self.failed_reads == 0
            && self.late_ticks == 0
            && self.undecoded == 0
            && self.throttled == 0
    }

    pub fn note(&mut self, reason: impl Into<String>) {
        *self.reasons.entry(reason.into()).or_insert(0) += 1;
    }

    /// Records one throttled period, with what the reader needs to know about
    /// it. The count is of periods, not of events: the events are the part
    /// nobody can count.
    pub fn throttle(&mut self, reason: impl Into<String>) {
        self.throttled += 1;
        self.note(reason);
    }

    fn absorb(&mut self, other: Losses) {
        self.dropped += other.dropped;
        self.failed_reads += other.failed_reads;
        self.late_ticks += other.late_ticks;
        self.undecoded += other.undecoded;
        self.throttled += other.throttled;
        for (reason, count) in other.reasons {
            *self.reasons.entry(reason).or_insert(0) += count;
        }
    }
}

/// What a run cost, measured rather than declared.
///
/// The catalog declares a cost *band* per backend -- `free`, `low`, `med` --
/// and until now that was a claim nobody had checked. This is the measurement
/// behind it: the cpu time this process burned collecting, against the cpu time
/// the thing being observed burned over the same window.
///
/// The denominator is the target's own cpu time on purpose. "Two percent
/// overhead" means "the measurement cost two percent of what the workload
/// cost"; a fraction of wall time would answer a different question and would
/// let the same collector read as cheap because the machine happened to be
/// idle. A target that burned no cpu, or that has no cpu account at all like a
/// network interface, has no denominator -- and that is said rather than
/// divided by.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Overhead {
    /// User plus system cpu time this whole process spent between the moment
    /// the collectors were attached and the moment the window closed.
    /// `getrusage(RUSAGE_SELF)`, differenced.
    pub collector_cpu_ns: u64,
    /// The share of `collector_cpu_ns` this collection is answerable for.
    ///
    /// One process serves every request in a session and `getrusage` cannot say
    /// which of them spent what, so the share is the fraction of the time spent
    /// inside `sample` that went to this collector. That is wall time, and
    /// therefore an approximation -- exactly equal to the whole when a session
    /// carries one request, which is the common case, and reported as a share
    /// when it does not.
    pub attributed_cpu_ns: u64,
    /// Cpu time the observed target spent over the same window.
    pub observed_cpu_ns: Option<u64>,
    /// Where the number above came from, or why there is none. It travels with
    /// the number because `getrusage(RUSAGE_CHILDREN)` and `/proc/<pid>/stat`
    /// and `/proc/stat` are three different accounts, and a reader comparing
    /// two runs needs to know which one they are reading.
    pub observed_from: String,
    /// Wall time of the window.
    pub wall_ns: u64,
    /// What measuring all of this cost: the `getrusage` calls, the target's
    /// procfs reads, the clock reads that share time out per collector, and --
    /// when a ceiling was exceeded -- moving the collection onto a cheaper
    /// backend. Enforcing a budget is part of the price of having one.
    ///
    /// Reported rather than subtracted. A measurement that hides its own price
    /// is exactly the thing this field exists to stop, and a budget check that
    /// ran often enough to dominate the answer would show up here first.
    pub measurement_ns: u64,
    /// How many times the cost was read during the run.
    pub measurements: u64,
}

impl Overhead {
    /// The overhead as a percentage of the observed workload, when there is a
    /// workload to be a percentage of.
    pub fn percent(&self) -> Option<f64> {
        match self.observed_cpu_ns {
            Some(observed) if observed > 0 => {
                Some(self.attributed_cpu_ns as f64 * 100.0 / observed as f64)
            }
            _ => None,
        }
    }
}

/// User plus system cpu time of one `getrusage` account, in nanoseconds.
fn rusage_ns(who: libc::c_int) -> u64 {
    // SAFETY: `getrusage` fills a struct this function owns, and writes nothing
    // else.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(who, &mut usage) } != 0 {
        return 0;
    }
    let ns =
        |time: libc::timeval| (time.tv_sec as u64) * 1_000_000_000 + (time.tv_usec as u64) * 1_000;
    ns(usage.ru_utime) + ns(usage.ru_stime)
}

/// The kernel's tick, which is what `/proc` reports cpu time in.
fn ticks_per_second() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one.
    match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
        ticks if ticks > 0 => ticks as u64,
        _ => 100,
    }
}

/// Cpu time one task has consumed, from its own `/proc` entry.
///
/// `cutime` and `cstime` are included: a shell that forks is still the workload
/// that was asked for, and leaving its children out would make every collector
/// look expensive against it.
fn task_cpu_ns(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesised and may contain spaces and parentheses
    // of its own, so the fields are counted from the last `)` rather than split
    // from the start. After it, the first field is `state`, which is field 3.
    let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    let ticks: u64 = fields
        .get(11..15)?
        .iter()
        .map(|field| field.parse::<u64>().unwrap_or(0))
        .sum();
    Some(ticks * 1_000_000_000 / ticks_per_second())
}

/// Cpu time the machine, or a set of cpus, spent on anything but idling.
///
/// `iowait` is idle: a cpu waiting for a disk is available to anything else
/// that wants it, and counting it as work would inflate the denominator and
/// make every collector look cheap on a machine that is mostly blocked.
fn cpus_busy_ns(wanted: Option<&[u32]>) -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let mut total = 0u64;
    let mut matched = false;
    for line in stat.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            break;
        };
        let (label, numbers) = match rest.split_once(char::is_whitespace) {
            Some(split) => split,
            None => continue,
        };
        match (wanted, label.parse::<u32>()) {
            // The summary line, which is what a machine-wide target wants.
            (None, Err(_)) => {}
            (Some(wanted), Ok(cpu)) if wanted.contains(&cpu) => {}
            _ => continue,
        }
        let fields: Vec<u64> = numbers
            .split_whitespace()
            .map(|field| field.parse().unwrap_or(0))
            .collect();
        // user nice system idle iowait irq softirq steal guest guest_nice.
        // The two guest columns are already counted inside user and nice, so
        // summing the line and taking the idle columns off would count a
        // virtualised machine's work twice; the busy columns are named instead.
        const BUSY: [usize; 6] = [0, 1, 2, 5, 6, 7];
        total += BUSY
            .iter()
            .filter_map(|column| fields.get(*column))
            .sum::<u64>();
        matched = true;
    }
    matched.then(|| total * 1_000_000_000 / ticks_per_second())
}

/// Cpu time a cgroup has been charged, from `cpu.stat`.
fn cgroup_cpu_ns(directory: &std::path::Path) -> Option<u64> {
    let stat = std::fs::read_to_string(directory.join("cpu.stat")).ok()?;
    stat.lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|usec| usec * 1_000)
}

/// Resolves a cgroup target to the directory that holds its `cpu.stat`.
///
/// A target may be written the way `/proc/self/cgroup` writes it, rooted at the
/// hierarchy, or as the mounted path; both name the same cgroup. The same
/// two-way reading is in `cpu/throttle.rs`, which is where it was worked out.
fn cgroup_directory(path: &std::path::Path) -> std::path::PathBuf {
    const ROOT: &str = "/sys/fs/cgroup";
    if path.starts_with(ROOT) {
        path.to_path_buf()
    } else {
        std::path::Path::new(ROOT).join(path.strip_prefix("/").unwrap_or(path))
    }
}

/// Reads the cpu time of whatever is being observed, so that the tool's own
/// cost can be a fraction of something real.
enum Meter {
    /// A program this tool launched. While it runs, its own `/proc` entry says
    /// what it has spent; once it has been reaped its entry is gone and the
    /// kernel has rolled its usage into this process's children account, which
    /// is the same number `time` prints and is read from there instead.
    Launched(i32),
    /// Processes or threads the request named.
    Tasks(Vec<i32>),
    /// The machine, or the cpus the request named.
    Cpus(Option<Vec<u32>>),
    Cgroup(std::path::PathBuf),
    /// Nothing here keeps a cpu account, with the reason.
    Nothing(&'static str),
}

impl Meter {
    fn of(target: &Target, launched: Option<i32>) -> Meter {
        match target {
            Target::Launch(_) => match launched {
                Some(pid) => Meter::Launched(pid),
                None => Meter::Nothing("the program was never launched"),
            },
            Target::Pid(pids) | Target::Tid(pids) => Meter::Tasks(pids.clone()),
            Target::Cgroup(path) => Meter::Cgroup(cgroup_directory(path)),
            Target::Cpu(set) => Meter::Cpus(Some(set.cpus().to_vec())),
            Target::System => Meter::Cpus(None),
            Target::Iface(_) => Meter::Nothing(
                "a network interface consumes no cpu time of its own, so there is nothing \
                 for an overhead to be a percentage of",
            ),
            Target::Container(_) => Meter::Nothing(
                "a container has to be resolved to its cgroup before its cpu time can be \
                 read, which is not built yet",
            ),
        }
    }

    /// The cumulative cpu time of the target, to be differenced against a
    /// baseline taken when the window opened.
    fn read(&self) -> Option<u64> {
        match self {
            Meter::Launched(pid) => {
                Some(task_cpu_ns(*pid).unwrap_or_else(|| rusage_ns(libc::RUSAGE_CHILDREN)))
            }
            Meter::Tasks(pids) => {
                let mut total = 0;
                let mut any = false;
                for pid in pids {
                    if let Some(cpu_ns) = task_cpu_ns(*pid) {
                        total += cpu_ns;
                        any = true;
                    }
                }
                any.then_some(total)
            }
            Meter::Cpus(set) => cpus_busy_ns(set.as_deref()),
            Meter::Cgroup(directory) => cgroup_cpu_ns(directory),
            Meter::Nothing(_) => None,
        }
    }

    /// What to record beside the number, so a reader knows which account it is.
    fn describes(&self) -> String {
        match self {
            Meter::Launched(pid) => format!(
                "the launched program's own cpu time (/proc/{pid}/stat while it ran, \
                 getrusage(RUSAGE_CHILDREN) once it was reaped)"
            ),
            Meter::Tasks(pids) => format!(
                "the cpu time of {} (/proc/<pid>/stat, children included)",
                pids.iter()
                    .map(|pid| pid.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Meter::Cpus(None) => "everything but idle on every cpu (/proc/stat)".to_owned(),
            Meter::Cpus(Some(cpus)) => format!(
                "everything but idle on cpu {} (/proc/stat)",
                cpus.iter()
                    .map(|cpu| cpu.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Meter::Cgroup(directory) => {
                format!("{}/cpu.stat usage_usec", directory.display())
            }
            Meter::Nothing(reason) => (*reason).to_owned(),
        }
    }
}

/// Where a run turns when it costs more than it was allowed.
///
/// The runner can tell that it is over budget. It cannot tell what is cheaper:
/// that is a question about the catalog and about this machine, and answering
/// it here would mean the collector layer reaching back into the resolver. So
/// the answer is handed in, and a run given no answer stops instead of
/// degrading.
pub trait Cheaper {
    /// The next backend to serve request `index` with -- cheaper than the one
    /// in use, and already resolved against this machine. `None` means the
    /// chain has nothing left, which is what ends the run.
    fn next(&mut self, index: usize, spent: &Overhead, ceiling: f64) -> Option<ResolvedRequest>;
}

/// The policy of a run that was given nowhere to degrade to: over budget, it
/// stops and says so.
pub struct StopWhenOverBudget;

impl Cheaper for StopWhenOverBudget {
    fn next(&mut self, _: usize, _: &Overhead, _: f64) -> Option<ResolvedRequest> {
        None
    }
}

/// One source of events.
pub trait Collector {
    fn id(&self) -> &CollectorId;

    /// The attribute that holds this collector's measured value.
    fn value_attr(&self) -> &'static str;

    /// Opens whatever the collector needs, before the observed program runs.
    fn start(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Produces the events observable right now.
    ///
    /// An error is a failed read, not the end of the run: a process that
    /// vanished mid-poll ends the collection, a transient read failure is
    /// counted and the run goes on.
    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError>;

    /// Closes the collector and returns anything still buffered.
    ///
    /// **This is not where a polled counter takes a closing read.** The runner
    /// closes the window itself: its last round of `sample` calls happens at
    /// the moment the stop condition is met, so the last interval of a series
    /// is already measured by the time this is called. A further read here
    /// covers the microseconds since -- `the_runner_closes_the_window` below
    /// measures them -- and produces a second point at the same offset with a
    /// value near zero, which drags every summary computed over the series.
    ///
    /// What does belong here: events a ring or a shim published since the last
    /// drain, and the single measurement of a `count` window, which has no
    /// series and whose closing read *is* the result.
    fn stop(&mut self) -> Vec<Event> {
        Vec::new()
    }

    /// How often this collector needs to be asked, when that is dictated by the
    /// collector rather than by the request. A streaming collector answers with
    /// the interval that keeps its buffer from overflowing.
    fn tick(&self) -> Option<StdDuration> {
        None
    }

    /// What the collector itself knows it lost.
    fn losses(&self) -> Losses {
        Losses::default()
    }

    /// The frames behind each stack identity this collector interned.
    ///
    /// Empty for everything that does not capture stacks, which is most of
    /// them. Read after `stop`, so a collector that finishes its symbolisation
    /// at the end has somewhere to do it.
    fn stacks(&self) -> BTreeMap<u64, Vec<String>> {
        BTreeMap::new()
    }

    /// What the collector had to give up to produce these numbers. Empty is the
    /// normal case; anything here reaches the user-visible output.
    fn notes(&self) -> Vec<String> {
        Vec::new()
    }
}

/// One collector this build knows how to create.
///
/// Families register through this list rather than through a central `match`,
/// so adding a source touches only that family's directory. Nine agents editing
/// one match arm is nine merge conflicts on the same three lines.
pub struct Factory {
    /// The catalog source this builds, such as `mem.rss`.
    pub source: &'static str,
    /// The backend id within that source's chain, such as `fs:proc:statm`.
    /// Backend ids are unique within a chain, which is what makes this pair a
    /// key.
    pub backend: &'static str,
    build: fn(&ResolvedRequest) -> Result<Box<dyn Prepared>, String>,
}

impl Factory {
    pub fn new(
        source: &'static str,
        backend: &'static str,
        build: fn(&ResolvedRequest) -> Result<Box<dyn Prepared>, String>,
    ) -> Self {
        Self {
            source,
            backend,
            build,
        }
    }
}

/// Every collector in this build.
///
/// The nine families are listed once. There will never be a tenth: the catalog
/// families are the vocabulary, not an extension point.
fn registry() -> Vec<Factory> {
    let families = [
        cpu::factories,
        gpu::factories,
        io::factories,
        mem::factories,
        net::factories,
        power::factories,
        proc::factories,
        runtime::factories,
        sched::factories,
    ];
    families.into_iter().flat_map(|family| family()).collect()
}

/// A collector before the thing it observes exists.
///
/// Two phases are needed because some collectors have to act on the target
/// before it runs: a preloaded shim is given at exec and cannot be handed to a
/// process that is already going. Splitting construction from attachment lets
/// the launch carry whatever the collector needs, without the runner knowing
/// what any particular collector wants.
pub trait Prepared {
    /// Environment the launched program must be given.
    fn launch_environment(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Set when this collector can only observe a program the tool launches,
    /// with the reason, so the refusal explains itself.
    fn requires_launch(&self) -> Option<&'static str> {
        None
    }

    /// Binds to the target now that it exists.
    ///
    /// The target is passed whole rather than as a pid, because not every
    /// source observes a process: an interface counter and a machine-wide file
    /// have no pid to be given, and inventing one for them would push the lie
    /// into every collector.
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String>;
}

/// What a single sample can go wrong with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleError {
    /// The thing being observed no longer exists. The run ends, cleanly.
    TargetGone(String),
    /// This read failed; another one may not.
    Transient(String),
}

/// Runs one resolved request to completion.
pub fn run(resolved: &ResolvedRequest) -> Result<Collection, CollectError> {
    let mut collections = run_all(std::slice::from_ref(resolved))?;
    Ok(collections.remove(0))
}

/// Runs several resolved requests over one target, in one window.
///
/// They share a window deliberately. Running them one after another would
/// measure several different moments and present them as one, and a join across
/// them would then be correlating a program with its own past. One launch, one
/// set of collectors, one clock.
///
/// A run started this way has nowhere to degrade to, so a `--budget` it exceeds
/// ends it. [`run_all_within`] is the same run with a ladder underneath.
pub fn run_all(resolved: &[ResolvedRequest]) -> Result<Vec<Collection>, CollectError> {
    run_all_within(resolved, &mut StopWhenOverBudget)
}

/// The same run, with somewhere to turn when a budget is exceeded.
///
/// The split keeps the collector layer from knowing about the catalog. The
/// runner can measure what it is spending and compare it against the ceiling;
/// *which backend is cheaper than this one and reachable here* is a question
/// about the catalog and about this machine, and it is answered by whoever
/// holds both.
pub fn run_all_within(
    resolved: &[ResolvedRequest],
    cheaper: &mut dyn Cheaper,
) -> Result<Vec<Collection>, CollectError> {
    assert!(!resolved.is_empty(), "a run needs at least one request");
    let target = &resolved[0].request.target;
    if let Some(other) = resolved
        .iter()
        .find(|candidate| &candidate.request.target != target)
    {
        return Err(CollectError::MixedTargets {
            first: target.to_string(),
            second: other.request.target.to_string(),
        });
    }

    // Asked first, because "no collector exists for this backend" is a
    // different answer from "this target is not wired up yet", and reporting
    // the second when the first is true sends the reader to the wrong place.
    let prepared: Vec<Box<dyn Prepared>> =
        resolved.iter().map(prepare).collect::<Result<_, _>>()?;

    let environment = merge_environments(&prepared)?;

    // A collector that can only be given to a program at exec is asked about
    // next, and before the stop condition, because the two refusals send the
    // reader to different places: "pass --duration" is advice for a request
    // that can work, and this one cannot. Answering the shallower question
    // first sent a caller off to add a flag that would change nothing.
    if !matches!(target, Target::Launch(_)) {
        for (index, prepared) in prepared.iter().enumerate() {
            if let Some(reason) = prepared.requires_launch() {
                return Err(CollectError::UnsupportedTarget {
                    source_id: resolved[index].request.source.to_string(),
                    target: format!("{} ({reason})", target.kind()),
                });
            }
        }
    }

    // A run that nothing will ever stop is refused before the target is
    // launched or anything is attached to it -- but after the collectors have
    // been asked for, because "no collector is written for this" is the deeper
    // answer and the one worth giving first. A duration, a program whose exit
    // ends the run, or a single read: with none of the three there is no
    // window, and observing a target that nothing will stop watching is worse
    // than refusing.
    let duration_ns = resolved
        .iter()
        .filter_map(|resolved| resolved.request.duration)
        .map(|duration| duration.as_nanos())
        .max();
    let snapshot = resolved
        .iter()
        .all(|resolved| resolved.request.mode == Mode::Snapshot);
    if duration_ns.is_none() && !matches!(target, Target::Launch(_)) && !snapshot {
        return Err(CollectError::NoStopCondition);
    }

    // A launched program is both the target and the clock: the run ends when it
    // does, which is what makes `omn <source> -- ./prog` a complete measurement
    // rather than a sample of one. It is held at a gate so that collectors are
    // attached before its first instruction; otherwise the interesting part of
    // a short program is over before anything is watching.
    let mut child = match target {
        Target::Launch(argv) => {
            Some(
                launch::gated(argv, &environment).map_err(|error| CollectError::Launch {
                    program: argv.first().cloned().unwrap_or_default(),
                    error: error.to_string(),
                })?,
            )
        }
        _ => None,
    };

    // A launched program replaces the target's own identity: the collectors
    // observe the process that was just created, not a pid the request named.
    // A target that names no process -- an interface, the machine -- is passed
    // through, and a collector that needs a pid refuses it by name.
    let bound = match &child {
        Some(child) => Target::Pid(vec![child.pid()]),
        None => target.clone(),
    };

    let mut collectors = Vec::with_capacity(prepared.len());
    for (index, prepared) in prepared.into_iter().enumerate() {
        let source_id = resolved[index].request.source.to_string();
        let mut collector = prepared
            .attach(&bound)
            .map_err(|reason| CollectError::Start {
                source_id: source_id.clone(),
                reason,
            })?;
        if let Err(reason) = collector.start() {
            if let Some(child) = child.as_mut() {
                child.terminate();
            }
            return Err(CollectError::Start { source_id, reason });
        }
        collectors.push(collector);
    }

    // Everything is attached; let the program run.
    if let Some(child) = child.as_mut() {
        child.release();
    }

    // The fastest collector sets the pace: draining a ring every second loses
    // events, and the cheap pollers do not mind being asked more often.
    let interval = collectors
        .iter()
        .filter_map(|collector| collector.tick())
        .min()
        .unwrap_or_else(|| match resolved[0].request.mode {
            Mode::Poll { interval } => StdDuration::from_nanos(interval.as_nanos()),
            Mode::Snapshot => StdDuration::ZERO,
            _ => StdDuration::from_nanos(Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
        });

    let deadline_ns = duration_ns.map(|longest| monotonic_ns() + longest);

    // The cost accounting opens here, with everything attached and the program
    // released: what was spent loading the catalog, probing the machine and
    // opening counters is this tool's startup, not the price of the
    // measurement, and charging the measurement for it would make every short
    // run look ruinous.
    let mut spending = Spending::open(
        Meter::of(target, child.as_ref().map(|child| child.pid())),
        bound.clone(),
        resolved.to_vec(),
        cheaper,
    );
    let started_ns = spending.opened_ns;
    let (mut events, mut losses, stopped_because) = drive(
        &mut collectors,
        Window { interval, snapshot },
        &mut spending,
        || {
            if let Some(child) = child.as_mut() {
                if let Some(status) = child.finished() {
                    return Some(format!("the launched program exited with {status}"));
                }
            }
            match deadline_ns {
                Some(deadline_ns) if monotonic_ns() >= deadline_ns => {
                    Some("the requested duration elapsed".to_owned())
                }
                _ => None,
            }
        },
    );

    // Whatever the kernel or the shim published between the last tick and the
    // stop still belongs to this run.
    for (index, collector) in collectors.iter_mut().enumerate() {
        let mut trailing = collector.stop();
        events[index].append(&mut trailing);
        let collector_losses = collector.losses();
        losses[index].absorb(collector_losses);
    }

    // A launched program that outlived the duration is stopped: the tool does
    // not leave behind processes it started.
    if let Some(child) = child.as_mut() {
        if child.finished().is_none() {
            child.terminate();
        }
    }

    let observed_ns = monotonic_ns().saturating_sub(started_ns);
    // Read once the child has been reaped: `getrusage(RUSAGE_CHILDREN)` is what
    // `time` prints, and it only prints it after the wait.
    let overheads = spending.close();
    Ok(collectors
        .iter()
        .enumerate()
        .map(|(index, collector)| {
            let mut collected = std::mem::take(&mut events[index]);
            collected.sort_by_key(|event| event.ts_ns);
            // The filter is the request's, and a degradation does not change
            // the request -- only how it was served.
            let filtered = apply_filter(&resolved[index], &mut collected);
            let mut notes = std::mem::take(&mut spending.notes[index]);
            notes.extend(collector.notes());
            Collection {
                observed_ns,
                value_attr: collector.value_attr().to_owned(),
                events: collected,
                losses: std::mem::take(&mut losses[index]),
                stopped_because: stopped_because.clone(),
                notes,
                stacks: collector.stacks(),
                filtered,
                served_by: std::mem::take(&mut spending.served_by[index]),
                overhead: overheads[index].clone(),
            }
        })
        .collect())
}

/// How often a run carrying a budget looks at what it is spending.
///
/// Every check is two cheap reads -- `getrusage` and one `/proc` file -- and
/// `Overhead::measurement_ns` reports what they came to over the run. Ten times
/// a second is often enough to stop a collection that is running away, and rare
/// enough that the answer is not mostly itself: at roughly ten microseconds a
/// check this is about a thousandth of a percent of one cpu.
const BUDGET_CHECK_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Where one collection's accounting starts.
///
/// The whole run, normally. A collection a budget moved onto a cheaper backend
/// starts again from the moment it moved: what the backend before it spent is
/// sunk, and charging the replacement for its predecessor would condemn it
/// before it had read anything.
#[derive(Debug, Clone, Copy, Default)]
struct Since {
    self_cpu_ns: u64,
    observed_cpu_ns: u64,
    inside_ns: u64,
}

/// The cost half of a run: what it is spending, what it was allowed to spend,
/// and where it turns when the two disagree.
struct Spending<'a> {
    meter: Meter,
    /// The target the collectors are bound to, which a replacement has to be
    /// attached to as well.
    bound: Target,
    /// The request each collector is serving *now*. A degradation changes it,
    /// which is why the resolved request written before the run cannot be the
    /// record of what produced the numbers.
    serving: Vec<ResolvedRequest>,
    ceilings: Vec<Option<f64>>,
    cheaper: &'a mut dyn Cheaper,
    /// Wall time spent inside each collector's own calls, which is how one
    /// process's cpu time is shared out between several collectors. Left at
    /// zero when a run has a single collector, because then the share is the
    /// whole and two clock reads per tick would buy nothing.
    inside_ns: Vec<u64>,
    since: Vec<Since>,
    opened_ns: u64,
    self_cpu_opened_ns: u64,
    observed_opened_ns: u64,
    last_check_ns: u64,
    measurement_ns: u64,
    measurements: u64,
    /// What a budget did to each collection, in the words the user will read.
    notes: Vec<Vec<String>>,
    /// The backends each collection has run through, in order.
    served_by: Vec<Vec<String>>,
}

impl<'a> Spending<'a> {
    /// Opens the accounting, with the collectors attached and the program
    /// running: what was spent resolving, probing and attaching is this tool's
    /// startup, not the price of the measurement.
    fn open(
        meter: Meter,
        bound: Target,
        serving: Vec<ResolvedRequest>,
        cheaper: &'a mut dyn Cheaper,
    ) -> Self {
        let count = serving.len();
        let serving_ids = serving
            .iter()
            .map(|resolved| vec![resolved.backend.id.clone()])
            .collect();
        let ceilings = serving
            .iter()
            .map(|resolved| resolved.request.budget.map(|budget| budget.percent()))
            .collect();
        let opened_ns = monotonic_ns();
        let self_cpu_opened_ns = rusage_ns(libc::RUSAGE_SELF);
        let observed_opened_ns = meter.read().unwrap_or(0);
        Self {
            meter,
            bound,
            serving,
            ceilings,
            cheaper,
            inside_ns: vec![0; count],
            since: vec![
                Since {
                    self_cpu_ns: self_cpu_opened_ns,
                    observed_cpu_ns: observed_opened_ns,
                    inside_ns: 0,
                };
                count
            ],
            opened_ns,
            self_cpu_opened_ns,
            observed_opened_ns,
            last_check_ns: opened_ns,
            measurement_ns: 0,
            measurements: 0,
            notes: vec![Vec::new(); count],
            served_by: serving_ids,
        }
    }

    /// Whether anything here has a ceiling at all. A run with no budget is
    /// never interrupted to be weighed.
    fn enforcing(&self) -> bool {
        self.ceilings.iter().any(Option::is_some)
    }

    /// What one collection has cost since a baseline.
    fn spent(
        &self,
        index: usize,
        since: Since,
        self_cpu_ns: u64,
        observed_cpu_ns: Option<u64>,
        now_ns: u64,
    ) -> Overhead {
        let collector_cpu_ns = self_cpu_ns.saturating_sub(since.self_cpu_ns);
        // One process serves every request, so `getrusage` answers for all of
        // them at once. With one collector the share is the whole and there is
        // nothing to approximate; with several it is the fraction of the time
        // spent inside collectors that went to this one.
        let total_inside_ns: u64 = self.inside_ns.iter().sum();
        let attributed_cpu_ns = match (self.inside_ns.len(), total_inside_ns) {
            (1, _) => collector_cpu_ns,
            (count, 0) => collector_cpu_ns / count as u64,
            (_, total) => {
                let mine = self.inside_ns[index].saturating_sub(since.inside_ns);
                (collector_cpu_ns as u128 * mine as u128 / total as u128) as u64
            }
        };
        Overhead {
            collector_cpu_ns,
            attributed_cpu_ns,
            observed_cpu_ns: observed_cpu_ns
                .map(|observed| observed.saturating_sub(since.observed_cpu_ns)),
            observed_from: self.meter.describes(),
            wall_ns: now_ns.saturating_sub(self.opened_ns),
            measurement_ns: self.measurement_ns,
            measurements: self.measurements,
        }
    }

    /// Looks at what the run has cost, and acts when a collection is over its
    /// ceiling. Returns the reason the run must stop, when there is one.
    fn check(
        &mut self,
        collectors: &mut [Box<dyn Collector>],
        events: &mut [Vec<Event>],
        losses: &mut [Losses],
    ) -> Option<String> {
        if !self.enforcing() {
            return None;
        }
        let entered_ns = monotonic_ns();
        if entered_ns.saturating_sub(self.last_check_ns) < BUDGET_CHECK_INTERVAL.as_nanos() as u64 {
            return None;
        }
        self.last_check_ns = entered_ns;
        let self_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
        let observed_cpu_ns = self.meter.read();
        self.measurements += 1;

        let mut stop = None;
        for index in 0..collectors.len() {
            let Some(ceiling) = self.ceilings[index] else {
                continue;
            };
            let spent = self.spent(
                index,
                self.since[index],
                self_cpu_ns,
                observed_cpu_ns,
                entered_ns,
            );
            // No denominator yet is not "within budget": it is "not knowable
            // yet", and a target that has burned no cpu at all has nothing for
            // an overhead to be a percentage of.
            let Some(percent) = spent.percent() else {
                continue;
            };
            if percent <= ceiling {
                continue;
            }
            let source = self.serving[index].request.source.to_string();
            let was = self.serving[index].backend.id.clone();
            // A collection a budget has already moved once is weighed on the
            // window since it moved, so the reason has to say which window it
            // is talking about or it reads as disagreeing with the run's own
            // total.
            let window = if self.served_by[index].len() > 1 {
                format!(" since it was moved onto `{was}`")
            } else {
                String::new()
            };
            match self.cheaper.next(index, &spent, ceiling) {
                Some(next) => match self.degrade(index, next, collectors, events, losses) {
                    Ok(onto) => self.notes[index].push(format!(
                        "over budget at {:.2} ms into the run: `{source}` through `{was}` spent \
                         {percent:.2} % of the target's cpu time against a ceiling of {ceiling} %, \
                         so it was moved onto `{onto}` for the rest of it",
                        spent.wall_ns as f64 / 1e6,
                    )),
                    Err(why) => {
                        stop = Some(format!(
                            "`{source}` spent {percent:.2} % of the target's cpu time{window} \
                             against a ceiling of {ceiling} %, and the cheaper backend could \
                             not be started: {why}"
                        ));
                        break;
                    }
                },
                None => {
                    stop = Some(format!(
                        "`{source}` spent {percent:.2} % of the target's cpu time{window} against \
                         a ceiling of {ceiling} %, and nothing cheaper than `{was}` is available"
                    ));
                    break;
                }
            }
        }

        self.measurement_ns += monotonic_ns().saturating_sub(entered_ns);
        stop
    }

    /// Moves one collection onto a cheaper backend without ending the run.
    ///
    /// Everything the outgoing collector still held is drained into the run
    /// first: those events were paid for and are as real as the ones that come
    /// after. What changes from here is how the next ones are obtained, which
    /// is why the note says *when* it changed.
    fn degrade(
        &mut self,
        index: usize,
        next: ResolvedRequest,
        collectors: &mut [Box<dyn Collector>],
        events: &mut [Vec<Event>],
        losses: &mut [Losses],
    ) -> Result<String, String> {
        let onto = next.backend.id.clone();
        let prepared = prepare(&next).map_err(|error| error.to_string())?;
        let mut replacement = prepared.attach(&self.bound)?;
        replacement.start()?;

        let mut trailing = collectors[index].stop();
        events[index].append(&mut trailing);
        losses[index].absorb(collectors[index].losses());
        losses[index].note(format!(
            "the budget moved this collection from `{}` to `{onto}` mid-run; the series is two \
             backends end to end",
            self.serving[index].backend.id
        ));
        collectors[index] = replacement;
        self.serving[index] = next;
        self.served_by[index].push(onto.clone());
        // The replacement is weighed on its own account.
        self.since[index] = Since {
            self_cpu_ns: rusage_ns(libc::RUSAGE_SELF),
            observed_cpu_ns: self.meter.read().unwrap_or(0),
            inside_ns: self.inside_ns[index],
        };
        Ok(onto)
    }

    /// Closes the accounting and shares the run's cost out between the
    /// collections. This is the whole window, not the part since a degradation:
    /// what the run cost is what it cost.
    fn close(&mut self) -> Vec<Overhead> {
        let entered_ns = monotonic_ns();
        let self_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
        let observed_cpu_ns = self.meter.read();
        self.measurements += 1;
        let closed_ns = monotonic_ns();
        self.measurement_ns += closed_ns.saturating_sub(entered_ns);

        let whole_run = Since {
            self_cpu_ns: self.self_cpu_opened_ns,
            observed_cpu_ns: self.observed_opened_ns,
            inside_ns: 0,
        };
        (0..self.ceilings.len())
            .map(|index| self.spent(index, whole_run, self_cpu_ns, observed_cpu_ns, closed_ns))
            .collect()
    }
}

#[cfg(test)]
impl Spending<'static> {
    /// A run with no ceiling and nothing to weigh it against, for the tests
    /// that are about the pacing rather than the price.
    fn unweighed(count: usize) -> Spending<'static> {
        let mut spending = Spending::open(
            Meter::Nothing("this run was not weighed"),
            Target::System,
            Vec::new(),
            Box::leak(Box::new(StopWhenOverBudget)),
        );
        spending.ceilings = vec![None; count];
        spending.inside_ns = vec![0; count];
        spending.since = vec![Since::default(); count];
        spending.notes = vec![Vec::new(); count];
        spending.served_by = vec![Vec::new(); count];
        spending
    }

    /// A run weighed against this process's own cpu time.
    ///
    /// Numerator and denominator are then the same account -- `getrusage` and
    /// `/proc/self/stat` report the same process -- so the ratio is around one
    /// hundred percent however much or little the machine is doing, and a test
    /// can put a ceiling either side of it without depending on the load. Which
    /// matters here: `cargo test` runs its tests in parallel threads of one
    /// process, and a measurement of "this process" is a measurement of all of
    /// them.
    fn weighed_against_this_process(serving: Vec<ResolvedRequest>) -> Spending<'static> {
        let own = std::process::id() as i32;
        Spending::open(
            Meter::Tasks(vec![own]),
            Target::Pid(vec![own]),
            serving,
            Box::leak(Box::new(StopWhenOverBudget)),
        )
    }
}

/// The pace of a run and what a single read means, as the runner sees them.
struct Window {
    interval: StdDuration,
    /// A single read, after which the run is over: there is no series and no
    /// second round.
    snapshot: bool,
}

/// Drives a set of collectors over one window, and closes it.
///
/// The order inside a round is the whole of the edge convention: **read
/// first, then decide**. A round reads every collector, and only then asks
/// whether the run is over, so the last round of reads happens at the moment
/// the run ends rather than one interval before it. That is what makes a
/// polled counter's series complete without any collector reading again in
/// `stop`, and `the_runner_closes_the_window` measures how little time is
/// left over for such a read to cover.
///
/// `ended` is consulted once per round and returns why the run should stop.
/// It is a closure rather than a launched child and a deadline so that the
/// convention can be tested against a stop condition a test controls.
fn drive(
    collectors: &mut [Box<dyn Collector>],
    window: Window,
    spending: &mut Spending<'_>,
    mut ended: impl FnMut() -> Option<String>,
) -> (Vec<Vec<Event>>, Vec<Losses>, String) {
    let mut events: Vec<Vec<Event>> = vec![Vec::new(); collectors.len()];
    let mut losses: Vec<Losses> = vec![Losses::default(); collectors.len()];
    let mut sequences = vec![0u64; collectors.len()];
    // Only a run serving several collectors has to share its cpu time out
    // between them, and only then are the two clock reads a round worth taking:
    // with one collector the share is the whole.
    let attributing = collectors.len() > 1;

    let stopped_because = loop {
        let tick_started_ns = monotonic_ns();
        let mut gone = None;
        for (index, collector) in collectors.iter_mut().enumerate() {
            let entered_ns = if attributing { monotonic_ns() } else { 0 };
            match collector.sample(sequences[index]) {
                Ok(mut sampled) => {
                    sequences[index] += sampled.len() as u64;
                    events[index].append(&mut sampled);
                }
                Err(SampleError::TargetGone(reason)) => gone = Some(reason),
                Err(SampleError::Transient(reason)) => {
                    losses[index].failed_reads += 1;
                    losses[index].note(reason);
                }
            }
            if attributing {
                spending.inside_ns[index] += monotonic_ns().saturating_sub(entered_ns);
            }
        }
        if let Some(reason) = gone {
            break reason;
        }

        if window.snapshot {
            break "snapshot: a single read".to_owned();
        }

        if let Some(reason) = ended() {
            break reason;
        }

        // Asked after the stop condition, so a run that was going to end anyway
        // ends for the reason it was going to end for rather than being
        // condemned on its last tick.
        if let Some(reason) = spending.check(collectors, &mut events, &mut losses) {
            break reason;
        }

        // Sleep for what is left of the interval, and count the tick as late
        // when the reads themselves took longer than the interval: at that
        // point the series no longer has the spacing it claims.
        let spent_ns = monotonic_ns().saturating_sub(tick_started_ns);
        match window
            .interval
            .checked_sub(StdDuration::from_nanos(spent_ns))
        {
            Some(remaining) => std::thread::sleep(remaining),
            None => {
                for losses in losses.iter_mut() {
                    losses.late_ticks += 1;
                    losses.note(format!(
                        "a round of reads took {spent_ns}ns, longer than the {}ns tick",
                        window.interval.as_nanos()
                    ));
                }
            }
        }
    };

    (events, losses, stopped_because)
}

/// Evaluates the request's filter over what was collected.
///
/// This is the other half of filter placement. The resolver decides where a
/// predicate runs and says so in the resolved file; for a backend that returns
/// whole records, "userspace" means here, after the event has been paid for. A
/// backend that was credited with pushing the predicate down is taken at its
/// word and its events are left alone -- running the filter twice would hide
/// the bug if it had not.
fn apply_filter(resolved: &ResolvedRequest, events: &mut Vec<Event>) -> Option<Filtered> {
    let predicate = resolved.request.filter.as_ref()?;
    if !matches!(resolved.filter, Some(FilterPlacement::Userspace(_))) {
        return None;
    }
    let before = events.len();
    let unseen_fields = predicate
        .fields()
        .into_iter()
        .filter(|field| *field != "kind" && !events.iter().any(|event| event.attr(field).is_some()))
        .map(str::to_owned)
        .collect();
    events.retain(|event| crate::filter::matches(predicate, event));
    Some(Filtered {
        predicate: predicate.to_string(),
        kept: events.len() as u64,
        dropped: (before - events.len()) as u64,
        unseen_fields,
    })
}

/// Collects what every collector needs in the launched program's environment,
/// refusing a disagreement rather than letting one win silently.
fn merge_environments(
    prepared: &[Box<dyn Prepared>],
) -> Result<Vec<(String, String)>, CollectError> {
    let mut merged: Vec<(String, String)> = Vec::new();
    for prepared in prepared {
        for (name, value) in prepared.launch_environment() {
            match merged.iter().find(|(existing, _)| *existing == name) {
                Some((_, existing)) if *existing != value => {
                    return Err(CollectError::ConflictingEnvironment {
                        variable: name,
                        first: existing.clone(),
                        second: value,
                    })
                }
                Some(_) => {}
                None => merged.push((name, value)),
            }
        }
    }
    Ok(merged)
}

/// Whether this build can actually collect a source through a backend.
///
/// The resolver asks before choosing. A machine that gains a privilege gains
/// backends, and without this a source that worked five minutes ago stops
/// working because something better became reachable that nothing implements --
/// which is an absurd way for a tool to react to being given more access.
pub fn implements(source_id: &str, backend: &str) -> bool {
    registry()
        .iter()
        .any(|factory| factory.source == source_id && factory.backend == backend)
}

/// The sources a collector exists for, for error messages that say what *is*
/// possible rather than only what is not.
///
/// Derived from the registry, because a hand-written second list had already
/// drifted out of agreement with the first one by the time it was needed.
pub fn implemented() -> Vec<String> {
    coverage()
        .into_iter()
        .map(|(source, backends)| format!("{source} ({})", backends.join(", ")))
        .collect()
}

/// Which backends this build can actually collect, by source.
///
/// The catalog says what the vocabulary is; this says how much of it has been
/// written. The two are deliberately separate -- a source with no collector is
/// still a valid request, and `--no-resolve` will still write it down for a
/// machine and a build that can serve it -- but a reader looking at the catalog
/// deserves to see which is which without running into it.
pub fn coverage() -> BTreeMap<&'static str, Vec<&'static str>> {
    let mut by_source: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for factory in registry() {
        by_source
            .entry(factory.source)
            .or_default()
            .push(factory.backend);
    }
    by_source
}

/// Finds the collector for a resolved backend.
///
/// The lookup is on the backend id, not on the source alone: two backends of the
/// same source read different things and produce differently trustworthy
/// numbers.
fn prepare(resolved: &ResolvedRequest) -> Result<Box<dyn Prepared>, CollectError> {
    let source_id = resolved.request.source.as_str();
    let backend = resolved.backend.id.as_str();
    let factory = registry()
        .into_iter()
        .find(|factory| factory.source == source_id && factory.backend == backend)
        .ok_or_else(|| CollectError::NoCollector {
            source_id: source_id.to_owned(),
            backend: backend.to_owned(),
        })?;
    (factory.build)(resolved).map_err(|reason| CollectError::Start {
        source_id: source_id.to_owned(),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    /// Every registered collector must name a real catalog entry and a real
    /// backend in its chain. A typo here would surface as "no collector is
    /// written for this" at run time, which reads like an unimplemented source
    /// rather than a mistake.
    #[test]
    fn every_registered_collector_names_a_real_catalog_backend() {
        let catalog = Catalog::embedded().unwrap();
        for factory in registry() {
            let entry = catalog
                .get(&factory.source.parse().unwrap())
                .unwrap_or_else(|| panic!("`{}` is not in the catalog", factory.source));
            assert!(
                entry.backend(factory.backend).is_some(),
                "`{}` has no backend `{}`",
                factory.source,
                factory.backend
            );
        }
    }

    #[test]
    fn no_two_collectors_claim_the_same_source_and_backend() {
        let mut seen = std::collections::BTreeSet::new();
        for factory in registry() {
            assert!(
                seen.insert((factory.source, factory.backend)),
                "two collectors claim `{}` on `{}`",
                factory.source,
                factory.backend
            );
        }
    }

    /// Two collectors that disagree about the launched program's environment
    /// cannot both be satisfied, and picking a winner silently would mean one
    /// of them measuring something other than what it was configured for.
    #[test]
    fn a_disagreement_about_the_child_environment_is_refused_rather_than_resolved() {
        struct Wants(&'static str, &'static str);
        impl Prepared for Wants {
            fn launch_environment(&self) -> Vec<(String, String)> {
                vec![(self.0.to_owned(), self.1.to_owned())]
            }
            fn attach(self: Box<Self>, _: &Target) -> Result<Box<dyn Collector>, String> {
                unreachable!("this test never attaches")
            }
        }

        // Agreeing on the same value is not a disagreement.
        let agreeing: Vec<Box<dyn Prepared>> = vec![
            Box::new(Wants("LD_PRELOAD", "/lib/a.so")),
            Box::new(Wants("LD_PRELOAD", "/lib/a.so")),
        ];
        let merged = merge_environments(&agreeing).expect("identical values agree");
        assert_eq!(merged.len(), 1, "the variable is set once, not twice");

        let conflicting: Vec<Box<dyn Prepared>> = vec![
            Box::new(Wants("LD_PRELOAD", "/lib/a.so")),
            Box::new(Wants("LD_PRELOAD", "/lib/b.so")),
        ];
        match merge_environments(&conflicting) {
            Err(CollectError::ConflictingEnvironment { variable, .. }) => {
                assert_eq!(variable, "LD_PRELOAD");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// One run observes one target. Running two targets in sequence would
    /// measure two different moments and present them as one window, and a join
    /// across that would correlate a program with something that was not
    /// happening at the same time.
    #[test]
    fn a_session_asking_for_two_targets_is_refused_before_anything_starts() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let own = std::process::id() as i32;

        let request = |target| crate::request::Request {
            source: "mem.rss".parse().unwrap(),
            target,
            mode: crate::request::Mode::Poll {
                interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
            },
            emit: vec![crate::request::Emit::Summary],
            keys: Vec::new(),
            filter: None,
            duration: Some("50ms".parse().unwrap()),
            budget: None,
            stack: None,
            backend: None,
            label: None,
        };
        let resolved: Vec<_> = [Target::Pid(vec![own]), Target::Pid(vec![1])]
            .into_iter()
            .map(|target| {
                crate::resolve::resolve(&catalog, &capabilities, &request(target))
                    .expect("resolves")
            })
            .collect();

        match run_all(&resolved) {
            Err(CollectError::MixedTargets { .. }) => {}
            other => panic!(
                "expected a refusal, got {:?}",
                other.map(|_| "a collection")
            ),
        }
    }

    /// Builds a resolved request for `mem.rss`, optionally pinned to a backend
    /// so the filter placement can be steered: `fs:proc` returns whole records
    /// and gets a userspace placement, an eBPF backend is credited with pushing
    /// the predicate down.
    fn resolved_with_filter(source: &str, backend: Option<&str>, filter: &str) -> ResolvedRequest {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let entry = catalog.get(&source.parse().unwrap()).unwrap();
        let request = crate::request::Request {
            source: source.parse().unwrap(),
            target: Target::Pid(vec![std::process::id() as i32]),
            mode: if entry.modes.contains(&crate::request::ModeKind::Poll) {
                crate::request::Mode::Poll {
                    interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
                }
            } else {
                crate::request::Mode::Trace
            },
            emit: vec![crate::request::Emit::Raw],
            keys: Vec::new(),
            filter: Some(filter.parse().unwrap()),
            duration: None,
            budget: None,
            stack: None,
            backend: backend.map(|id| id.parse().unwrap()),
            label: None,
        };
        crate::resolve::resolve(&catalog, &capabilities, &request).expect("resolves")
    }

    fn event_with(attrs: Vec<crate::event::Attr>) -> Event {
        Event {
            ts_ns: 1,
            ts_err_ns: 0,
            kind: "mem.rss".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: CollectorId::new("test"),
            seq: 0,
        }
    }

    /// The filter has to remove what it does not match, and the count has to
    /// survive: a series of three events that was a series of forty before the
    /// predicate ran describes a different run.
    #[test]
    fn a_userspace_filter_removes_what_it_does_not_match_and_counts_it() {
        let resolved = resolved_with_filter("mem.rss", Some("fs:proc:statm"), "comm == 'keep'");
        let mut events = vec![
            event_with(vec![crate::event::Attr::text("comm", "keep")]),
            event_with(vec![crate::event::Attr::text("comm", "drop")]),
            event_with(vec![crate::event::Attr::text("comm", "keep")]),
        ];

        let filtered = apply_filter(&resolved, &mut events).expect("a filtered request reports it");
        assert_eq!(events.len(), 2);
        assert_eq!((filtered.kept, filtered.dropped), (2, 1));
        assert_eq!(filtered.predicate, "comm == 'keep'");
        assert!(filtered.unseen_fields.is_empty(), "{filtered:?}");
    }

    /// A backend the resolver credited with pushing the predicate into the
    /// kernel is taken at its word. Running the filter again here would produce
    /// the same events either way, which is exactly why it must not happen: it
    /// would hide a collector that ignored the predicate it was given.
    #[test]
    fn a_filter_the_backend_pushed_down_is_not_run_a_second_time() {
        let resolved = resolved_with_filter("io.latency", Some("bpf:tp"), "bytes > 4096");
        assert!(
            matches!(
                resolved.filter,
                Some(crate::resolve::FilterPlacement::Kernel(_))
            ),
            "this test needs a backend credited with pushing the filter down"
        );

        let mut events = vec![event_with(vec![crate::event::Attr::uint("bytes", 1)])];
        assert!(apply_filter(&resolved, &mut events).is_none());
        assert_eq!(events.len(), 1, "userspace must not re-filter kernel work");
    }

    /// A predicate over a field nothing produces is false everywhere, which
    /// empties the series and looks like a workload that did nothing. The field
    /// is named so the reader can tell the two apart.
    #[test]
    fn a_predicate_over_a_field_no_event_carries_says_which_field() {
        let resolved = resolved_with_filter("mem.rss", Some("fs:proc:statm"), "comm == 'anything'");
        let mut events = vec![event_with(vec![crate::event::Attr::uint("bytes", 4096)])];

        let filtered = apply_filter(&resolved, &mut events).expect("a filtered request reports it");
        assert!(events.is_empty());
        assert_eq!(filtered.unseen_fields, vec!["comm".to_owned()]);
    }

    /// `kind` is on every event by construction, so it is never a missing
    /// field even when nothing matches it.
    #[test]
    fn the_kind_field_is_never_reported_as_one_no_event_carried() {
        let resolved =
            resolved_with_filter("mem.rss", Some("fs:proc:statm"), "kind == 'io.latency'");
        let mut events = vec![event_with(Vec::new())];

        let filtered = apply_filter(&resolved, &mut events).expect("a filtered request reports it");
        assert!(events.is_empty());
        assert!(filtered.unseen_fields.is_empty(), "{filtered:?}");
    }

    /// A request with no filter reports nothing rather than reporting that a
    /// filter kept everything: there was no predicate to keep anything.
    #[test]
    fn a_request_without_a_filter_reports_no_filtering() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let request = crate::request::Request {
            source: "mem.rss".parse().unwrap(),
            target: Target::Pid(vec![std::process::id() as i32]),
            mode: crate::request::Mode::Snapshot,
            emit: vec![crate::request::Emit::Raw],
            keys: Vec::new(),
            filter: None,
            duration: None,
            budget: None,
            stack: None,
            backend: None,
            label: None,
        };
        let resolved = crate::resolve::resolve(&catalog, &capabilities, &request).unwrap();
        let mut events = vec![event_with(Vec::new())];
        assert!(apply_filter(&resolved, &mut events).is_none());
        assert_eq!(events.len(), 1);
    }

    /// A system target has no program to end the run, so a duration is the only
    /// thing that can. Starting collectors that nothing will ever stop is worse
    /// than refusing.
    #[test]
    fn a_system_run_with_nothing_to_stop_it_is_refused() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let request = crate::request::Request {
            source: "gpu.temp".parse().unwrap(),
            target: Target::System,
            mode: crate::request::Mode::Poll {
                interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
            },
            emit: vec![crate::request::Emit::Raw],
            keys: Vec::new(),
            filter: None,
            duration: None,
            budget: None,
            stack: None,
            backend: None,
            label: None,
        };
        let resolved = crate::resolve::resolve(&catalog, &capabilities, &request).unwrap();
        match run(&resolved) {
            Err(CollectError::NoStopCondition) => {}
            // A machine with no GPU cannot get that far, and says so first.
            Err(CollectError::Start { reason, .. }) => {
                eprintln!("skipped: {reason}");
            }
            other => panic!(
                "expected a refusal, got {:?}",
                other.map(|_| "a collection")
            ),
        }
    }

    #[test]
    fn losses_merge_without_losing_their_reasons() {
        let mut first = Losses {
            failed_reads: 1,
            ..Losses::default()
        };
        first.note("a");
        let mut second = Losses {
            dropped: 5,
            ..Losses::default()
        };
        second.throttle("a");
        second.note("b");

        first.absorb(second);
        assert_eq!((first.dropped, first.failed_reads), (5, 1));
        assert_eq!(
            first.throttled, 1,
            "a throttled period must survive being merged, like every other loss"
        );
        assert_eq!(first.reasons.get("a"), Some(&2));
        assert_eq!(first.reasons.get("b"), Some(&1));
        assert!(!first.is_clean());
    }

    /// A throttled period is a loss whose size nobody knows.
    ///
    /// The kernel stops delivering for a while; how many events happened in
    /// that while is not recorded anywhere. Adding one to `dropped` -- which
    /// is what `sched.switches` did -- says "one event was lost", which is a
    /// number, and the honest answer is that there is no number. So it is
    /// counted apart, and a run that hit it is not clean.
    #[test]
    fn a_throttled_period_is_counted_apart_from_the_events_it_did_not_lose() {
        let mut losses = Losses::default();
        assert!(losses.is_clean(), "nothing has gone wrong yet");

        losses.throttle("the kernel throttled this event: see kernel.perf_event_max_sample_rate");

        assert_eq!(losses.throttled, 1, "one period, not one event");
        assert_eq!(
            losses.dropped, 0,
            "a throttle is not a drop: folding it in would claim a count nobody has"
        );
        assert!(
            !losses.is_clean(),
            "a run the kernel throttled is not a complete run, and must not read as one"
        );
        assert!(
            losses
                .reasons
                .keys()
                .any(|reason| reason.contains("perf_event_max_sample_rate")),
            "the reader has to be told what to raise: {:?}",
            losses.reasons
        );
    }

    /// Cpu time this thread has been charged, through the one interface that
    /// answers for a thread rather than a process.
    ///
    /// It is deliberately not `getrusage`: this is the independent witness the
    /// cost accounting is checked against, and a witness that shares the caller
    /// is not one. Wall time will not do either -- a spin of 200 ms of wall
    /// time is 200 ms of cpu only on an idle machine, and `cargo test` runs
    /// seven other threads flat out.
    fn thread_cpu_ns() -> u64 {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `clock_gettime` writes a `timespec` this function owns.
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
        (time.tv_sec as u64) * 1_000_000_000 + (time.tv_nsec as u64)
    }

    /// A collector that burns a known amount of cpu, and says how much.
    ///
    /// The quantity injected is cpu time rather than wall time, so what it
    /// spends does not depend on how contended the machine is; the loop runs
    /// until the thread has been charged what it was asked for.
    struct Burning {
        id: CollectorId,
        spin_ns: u64,
        spun_ns: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl Collector for Burning {
        fn id(&self) -> &CollectorId {
            &self.id
        }

        fn value_attr(&self) -> &'static str {
            "bytes"
        }

        fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
            let started_ns = thread_cpu_ns();
            while thread_cpu_ns().saturating_sub(started_ns) < self.spin_ns {}
            self.spun_ns
                .set(self.spun_ns.get() + thread_cpu_ns().saturating_sub(started_ns));
            Ok(Vec::new())
        }
    }

    fn burning(spin_ns: u64) -> (Vec<Box<dyn Collector>>, std::rc::Rc<std::cell::Cell<u64>>) {
        let spun_ns = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let collectors: Vec<Box<dyn Collector>> = vec![Box::new(Burning {
            id: CollectorId::new("burning"),
            spin_ns,
            spun_ns: spun_ns.clone(),
        })];
        (collectors, spun_ns)
    }

    fn until(deadline_ns: u64) -> impl FnMut() -> Option<String> {
        move || (monotonic_ns() >= deadline_ns).then(|| "the deadline".to_owned())
    }

    /// What a run says it spent is the cpu it actually burned.
    ///
    /// Ground truth is a known quantity of work injected into the collector and
    /// timed by a different clock: 200 ms of spinning, which cannot take less
    /// than 200 ms of cpu and, with nothing else in the process, should not take
    /// much more.
    ///
    /// It runs in a forked child because `getrusage(RUSAGE_SELF)` answers for
    /// the whole process and `cargo test` runs its tests in parallel threads of
    /// one. Measured from this thread, the number would be this test's work plus
    /// whatever seven others happened to be doing, which is the right answer to
    /// a different question. The child is single-threaded, so the two accounts
    /// describe the same work.
    #[test]
    fn what_a_run_reports_spending_is_the_cpu_it_actually_burned() {
        const AGREED: i32 = 0;
        const UNDER_REPORTED: i32 = 1;
        const OVER_REPORTED: i32 = 2;
        const NOTHING_WAS_BURNED: i32 = 3;

        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            let (mut collectors, spun_ns) = burning(20_000_000);
            let mut spending = Spending::unweighed(1);
            let deadline_ns = monotonic_ns() + 200_000_000;
            drive(
                &mut collectors,
                Window {
                    interval: StdDuration::ZERO,
                    snapshot: false,
                },
                &mut spending,
                until(deadline_ns),
            );
            let reported_ns = spending.close().remove(0).collector_cpu_ns as f64;
            let spun_ns = spun_ns.get() as f64;
            let code = if spun_ns < 150_000_000.0 {
                NOTHING_WAS_BURNED
            } else if reported_ns < spun_ns * 0.8 {
                UNDER_REPORTED
            } else if reported_ns > spun_ns * 1.5 {
                OVER_REPORTED
            } else {
                AGREED
            };
            unsafe { libc::_exit(code) };
        }

        let mut status = 0;
        assert!(
            unsafe { libc::waitpid(child, &mut status, 0) } == child,
            "waiting for the child"
        );
        assert!(libc::WIFEXITED(status), "the child did not exit normally");
        match libc::WEXITSTATUS(status) {
            AGREED => {}
            NOTHING_WAS_BURNED => panic!(
                "the collector was asked to spin for 200 ms and did not, so this measured nothing"
            ),
            UNDER_REPORTED => panic!(
                "the run reported spending less cpu than the collector provably burned: a budget \
                 built on that would let a collector cost what it likes"
            ),
            OVER_REPORTED => panic!(
                "the run reported spending far more cpu than the collector burned, so something \
                 other than the measurement is being charged to it"
            ),
            other => panic!("the child exited with {other}"),
        }
    }

    /// A `mem.rss` request over this process, with a ceiling.
    fn weighed(budget: &str) -> ResolvedRequest {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let request = crate::request::Request {
            source: "mem.rss".parse().unwrap(),
            target: Target::Pid(vec![std::process::id() as i32]),
            mode: crate::request::Mode::Poll {
                interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
            },
            emit: vec![crate::request::Emit::Summary],
            keys: Vec::new(),
            filter: None,
            duration: Some("1s".parse().unwrap()),
            budget: Some(budget.parse().unwrap()),
            stack: None,
            backend: None,
            label: None,
        };
        crate::resolve::resolve(&catalog, &capabilities, &request).expect("resolves")
    }

    /// A run that costs more than it was allowed, with nowhere cheaper to go,
    /// ends -- and the reason names the ceiling rather than the clock.
    ///
    /// Both sides are checked, because a test that only shows a run stopping
    /// would pass just as well against a runner that stops every run. The two
    /// differ in the ceiling and in nothing else: the collector burns the same
    /// cpu, over the same window, against the same denominator.
    ///
    /// That denominator is this process's own cpu time, so the ratio is around
    /// one hundred percent whatever else the machine is doing -- which is what
    /// puts 50 % reliably below it and 500 % reliably above.
    #[test]
    fn a_run_over_its_ceiling_with_nowhere_cheaper_to_go_stops_and_says_which() {
        let stopped_because = |ceiling: &str| {
            let (mut collectors, spun_ns) = burning(20_000_000);
            let mut spending = Spending::weighed_against_this_process(vec![weighed(ceiling)]);
            let deadline_ns = monotonic_ns() + 600_000_000;
            let (_, _, stopped_because) = drive(
                &mut collectors,
                Window {
                    interval: StdDuration::ZERO,
                    snapshot: false,
                },
                &mut spending,
                until(deadline_ns),
            );
            // Enough to have been weighed at least once: the run under the
            // tighter ceiling ends early by design, so this is the floor both
            // sides share rather than the length of the window.
            // Enough to have been weighed at least once. The run under the
            // tighter ceiling ends about a hundred milliseconds in by design,
            // so this is the floor the two sides share rather than the length
            // of the window.
            assert!(
                spun_ns.get() > 20_000_000,
                "the collector has to cost something before a ceiling can mean anything, and it \
                 only spun for {}ns",
                spun_ns.get()
            );
            stopped_because
        };

        let over = stopped_because("50%");
        assert!(
            over.contains("ceiling of 50") && over.contains("nothing cheaper"),
            "a run the budget ended has to say so, and say what it would have degraded onto: \
             {over}"
        );

        let under = stopped_because("500%");
        assert_eq!(
            under, "the deadline",
            "a run inside its ceiling must end for the reason it was going to end for"
        );
    }

    /// The ceiling is checked often enough to act and rarely enough not to
    /// become the measurement.
    ///
    /// A budget that polled `getrusage` every tick would answer mostly about
    /// itself, which is the trap this whole stream is built against. The run
    /// below lasts 600 ms, so a hundred-millisecond cadence is about six reads;
    /// what those reads cost is reported rather than hidden, and has to stay a
    /// vanishing fraction of the window.
    #[test]
    fn weighing_a_run_costs_a_fraction_of_the_run() {
        let (mut collectors, _) = burning(5_000_000);
        let mut spending = Spending::weighed_against_this_process(vec![weighed("500%")]);
        let deadline_ns = monotonic_ns() + 600_000_000;
        drive(
            &mut collectors,
            Window {
                interval: StdDuration::ZERO,
                snapshot: false,
            },
            &mut spending,
            until(deadline_ns),
        );
        let overhead = spending.close().remove(0);

        assert!(
            (4..=12).contains(&overhead.measurements),
            "a 600 ms run at a 100 ms cadence is about six weighings, not {}",
            overhead.measurements
        );
        assert!(
            overhead.measurement_ns * 100 < overhead.wall_ns,
            "weighing the run cost {}ns of a {}ns window, which is enough to change the answer \
             it was asked for",
            overhead.measurement_ns,
            overhead.wall_ns
        );
    }

    /// A run with no ceiling is never interrupted to be weighed.
    ///
    /// The two `getrusage` calls at either end are the whole price of knowing
    /// what a run cost, and that is the reason every run reports it rather than
    /// only the ones that asked.
    #[test]
    fn a_run_with_no_ceiling_is_weighed_twice_and_no_more() {
        let (mut collectors, _) = burning(5_000_000);
        let mut spending = Spending::unweighed(1);
        let deadline_ns = monotonic_ns() + 400_000_000;
        drive(
            &mut collectors,
            Window {
                interval: StdDuration::ZERO,
                snapshot: false,
            },
            &mut spending,
            until(deadline_ns),
        );
        let overhead = spending.close().remove(0);
        assert_eq!(
            overhead.measurements, 1,
            "an unbudgeted run reads the clock once at the end; the other read is at `open`, \
             before there is anything to report"
        );
        assert!(
            overhead.wall_ns >= 400_000_000,
            "the window has to be the window: {}",
            overhead.wall_ns
        );
    }

    /// The cpu time of a target that keeps no cpu account is absent, not zero.
    ///
    /// Zero would make every collector infinitely expensive against an
    /// interface and infinitely cheap against a missing one; absent is the
    /// answer, with what would have been read had there been anything.
    #[test]
    fn a_target_with_no_cpu_account_reports_that_rather_than_zero() {
        let meter = Meter::of(&Target::Iface("lo".to_owned()), None);
        assert!(meter.read().is_none());
        assert!(
            meter.describes().contains("no cpu time"),
            "{}",
            meter.describes()
        );

        // And one that does keep an account answers with it, and the answer
        // follows the work: a meter that returned a constant would satisfy
        // every other check here and make every overhead ratio meaningless.
        let own = std::process::id() as i32;
        let mine = Meter::of(&Target::Pid(vec![own]), None);
        let before_ns = mine.read().expect("this process has a cpu account");
        let until_ns = monotonic_ns() + 80_000_000;
        while monotonic_ns() < until_ns {}
        let after_ns = mine.read().expect("and still has one");
        assert!(
            after_ns >= before_ns + 50_000_000,
            "80 ms of spinning moved the meter from {before_ns}ns to {after_ns}ns, which is less \
             than the work it was given"
        );
    }

    /// `/proc/<pid>/stat` agrees with `getrusage` about this process.
    ///
    /// The two are read through completely different interfaces, and the
    /// overhead ratio divides one by the other: a run's own cost comes from
    /// `getrusage` and its target's from `/proc`. If they disagreed about the
    /// same process, every percentage this stream produces would be wrong by
    /// whatever the disagreement was.
    ///
    /// `/proc` counts `cutime` and `cstime` as well, because a shell that forks
    /// is still the workload that was asked for -- so the account to compare it
    /// against is `RUSAGE_SELF` *plus* `RUSAGE_CHILDREN`. Getting that pairing
    /// wrong is exactly the kind of mistake that shows up as a plausible number
    /// nobody can explain, and this suite forks enough children for the two to
    /// come apart if it were wrong.
    ///
    /// The `/proc` reads sandwich the `getrusage` one because the process does
    /// not stop working in between: `cargo test` has seven other threads
    /// burning cpu, so two readings taken a millisecond apart legitimately
    /// differ. What cannot happen is the middle reading falling outside the two
    /// around it.
    #[test]
    fn the_two_cpu_accounts_agree_about_the_same_process() {
        let own = std::process::id() as i32;
        let before_ns = task_cpu_ns(own).expect("this process has a /proc entry");
        let rusage_ns = rusage_ns(libc::RUSAGE_SELF) + rusage_ns(libc::RUSAGE_CHILDREN);
        let after_ns = task_cpu_ns(own).expect("and still has one");
        let tick_ns = 1_000_000_000 / ticks_per_second();

        // `/proc` truncates each of its four fields down to a whole tick, so it
        // can read up to four ticks short of the same instant however many of
        // them are non-zero -- which is the slack here, not measurement error.
        let slack_ns = 5 * tick_ns;
        assert!(
            rusage_ns + tick_ns >= before_ns && rusage_ns <= after_ns + slack_ns,
            "getrusage says {rusage_ns}ns, which is outside the {before_ns}ns..{after_ns}ns that \
             /proc reported either side of it by more than the {slack_ns}ns four truncated \
             {tick_ns}ns ticks can account for"
        );
    }

    /// A collector that records nothing but the clock at every call.
    struct Clocked {
        id: CollectorId,
        reads_ns: std::rc::Rc<std::cell::RefCell<Vec<u64>>>,
        stopped_ns: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl Collector for Clocked {
        fn id(&self) -> &CollectorId {
            &self.id
        }

        fn value_attr(&self) -> &'static str {
            "bytes"
        }

        fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
            self.reads_ns.borrow_mut().push(monotonic_ns());
            Ok(Vec::new())
        }

        fn stop(&mut self) -> Vec<Event> {
            self.stopped_ns.set(monotonic_ns());
            Vec::new()
        }
    }

    /// The runner closes the window, so a collector must not close it again.
    ///
    /// This is the measurement behind that decision rather than an assertion
    /// of it. A round of reads happens *before* the run asks whether it is
    /// over, so the last round lands at the end of the window: here the
    /// deadline falls at 90 ms between two 20 ms ticks, and the last read is
    /// the one at 100 ms. What is left for a closing read in `stop` to cover
    /// is the handful of microseconds after it -- and a point covering that,
    /// with a counter delta of zero, is what dragged `io`'s summaries and is
    /// what `net/dev.rs` still produces.
    #[test]
    fn the_runner_closes_the_window() {
        let reads_ns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let stopped_ns = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let mut collectors: Vec<Box<dyn Collector>> = vec![Box::new(Clocked {
            id: CollectorId::new("clocked"),
            reads_ns: reads_ns.clone(),
            stopped_ns: stopped_ns.clone(),
        })];

        let interval = StdDuration::from_millis(20);
        let deadline_ns = monotonic_ns() + 90_000_000;
        let (_, _, stopped_because) = drive(
            &mut collectors,
            Window {
                interval,
                snapshot: false,
            },
            &mut Spending::unweighed(1),
            || (monotonic_ns() >= deadline_ns).then(|| "the deadline".to_owned()),
        );
        // The runner calls `stop` right after the loop; this stands in for it.
        for collector in collectors.iter_mut() {
            collector.stop();
        }

        assert_eq!(stopped_because, "the deadline");
        let reads_ns = reads_ns.borrow();
        assert!(
            reads_ns.len() >= 5,
            "a 90 ms window at a 20 ms tick is at least five reads, got {}",
            reads_ns.len()
        );

        let last_read_ns = *reads_ns.last().expect("at least one read");
        assert!(
            last_read_ns >= deadline_ns,
            "the last read must land at the end of the window, not one tick short of it: \
             it came {} ns early",
            deadline_ns - last_read_ns
        );

        let left_over_ns = stopped_ns.get().saturating_sub(last_read_ns);
        assert!(
            left_over_ns * 10 < interval.as_nanos() as u64,
            "a closing read in `stop` would cover {left_over_ns} ns of a {} ns tick, which is \
             not an interval: it is a second point at the same offset",
            interval.as_nanos()
        );
    }
}
