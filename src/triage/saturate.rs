//! `omn saturate`: USE, with Little's law as its arithmetic.
//!
//! Utilization, saturation and errors per resource, from sources that already
//! collect. The arithmetic is stated rather than left for each source to imply:
//! **L = λW**. A traced source hands up every wait with its length; summed, the
//! lengths over the window are the mean number of waiters (`L = ΣW / T`), which
//! is also the arrival rate times the mean wait (`λ = n / T`, `W = ΣW / n`).
//! That identity is what turns a run queue latency into a run queue length, a
//! block device's latencies into its occupancy, and a mutex's hold times into
//! its utilization -- and it is exact over the window, not an estimate, because
//! every term is a sum the kernel or the shim handed up.
//!
//! Per resource, from the workload's side:
//!
//! | resource | utilization | saturation | errors |
//! |---|---|---|---|
//! | cpu | cpus busy with it: cpu time / T | tasks waiting for a cpu: Σ run queue wait / T | not per process: `cpu.throttle` is per cgroup |
//! | disk | requests in flight at the device: Σ latency / T | tasks blocked on it: Σ uninterruptible / T; the device's own queue depth beside it | requests completed with an error status |
//! | lock | time a mutex was held: Σ hold / T, at most 1 | threads waiting for it: Σ wait / T | -- |
//! | memory | -- | the cgroup's memory stall share (PSI); without PSI, tasks in direct reclaim: Σ reclaim / T | -- |
//!
//! The objective decides which column the verdict reads:
//!
//! - **throughput** names the resource with the longest queue of the workload's
//!   own work *waiting for something another task holds* -- a cpu, a lock. When
//!   no queue reaches a tenth of a waiter on average, nothing is saturated and
//!   the answer says so: the work is limited by its own concurrency, which is
//!   the answer for a program that waits on one synchronous write at a time.
//! - **latency** is refused, and names `omn latency`: a latency is one task's
//!   wall clock, and a resource walk adds up every task's -- a lock one thread
//!   waits on and a cpu another thread spins on are both busy, and the sum
//!   names neither.
//! - **tail** names the resource whose individual waits are the longest at
//!   their 99th percentile, read off each source's histogram with the
//!   histogram's `Shortfall` carried -- a percentile from a lossy run is a
//!   lower bound, and finding 37 exists for exactly this reader.
//! - **energy** reads the energy counters and credits the workload with the
//!   share of the machine's cpu time it held, which is the only part of a
//!   package meter a process can be credited with. Disks and networks are not
//!   metered here, and the answer says so rather than calling them free.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::capabilities::Capabilities;
use crate::catalog::Catalog;
use crate::emit::{Output, Results, SeriesHist, Shortfall};
use crate::event::Event;
use crate::join::Evidence;
use crate::request::{KeyKind, Request, Session, SourceId, Target};
use crate::scalar::Duration;
use crate::stack::unwind::SymbolSource;

use super::engine::{self, Dropped, Run};
use super::latency::notes;
use super::objective::{Method, Objective, Refusal};
use super::{span, TriageError};

pub const CPU_WAIT: &str = "cpu-wait";
pub const CPU_TIME: &str = "cpu-time";
pub const DISK: &str = "disk";
pub const DISK_BLOCKED: &str = "disk-blocked";
pub const DEVICE_QUEUE: &str = "device-queue";
pub const LOCK: &str = "lock";
pub const FUTEX: &str = "futex";
pub const MEMORY: &str = "memory";
pub const RECLAIM: &str = "memory-reclaim";
pub const ENERGY: &str = "energy";
pub const MACHINE_CPU: &str = "machine-cpu";

/// Every request this mode can do without. The run queue and the disk are the
/// two it cannot: without them there is no cpu and no disk row at all.
pub const OPTIONAL: [&str; 8] = [
    CPU_TIME,
    DEVICE_QUEUE,
    LOCK,
    FUTEX,
    MEMORY,
    ENERGY,
    MACHINE_CPU,
    DISK_BLOCKED,
];

/// Below this mean queue length nothing is saturated: a tenth of a waiter on
/// average is one task waiting a tenth of the time.
pub const SATURATED: f64 = 0.1;

/// What `omn saturate` was asked.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    pub objective: Objective,
    pub target: Target,
    pub duration: Option<Duration>,
    /// `--symbols`, set on every request whose collector reads it.
    pub symbols: Option<SymbolSource>,
}

impl Ask {
    pub fn check(&self) -> Result<(), Refusal> {
        Method::Saturate.require(Some(self.objective))?;
        match &self.target {
            Target::Launch(argv) if !argv.is_empty() => Ok(()),
            Target::Pid(pids) if pids.len() == 1 => match self.duration {
                Some(_) => Ok(()),
                None => Err(Refusal(
                    "`omn saturate --pid` needs --duration: a process already running has no \
                     exit to end the run on"
                        .to_owned(),
                )),
            },
            other => Err(Refusal(format!(
                "`omn saturate` answers for one workload -- `-- PROGRAM` or `--pid PID` -- and \
                 was given {other}"
            ))),
        }
    }

    fn launched(&self) -> bool {
        matches!(self.target, Target::Launch(_))
    }

