//! The self-benchmark's harness: `ROADMAP.md` phase 8.
//!
//! Shared by every `tests/bench_*.rs` through `#[path]`, because an integration
//! test is its own crate and this code belongs to none of them. It is not an
//! example target: Cargo only discovers `examples/*.rs` and
//! `examples/*/main.rs`, so a file here is compiled only where it is included.
//!
//! What it holds, in the order the phase lists them:
//!
//! - the reference workloads, which are one C program (`workload.c`) that
//!   measures itself and reports what it did;
//! - the statistics: median, quartiles, IQR, and the coefficient of variation
//!   the gate is decided on;
//! - the **variance gate**, which refuses before anything is measured;
//! - the round-robin schedule, warmup, LLC eviction between repetitions and
//!   the throttle check that invalidates a repetition;
//! - running a workload bare and under `omn`, and reading back what the tool
//!   says it cost;
//! - the plan of every (source, backend) this build can serve here, and the
//!   table.
//!
//! Every number the harness asserts on comes from the workload's own report
//! or from the kernel, never from the tool alone: a bench that checks the tool
//! against itself measures nothing.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

/// The repository root.
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Where every run of the bench writes, one directory per run.
pub fn bench_dir() -> PathBuf {
    root().join("target/bench")
}

// ------------------------------------------------------------- statistics

/// What a series of repetitions says, robustly and otherwise.
///
/// The median and the IQR are what the table reports, because one repetition
/// the scheduler interrupted should move neither. The mean and the standard
/// deviation are kept for one purpose: the gate is written in the roadmap as a
/// coefficient of variation, and a CV is `stddev / mean`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub n: usize,
    pub min: f64,
    pub q1: f64,
    pub median: f64,
    pub q3: f64,
    pub max: f64,
    pub mean: f64,
    /// The sample standard deviation, with `n - 1`.
    pub stddev: f64,
}

impl Stats {
    /// `None` for an empty series, which has no median to report.
    pub fn of(samples: &[f64]) -> Option<Stats> {
        if samples.is_empty() || samples.iter().any(|value| !value.is_finite()) {
            return None;
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        let n = sorted.len();
        let mean = sorted.iter().sum::<f64>() / n as f64;
        let stddev = if n > 1 {
            (sorted.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
        } else {
            0.0
        };
        Some(Stats {
            n,
            min: sorted[0],
            q1: quantile(&sorted, 0.25),
            median: quantile(&sorted, 0.5),
            q3: quantile(&sorted, 0.75),
            max: sorted[n - 1],
            mean,
            stddev,
        })
    }

    pub fn iqr(&self) -> f64 {
        self.q3 - self.q1
    }

    /// The coefficient of variation. Infinite for a zero mean, so a series
    /// that measured nothing can never pass a gate written as a bound on it.
    pub fn cv(&self) -> f64 {
        if self.mean == 0.0 {
            f64::INFINITY
        } else {
            self.stddev / self.mean.abs()
        }
    }

    /// The smallest relative difference between this series' median and
    /// another's of the same spread and size that the bench can tell from
    /// noise, at about 95 % confidence.
    ///
    /// The table compares medians, not single runs, and a median of `n`
    /// runs is known far better than one run: its standard error is about
    /// `1.2533 * stddev / sqrt(n)`. The difference of two such medians has
    /// `sqrt(2)` times that, and two of those is the 95 % bound. So the
    /// question the roadmap asks -- can this machine resolve a 1-3 %
    /// overhead? -- is `resolution() <= 1 %`, not `cv() <= 1 %`, which held a
    /// median of twenty to the spread of one run and refused every machine
    /// with a disk or a scheduler in its workloads.
    pub fn resolution(&self) -> f64 {
        resolution_at(self.cv(), self.n)
    }

    /// Repetitions this series' spread needs to resolve `target`.
    pub fn repetitions_to_resolve(&self, target: f64) -> usize {
        let cv = self.cv();
        if !cv.is_finite() {
            return usize::MAX;
        }
        ((RESOLUTION_FACTOR * cv / target).powi(2).ceil() as usize).max(MIN_REPETITIONS)
    }
}

/// `2 * sqrt(2) * 1.2533`: a 95 % bound on the difference of two medians, in
/// units of one run's standard deviation over `sqrt(n)`. See
/// [`Stats::resolution`].
pub const RESOLUTION_FACTOR: f64 = 2.0 * std::f64::consts::SQRT_2 * 1.2533;

/// [`Stats::resolution`] for a spread and a size.
pub fn resolution_at(cv: f64, n: usize) -> f64 {
    if n == 0 {
        return f64::INFINITY;
    }
    RESOLUTION_FACTOR * cv / (n as f64).sqrt()
}

/// A quantile by linear interpolation between order statistics -- R's type 7,
/// numpy's default -- so a reader can reproduce any number in the table with
/// the tool they already have.
pub fn quantile(sorted: &[f64], p: f64) -> f64 {
    assert!(!sorted.is_empty(), "a quantile of nothing");
    let h = (sorted.len() - 1) as f64 * p.clamp(0.0, 1.0);
    let lo = h.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    sorted[lo] + (h - lo as f64) * (sorted[hi] - sorted[lo])
}

// ------------------------------------------------------------------ gate

/// The roadmap's aim: resolve an overhead of 1 %, the low end of the 1-3 % a
/// collector is expected to cost. Judged on the median, see
/// [`Stats::resolution`].
pub const RESOLUTION_TARGET: f64 = 0.01;

/// The roadmap's floor: at least twenty repetitions per method.
pub const MIN_REPETITIONS: usize = 20;

/// The ceiling on repetitions a noisy workload is given to reach
/// [`RESOLUTION_TARGET`]. Past it the workload is excluded by name, with the
/// resolution it did reach, rather than measured with a bound it cannot keep.
/// Two hundred is ten times the floor, and at the slowest reference workload
/// about seventy seconds of the baseline.
pub const MAX_REPETITIONS: usize = 200;

/// The environment the gate reads beside the samples.
#[derive(Debug, Clone, Default)]
pub struct Conditions {
    /// The scaling governor of every cpu, deduplicated.
    pub governors: Vec<String>,
    /// `/proc/loadavg`'s one-minute figure, and the cpu count to read it by.
    pub load_1m: Option<f64>,
    pub cpus: usize,
    /// Whether the clocks may boost, which is what "locked clocks" is about.
    pub boost: Option<bool>,
    pub transparent_hugepage: Option<String>,
    /// How many cpus' worth of time the cpus the workloads are not pinned to
    /// spent busy while the last baseline ran. See [`MAX_FOREIGN_CPUS`].
    pub foreign_busy_cpus: Option<f64>,
}

/// The most the rest of the machine may be doing while the baseline runs, in
/// cpus' worth of busy time on the cpus no workload is pinned to.
///
/// A variance gate cannot see a steady load: a machine with a hog on every cpu
/// slowed `memory` by 17 % here and left its spread at 0.74 %, so both the
/// median-resolution gate and the CV gate before it passed a machine loaded on
/// purpose. What the load does show is busy time on the cpus the workloads
/// are not on: 0.35 cpus here for the harness itself -- the eviction sweep,
/// the test runner -- and a quiet desktop together, and 13.1 with a hog on
/// every cpu. Two is well clear of both.
pub const MAX_FOREIGN_CPUS: f64 = 2.0;

/// Written by [`baselines`], read by [`Conditions::read`]: the busy time of
/// the unpinned cpus over the last baseline, in cpus.
static FOREIGN_BUSY: std::sync::Mutex<Option<f64>> = std::sync::Mutex::new(None);

/// Per cpu, the busy and total ticks `/proc/stat` has counted so far.
fn cpu_ticks() -> BTreeMap<usize, (u64, u64)> {
    let mut ticks = BTreeMap::new();
    let Ok(stat) = std::fs::read_to_string("/proc/stat") else {
        return ticks;
    };
    for line in stat.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let Some(cpu) = fields.next().and_then(|cpu| cpu.parse::<usize>().ok()) else {
            continue;
        };
        let values: Vec<u64> = fields.filter_map(|value| value.parse().ok()).collect();
        if values.len() < 5 {
            continue;
        }
        let total: u64 = values.iter().sum();
        let idle = values[3] + values[4];
        ticks.insert(cpu, (total - idle, total));
    }
    ticks
}

/// Busy time between two readings over the cpus below `first_pinned`, in cpus.
fn foreign_busy(
    before: &BTreeMap<usize, (u64, u64)>,
    after: &BTreeMap<usize, (u64, u64)>,
    first_pinned: usize,
) -> Option<f64> {
    let (mut busy, mut total, mut cpus) = (0u64, 0u64, 0usize);
    for (cpu, (busy_after, total_after)) in after {
        if *cpu >= first_pinned {
            continue;
        }
        let (busy_before, total_before) = before.get(cpu)?;
        busy += busy_after.saturating_sub(*busy_before);
        total += total_after.saturating_sub(*total_before);
        cpus += 1;
    }
    (total > 0 && cpus > 0).then(|| busy as f64 / (total as f64 / cpus as f64))
}

impl Conditions {
    /// Read from the machine, now.
    pub fn read() -> Conditions {
        let mut governors: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu/cpufreq") {
            for entry in entries.flatten() {
                if let Some(governor) = read_trimmed(&entry.path().join("scaling_governor")) {
                    if !governors.contains(&governor) {
                        governors.push(governor);
                    }
                }
            }
        }
        governors.sort();
        Conditions {
            governors,
            load_1m: read_trimmed(Path::new("/proc/loadavg"))
                .and_then(|line| line.split_whitespace().next()?.parse().ok()),
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            boost: read_trimmed(Path::new("/sys/devices/system/cpu/cpufreq/boost"))
                .map(|value| value == "1"),
            transparent_hugepage: read_trimmed(Path::new(
                "/sys/kernel/mm/transparent_hugepage/enabled",
            ))
            .and_then(|line| {
                line.split_whitespace()
                    .find_map(|word| word.strip_prefix('[')?.strip_suffix(']'))
                    .map(str::to_owned)
            }),
            foreign_busy_cpus: *FOREIGN_BUSY.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }
}

/// One workload's baseline, as the gate saw it.
#[derive(Debug, Clone)]
pub struct Baseline {
    pub workload: String,
    pub stats: Stats,
}

/// Why the gate refused, with every number that made it refuse.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub reasons: Vec<String>,
    pub baselines: Vec<Baseline>,
    pub conditions: Conditions,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "the variance gate refused: nothing on this machine can be measured now, and \
             nothing was"
        )?;
        for reason in &self.reasons {
            writeln!(f, "  - {reason}")?;
        }
        write!(f, "{}", gate_table(&self.baselines))?;
        if let Some(load) = self.conditions.load_1m {
            writeln!(
                f,
                "  load average {load:.2} over {} cpus while the baseline ran",
                self.conditions.cpus
            )?;
        }
        if let Some(busy) = self.conditions.foreign_busy_cpus {
            writeln!(f, "  the unpinned cpus were {busy:.2} cpus busy meanwhile")?;
        }
        Ok(())
    }
}