    /// The session this mode asks the engine for.
    pub fn session(&self) -> Session {
        let request = |label: &str, source: &str, target: &Target, mode: &str, emit: &str| {
            let keyed =
                !matches!(target, Target::System) && !matches!(source, "mem.pressure" | "cpu.util");
            Request {
                source: source.parse::<SourceId>().expect("a catalog source"),
                target: target.clone(),
                mode: mode.parse().expect("a mode"),
                emit: vec![emit.parse().expect("an emit")],
                keys: if keyed {
                    vec![KeyKind::Task]
                } else {
                    Vec::new()
                },
                filter: None,
                duration: self.duration,
                budget: None,
                stack: None,
                symbols: None,
                backend: None,
                label: Some(label.to_owned()),
                window: None,
            }
        };
        let workload = &self.target;
        let mut requests = vec![
            request(CPU_WAIT, "sched.runqlat", workload, "trace", "hist"),
            request(DISK, "io.latency", workload, "trace", "hist"),
            request(DISK_BLOCKED, "sched.offcpu", workload, "trace", "summary"),
            request(
                DEVICE_QUEUE,
                "io.inflight",
                &Target::System,
                "poll@10ms",
                "summary",
            ),
            request(LOCK, "sched.mutex", workload, "trace", "hist"),
            request(FUTEX, "sched.futex", workload, "trace", "hist"),
            request(MEMORY, "mem.pressure", workload, "poll@100ms", "summary"),
            request(RECLAIM, "mem.reclaim", workload, "trace", "summary"),
        ];
        // A launched program's cpu time is exact at the reap; a process
        // already running has no reap, and is polled instead.
        if !self.launched() {
            requests.push(request(
                CPU_TIME,
                "cpu.oncpu_time",
                workload,
                "poll@100ms",
                "summary",
            ));
        }
        if self.objective == Objective::Energy {
            requests.push(request(
                ENERGY,
                "power.energy",
                &Target::System,
                "count",
                "summary",
            ));
            requests.push(request(
                MACHINE_CPU,
                "cpu.util",
                &Target::System,
                "poll@100ms",
                "summary",
            ));
        }
        super::diff::ask_symbols(&mut requests, self.symbols);
        Session {
            name: Some(format!("omn saturate --objective {}", self.objective)),
            duration: self.duration,
            budget: None,
            out: None,
            format: Default::default(),
            requests,
            joins: Vec::new(),
            decompose: Vec::new(),
            escalate: None,
            record: None,
            launch: Vec::new(),
        }
    }
}

/// A resource the USE method walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Cpu,
    Disk,
    Lock,
    Memory,
}

impl Resource {
    pub fn as_str(self) -> &'static str {
        match self {
            Resource::Cpu => "cpu",
            Resource::Disk => "disk",
            Resource::Lock => "lock",
            Resource::Memory => "memory",
        }
    }
}

/// One application of L = λW over the window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Little {
    /// What arrives and what it waits for: `wakeups waiting for a cpu`.
    pub what: String,
    pub source: String,
    /// n: how many arrived.
    pub arrivals: f64,
    /// ΣW, in nanoseconds.
    pub total_ns: f64,
    /// λ = n / T, per second.
    pub per_second: f64,
    /// W = ΣW / n.
    pub mean_ns: f64,
    /// L = λW = ΣW / T: how many were waiting, on average.
    pub mean_count: f64,
    /// The source's run was lossy, so every term is a lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
}

impl Little {
    fn of(
        what: &str,
        source: &str,
        arrivals: f64,
        total_ns: f64,
        window_ns: f64,
        short: Option<Shortfall>,
    ) -> Self {
        let seconds = window_ns / 1e9;
        Little {
            what: what.to_owned(),
            source: source.to_owned(),
            arrivals,
            total_ns,
            per_second: if seconds > 0.0 {
                arrivals / seconds
            } else {
                0.0
            },
            mean_ns: if arrivals > 0.0 {
                total_ns / arrivals
            } else {
                0.0
            },
            mean_count: if window_ns > 0.0 {
                total_ns / window_ns
            } else {
                0.0
            },
            short,
        }
    }

    fn line(&self) -> String {
        let mark = if self.short.is_some() {
            Shortfall::MARK
        } else {
            ""
        };
        format!(
            "{}: L = λW = {mark}{:.1}/s x {} = {mark}{:.3}  ({} of them over the window, {})",
            self.what,
            self.per_second,
            span(self.mean_ns),
            self.mean_count,
            self.arrivals.round(),
            self.source
        )
    }
}

/// A 99th percentile, read off a log2 histogram: known to its bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Percentile {
    pub what: String,
    pub source: String,
    /// The bucket the 99th percentile falls in, `[low, high)` nanoseconds.
    pub low: f64,
    pub high: f64,
    pub count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
}

impl Percentile {
    fn line(&self) -> String {
        let mark = if self.short.is_some() {
            Shortfall::MARK
        } else {
            ""
        };
        format!(
            "{}: p99 in [{mark}{}, {}) over {} ({})",
            self.what,
            span(self.low),
            span(self.high),
            self.count,
            self.source
        )
    }
}

/// One of the three USE numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub value: f64,
    /// What the value is a count of, or a fraction of.
    pub unit: String,
    pub says: String,
    pub source: String,
    /// Whose it is: the workload's, or the machine's or a cgroup's, which hold
    /// more than the workload.
    pub whose: String,
    pub evidence: Evidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
}

/// One resource, walked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Use {
    pub resource: Resource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization: Option<Reading>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saturation: Option<Reading>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Reading>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub little: Vec<Little>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tail: Vec<Percentile>,
    /// What could not be read, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread: Vec<String>,
}

impl Use {
    fn new(resource: Resource) -> Self {
        Use {
            resource,
            utilization: None,
            saturation: None,
            errors: None,
            little: Vec::new(),
            tail: Vec::new(),
            unread: Vec::new(),
        }
    }
}

/// The energy the machine metered over the window, and the workload's share.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Energy {
    /// Joules per metered domain, over the window.
    pub domains: BTreeMap<String, f64>,
    pub window_ns: f64,
    /// The workload's cpu time over the machine's busy cpu time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_share: Option<f64>,
    /// The package energy times that share: an estimate, credited by cpu time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credited_joules: Option<f64>,
    pub evidence: Evidence,
    pub unmetered: Vec<String>,
}

/// What the answer names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub objective: Objective,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<Resource>,
    pub sentence: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lower_bound: bool,
    /// Why no resource is named, when none is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undecided: Option<String>,
}

/// Everything `omn saturate` answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub method: Method,
    pub objective: Objective,
    pub target: String,
    /// T: the program's life for a launched one, the observed window
    /// otherwise.
    pub window_ns: f64,
    pub verdict: Verdict,
    pub resources: Vec<Use>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy: Option<Energy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<Dropped>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// What the collectors said they broke in the workload, each sentence
    /// once ([`engine::Ran::failures`]): the uses above are read off a
    /// workload the tool broke.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
    /// How the launched workload failed, when it did
    /// ([`engine::program_failure`]): the uses above are of a run that failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_failed: Option<String>,
}

pub fn answer(
    catalog: &Catalog,
    capabilities: &Capabilities,
    ask: &Ask,
) -> Result<Answer, TriageError> {
    ask.check()?;
    let mut session = ask.session();
    let dropped = engine::prune(catalog, capabilities, &mut session, &OPTIONAL)?;
    let ran = engine::run(catalog, capabilities, &session)?;
    let mut answer = read(ask, &ran.run, dropped);
    answer.failures = ran.failures;
    answer.program_failed = engine::program_failure(&ran.run);
    Ok(answer)
}

fn attr(event: &Event, name: &str) -> Option<f64> {
    event.attr(name).and_then(|value| value.as_f64())
}

fn short_of(results: &Results) -> Option<Shortfall> {
    Shortfall::of(&results.losses, results.events_seen as u64, false)
}

/// The window every L is over.
fn window_of(run: &Run) -> f64 {
    let lifetime = run
        .output
        .results
        .iter()
        .find_map(|results| results.lifetime.as_ref());
    match lifetime {
        Some(life) if life.exited_ns > life.released_ns => {
            (life.exited_ns - life.released_ns) as f64
        }
        _ => run
            .output
            .results
            .iter()
            .map(|results| results.observed_ns)
            .max()
            .unwrap_or(0) as f64,
    }
}

/// The 99th percentile of every series of a histogram, merged.
fn p99(results: &Results, what: &str) -> Option<Percentile> {
    let series: Vec<&SeriesHist> = results.outputs.iter().find_map(|output| match output {
        Output::Hist { series, .. } => Some(series.iter().collect()),
        _ => None,
    })?;
    let mut buckets: BTreeMap<u64, (u64, usize)> = BTreeMap::new();
    let mut short = None;
    for one in &series {
        for bucket in &one.buckets {
            let entry = buckets
                .entry(bucket.low)
                .or_insert((bucket.high.unwrap_or(u64::MAX), 0));
            entry.1 += bucket.count;
        }
        short = short.or_else(|| one.short.clone());
    }
    let count: usize = buckets.values().map(|(_, count)| count).sum();
    if count == 0 {
        return None;
    }
    let rank = (count as f64 * 0.99).ceil() as usize;
    let mut seen = 0;
    for (low, (high, n)) in &buckets {
        seen += n;
        if seen >= rank {
            return Some(Percentile {
                what: what.to_owned(),
                source: results.source.clone(),
                low: *low as f64,
                high: *high as f64,
                count,
                short,
            });
        }
    }
    None
}