/// The baselines as the gate saw them, one line per workload.
pub fn gate_table(baselines: &[Baseline]) -> String {
    let mut out =
        String::from("  WORKLOAD      N   MEDIAN        IQR           CV       RESOLVES   GATE\n");
    for baseline in baselines {
        let stats = &baseline.stats;
        out.push_str(&format!(
            "  {:<12} {:>3}   {:<12}  {:<12}  {:>6.2} %  {:>6.2} %   {}\n",
            baseline.workload,
            stats.n,
            span(stats.median),
            span(stats.iqr()),
            stats.cv() * 100.0,
            stats.resolution() * 100.0,
            if stats.n < MIN_REPETITIONS {
                "REFUSE"
            } else if admitted(stats) {
                "pass"
            } else {
                "EXCLUDE"
            }
        ));
    }
    out
}

/// Whether a baseline is measured: enough repetitions, and a median that
/// resolves [`RESOLUTION_TARGET`].
fn admitted(stats: &Stats) -> bool {
    stats.n >= MIN_REPETITIONS && stats.resolution() <= RESOLUTION_TARGET
}

/// What the gate let through: each admitted workload with the repetitions it
/// needs, and each excluded one with why.
#[derive(Debug, Clone, Default)]
pub struct Gate {
    pub admitted: BTreeMap<String, usize>,
    pub excluded: BTreeMap<String, String>,
}

impl Gate {
    /// Repetitions to measure `workload` with: what its baseline needed to
    /// resolve the target, never under `floor`. `None` when it was excluded
    /// or never judged.
    pub fn repetitions(&self, workload: &str, floor: usize) -> Option<usize> {
        self.admitted
            .get(workload)
            .map(|needed| (*needed).max(floor))
    }

    /// One line per excluded workload, for the table's "not measured".
    pub fn exclusions(&self) -> Vec<String> {
        self.excluded
            .iter()
            .map(|(workload, why)| format!("`{workload}`: {why}"))
            .collect()
    }
}

/// Decides whether this machine may be measured on, and what on.
///
/// Each workload is judged on its own. One whose median resolves
/// [`RESOLUTION_TARGET`] is admitted with the repetitions that took; one that
/// cannot within [`MAX_REPETITIONS`] is **excluded by name**, with the
/// resolution it reached, and the rest are measured -- a noisy disk says
/// nothing about whether a cpu counter's cost can be read. The run is refused
/// outright only when nothing can be measured: no baseline, fewer than
/// [`MIN_REPETITIONS`] anywhere (a smoke run), every workload excluded, or a
/// governor other than `performance`, which is the one environment control a
/// reader can fix and the bench must not fix for them (boundary 5). Boost and
/// THP are recorded rather than refused, because their effect, when there is
/// one, is exactly what the spread already measures.
pub fn judge(baselines: &[Baseline], conditions: &Conditions) -> Result<Gate, Box<Refusal>> {
    let mut reasons = Vec::new();
    let mut gate = Gate::default();
    if baselines.is_empty() {
        reasons.push("no baseline was measured, so there is no variance to judge".into());
    }
    for baseline in baselines {
        let stats = &baseline.stats;
        if stats.n < MIN_REPETITIONS {
            reasons.push(format!(
                "`{}` has {} repetitions and the protocol asks for at least {MIN_REPETITIONS}",
                baseline.workload, stats.n
            ));
        } else if admitted(stats) {
            gate.admitted.insert(baseline.workload.clone(), stats.n);
        } else {
            gate.excluded.insert(
                baseline.workload.clone(),
                format!(
                    "varies by {:.2} % run to run, so a median of {} resolves {:.2} %, over the \
                     {:.0} % the protocol aims for; it would need {} repetitions and the \
                     ceiling is {MAX_REPETITIONS}",
                    stats.cv() * 100.0,
                    stats.n,
                    stats.resolution() * 100.0,
                    RESOLUTION_TARGET * 100.0,
                    stats.repetitions_to_resolve(RESOLUTION_TARGET),
                ),
            );
        }
    }
    if !baselines.is_empty() && reasons.is_empty() && gate.admitted.is_empty() {
        reasons.push(format!(
            "every workload was excluded, so nothing can be measured: {}",
            gate.exclusions().join("; ")
        ));
    }
    if conditions.governors.iter().any(|g| g != "performance") {
        reasons.push(format!(
            "the scaling governor is `{}`, not `performance`; the bench reads it and does not \
             change it: `cpupower frequency-set -g performance` is the reader's to run",
            conditions.governors.join(", ")
        ));
    }
    if let Some(busy) = conditions.foreign_busy_cpus {
        if busy > MAX_FOREIGN_CPUS {
            reasons.push(format!(
                "the rest of the machine kept {busy:.1} cpus busy while the baseline ran, over \
                 the {MAX_FOREIGN_CPUS:.0} the protocol allows: a steady load slows every \
                 workload alike and hides from any spread, so it is refused on what it costs \
                 the other cpus"
            ));
        }
    }
    if reasons.is_empty() {
        Ok(gate)
    } else {
        Err(Box::new(Refusal {
            reasons,
            baselines: baselines.to_vec(),
            conditions: conditions.clone(),
        }))
    }
}