/// Reads the run into the USE table and the verdict.
pub fn read(ask: &Ask, run: &Run, dropped: Vec<Dropped>) -> Answer {
    let window = window_of(run);
    let mut notes = notes(run);
    if let Some(symbols) = ask.symbols {
        if super::diff::ask_symbols(&mut ask.session().requests, Some(symbols)).is_empty() {
            notes.push(format!(
                "`--symbols {symbols}` names stacks, and no request of `omn saturate` reads one: \
                 the USE method names resources, not functions. `omn latency` names them"
            ));
        }
    }
    let mut resources = Vec::new();

    // ---------------------------------------------------------------- cpu
    let mut cpu = Use::new(Resource::Cpu);
    let cpu_ns = run
        .output
        .results
        .iter()
        .find_map(|results| results.lifetime.as_ref().and_then(|life| life.cpu_ns))
        .map(|ns| (ns as f64, "the reap's rusage".to_owned()))
        .or_else(|| {
            let results = run.result(CPU_TIME)?;
            let total: f64 = run
                .events_of("cpu.oncpu_time")
                .filter_map(|event| event.attrs.iter().find_map(|attr| attr.value.as_f64()))
                .sum();
            Some((total, format!("{}, polled", results.source)))
        });
    match cpu_ns {
        Some((ns, from)) if window > 0.0 => {
            cpu.utilization = Some(Reading {
                value: ns / window,
                unit: "cpus".to_owned(),
                says: format!(
                    "{:.2} cpus busy with it on average: {} of cpu time over {}",
                    ns / window,
                    span(ns),
                    span(window)
                ),
                source: from,
                whose: "the workload's".to_owned(),
                evidence: Evidence::Identity,
                short: None,
            });
        }
        _ => cpu
            .unread
            .push("no cpu time: nothing reaped the program and no poll read it".to_owned()),
    }
    match run.result(CPU_WAIT) {
        Some(results) => {
            let (mut n, mut total) = (0.0, 0.0);
            for event in run.events_of("sched.runqlat") {
                n += attr(event, "count").unwrap_or(1.0);
                total += attr(event, "total_ns")
                    .or_else(|| attr(event, "wait_ns"))
                    .unwrap_or(0.0);
            }
            let little = Little::of(
                "wakeups waiting for a cpu",
                &results.source,
                n,
                total,
                window,
                short_of(results),
            );
            cpu.saturation = Some(Reading {
                value: little.mean_count,
                unit: "tasks".to_owned(),
                says: format!(
                    "{:.3} of its tasks runnable and waiting for a cpu, on average",
                    little.mean_count
                ),
                source: results.source.clone(),
                whose: "the workload's".to_owned(),
                evidence: Evidence::Key,
                short: little.short.clone(),
            });
            cpu.little.push(little);
            cpu.tail.extend(p99(results, "the wait for a cpu"));
        }
        None => cpu.unread.push("no run queue latency".to_owned()),
    }
    cpu.errors = None;
    cpu.unread
        .push("errors: not per process -- throttling is a cgroup's (`cpu.throttle`)".to_owned());
    resources.push(cpu);

    // ---------------------------------------------------------------- disk
    let mut disk = Use::new(Resource::Disk);
    match run.result(DISK) {
        Some(results) => {
            let (mut n, mut total, mut failed) = (0.0, 0.0, 0u64);
            for event in run.events_of("io.latency") {
                n += attr(event, "count").unwrap_or(1.0);
                total += attr(event, "latency_ns").unwrap_or(0.0);
                if attr(event, "status").is_some_and(|status| status != 0.0) {
                    failed += 1;
                }
            }
            let little = Little::of(
                "requests in flight at the device",
                &results.source,
                n,
                total,
                window,
                short_of(results),
            );
            disk.utilization = Some(Reading {
                value: little.mean_count,
                unit: "requests".to_owned(),
                says: format!(
                    "{:.3} of its requests in flight at the device, on average -- the device \
                     busy with it, not the device's capacity, which nothing here reads",
                    little.mean_count
                ),
                source: results.source.clone(),
                whose: "the workload's".to_owned(),
                evidence: Evidence::Key,
                short: little.short.clone(),
            });
            disk.errors = Some(Reading {
                value: failed as f64,
                unit: "requests".to_owned(),
                says: format!("{failed} request(s) completed with an error status"),
                source: results.source.clone(),
                whose: "the workload's".to_owned(),
                evidence: Evidence::Key,
                short: little.short.clone(),
            });
            disk.little.push(little);
            disk.tail.extend(p99(results, "a request's device latency"));
        }
        None => disk.unread.push("no block request latency".to_owned()),
    }
    match run.result(DISK_BLOCKED) {
        Some(results) => {
            let (mut n, mut total) = (0.0, 0.0);
            for event in run.events_of("sched.offcpu") {
                if event.attr("state").map(ToString::to_string).as_deref()
                    != Some("uninterruptible")
                {
                    continue;
                }
                n += attr(event, "blocks").unwrap_or(1.0);
                total += attr(event, "total_ns").unwrap_or(0.0)
                    - attr(event, "queued_ns").unwrap_or(0.0);
            }
            let little = Little::of(
                "blocks waiting uninterruptibly (usually on a disk)",
                &results.source,
                n,
                total,
                window,
                short_of(results),
            );
            disk.saturation = Some(Reading {
                value: little.mean_count,
                unit: "tasks".to_owned(),
                says: format!(
                    "{:.3} of its tasks blocked on a device, on average: waiting for its own \
                     requests, which is not a queue behind anyone else's",
                    little.mean_count
                ),
                source: results.source.clone(),
                whose: "the workload's".to_owned(),
                evidence: Evidence::Key,
                short: little.short.clone(),
            });
            disk.little.push(little);
        }
        None => disk
            .unread
            .push("no blocked time: `sched.offcpu` did not run".to_owned()),
    }
    if let Some(results) = run.result(DEVICE_QUEUE) {
        let busiest = results.outputs.iter().find_map(|output| match output {
            Output::Summary { series } => series
                .iter()
                .max_by(|a, b| a.mean.total_cmp(&b.mean))
                .map(|one| (one.key.to_string(), one.mean, one.max)),
            _ => None,
        });
        if let Some((device, mean, max)) = busiest {
            disk.unread.push(format!(
                "the busiest device's own queue, every process's: {mean:.2} in flight on \
                 average, {max:.0} at most ({device}, `io.inflight`, polled -- a queue that fills \
                 and drains between two polls leaves no trace)"
            ));
        }
    }
    resources.push(disk);

    // ---------------------------------------------------------------- lock
    let mut lock = Use::new(Resource::Lock);
    if let Some(results) = run.result(LOCK) {
        let mut by_addr: BTreeMap<u64, (f64, f64, f64)> = BTreeMap::new();
        for event in run.events_of("sched.mutex") {
            let addr = attr(event, "addr").unwrap_or(0.0) as u64;
            let entry = by_addr.entry(addr).or_default();
            entry.0 += 1.0;
            entry.1 += attr(event, "wait_ns").unwrap_or(0.0);
            entry.2 += attr(event, "hold_ns").unwrap_or(0.0);
        }
        if let Some((addr, (n, wait, hold))) = by_addr
            .into_iter()
            .max_by(|a, b| (a.1 .1 + a.1 .2).total_cmp(&(b.1 .1 + b.1 .2)))
        {
            let short = short_of(results);
            let held = Little::of(
                &format!("acquisitions holding the mutex at {addr:#x}"),
                &results.source,
                n,
                hold,
                window,
                short.clone(),
            );
            let waited = Little::of(
                &format!("acquisitions waiting for the mutex at {addr:#x}"),
                &results.source,
                n,
                wait,
                window,
                short.clone(),
            );
            lock.utilization = Some(Reading {
                value: held.mean_count,
                unit: "fraction".to_owned(),
                says: format!(
                    "the mutex at {addr:#x} held {:.1} % of the time",
                    held.mean_count * 100.0
                ),
                source: results.source.clone(),
                whose: "the workload's".to_owned(),
                evidence: Evidence::Key,
                short: short.clone(),
            });
            lock.saturation = Some(Reading {
                value: waited.mean_count,
                unit: "threads".to_owned(),
                says: format!(
                    "{:.3} threads waiting for it, on average",
                    waited.mean_count
                ),
                source: results.source.clone(),
                whose: "the workload's".to_owned(),
                evidence: Evidence::Key,
                short,
            });
            lock.little.push(held);
            lock.little.push(waited);
        }
        lock.tail
            .extend(p99(results, "the wait for a pthread mutex"));
    }
    if let Some(results) = run.result(FUTEX) {
        let (mut n, mut total) = (0.0, 0.0);
        for event in run.events_of("sched.futex") {
            n += attr(event, "count").unwrap_or(1.0);
            total += attr(event, "total_ns").unwrap_or(0.0);
        }
        let little = Little::of(
            "futex waits (any lock, condition or join)",
            &results.source,
            n,
            total,
            window,
            short_of(results),
        );
        if lock.saturation.is_none() && little.arrivals > 0.0 {
            lock.saturation = Some(Reading {
                value: little.mean_count,
                unit: "threads".to_owned(),
                says: format!(
                    "{:.3} threads in futex(2), on average: a lock, a condition variable or a \
                     join -- a futex does not say which",
                    little.mean_count
                ),
                source: results.source.clone(),
                whose: "the workload's".to_owned(),
                evidence: Evidence::Key,
                short: little.short.clone(),
            });
        }
        lock.little.push(little);
        if lock.tail.is_empty() {
            lock.tail.extend(p99(results, "a futex wait"));
        }
    }
    if lock.little.is_empty() {
        lock.unread
            .push("neither `sched.mutex` nor `sched.futex` ran".to_owned());
    }
    resources.push(lock);

    // ---------------------------------------------------------------- memory
    let mut memory = Use::new(Resource::Memory);
    match run.result(MEMORY) {
        Some(results) => {
            let (mut stalled, mut interval) = (0.0, 0.0);
            for event in run.events_of("mem.pressure") {
                stalled += attr(event, "stalled_ns").unwrap_or(0.0);
                interval += attr(event, "interval_ns").unwrap_or(0.0);
            }
            if interval > 0.0 {
                memory.saturation = Some(Reading {
                    value: stalled / interval,
                    unit: "fraction".to_owned(),
                    says: format!(
                        "some task of its cgroup stalled on memory {:.2} % of the time",
                        stalled / interval * 100.0
                    ),
                    source: results.source.clone(),
                    whose: "its cgroup's, which may hold more than this workload".to_owned(),
                    evidence: Evidence::Temporal,
                    short: None,
                });
            }
        }
        None => memory.unread.push("no memory pressure reading".to_owned()),
    }
    // The workload's own stalls in direct reclaim, which PSI's memory share
    // counts among others: a Little line either way, and the saturation where
    // there is no PSI (a kernel built without it, or booted with `psi=0`),
    // said to be the narrower number it is.
    match run.result(RECLAIM) {
        Some(results) => {
            let (mut n, mut total) = (0.0, 0.0);
            for event in run.events_of("mem.reclaim") {
                n += attr(event, "count").unwrap_or(1.0);
                total += attr(event, "total_ns").unwrap_or(0.0);
            }
            let little = Little::of(
                "stalls in direct reclaim",
                &results.source,
                n,
                total,
                window,
                short_of(results),
            );
            if memory.saturation.is_none() {
                memory.saturation = Some(Reading {
                    value: little.mean_count,
                    unit: "tasks".to_owned(),
                    says: format!(
                        "{:.3} of its tasks stalled in direct reclaim, on average -- without PSI, \
                         the narrower number: a refault on a thrashing page and a wait for a \
                         swap-in are memory stalls PSI counts and this does not",
                        little.mean_count
                    ),
                    source: results.source.clone(),
                    whose: "the workload's".to_owned(),
                    evidence: Evidence::Key,
                    short: little.short.clone(),
                });
                memory
                    .unread
                    .retain(|unread| unread != "no memory pressure reading");
            }
            memory.little.push(little);
        }
        None if memory.saturation.is_none() => memory
            .unread
            .push("no direct reclaim reading: `mem.reclaim` did not run".to_owned()),
        None => {}
    }
    resources.push(memory);

    let energy = (ask.objective == Objective::Energy).then(|| energy(run, window, &resources));
    if ask.objective == Objective::Energy && energy.as_ref().is_some_and(|e| e.domains.is_empty()) {
        notes.push("no energy counter ran on this machine".to_owned());
    }
    let verdict = verdict(ask.objective, &resources, energy.as_ref());
    Answer {
        method: Method::Saturate,
        objective: ask.objective,
        target: ask.target.to_string(),
        window_ns: window,
        verdict,
        resources,
        energy,
        dropped,
        notes,
        failures: Vec::new(),
        program_failed: None,
    }
}