// -------------------------------------------------------------- schedule

/// The order `methods` methods are run in, `repetitions` times each.
///
/// Round-robin rather than blocked: every repetition runs every method once,
/// so a drift of the machine over the run -- a thermal rise, a neighbour
/// starting a build -- lands on every method alike instead of on whichever
/// happened to run last. And rotated, so that no method always goes first
/// after the eviction: in `repetitions` a multiple of `methods`, each method
/// holds each position equally often.
pub fn round_robin(methods: usize, repetitions: usize) -> Vec<usize> {
    let mut order = Vec::with_capacity(methods * repetitions);
    for repetition in 0..repetitions {
        for slot in 0..methods {
            order.push((repetition + slot) % methods);
        }
    }
    order
}

/// Repetitions per method: [`MIN_REPETITIONS`], or `OMN_BENCH_REPETITIONS` for
/// a smoke run of the mechanics. Under twenty the gate refuses by
/// construction, so a smoke run can never be mistaken for the measurement:
/// its documents are named `smoke-` and say NOT A MEASUREMENT at the top.
pub fn repetitions() -> usize {
    std::env::var("OMN_BENCH_REPETITIONS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(MIN_REPETITIONS)
}

/// Runs discarded before the measured ones, per method.
pub const WARMUP: usize = 2;

// ----------------------------------------------------------- the machine

/// The size of the largest cache the kernel describes, which is the one the
/// eviction has to overflow.
pub fn llc_bytes() -> Option<usize> {
    let mut largest = None;
    let entries = std::fs::read_dir("/sys/devices/system/cpu/cpu0/cache").ok()?;
    for entry in entries.flatten() {
        let Some(size) = read_trimmed(&entry.path().join("size")) else {
            continue;
        };
        let bytes = match size.strip_suffix('K') {
            Some(kib) => kib.parse::<usize>().ok().map(|k| k << 10),
            None => match size.strip_suffix('M') {
                Some(mib) => mib.parse::<usize>().ok().map(|m| m << 20),
                None => size.parse().ok(),
            },
        };
        largest = largest.max(bytes);
    }
    largest
}

/// Leaves the last level cache holding nothing any workload wrote.
///
/// Twice the LLC, written a line at a time: a read would be enough to evict
/// on an inclusive cache and is not on a victim cache like this Zen's L3, which
/// is filled by what the L2 evicts. What is swept is only this process's view;
/// on a machine with several NUMA nodes each would need its own pass, and the
/// bench records how many nodes there were.
pub struct Evictor {
    buffer: Vec<u8>,
    pass: u8,
}

impl Evictor {
    pub fn new() -> Evictor {
        let bytes = llc_bytes().unwrap_or(32 << 20) * 2;
        Evictor {
            buffer: vec![0; bytes],
            pass: 0,
        }
    }

    pub fn bytes(&self) -> usize {
        self.buffer.len()
    }

    pub fn evict(&mut self) {
        self.pass = self.pass.wrapping_add(1);
        for line in self.buffer.chunks_mut(64) {
            line[0] = self.pass;
        }
        std::hint::black_box(&self.buffer);
    }
}

/// The kernel's thermal throttle counters, summed over every cpu, where the
/// kernel publishes them. `None` is "not observable here", which the report
/// says rather than reading as "never throttled".
///
/// These are the files `power.throttle`'s `fs:sys` backend reads, and they are
/// read here directly rather than through the tool, on purpose (`DECISIONS.md`,
/// the sixteenth wave). The guardrail wraps the bare runs as well as the
/// traced ones, and the bare run is the baseline every dilation is measured
/// against: running `omn` beside it would make it a traced run, and the
/// dilation column a comparison of two traced runs. Read before and after a
/// repetition, outside the timed region, it costs the workload nothing. And a
/// guardrail measured by the tool under test would pass whatever defect the
/// tool had in that source.
pub fn throttle_count() -> Option<u64> {
    let mut total = None;
    let entries = std::fs::read_dir("/sys/devices/system/cpu").ok()?;
    for entry in entries.flatten() {
        let dir = entry.path().join("thermal_throttle");
        for counter in ["core_throttle_count", "package_throttle_count"] {
            if let Some(value) =
                read_trimmed(&dir.join(counter)).and_then(|v| v.parse::<u64>().ok())
            {
                *total.get_or_insert(0u64) += value;
            }
        }
    }
    total
}

/// NUMA nodes with memory, for the record.
pub fn numa_nodes() -> usize {
    std::fs::read_dir("/sys/devices/system/node")
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.strip_prefix("node")
                        .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
                })
                .count()
        })
        .unwrap_or(0)
        .max(1)
}

/// The cpus the workload is pinned to: the last ones, away from cpu 0 where
/// most interrupts land, and one per thread.
pub fn workload_cpus(threads: usize) -> String {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let threads = threads.max(1).min(cpus);
    (cpus - threads..cpus)
        .map(|cpu| cpu.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

// ------------------------------------------------------------- workloads

/// The workload program, built once per test process.
pub fn workload_binary() -> Result<PathBuf, String> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let dir = bench_dir();
            std::fs::create_dir_all(&dir)
                .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
            let path = dir.join("workload");
            // Beside and renamed, so a half-written binary is never run.
            let partial = dir.join(format!("workload.{}", std::process::id()));
            let built = Command::new("cc")
                .args(["-O2", "-pthread", "-o"])
                .arg(&partial)
                .arg(root().join("examples/bench/workload.c"))
                .output()
                .map_err(|error| format!("no C compiler (`cc`) for the workloads: {error}"))?;
            if !built.status.success() {
                return Err(format!(
                    "the workloads did not build:\n{}",
                    String::from_utf8_lossy(&built.stderr)
                ));
            }
            std::fs::rename(&partial, &path).map_err(|error| error.to_string())?;
            Ok(path)
        })
        .clone()
}

/// A name for the workload binary that only this test process execs: a hard
/// link, so the `spawn` workload's children can be told apart from every other
/// exec on a shared machine by filename alone.
pub fn spawn_path() -> Result<PathBuf, String> {
    static LINKED: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    LINKED
        .get_or_init(|| {
            sweep_leftovers();
            let path = bench_dir().join(format!("launch-{}", std::process::id()));
            let _ = std::fs::remove_file(&path);
            std::fs::hard_link(workload_binary()?, &path).map_err(|error| error.to_string())?;
            Ok(path)
        })
        .clone()
}

/// Removes what earlier test processes left in the bench directory: the
/// per-process links, reports and output directories of a pid that is no
/// longer running. The documents of finished runs are kept.
fn sweep_leftovers() {
    let Ok(entries) = std::fs::read_dir(bench_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let pid = name
            .strip_prefix("launch-")
            .or_else(|| name.strip_prefix("out."))
            .or_else(|| name.strip_prefix("report."))
            .and_then(|rest| rest.split('.').next())
            .and_then(|pid| pid.parse::<u32>().ok());
        let Some(pid) = pid else { continue };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let path = entry.path();
            let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
        }
    }
}