fn energy(run: &Run, window: f64, resources: &[Use]) -> Energy {
    let mut domains = BTreeMap::new();
    if let Some(results) = run.result(ENERGY) {
        for event in run.events_of("power.energy") {
            let domain = event
                .attr("domain")
                .map(ToString::to_string)
                .unwrap_or_else(|| results.label.clone());
            let joules = attr(event, "joules")
                .or_else(|| event.attrs.iter().find_map(|attr| attr.value.as_f64()))
                .unwrap_or(0.0);
            *domains.entry(domain).or_insert(0.0) += joules;
        }
    }
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    let busy = run.result(MACHINE_CPU).and_then(|results| {
        results.outputs.iter().find_map(|output| match output {
            Output::Summary { series } => series.first().map(|one| one.mean),
            _ => None,
        })
    });
    let workload_cpus = resources
        .iter()
        .find(|one| one.resource == Resource::Cpu)
        .and_then(|cpu| cpu.utilization.as_ref())
        .map(|reading| reading.value);
    let cpu_share = match (workload_cpus, busy) {
        (Some(workload), Some(busy)) if busy > 0.0 => Some((workload / (busy * cpus)).min(1.0)),
        _ => None,
    };
    let package = domains
        .iter()
        .find(|(name, _)| name.contains("pkg") || name.contains("package"))
        .map(|(_, joules)| *joules)
        .or_else(|| domains.values().copied().reduce(f64::max));
    Energy {
        credited_joules: cpu_share.zip(package).map(|(share, joules)| share * joules),
        domains,
        window_ns: window,
        cpu_share,
        evidence: Evidence::Temporal,
        unmetered: vec![
            "the disk's energy: no counter on this machine meters a block device".to_owned(),
            "the network's energy: no counter meters a NIC".to_owned(),
        ],
    }
}