/// One invocation of the workload program.
#[derive(Debug, Clone)]
pub struct Workload {
    /// The name the table uses.
    pub name: String,
    pub args: Vec<String>,
    pub threads: usize,
}

impl Workload {
    pub fn new(name: &str, args: &[&str], threads: usize) -> Workload {
        Workload {
            name: name.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            threads,
        }
    }
}

/// The file the IO workload reads, on the repository's disk rather than in
/// `/tmp`, which is a tmpfs on this machine and would have no device to wait
/// on.
pub fn io_file() -> PathBuf {
    bench_dir().join("io.dat")
}

/// The size of the file the IO workload reads.
pub const IO_MIB: u64 = 256;

/// The six reference workloads of phase 8.
///
/// Sized by measurement, not by taste: each takes 40 to 360 ms here, because
/// the first sizes tried (a single launch, a 64 MiB read, 4 x 100 000 lock
/// round trips) varied by 6 to 12 % run to run on a machine with a load
/// average under one, which no 1 % gate can pass. Longer samples brought
/// `spawn`, `io` and `contended` to 1.1-1.5 % with the other streams building
/// beside them; `docs/bench.md` has the numbers.
pub fn reference_workloads() -> Vec<Workload> {
    let io = io_file();
    let io = io.to_string_lossy();
    vec![
        Workload::new("launch", &["spawn", "200"], 1),
        Workload::new("compute", &["compute", "100000000"], 1),
        Workload::new("memory", &["memory", "256", "4000000"], 1),
        Workload::new("syscall", &["syscall", "200000"], 1),
        Workload::new("io", &["io", &io], 1),
        Workload::new("contended", &["contended", "4", "1000000"], 4),
    ]
}

/// Writes the IO workload's file once. It is clean after the `fsync`, which
/// is what lets every run drop it from the cache without root.
pub fn prepare_io_file() -> Result<(), String> {
    let path = io_file();
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() == IO_MIB << 20) {
        return Ok(());
    }
    let status = Command::new(workload_binary()?)
        .args(["io-prepare", &path.to_string_lossy(), &IO_MIB.to_string()])
        .stdout(Stdio::null())
        .status()
        .map_err(|error| error.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("io-prepare failed: {status}"))
}

/// What one run said about itself, and what it cost from outside.
#[derive(Debug, Clone)]
pub struct Run {
    /// The workload's own report.
    pub report: Value,
    /// Spawn to reap, from the harness.
    pub wall_ns: u64,
    /// Whether a throttle counter moved during it. A throttled run is invalid
    /// and is not averaged in.
    pub throttled: bool,
}

impl Run {
    pub fn get(&self, field: &str) -> Option<u64> {
        self.report.get(field)?.as_u64()
    }

    /// The workload's own measure of its work: its timed section where it has
    /// one, and the harness's spawn-to-reap for the launch workload, whose
    /// work is being started.
    pub fn elapsed_ns(&self) -> u64 {
        self.get("elapsed_ns").unwrap_or(self.wall_ns)
    }
}

/// A report file of this call alone. Two tests of one binary run in threads of
/// one process, so the pid and the tag are shared, and one test's run read or
/// removed the other's report ("the workload wrote no report"): the counter
/// makes each call's name its own. The pid stays first, which is what
/// `sweep_leftovers` reads.
fn report_path(tag: &str) -> PathBuf {
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    bench_dir().join(format!("report.{}.{call}.{tag}.json", std::process::id()))
}

fn read_report(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        format!(
            "the workload wrote no report at {}: {error}",
            path.display()
        )
    })?;
    let _ = std::fs::remove_file(path);
    serde_json::from_str(&text).map_err(|error| format!("unreadable report {text:?}: {error}"))
}

/// Runs the workload with no tracer at all: the baseline.
pub fn run_bare(workload: &Workload) -> Result<Run, String> {
    let report = report_path("bare");
    let throttle_before = throttle_count();
    let started = Instant::now();
    let status = Command::new(workload_binary()?)
        .args(&workload.args)
        .env("BENCH_REPORT", &report)
        .env("BENCH_CPUS", workload_cpus(workload.threads))
        .env("BENCH_SPAWN_PATH", spawn_path()?)
        .stdout(Stdio::null())
        .status()
        .map_err(|error| error.to_string())?;
    let wall_ns = started.elapsed().as_nanos() as u64;
    if !status.success() {
        return Err(format!("`{}` failed: {status}", workload.name));
    }
    Ok(Run {
        report: read_report(&report)?,
        wall_ns,
        throttled: throttle_count() != throttle_before,
    })
}

// ----------------------------------------------------------------- omn

/// Which `omn` the bench measures, and whether it is the one worth measuring.
#[derive(Debug, Clone)]
pub struct Omn {
    pub path: PathBuf,
    /// A debug build's collector is unoptimised Rust, and its cost is not the
    /// tool's. The table says which build it measured.
    pub release: bool,
    pub capabilities: bool,
}

impl Omn {
    /// `OMN_BENCH_BIN` if set. Otherwise the release build if it carries the
    /// capabilities, else the debug build if it does, else the release build
    /// -- whose description then says what is missing. `make` grants them to
    /// the debug binary only, so a release build without them next to a
    /// debug build with them would otherwise measure a table with every eBPF
    /// row refused.
    pub fn locate() -> Omn {
        if let Some(path) = std::env::var_os("OMN_BENCH_BIN") {
            return Omn::at(PathBuf::from(path));
        }
        let release = root().join("target/release/omn");
        let debug = root().join("target/debug/omn");
        let path = if has_file_capabilities(&release) {
            release
        } else if has_file_capabilities(&debug) || !release.exists() {
            debug
        } else {
            release
        };
        Omn::at(path)
    }

    fn at(path: PathBuf) -> Omn {
        Omn {
            release: path.components().any(|part| part.as_os_str() == "release"),
            capabilities: has_file_capabilities(&path),
            path,
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} ({} build, {})",
            self.path.display(),
            if self.release { "release" } else { "DEBUG" },
            if self.capabilities {
                "carrying CAP_BPF and CAP_PERFMON"
            } else {
                "WITHOUT capabilities: the eBPF and per-cpu perf rows will be missing"
            }
        )
    }
}

/// Whether a file carries file capabilities at all, read from its
/// `security.capability` attribute rather than from `getcap`, which may not be
/// installed.
pub fn has_file_capabilities(path: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let name = c"security.capability";
    // SAFETY: a NUL-terminated path and name; a null buffer of size zero asks
    // only for the attribute's size.
    let size = unsafe { libc::getxattr(c_path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    size > 0
}

/// How one (source, backend) is asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct Method {
    pub source: String,
    /// `None` lets the resolver choose, which is how the bench learns what it
    /// chose.
    pub backend: Option<String>,
    pub mode: String,
    pub emit: String,
    pub filter: Option<String>,
    /// A system-wide source is given a window instead of a program.
    pub system: bool,
}

impl Method {
    pub fn label(&self) -> String {
        match &self.backend {
            Some(backend) => format!("{} via {backend}", self.source),
            None => format!("{} (resolved)", self.source),
        }
    }

    fn args(&self) -> Vec<String> {
        let mut args = vec![
            "trace".into(),
            self.source.clone(),
            "-m".into(),
            self.mode.clone(),
            "-e".into(),
            self.emit.clone(),
        ];
        if let Some(backend) = &self.backend {
            args.push("--backend".into());
            args.push(format!("force:{backend}"));
        }
        if let Some(filter) = &self.filter {
            args.push("--filter".into());
            args.push(filter.clone());
        }
        args
    }
}

/// A run under the tool: the workload's own report, and the tool's documents.
#[derive(Debug, Clone)]
pub struct Traced {
    pub run: Run,
    /// `cost.json`'s entry for this request.
    pub cost: Value,
    /// `results.json`'s entry for this request.
    pub results: Value,
    /// The tool's own peak resident size, polled from `/proc/<pid>/status`
    /// while it ran -- the agent's RSS, measured apart from the target's,
    /// which is the third exit criterion of the phase.
    pub omn_peak_rss_kb: Option<u64>,
    pub stderr: String,
}

impl Traced {
    fn overhead(&self, field: &str) -> Option<u64> {
        self.cost.get("overhead")?.get(field)?.as_u64()
    }

    /// Cpu the tool spent that it attributes to this collection.
    pub fn collector_cpu_ns(&self) -> Option<u64> {
        self.overhead("attributed_cpu_ns")
    }

    pub fn measured_percent(&self) -> Option<f64> {
        self.cost.get("measured_percent")?.as_f64()
    }

    /// The backend that actually served, which is not the forced one only if
    /// something is wrong.
    pub fn backend(&self) -> Option<&str> {
        self.cost.get("backend")?.as_str()
    }

    /// Every loss counter the tool reported, added up.
    pub fn losses(&self) -> u64 {
        self.results
            .get("losses")
            .and_then(Value::as_object)
            .map(|losses| losses.values().filter_map(Value::as_u64).sum())
            .unwrap_or(0)
    }

    pub fn events_seen(&self) -> u64 {
        self.results
            .get("events_seen")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }

    /// The raw events, when the emit was `raw`.
    pub fn raw_events(&self) -> Vec<&Value> {
        self.results
            .get("outputs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|output| output.get("events")?.as_array())
            .flatten()
            .collect()
    }

    /// The count a `summary` emit carries, over every series.
    pub fn summary_count(&self) -> Option<u64> {
        let outputs = self.results.get("outputs")?.as_array()?;
        let series = outputs
            .iter()
            .find_map(|output| output.get("series")?.as_array())?;
        Some(
            series
                .iter()
                .filter_map(|one| one.get("count")?.as_u64())
                .sum(),
        )
    }
}

/// Runs `workload` under `omn` with `method`, and reads back what it wrote.
pub fn run_traced(omn: &Omn, method: &Method, workload: &Workload) -> Result<Traced, String> {
    // A relink drops file capabilities, and any `cargo build` or `cargo test`
    // that touches the crate relinks `target/debug/omn`. A smoke run lost them
    // half way through exactly that way, and every eBPF row after it failed
    // as if the machine had changed. Checked before every run: a getxattr.
    if omn.capabilities && !has_file_capabilities(&omn.path) {
        panic!(
            "{} lost its capabilities during the bench -- something relinked it (a cargo \
             build or test touching the crate). Nothing after this point would measure the \
             eBPF or per-cpu perf rows; rerun `make` (or grant them again) and start over.",
            omn.path.display()
        );
    }
    let out = bench_dir().join(format!("out.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let report = report_path("traced");
    let mut command = Command::new(&omn.path);
    command
        .args(method.args())
        .args(["--format", "json", "-o"])
        .arg(&out)
        .env("BENCH_REPORT", &report)
        .env("BENCH_CPUS", workload_cpus(workload.threads))
        .env("BENCH_SPAWN_PATH", spawn_path()?)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if method.system {
        // A system-wide collection has no program to follow: it is given a
        // window, and the workload is run bare inside it once the tool has had
        // time to attach. Nothing tells the harness when the attach is done,
        // so the margin is generous: a quarter of a second, where the slowest
        // eBPF attach measured here takes tens of milliseconds.
        command.args(["--system", "-d", SYSTEM_WINDOW]);
    } else {
        command
            .arg("--")
            .arg(workload_binary()?)
            .args(&workload.args);
    }

    let throttle_before = throttle_count();
    let started = Instant::now();
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let side = if method.system {
        std::thread::sleep(ATTACH_MARGIN);
        Some(run_bare(workload)?)
    } else {
        None
    };
    let status_path = PathBuf::from(format!("/proc/{}/status", child.id()));
    let mut peak = None;
    let status = loop {
        if let Some(hwm) = read_hwm(&status_path) {
            peak = Some(hwm);
        }
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => break status,
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    };
    let wall_ns = started.elapsed().as_nanos() as u64;
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stderr);
    }
    if !status.success() {
        return Err(format!("`{}` failed ({status}):\n{stderr}", method.label()));
    }
    let first = |name: &str| -> Result<Value, String> {
        let text = std::fs::read_to_string(out.join(name))
            .map_err(|error| format!("{} has no {name}: {error}", out.display()))?;
        let value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        let entry = match name {
            "results.json" => value.get("results").and_then(|r| r.get(0)).cloned(),
            _ => value.get(0).cloned(),
        };
        entry.ok_or_else(|| format!("{name} holds no request"))
    };
    let (cost, results) = (first("cost.json"), first("results.json"));
    let _ = std::fs::remove_dir_all(&out);
    let run = match side {
        Some(run) => run,
        None => Run {
            report: read_report(&report)?,
            wall_ns,
            throttled: false,
        },
    };
    Ok(Traced {
        run: Run {
            throttled: run.throttled || throttle_count() != throttle_before,
            ..run
        },
        cost: cost?,
        results: results?,
        omn_peak_rss_kb: peak,
        stderr,
    })
}

/// The window a system-wide collection is given, and how long after its start
/// the workload begins. Every reference workload ends well inside it.
pub const SYSTEM_WINDOW: &str = "1s";
pub const ATTACH_MARGIN: Duration = Duration::from_millis(250);

fn read_hwm(path: &Path) -> Option<u64> {
    let status = std::fs::read_to_string(path).ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Whether the tool accepts `method` on this machine, asked of the tool
/// itself: `--dump-resolved` runs the validator and the resolver and measures
/// nothing. `Ok` carries the backend the resolver chose.
pub fn resolves(omn: &Omn, method: &Method) -> Result<String, String> {
    let mut command = Command::new(&omn.path);
    command.args(method.args()).arg("--dump-resolved");
    if method.system {
        command.args(["--system", "-d", SYSTEM_WINDOW]);
    } else {
        command.args(["--", "true"]);
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let resolved: Value =
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
    // A forced backend is accepted whatever its probes said -- the finding
    // filed under stream X -- and then fails at attach, after the launch. The
    // resolved file carries every probe's answer, so the plan reads them.
    let requirements = resolved
        .pointer("/backend/requirements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for requirement in requirements {
        let state = requirement.pointer("/probe/state").and_then(Value::as_str);
        if state != Some("available") {
            return Err(format!(
                "error: forced past `{}`, which probes {}: {}",
                requirement
                    .get("needs")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                state.unwrap_or("?"),
                requirement
                    .pointer("/probe/detail")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ));
        }
    }
    resolved
        .pointer("/backend/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "the resolved request names no backend".into())
}

// ------------------------------------------------------------------ plan

/// One row the table will have.
#[derive(Debug, Clone)]
pub struct Planned {
    pub method: Method,
    pub workload: Workload,
    /// Whether this is the backend the resolver picks when nothing is forced.
    pub chosen: bool,
    /// What the catalog says this backend loses, if it is a fallback.
    pub degraded: Option<String>,
    pub equivalent: bool,
    /// The cost band the catalog declares for this backend, which the row's
    /// measurement is held to.
    pub declared: omn::catalog::Cost,
}

/// A (source, backend) the bench does not measure, and why.
#[derive(Debug, Clone)]
pub struct Unplanned {
    pub source: String,
    pub backend: String,
    pub why: String,
}

/// The workload a source is measured under: the one that exercises what it
/// observes, so its cost is the cost of observing something.
pub fn workload_for(source: &str) -> Workload {
    let named = |name: &str| {
        reference_workloads()
            .into_iter()
            .find(|workload| workload.name == name)
            .expect("a reference workload")
    };
    match source {
        s if s.starts_with("mem.heap.") => {
            Workload::new("malloc", &["async-malloc", "1", "1000000"], 1)
        }
        "sched.mutex" | "sched.futex" | "sched.lock" => named("contended"),
        "proc.syscall" | "proc.signals" => named("syscall"),
        "proc.exec" | "proc.exit" => named("launch"),
        s => match s.split('.').next().unwrap_or("") {
            "mem" => named("memory"),
            "io" => named("io"),
            "sched" => named("contended"),
            "proc" => named("syscall"),
            _ => named("compute"),
        },
    }
}

/// The mode strings tried, in order. A poll is measured at 100 ms and a sample
/// at 997 Hz: both set what the backend costs, and the table states them.
const MODES: [&str; 5] = ["count", "trace", "poll@100ms", "sample@997hz", "snapshot"];
const EMITS: [&str; 5] = ["summary", "rate", "hist", "raw", "topk"];

/// Every (source, backend) this build can serve on this machine, each with a
/// mode and emit the tool accepts for it, and every one it cannot, with why.
///
/// The candidates come from the catalog and from the collector registry; the
/// verdict comes from the tool, by `--dump-resolved` with the backend forced,
/// so the plan cannot disagree with what a run would do.
///
/// `only` restricts it to the sources whose id starts with that prefix.
pub fn plan(omn: &Omn, only: Option<&str>) -> (Vec<Planned>, Vec<Unplanned>) {
    let catalog = omn::Catalog::embedded().expect("the embedded catalog loads");
    let mut planned = Vec::new();
    let mut unplanned = Vec::new();
    for entry in catalog.entries() {
        let source = entry.id.to_string();
        if only.is_some_and(|prefix| !source.starts_with(prefix)) {
            continue;
        }
        let modes: Vec<String> = entry.modes.iter().map(|m| m.to_string()).collect();
        let emits: Vec<String> = entry.emits.iter().map(|e| e.to_string()).collect();
        let supports = |kind| entry.target_support(kind) != omn::catalog::TargetSupport::No;
        // A launched program first, where the source follows one: its cost is
        // then a share of the program's own cpu. The whole machine otherwise.
        let targets: Vec<bool> = [
            supports(omn::request::TargetKind::Launch).then_some(false),
            supports(omn::request::TargetKind::System).then_some(true),
        ]
        .into_iter()
        .flatten()
        .collect();
        if targets.is_empty() {
            for backend in &entry.backends {
                unplanned.push(Unplanned {
                    source: source.clone(),
                    backend: backend.id.clone(),
                    why: "follows only a cgroup or a container, and the bench launches a \
                          program or watches the machine"
                        .into(),
                });
            }
            continue;
        }
        let candidates = |backend: Option<&str>| -> Vec<Method> {
            targets
                .iter()
                .flat_map(|system| {
                    combinations(&modes, &emits).map(move |(mode, emit)| (*system, mode, emit))
                })
                .map(|(system, mode, emit)| Method {
                    source: source.clone(),
                    backend: backend.map(str::to_owned),
                    mode,
                    emit,
                    filter: None,
                    system,
                })
                .collect()
        };
        // The resolver's own pick, under the first setting the tool accepts.
        // A backend with no collector can be the resolver's pick in a
        // resolved file -- a launch of `io.bytes` resolves to an unimplemented
        // `bpf:raw_tp` -- and then the run is refused; the pick that matters
        // is one a run would use.
        let chosen: Option<(String, Method)> = candidates(None).into_iter().find_map(|method| {
            let id = resolves(omn, &method).ok()?;
            omn::collect::implements(&source, &id).then_some((id, method))
        });
        let workload = workload_for(&source);
        for backend in &entry.backends {
            if !omn::collect::implements(&source, &backend.id) {
                unplanned.push(Unplanned {
                    source: source.clone(),
                    backend: backend.id.clone(),
                    why: "no collector is written for it".into(),
                });
                continue;
            }
            // The chosen backend's setting first, so that two backends of one
            // source are compared under the same mode and target wherever
            // both accept it.
            let mut ordered = candidates(Some(&backend.id));
            if let Some((_, like)) = &chosen {
                ordered.sort_by_key(|m| (m.system != like.system, m.mode != like.mode));
            }
            // Accepted by the validator is not enough: one trial run each,
            // because a mode the catalog declares and the validator accepts
            // can still be refused by the collector after the launch. Those
            // are findings, and the table lists them. The reason kept is the
            // most informative one met: a refused run over a missing probe
            // over a combination the validator turned down.
            let mut best: Option<(u8, String)> = None;
            let mut keep = |rank: u8, error: String| {
                if best.as_ref().is_none_or(|(kept, _)| rank < *kept) {
                    best = Some((rank, error));
                }
            };
            let found = ordered
                .into_iter()
                .find(|method| match resolves(omn, method) {
                    Err(error) if error.contains("forced past") => {
                        keep(1, error);
                        false
                    }
                    Err(error) => {
                        keep(2, error);
                        false
                    }
                    Ok(_) => match run_traced(omn, method, &workload) {
                        Ok(_) => true,
                        Err(error) => {
                            keep(0, error);
                            false
                        }
                    },
                });
            let last_error = best
                .map(|(_, error)| error)
                .unwrap_or_else(|| "no mode and emit the source declares".into());
            match found {
                Some(method) => planned.push(Planned {
                    workload: workload.clone(),
                    chosen: chosen.as_ref().map(|(id, _)| id.as_str()) == Some(backend.id.as_str()),
                    degraded: backend.degraded.clone(),
                    equivalent: backend.equivalent,
                    declared: backend.cost,
                    method,
                }),
                None => unplanned.push(Unplanned {
                    source: source.clone(),
                    backend: backend.id.clone(),
                    why: first_line(&last_error),
                }),
            }
        }
    }
    (planned, unplanned)
}

fn combinations<'a>(
    modes: &'a [String],
    emits: &'a [String],
) -> impl Iterator<Item = (String, String)> + 'a {
    MODES
        .iter()
        .filter(move |mode| {
            let kind = mode.split('@').next().unwrap_or(mode);
            modes.iter().any(|declared| declared == kind)
        })
        .flat_map(move |mode| {
            EMITS
                .iter()
                .filter(move |emit| emits.iter().any(|declared| declared == *emit))
                .map(move |emit| ((*mode).to_owned(), (*emit).to_owned()))
        })
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| line.trim_start().starts_with("error"))
        .or_else(|| text.lines().next())
        .unwrap_or("")
        .trim()
        .to_owned()
}

// --------------------------------------------------------------- verdict

/// What the measurement says about the resolver's choice for one source.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// One backend is served here: there was no choice to make.
    Alone,
    /// The chosen backend is not beaten by any other that loses nothing.
    Confirmed(String),
    /// Another backend is as good and clearly cheaper: the chain's order is
    /// wrong for this machine, and the row says which way.
    Corrected(String),
}

/// One backend's measured row, as the verdict reads it.
#[derive(Debug, Clone)]
pub struct Measured {
    pub backend: String,
    /// The mode and target it was measured under: only rows measured alike
    /// are compared.
    pub setting: String,
    pub chosen: bool,
    pub degraded: Option<String>,
    pub equivalent: bool,
    /// Collector cpu per repetition, in ns.
    pub cost: Stats,
    pub losses: u64,
}

/// What one row of the cost table says against the band its backend
/// declares.
#[derive(Debug, Clone, PartialEq)]
pub enum Band {
    /// Measured, and inside the band.
    Within(String),
    /// Measured, and over the band: the row is flagged, and the flag is the
    /// finding -- a band the machine does not keep is a catalog claim to
    /// correct, not a table to throw away.
    Over(String),
    /// A column the row needs is not there. A table with such a row is wrong,
    /// and the test that writes it fails.
    Missing(String),
}

impl Band {
    /// The table cell.
    pub fn cell(&self) -> String {
        match self {
            Band::Within(why) => format!("within: {why}"),
            Band::Over(why) => format!("**over**: {why}"),
            Band::Missing(why) => format!("**missing**: {why}"),
        }
    }
}

/// Holds one row to the band its backend declares.
///
/// The cost compared is the larger of the share of the target's cpu
/// `cost.json` reports and the dilation the workload measured of itself: an
/// in-kernel collector's cost is in the target's dilation and not in the
/// tool's cpu (`DECISIONS.md`, the bench's correction to the bands), so a
/// band judged on the tool's cpu alone would pass every eBPF program. Either
/// column is enough to judge by; a row with neither, with no repetition that
/// completed, or with no collector cpu at all is missing what the table is for.
/// `measured` is how many repetitions completed; `losses` is always a count,
/// so its column is there by construction and it is `measured` that says it
/// was counted over something.
pub fn judge_band(
    declared: omn::catalog::Cost,
    measured: usize,
    collector: Option<Stats>,
    percent: Option<Stats>,
    dilation: Option<Stats>,
) -> Band {
    if measured == 0 {
        return Band::Missing("no repetition completed, so no column is measured".into());
    }
    if collector.is_none() {
        return Band::Missing(format!(
            "{measured} repetitions completed and none reported its collector cpu"
        ));
    }
    let share = percent.map(|stats| stats.median);
    let dilated = dilation.map(|stats| stats.median * 100.0);
    let (cost, from) = match (share, dilated) {
        (Some(share), Some(dilated)) if dilated > share => (dilated, "dilation"),
        (Some(share), _) => (share, "share of the target's cpu"),
        (None, Some(dilated)) => (dilated, "dilation"),
        (None, None) => {
            return Band::Missing(
                "neither a share of the target's cpu nor a dilation to hold to the band".into(),
            )
        }
    };
    let ceiling = declared.worst_case_percent();
    let said = format!(
        "{cost:.2} % ({from}) against `{}`'s {}",
        declared.as_str(),
        if ceiling.is_finite() {
            format!("{ceiling} %")
        } else {
            "no bound".into()
        }
    );
    if cost <= ceiling {
        Band::Within(said)
    } else {
        Band::Over(said)
    }
}

/// One row of the backends table, as the check of the whole table reads it.
pub struct TableRow<'a> {
    /// `source through backend`, for the failure that names it.
    pub label: String,
    /// The backend the row forced.
    pub forced: &'a str,
    /// The backends its runs were served by, from their own `cost.json`.
    pub served_by: &'a [String],
    pub band: &'a Band,
}