fn verdict(objective: Objective, resources: &[Use], energy: Option<&Energy>) -> Verdict {
    let lower_bound = |resource: &Use| {
        resource.little.iter().any(|little| little.short.is_some())
            || resource.tail.iter().any(|tail| tail.short.is_some())
    };
    match objective {
        Objective::Throughput => {
            // Queues behind something another task holds: a cpu, a lock. A
            // task blocked on its own disk request is not queued behind anyone.
            let queues: Vec<(&Use, f64)> = resources
                .iter()
                .filter(|one| matches!(one.resource, Resource::Cpu | Resource::Lock))
                .filter_map(|one| one.saturation.as_ref().map(|reading| (one, reading.value)))
                .collect();
            let longest = queues.iter().max_by(|a, b| a.1.total_cmp(&b.1));
            match longest {
                Some((resource, length)) if *length >= SATURATED => Verdict {
                    objective,
                    resource: Some(resource.resource),
                    sentence: format!(
                        "for throughput, the workload queues for the {}: {}",
                        resource.resource.as_str(),
                        resource
                            .saturation
                            .as_ref()
                            .map_or(String::new(), |r| r.says.clone())
                    ),
                    lower_bound: lower_bound(resource),
                    undecided: None,
                },
                _ => {
                    let used = |resource: Resource| {
                        resources
                            .iter()
                            .find(|one| one.resource == resource)
                            .and_then(|one| one.utilization.as_ref())
                    };
                    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
                    let mut uses = Vec::new();
                    if let Some(cpu) = used(Resource::Cpu) {
                        uses.push(format!("{:.2} of this machine's {cpus} cpus", cpu.value));
                    }
                    if let Some(disk) = used(Resource::Disk) {
                        uses.push(format!(
                            "{:.2} requests in flight at the device",
                            disk.value
                        ));
                    }
                    let blocked = resources
                        .iter()
                        .find(|one| one.resource == Resource::Disk)
                        .and_then(|disk| disk.saturation.as_ref())
                        .filter(|reading| reading.value >= SATURATED)
                        .map_or(String::new(), |reading| {
                            format!(
                                " Its tasks are blocked on a device {:.0} % of the time, each \
                                 on its own request: that is a latency to shorten or to \
                                 overlap, not a queue to drain.",
                                reading.value * 100.0
                            )
                        });
                    let why = format!(
                        "no queue of the workload's own work reaches {SATURATED} waiters on \
                         average: nothing it uses is saturated, so its throughput is limited by \
                         its own concurrency -- how much of its work it keeps in flight at once{}",
                        if uses.is_empty() {
                            String::new()
                        } else {
                            format!(", which is {} on average", uses.join(" and "))
                        }
                    );
                    Verdict {
                        objective,
                        resource: None,
                        sentence: format!("for throughput, {why}.{blocked}"),
                        lower_bound: false,
                        undecided: Some(why),
                    }
                }
            }
        }
        Objective::Latency => Verdict {
            objective,
            resource: None,
            sentence: "for latency, `omn latency` answers: a latency is one task's wall clock"
                .to_owned(),
            lower_bound: false,
            undecided: Some("not an objective this method answers for".to_owned()),
        },
        Objective::Tail => {
            let mut ranked: Vec<(&Use, &Percentile)> = resources
                .iter()
                .flat_map(|one| one.tail.iter().map(move |tail| (one, tail)))
                .collect();
            ranked.sort_by(|a, b| b.1.high.total_cmp(&a.1.high));
            match ranked.as_slice() {
                [(first, top), rest @ ..]
                    if rest.first().is_none_or(|(_, next)| top.high > next.high) =>
                {
                    Verdict {
                        objective,
                        resource: Some(first.resource),
                        sentence: format!(
                            "for the tail, the longest single waits are {}{}",
                            top.line(),
                            if top.short.is_some() {
                                ". The run was lossy, and what it lost are the slow ones: this \
                                 percentile is a lower bound"
                            } else {
                                ""
                            }
                        ),
                        lower_bound: top.short.is_some(),
                        undecided: None,
                    }
                }
                [(_, top), (_, next), ..] => {
                    let why = format!(
                        "{} and {} have their 99th percentile in the same bucket, [{}, {}): a \
                         log2 histogram cannot tell them apart",
                        top.what,
                        next.what,
                        span(top.low),
                        span(top.high)
                    );
                    Verdict {
                        objective,
                        resource: None,
                        sentence: format!("for the tail, undecided: {why}"),
                        lower_bound: false,
                        undecided: Some(why),
                    }
                }
                _ => Verdict {
                    objective,
                    resource: None,
                    sentence: "for the tail, no wait distribution was collected".to_owned(),
                    lower_bound: false,
                    undecided: Some("no histogram".to_owned()),
                },
            }
        }
        Objective::Energy => match energy {
            Some(energy) if !energy.domains.is_empty() => {
                let credited = match (energy.cpu_share, energy.credited_joules) {
                    (Some(share), Some(joules)) => format!(
                        "it held {:.1} % of the machine's busy cpu time, so about {joules:.2} J of \
                         the package's energy is its -- credited by cpu time, which is an \
                         estimate (TEMPORAL): the meter is the package's, not the process's",
                        share * 100.0
                    ),
                    _ => "its share of the package could not be estimated: the machine's busy cpu \
                          time was not read"
                        .to_owned(),
                };
                Verdict {
                    objective,
                    resource: Some(Resource::Cpu),
                    sentence: format!(
                        "for energy, what this machine meters of the workload is its cpu time: \
                         {credited}. {}",
                        energy.unmetered.join("; ")
                    ),
                    lower_bound: false,
                    undecided: None,
                }
            }
            _ => Verdict {
                objective,
                resource: None,
                sentence: "for energy, no energy counter could be read on this machine".to_owned(),
                lower_bound: false,
                undecided: Some("no energy counter".to_owned()),
            },
        },
    }
}