/// Every reason the backends table is wrong, which fails the test that wrote
/// it; empty for a table that may be pasted.
///
/// A table is wrong when it measured nothing; when a planned pair is neither
/// a row nor listed as excluded by the gate, so a row went missing without a
/// word; when a row was served by another backend than the one it forced, so
/// its numbers are another backend's; and when a row is missing a column
/// ([`judge_band`]). A row over its band is not here: that is a catalog claim
/// to correct, flagged in the table.
pub fn table_failures(planned: usize, excluded: usize, rows: &[TableRow<'_>]) -> Vec<String> {
    let mut failures = Vec::new();
    if rows.is_empty() {
        failures.push(format!(
            "the table has no row: {planned} pair(s) were planned, {excluded} excluded by the \
             gate, and nothing was measured"
        ));
    }
    if rows.len() + excluded != planned {
        failures.push(format!(
            "{planned} pair(s) were planned and the table holds {} row(s) and {excluded} \
             exclusion(s): a row went missing without being listed",
            rows.len()
        ));
    }
    for row in rows {
        if !row.served_by.is_empty() && row.served_by != [row.forced.to_owned()] {
            failures.push(format!(
                "{}: forced `{}` and was served by {:?}",
                row.label, row.forced, row.served_by
            ));
        }
        if let Band::Missing(why) = row.band {
            failures.push(format!("{}: the row is missing a column: {why}", row.label));
        }
    }
    failures
}

/// Judges the resolver against the measurements of every backend of one
/// source.
///
/// The chain is ordered by what answers the question best, not by price
/// (`docs/contributing.md`), so a cheaper fallback that degrades the answer
/// does not overturn the choice: the price is the reason the fallback exists.
/// The choice is corrected only by a backend that is **clearly** cheaper --
/// its median below the chosen one's first quartile, so the two spreads do not
/// overlap on that side -- loses no more, and degrades nothing: declared
/// `equivalent`, or the chosen one is itself the fallback.
pub fn verdict(rows: &[Measured]) -> Verdict {
    let Some(chosen) = rows.iter().find(|row| row.chosen) else {
        return Verdict::Confirmed("the resolver chose a backend the bench could not run".into());
    };
    if rows.len() < 2 {
        return Verdict::Alone;
    }
    let mut notes = Vec::new();
    for other in rows.iter().filter(|row| !row.chosen) {
        if other.setting != chosen.setting {
            notes.push(format!(
                "`{}` was measured as {} against {}, and is not compared",
                other.backend, other.setting, chosen.setting
            ));
            continue;
        }
        let cheaper = other.cost.median < chosen.cost.q1;
        if !cheaper {
            notes.push(format!("`{}` is not clearly cheaper", other.backend));
            continue;
        }
        if other.losses > chosen.losses {
            notes.push(format!("`{}` is cheaper and loses more", other.backend));
            continue;
        }
        if other.equivalent || other.degraded.is_none() {
            return Verdict::Corrected(format!(
                "`{}` costs {} against the chosen `{}`'s {} and degrades nothing",
                other.backend,
                span(other.cost.median),
                chosen.backend,
                span(chosen.cost.median)
            ));
        }
        notes.push(format!(
            "`{}` is cheaper and degrades: {}",
            other.backend,
            other.degraded.as_deref().unwrap_or("")
        ));
    }
    Verdict::Confirmed(notes.join("; "))
}

// ------------------------------------------------------------ resolution

/// One pair of marker events, as the workload stamped them and as the tool
/// did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pair {
    /// The gap the workload measured between its own two stamps.
    pub own_gap_ns: i64,
    /// The gap between the tool's timestamps of the same two events, if both
    /// were seen.
    pub tool_gap_ns: Option<i64>,
}

/// What one gap of the sweep says about one backend.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub gap_ns: u64,
    pub pairs: usize,
    /// Pairs whose two events both reached the output.
    pub seen: usize,
    /// Pairs seen as two events, in order, a positive distance apart.
    pub distinguished: usize,
    /// The median of |tool gap - own gap| over the seen pairs.
    pub median_error_ns: Option<f64>,
    /// The median of tool gap - own gap, signed: a constant here is a bias
    /// between the two events' paths to the probe, not a lack of resolution.
    pub median_bias_ns: Option<f64>,
}

impl Resolved {
    pub fn from_pairs(gap_ns: u64, pairs: &[Pair]) -> Resolved {
        let seen: Vec<(i64, i64)> = pairs
            .iter()
            .filter_map(|pair| Some((pair.own_gap_ns, pair.tool_gap_ns?)))
            .collect();
        let errors: Vec<f64> = seen
            .iter()
            .map(|(own, tool)| (tool - own).unsigned_abs() as f64)
            .collect();
        Resolved {
            gap_ns,
            pairs: pairs.len(),
            seen: seen.len(),
            distinguished: seen.iter().filter(|(_, tool)| *tool > 0).count(),
            median_error_ns: Stats::of(&errors).map(|stats| stats.median),
            median_bias_ns: Stats::of(
                &seen
                    .iter()
                    .map(|(own, tool)| (tool - own) as f64)
                    .collect::<Vec<_>>(),
            )
            .map(|stats| stats.median),
        }
    }