impl Answer {
    pub fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "omn saturate  --objective {}  {}",
            self.objective, self.target
        );
        let _ = writeln!(
            out,
            "  over {}, the window every L below is a mean over",
            span(self.window_ns)
        );
        let _ = writeln!(out, "\n  {}\n", self.verdict.sentence);
        if let Some(failed) = &self.program_failed {
            let _ = writeln!(
                out,
                "  warning: the program {failed}: these uses are of a run that failed, which is \
                 rarely the one the question was about\n"
            );
        }
        for resource in &self.resources {
            let _ = writeln!(out, "  {}", resource.resource.as_str());
            for (column, reading) in [
                ("utilization", &resource.utilization),
                ("saturation", &resource.saturation),
                ("errors", &resource.errors),
            ] {
                if let Some(reading) = reading {
                    let mark = if reading.short.is_some() {
                        Shortfall::MARK
                    } else {
                        ""
                    };
                    let _ = writeln!(
                        out,
                        "    {column:<12} {mark}{:>9.3} {:<9} {}  [{}, {}, {}]",
                        reading.value,
                        reading.unit,
                        reading.says,
                        reading.source,
                        reading.whose,
                        reading.evidence
                    );
                }
            }
            for little in &resource.little {
                let _ = writeln!(out, "    little       {}", little.line());
            }
            for tail in &resource.tail {
                let _ = writeln!(out, "    tail         {}", tail.line());
            }
            for unread in &resource.unread {
                let _ = writeln!(out, "    not read     {unread}");
            }
            let _ = writeln!(out);
        }
        if let Some(energy) = &self.energy {
            let _ = writeln!(out, "  energy");
            for (domain, joules) in &energy.domains {
                let _ = writeln!(
                    out,
                    "    {domain:<12} {joules:>9.3} J over {} ({:.1} W, the machine's)",
                    span(energy.window_ns),
                    joules / (energy.window_ns / 1e9)
                );
            }
            for unmetered in &energy.unmetered {
                let _ = writeln!(out, "    not metered  {unmetered}");
            }
            let _ = writeln!(out);
        }
        for shortfall in self.resources.iter().flat_map(|one| {
            one.little
                .iter()
                .filter_map(|little| little.short.as_ref().map(|short| (&little.source, short)))
        }) {
            let _ = writeln!(out, "  short: [{}] {}", shortfall.0, shortfall.1.note);
        }
        for dropped in &self.dropped {
            let _ = writeln!(
                out,
                "  without `{}` ({}): {}",
                dropped.label, dropped.source, dropped.reason
            );
        }
        for note in &self.notes {
            let _ = writeln!(out, "  note: {note}");
        }
        for failure in &self.failures {
            let _ = writeln!(out, "  broken by the tool: {failure}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(objective: Objective) -> Ask {
        Ask {
            objective,
            target: Target::Launch(vec!["true".to_owned()]),
            duration: None,
            symbols: None,
        }
    }

    #[test]
    fn littles_law_is_the_sum_of_waits_over_the_window() {
        // Three tasks each waiting the whole of a one-second window: three
        // waiters on average, whatever the number of waits was.
        let little = Little::of("x", "s", 300.0, 3e9, 1e9, None);
        assert!((little.mean_count - 3.0).abs() < 1e-9);
        assert!((little.per_second * little.mean_ns / 1e9 - little.mean_count).abs() < 1e-9);
    }

    #[test]
    fn the_session_it_writes_validates_against_the_catalog() {
        let catalog = Catalog::embedded().unwrap();
        for objective in Method::Saturate.answers() {
            let session = ask(*objective).session();
            crate::validate::validate_session(&catalog, &session)
                .unwrap_or_else(|rejections| panic!("{objective}: {rejections:?}"));
        }
        let attached = Ask {
            objective: Objective::Throughput,
            target: Target::Pid(vec![1]),
            duration: Some("1s".parse().unwrap()),
            symbols: None,
        };
        crate::validate::validate_session(&catalog, &attached.session())
            .unwrap_or_else(|rejections| panic!("{rejections:?}"));
    }

    #[test]
    fn an_attached_process_needs_a_duration() {
        let ask = Ask {
            objective: Objective::Throughput,
            target: Target::Pid(vec![1]),
            duration: None,
            symbols: None,
        };
        assert!(ask.check().is_err());
    }

    fn with_queue(resource: Resource, length: f64) -> Use {
        let mut one = Use::new(resource);
        one.saturation = Some(Reading {
            value: length,
            unit: "tasks".to_owned(),
            says: String::new(),
            source: "s".to_owned(),
            whose: "w".to_owned(),
            evidence: Evidence::Key,
            short: None,
        });
        one
    }

    #[test]
    fn throughput_names_the_longest_queue_and_nothing_below_the_line() {
        let saturated = [
            with_queue(Resource::Cpu, 2.9),
            with_queue(Resource::Lock, 0.2),
        ];
        assert_eq!(
            verdict(Objective::Throughput, &saturated, None).resource,
            Some(Resource::Cpu)
        );
        // A disk its tasks are blocked on is not a queue.
        let idle = [
            with_queue(Resource::Cpu, 0.01),
            with_queue(Resource::Disk, 0.9),
        ];
        let verdict = verdict(Objective::Throughput, &idle, None);
        assert_eq!(verdict.resource, None, "{}", verdict.sentence);
        assert!(
            verdict.sentence.contains("concurrency"),
            "{}",
            verdict.sentence
        );
    }

    #[test]
    fn a_percentile_of_a_lossy_run_is_marked_a_lower_bound() {
        let mut cpu = Use::new(Resource::Cpu);
        cpu.tail.push(Percentile {
            what: "w".to_owned(),
            source: "s".to_owned(),
            low: 1024.0,
            high: 2048.0,
            count: 10,
            short: Shortfall::of(
                &crate::collect::Losses {
                    dropped: 5,
                    ..Default::default()
                },
                10,
                false,
            ),
        });
        let verdict = verdict(Objective::Tail, &[cpu], None);
        assert!(verdict.lower_bound);
        assert!(
            verdict.sentence.contains(Shortfall::MARK),
            "{}",
            verdict.sentence
        );
    }
}