    /// Two events this far apart are told apart: every pair seen, at least
    /// 95 % of them as two ordered events, and the tool's gap off the
    /// workload's by less than half the gap.
    pub fn resolves(&self) -> bool {
        self.pairs > 0
            && self.seen == self.pairs
            && self.distinguished * 100 >= self.pairs * 95
            && self
                .median_error_ns
                .is_some_and(|error| error <= self.gap_ns as f64 / 2.0)
    }
}

/// An event attribute by name.
pub fn attr<'a>(event: &'a Value, name: &str) -> Option<&'a Value> {
    event
        .get("attrs")?
        .as_array()?
        .iter()
        .find(|one| one.get("name").and_then(Value::as_str) == Some(name))?
        .get("value")
}

/// Pairs the workload's stamps with the tool's timestamps of the same events.
///
/// Both are on `CLOCK_MONOTONIC`, so the match is by absolute time: pair `i`
/// owns the tool events from its first stamp (less `early_ns`, for a clock
/// read the tool takes before the workload's own) until `window_ns` later, and
/// pairs are milliseconds apart while a gap is at most 50 us, so no event can
/// belong to two. A pair is seen when exactly two events fall in its window;
/// one, or three, is a pair the tool did not report as two events.
pub fn match_pairs(stamps: &[u64], tool_ts: &[u64], early_ns: u64, window_ns: u64) -> Vec<Pair> {
    let mut sorted = tool_ts.to_vec();
    sorted.sort_unstable();
    stamps
        .chunks_exact(2)
        .map(|pair| {
            let from = pair[0].saturating_sub(early_ns);
            let to = pair[0] + window_ns;
            let inside: Vec<u64> = sorted
                .iter()
                .copied()
                .filter(|ts| (from..=to).contains(ts))
                .collect();
            Pair {
                own_gap_ns: pair[1] as i64 - pair[0] as i64,
                tool_gap_ns: (inside.len() == 2).then(|| inside[1] as i64 - inside[0] as i64),
            }
        })
        .collect()
}

/// The smallest gap of a sweep that resolves, with every larger gap resolving
/// too: a backend that tells 2 µs apart and not 5 µs has no effective
/// resolution worth quoting, and the answer is then `None`.
pub fn effective_resolution(sweep: &[Resolved]) -> Option<u64> {
    let mut sorted: Vec<&Resolved> = sweep.iter().collect();
    sorted.sort_by_key(|one| one.gap_ns);
    let mut answer = None;
    for one in sorted.iter().rev() {
        if one.resolves() {
            answer = Some(one.gap_ns);
        } else {
            break;
        }
    }
    answer
}

// ----------------------------------------------------------------- output

/// A duration in ns, written the way the tool writes one.
pub fn span(ns: f64) -> String {
    let ns = ns.abs();
    if ns >= 1e9 {
        format!("{:.3}s", ns / 1e9)
    } else if ns >= 1e6 {
        format!("{:.2}ms", ns / 1e6)
    } else if ns >= 1e3 {
        format!("{:.2}us", ns / 1e3)
    } else {
        format!("{ns:.0}ns")
    }
}

/// A fresh directory for this run's documents.
pub fn run_dir(kind: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = bench_dir().join(format!("{kind}-{stamp}"));
    std::fs::create_dir_all(&dir).expect("the bench directory");
    dir
}

/// The environment the documents are stamped with: the tool's own capture,
/// which is what its resolved files carry, and what the bench adds to it.
pub fn environment_record(omn: &Omn) -> Value {
    let environment = omn::capabilities::Capabilities::detect()
        .environment()
        .clone();
    let conditions = Conditions::read();
    serde_json::json!({
        "environment": environment,
        "bench": {
            "omn": omn.describe(),
            "governors": conditions.governors,
            "boost": conditions.boost,
            "load_1m": conditions.load_1m,
            "llc_bytes": llc_bytes(),
            "eviction_bytes": Evictor::new().bytes(),
            "numa_nodes": numa_nodes(),
            "throttle_counters": throttle_count().map_or(
                Value::from("not published by this kernel for this cpu, so a throttled run cannot be told from one that was not"),
                Value::from,
            ),
            "workload_cpus": workload_cpus(1),
        }
    })
}

/// The bench's notes to itself, printed to stderr so `--nocapture` shows them.
pub fn say(line: impl AsRef<str>) {
    eprintln!("bench: {}", line.as_ref());
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

/// The measured baseline of each workload: warmup, then `repetitions` rounds
/// with the LLC evicted before each run. Throttled runs are dropped and
/// counted.
pub fn baselines(
    workloads: &[Workload],
    repetitions: usize,
) -> Result<(Vec<Baseline>, BTreeMap<String, usize>), String> {
    let mut evictor = Evictor::new();
    let mut samples: Vec<Vec<f64>> = vec![Vec::new(); workloads.len()];
    let pinned = workloads.iter().map(|w| w.threads).max().unwrap_or(1);
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let ticks_before = cpu_ticks();
    let mut throttled: BTreeMap<String, usize> = BTreeMap::new();
    for workload in workloads {
        for _ in 0..WARMUP {
            run_bare(workload)?;
        }
    }
    let mut measure = |index: usize, samples: &mut Vec<Vec<f64>>| -> Result<(), String> {
        evictor.evict();
        let run = run_bare(&workloads[index])?;
        if run.throttled {
            *throttled.entry(workloads[index].name.clone()).or_default() += 1;
        } else {
            samples[index].push(run.elapsed_ns() as f64);
        }
        Ok(())
    };
    for index in round_robin(workloads.len(), repetitions) {
        measure(index, &mut samples)?;
    }
    // A workload whose median does not resolve the target yet is given the
    // repetitions its spread asks for, up to the ceiling, round-robin among
    // the ones that need more -- never in a smoke run, which is under the
    // floor and refused anyway. The spread is re-read once the top-up is in,
    // because the first twenty runs estimate it only roughly.
    if repetitions >= MIN_REPETITIONS {
        loop {
            let short: Vec<(usize, usize)> = samples
                .iter()
                .enumerate()
                .filter_map(|(index, series)| {
                    let stats = Stats::of(series)?;
                    let needed = stats
                        .repetitions_to_resolve(RESOLUTION_TARGET)
                        .min(MAX_REPETITIONS);
                    (stats.resolution() > RESOLUTION_TARGET && stats.n < needed)
                        .then(|| (index, needed - stats.n))
                })
                .collect();
            if short.is_empty() {
                break;
            }
            let rounds = short.iter().map(|(_, more)| *more).max().unwrap_or(0);
            for round in 0..rounds {
                for (slot, _) in short.iter().enumerate() {
                    let (index, more) = short[(round + slot) % short.len()];
                    if round < more {
                        measure(index, &mut samples)?;
                    }
                }
            }
        }
    }
    *FOREIGN_BUSY.lock().unwrap_or_else(|e| e.into_inner()) =
        foreign_busy(&ticks_before, &cpu_ticks(), cpus.saturating_sub(pinned));
    let baselines = workloads
        .iter()
        .zip(samples)
        .filter_map(|(workload, samples)| {
            Some(Baseline {
                workload: workload.name.clone(),
                stats: Stats::of(&samples)?,
            })
        })
        .collect();
    Ok((baselines, throttled))
}
