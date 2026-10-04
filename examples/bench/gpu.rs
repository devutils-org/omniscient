//! What a GPU tracer costs: the bench's GPU half.
//!
//! Included by `tests/bench_gpu.rs` through `#[path]`, beside `harness.rs`,
//! whose statistics, runner pieces and plan it reuses.
//!
//! A GPU tracer's cost is not one number. A tracer that does work per kernel
//! costs a launch-bound program a large share of its time and a compute-bound
//! one almost nothing, so every row is measured on three regimes (`gpu.c`):
//! launch-bound, compute-bound and memory-bound.
//!
//! And it is measured against two baselines, because the CUDA driver loads an
//! injection library at `cuInit` whether or not it then traces anything:
//!
//! - **bare**: the workload alone;
//! - **control**: the workload with the injection library loaded and disarmed
//!   (`CUDA_INJECTION64_PATH` set, `OMN_CUPTI_REPORT=0`). What the dynamic
//!   loader costs is control against bare; what the tracer costs is armed
//!   against control. Without the control, the loader's cost would be
//!   published as CUPTI's.
//!
//! Every row is held to the band its backend declares on the worst of its
//! regimes, by the rule `docs/bench.md` writes down beside the record.
//!
//! The workload's threads are placed apart (`GPU_CPUS`): the launching thread
//! alone on one cpu for the timed section, every other thread -- the
//! driver's, a tracer's in the target -- on the others. On one shared cpu a
//! tracer thread's work would be the launching thread's wall time, and the
//! table could not say which thread a cost lands on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Instant;

use serde_json::{json, Value};

use crate::harness::{
    bench_dir, combinations, first_line, has_file_capabilities, resolves, root, say, workload_cpus,
    Method, Omn, Stats,
};

/// One GPU workload, in one regime.
#[derive(Debug, Clone)]
pub struct GpuWorkload {
    /// The name the table uses.
    pub name: &'static str,
    /// What bounds it, in words.
    pub regime: &'static str,
    pub args: Vec<String>,
}

/// The three regimes, sized to take 60 to 100 ms of timed section here: long
/// enough for a median of twenty to resolve a few percent, short enough that
/// a table of every GPU tracer takes minutes, not an hour.
///
/// `launch` is the worst case `gpu.kernel`'s stream named: 50 000 launches,
/// a kernel every two microseconds. `compute` is the common case: 50 kernels
/// of about 1.6 ms each.
pub fn gpu_workloads() -> Vec<GpuWorkload> {
    let new = |name, regime, args: &[&str]| GpuWorkload {
        name,
        regime,
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
    };
    vec![
        new("launch", "launch-bound", &["launches", "50000"]),
        new("compute", "compute-bound", &["compute", "50", "200000"]),
        new("memory", "memory-bound", &["memory", "200", "64"]),
    ]
}

/// How many cpus the GPU workload is given: the first for the launching
/// thread alone, the rest for every other thread of the process (`gpu.c`).
/// Four is more than the driver's and a tracer's threads keep busy here
/// (CUPTI's run left 17 ms of other threads' cpu over a 170 ms section).
pub const GPU_CPUS: usize = 4;

/// The GPU workload program, built once per test process and linked against
/// `libcuda.so.1` by name. `Err` is the machine's: no compiler, or no driver
/// library to link, worded the way `.github/expected-skips.txt` expects a
/// fixture without `-l:libcuda` to be.
pub fn gpu_binary() -> Result<PathBuf, String> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let dir = bench_dir();
            std::fs::create_dir_all(&dir)
                .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
            let path = dir.join("gpu");
            let partial = dir.join(format!("gpu.{}", std::process::id()));
            let built = Command::new("cc")
                .args(["-O2", "-Wall", "-o"])
                .arg(&partial)
                .arg(root().join("examples/bench/gpu.c"))
                .arg("-l:libcuda.so.1")
                .output()
                .map_err(|error| format!("no C compiler (`cc`) for the GPU workload: {error}"))?;
            if !built.status.success() {
                return Err(format!(
                    "the GPU bench's fixture did not build, most likely because this machine \
                     has no -l:libcuda.so.1 (no NVIDIA driver): {}",
                    first_line(&String::from_utf8_lossy(&built.stderr))
                ));
            }
            std::fs::rename(&partial, &path).map_err(|error| error.to_string())?;
            Ok(path)
        })
        .clone()
}

/// The injection library beside the `omn` the bench measures -- the one that
/// binary would load, so the control loads the same file the armed run does.
pub fn injection_library(omn: &Omn) -> Option<PathBuf> {
    let path = omn.path.parent()?.join("libomn_cupti.so");
    path.exists().then_some(path)
}

/// How one column of the table is run.
#[derive(Debug, Clone, PartialEq)]
pub enum Arm {
    /// The workload alone.
    Bare,
    /// The injection library loaded by the driver and disarmed: the negative
    /// control.
    Control(PathBuf),
    /// Under `omn`, with one (source, backend) forced.
    Served(Method),
    /// `nsys profile --trace=cuda`, the reference reader: measured, never
    /// served, never held to a band.
    Nsys(PathBuf),
}

impl Arm {
    pub fn label(&self) -> String {
        match self {
            Arm::Bare => "bare".into(),
            Arm::Control(_) => "control (library loaded, disarmed)".into(),
            Arm::Served(method) => method.label(),
            Arm::Nsys(_) => "nsys (reference)".into(),
        }
    }

    /// The baseline a served row's tracer is measured against: the control
    /// for a backend loaded through `CUDA_INJECTION64_PATH`, the bare run for
    /// any other -- a shim or a poll loads nothing the control would.
    pub fn baseline_is_control(&self) -> bool {
        matches!(self, Arm::Served(method) if method.backend.as_deref() == Some("vendor:cupti"))
    }
}

/// What one run said about itself, and what `omn` said it cost.
#[derive(Debug, Clone)]
pub struct GpuRun {
    pub report: Value,
    /// `cost.json`'s entry, under `omn`.
    pub cost: Option<Value>,
    /// `results.json`'s entry, under `omn`.
    pub results: Option<Value>,
}

impl GpuRun {
    pub fn field(&self, name: &str) -> Option<f64> {
        self.report.get(name)?.as_f64()
    }

    pub fn share_percent(&self) -> Option<f64> {
        self.cost.as_ref()?.get("measured_percent")?.as_f64()
    }

    pub fn collector_cpu_ns(&self) -> Option<f64> {
        self.cost
            .as_ref()?
            .pointer("/overhead/attributed_cpu_ns")?
            .as_f64()
    }

    pub fn served_by(&self) -> Option<String> {
        Some(self.cost.as_ref()?.get("backend")?.as_str()?.to_owned())
    }

    /// Every loss counter the tool reported, added up.
    pub fn losses(&self) -> u64 {
        self.losses_by_kind().values().sum()
    }

    /// Every loss counter the tool reported, by its name.
    pub fn losses_by_kind(&self) -> BTreeMap<String, u64> {
        self.results
            .as_ref()
            .and_then(|results| results.get("losses"))
            .and_then(Value::as_object)
            .map(|losses| {
                losses
                    .iter()
                    .filter_map(|(kind, count)| Some((kind.clone(), count.as_u64()?)))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn events(&self) -> u64 {
        self.results
            .as_ref()
            .and_then(|results| results.get("events_seen"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }
}

fn report_path(tag: &str) -> PathBuf {
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    bench_dir().join(format!(
        "report.{}.{call}.gpu-{tag}.json",
        std::process::id()
    ))
}

fn first_entry(dir: &Path, name: &str) -> Result<Value, String> {
    let text = std::fs::read_to_string(dir.join(name))
        .map_err(|error| format!("{} has no {name}: {error}", dir.display()))?;
    let value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    let entry = match name {
        "results.json" => value.get("results").and_then(|r| r.get(0)).cloned(),
        _ => value.get(0).cloned(),
    };
    entry.ok_or_else(|| format!("{name} holds no request"))
}

/// Runs `workload` under `arm`, and reads back what it and the tool wrote.
pub fn run_gpu(omn: &Omn, arm: &Arm, workload: &GpuWorkload) -> Result<GpuRun, String> {
    let binary = gpu_binary()?;
    let report = report_path("run");
    let out = bench_dir().join(format!("out.{}.gpu", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let mut command = match arm {
        Arm::Bare | Arm::Control(_) => Command::new(&binary),
        Arm::Served(method) => {
            if omn.capabilities && !has_file_capabilities(&omn.path) {
                panic!(
                    "{} lost its capabilities during the bench -- something relinked it",
                    omn.path.display()
                );
            }
            let mut command = Command::new(&omn.path);
            command
                .args(method.args())
                .args(["--format", "json", "-o"])
                .arg(&out)
                .arg("--")
                .arg(&binary);
            command
        }
        Arm::Nsys(nsys) => {
            let mut command = Command::new(nsys);
            command
                .args([
                    "profile",
                    "--trace=cuda",
                    "--sample=none",
                    "--cpuctxsw=none",
                    "--force-overwrite",
                    "true",
                    "-o",
                ])
                .arg(&out)
                .arg(&binary);
            command
        }
    };
    command
        .args(&workload.args)
        .env("BENCH_REPORT", &report)
        .env("BENCH_CPUS", workload_cpus(GPU_CPUS))
        .env_remove("CUDA_INJECTION64_PATH")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Arm::Control(library) = arm {
        command
            .env("CUDA_INJECTION64_PATH", library)
            .env("OMN_CUPTI_REPORT", "0");
    }
    let output = command.output().map_err(|error| error.to_string())?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        let _ = std::fs::remove_file(&report);
        return Err(format!(
            "`{}` on `{}` failed ({}):\n{stderr}",
            arm.label(),
            workload.name,
            output.status
        ));
    }
    let text = std::fs::read_to_string(&report)
        .map_err(|error| format!("the GPU workload wrote no report: {error}\n{stderr}"))?;
    let _ = std::fs::remove_file(&report);
    let report: Value = serde_json::from_str(&text)
        .map_err(|error| format!("unreadable report {text:?}: {error}"))?;
    let (cost, results) = match arm {
        Arm::Served(_) => (
            Some(first_entry(&out, "cost.json")?),
            Some(first_entry(&out, "results.json")?),
        ),
        _ => (None, None),
    };
    let _ = std::fs::remove_dir_all(&out);
    let _ = std::fs::remove_file(out.with_extension("gpu.nsys-rep"));
    Ok(GpuRun {
        report,
        cost,
        results,
    })
}

/// Whether this machine can run the GPU workload at all: `Err` carries the
/// machine's reason, which a test prints as its skip.
pub fn gpu_ready() -> Result<(), String> {
    let binary = gpu_binary()?;
    let output = Command::new(&binary)
        .args(["compute", "1", "1000"])
        .env("BENCH_GPU_WARM_MS", "0")
        .env("BENCH_REPORT", "/dev/null")
        .env_remove("CUDA_INJECTION64_PATH")
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(2) && stderr.contains("refused with CUDA error") {
        return Err(format!(
            "no NVIDIA GPU this driver will open here: {}",
            stderr.trim()
        ));
    }
    if stderr.contains("libcuda") {
        return Err(format!("no NVIDIA driver library here: {}", stderr.trim()));
    }
    panic!("the GPU workload failed on its own, without a tracer: {stderr}");
}

// ------------------------------------------------------------------ plan

/// A (source, backend) the GPU bench does not measure, and why.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Unplanned {
    pub source: String,
    pub backend: String,
    pub why: String,
    /// The pair has a collector, the tool accepted it here, and its trial run
    /// failed: a defect of the bench or of the collector, not of the machine,
    /// which fails the table rather than leaving a row out.
    pub failed_its_trial: bool,
}

/// A (source, backend) the GPU bench measures, with the setting it is asked
/// under.
#[derive(Debug, Clone)]
pub struct GpuPlanned {
    pub method: Method,
    pub declared: omn::catalog::Cost,
}

/// Every `gpu` (source, backend) this build serves for a launched program on
/// this machine, each with the first mode and emit the tool accepts and a
/// trial run completes, and every one it cannot, with why. NVML's polls are
/// planned too: the backends table measured them on a cpu workload, and
/// whether a poll's price moves with what the device is doing is a question
/// only a GPU workload answers.
pub fn gpu_plan(omn: &Omn) -> (Vec<GpuPlanned>, Vec<Unplanned>) {
    let catalog = omn::Catalog::embedded().expect("the embedded catalog loads");
    let trial = &gpu_workloads()[1];
    let mut planned = Vec::new();
    let mut unplanned = Vec::new();
    for entry in catalog.entries() {
        if entry.family != "gpu"
            || entry.target_support(omn::request::TargetKind::Launch)
                == omn::catalog::TargetSupport::No
        {
            continue;
        }
        let source = entry.id.to_string();
        let modes: Vec<String> = entry.modes.iter().map(|m| m.to_string()).collect();
        let emits: Vec<String> = entry.emits.iter().map(|e| e.to_string()).collect();
        for backend in &entry.backends {
            if !omn::collect::implements(&source, &backend.id) {
                unplanned.push(Unplanned {
                    source: source.clone(),
                    backend: backend.id.clone(),
                    why: "no collector is written for it".into(),
                    failed_its_trial: false,
                });
                continue;
            }
            let mut last = String::from("no mode and emit the source declares");
            let mut accepted = false;
            let found = combinations(&modes, &emits)
                .map(|(mode, emit)| Method {
                    source: source.clone(),
                    backend: Some(backend.id.clone()),
                    mode,
                    emit,
                    filter: None,
                    system: false,
                })
                .find(|method| {
                    if let Err(error) = resolves(omn, method) {
                        last = first_line(&error);
                        return false;
                    }
                    accepted = true;
                    match run_gpu(omn, &Arm::Served(method.clone()), trial) {
                        Ok(_) => true,
                        Err(error) => {
                            last = first_line(&error);
                            false
                        }
                    }
                });
            match found {
                Some(method) => planned.push(GpuPlanned {
                    method,
                    declared: backend.cost,
                }),
                None => unplanned.push(Unplanned {
                    source: source.clone(),
                    backend: backend.id.clone(),
                    why: last,
                    failed_its_trial: accepted,
                }),
            }
        }
    }
    (planned, unplanned)
}

// ------------------------------------------------------------- the rows

/// Every repetition of one (arm, workload).
#[derive(Debug, Default, Clone)]
pub struct Series {
    pub elapsed: Vec<f64>,
    pub section_cpu: Vec<f64>,
    /// The launching thread's cpu over the timed section.
    pub thread_cpu: Vec<f64>,
    /// Every other thread's: the process's less the launching thread's.
    pub others_cpu: Vec<f64>,
    pub init: Vec<f64>,
    pub exit: Vec<f64>,
    pub share: Vec<f64>,
    pub collector: Vec<f64>,
    pub events: Vec<f64>,
    pub losses: u64,
    /// The same losses by the counter that reported them.
    pub losses_by_kind: BTreeMap<String, u64>,
    pub served_by: Vec<String>,
    /// The largest number of other threads any run allowed on the launching
    /// thread's cpu: zero when the placement held.
    pub sharing: u64,
    /// What each run said of its injection library: 1 mapped, 0 not, -1 no
    /// `CUDA_INJECTION64_PATH`.
    pub injection: Vec<i64>,
}

impl Series {
    pub fn push(&mut self, run: &GpuRun) {
        let take = |into: &mut Vec<f64>, name: &str| {
            if let Some(value) = run.field(name) {
                into.push(value);
            }
        };
        take(&mut self.elapsed, "elapsed_ns");
        take(&mut self.section_cpu, "section_cpu_ns");
        take(&mut self.thread_cpu, "section_thread_cpu_ns");
        if let (Some(all), Some(own)) = (
            run.field("section_cpu_ns"),
            run.field("section_thread_cpu_ns"),
        ) {
            self.others_cpu.push(all - own);
        }
        self.sharing = self
            .sharing
            .max(run.field("threads_on_launch_cpu").unwrap_or(0.0) as u64);
        if let Some(injection) = run.report.get("injection").and_then(Value::as_i64) {
            self.injection.push(injection);
        }
        take(&mut self.init, "init_ns");
        take(&mut self.exit, "exit_ns");
        if let Some(share) = run.share_percent() {
            self.share.push(share);
        }
        if let Some(cpu) = run.collector_cpu_ns() {
            self.collector.push(cpu);
        }
        if run.cost.is_some() {
            self.events.push(run.events() as f64);
            self.losses += run.losses();
            for (kind, count) in run.losses_by_kind() {
                *self.losses_by_kind.entry(kind).or_default() += count;
            }
        }
        if let Some(backend) = run.served_by() {
            if !self.served_by.contains(&backend) {
                self.served_by.push(backend);
            }
        }
    }
}

fn median(values: &[f64]) -> Option<f64> {
    Stats::of(values).map(|stats| stats.median)
}

/// The difference of one median from another, in ms.
pub fn difference_ms(of: &[f64], from: &[f64]) -> Option<f64> {
    Some((median(of)? - median(from)?) / 1e6)
}

/// The relative change of one median against another, in percent.
pub fn dilation(measured: &[f64], baseline: &[f64]) -> Option<f64> {
    let (measured, baseline) = (median(measured)?, median(baseline)?);
    (baseline > 0.0).then(|| (measured / baseline - 1.0) * 100.0)
}

/// What a median of `baseline`'s spread and size can tell from zero, in
/// percent: the gate's measure (`Stats::resolution`), with the spread read
/// from the IQR (`IQR / 1.349`, a normal law's standard deviation) rather
/// than from the standard deviation.
///
/// The GPU table is taken on a device and a machine other work shares, and
/// one repetition another tenant held up moves a standard deviation and not
/// an IQR: the first table, at a load of 8, put the `compute` floor at 24 %
/// on a series whose IQR was 1.4 % of its median. The medians the table
/// compares are robust to that repetition; their floor should be too.
pub fn resolves_percent(baseline: &[f64]) -> Option<f64> {
    let stats = Stats::of(baseline)?;
    if stats.median <= 0.0 {
        return None;
    }
    let spread = stats.iqr() / 1.349 / stats.median;
    Some(crate::harness::resolution_at(spread, stats.n) * 100.0)
}

/// The cost a row is judged on: the largest of the share of the target's cpu
/// the tool reported, the dilation of the timed section and the dilation of
/// the process's cpu over it, each against the row's baseline. A dilation no
/// larger than what the baseline resolves reads as zero, as in the backends
/// record: the bench cannot tell it from zero. A negative one is zero too.
///
/// The cpu dilation is there because a tracer's own thread in the target --
/// CUPTI's flush thread -- costs the program cpu on another cpu than the
/// launching thread's (`GPU_CPUS` keeps them apart), which need not stretch
/// the timed section, and the tool's `cost.json` cannot see inside the
/// target ("a vendor profiler injects into the target, and says nothing of
/// what that cost it").
pub fn judged_cost(
    share: Option<f64>,
    elapsed_dilation: Option<f64>,
    cpu_dilation: Option<f64>,
    resolution: f64,
) -> Option<(f64, &'static str)> {
    let counted = |value: Option<f64>| value.map(|v| if v <= resolution { 0.0 } else { v });
    [
        (share, "share of the target's cpu"),
        (counted(elapsed_dilation), "dilation"),
        (counted(cpu_dilation), "cpu dilation"),
    ]
    .into_iter()
    .filter_map(|(value, from)| Some((value?, from)))
    .fold(
        None,
        |best: Option<(f64, &str)>, (value, from)| match best {
            Some((kept, _)) if kept >= value => best,
            _ => Some((value, from)),
        },
    )
}

/// The cheapest band whose ceiling holds `percent`.
pub fn band_for(percent: f64) -> omn::catalog::Cost {
    use omn::catalog::Cost;
    [Cost::Free, Cost::Low, Cost::Med, Cost::High, Cost::Extreme]
        .into_iter()
        .find(|band| percent <= band.worst_case_percent())
        .unwrap_or(Cost::Extreme)
}

/// One row of the GPU table: a served (source, backend) on one regime.
#[derive(Debug, Clone)]
pub struct GpuRow {
    pub source: String,
    pub backend: String,
    pub mode: String,
    pub workload: String,
    pub share: Option<f64>,
    pub dilation: Option<f64>,
    pub cpu_dilation: Option<f64>,
    /// The launching thread's cpu dilation: where a per-launch cost lands.
    pub thread_cpu_dilation: Option<f64>,
    /// Every other thread's cpu, against the baseline's, in ms.
    pub others_cpu_ms: Option<f64>,
    pub init_ms: Option<f64>,
    pub exit_ms: Option<f64>,
    pub against: &'static str,
    pub resolution: f64,
    /// Every record lost over every repetition, and every event seen.
    pub losses: u64,
    pub events_total: u64,
    pub events: Option<f64>,
    pub served_by: Vec<String>,
    pub declared: omn::catalog::Cost,
}

impl GpuRow {
    pub fn cost(&self) -> Option<(f64, &'static str)> {
        judged_cost(
            self.share,
            self.dilation,
            self.cpu_dilation,
            self.resolution,
        )
    }
}

/// What a workload's baselines resolve, in percent: the coarsest of the
/// bare and the control series' timed sections and cpu, so one floor holds
/// every row of the workload, whichever baseline it is measured against.
pub fn workload_resolution(bare: &Series, control: Option<&Series>) -> f64 {
    let mut series = vec![&bare.elapsed, &bare.section_cpu];
    if let Some(control) = control {
        series.push(&control.elapsed);
        series.push(&control.section_cpu);
    }
    series
        .into_iter()
        .map(|values| resolves_percent(values).unwrap_or(f64::INFINITY))
        .fold(0.0, f64::max)
}

/// Builds the row of `planned` on one workload from its series and its
/// baselines'.
pub fn row(
    planned: &GpuPlanned,
    workload: &str,
    series: &Series,
    bare: &Series,
    control: Option<&Series>,
    resolution: f64,
) -> GpuRow {
    let arm = Arm::Served(planned.method.clone());
    let (baseline, against) = match control {
        Some(control) if arm.baseline_is_control() => (control, "control"),
        _ => (bare, "bare"),
    };
    GpuRow {
        source: planned.method.source.clone(),
        backend: planned.method.backend.clone().unwrap_or_default(),
        mode: planned.method.mode.clone(),
        workload: workload.to_owned(),
        share: median(&series.share),
        dilation: dilation(&series.elapsed, &baseline.elapsed),
        cpu_dilation: dilation(&series.section_cpu, &baseline.section_cpu),
        thread_cpu_dilation: dilation(&series.thread_cpu, &baseline.thread_cpu),
        others_cpu_ms: difference_ms(&series.others_cpu, &baseline.others_cpu),
        init_ms: difference_ms(&series.init, &baseline.init),
        exit_ms: difference_ms(&series.exit, &baseline.exit),
        against,
        resolution,
        losses: series.losses,
        events_total: series.events.iter().sum::<f64>() as u64,
        events: median(&series.events),
        served_by: series.served_by.clone(),
        declared: planned.declared,
    }
}

/// Every reason the GPU table is wrong, which fails the test that wrote it: a
/// pair with a collector the tool accepted whose trial run failed, a planned
/// pair with no row on some workload, a row served by another backend than it
/// forced, a CUPTI row measured against anything but the control, a row with
/// nothing to judge it on, and a row that lost records -- a tracer that
/// dropped records under the load understates its own cost, so its row
/// cannot set a band. A row over its band is not here: it is a catalog claim
/// to correct, flagged in the table.
pub fn gpu_table_failures(
    planned: &[GpuPlanned],
    unplanned: &[Unplanned],
    rows: &[GpuRow],
    workloads: usize,
) -> Vec<String> {
    let mut failures = Vec::new();
    if rows.is_empty() {
        failures.push("the GPU table measured nothing".to_owned());
    }
    for pair in unplanned.iter().filter(|pair| pair.failed_its_trial) {
        failures.push(format!(
            "{} through {} was accepted here and failed its trial run, so it has no row: {}",
            pair.source, pair.backend, pair.why
        ));
    }
    for pair in planned {
        let backend = pair.method.backend.as_deref().unwrap_or("");
        let count = rows
            .iter()
            .filter(|row| row.source == pair.method.source && row.backend == backend)
            .count();
        if count != workloads {
            failures.push(format!(
                "{} through {backend} has {count} rows for {workloads} workloads",
                pair.method.source
            ));
        }
    }
    for row in rows {
        let what = format!("{} through {} on {}", row.source, row.backend, row.workload);
        if row.served_by.iter().any(|served| *served != row.backend) {
            failures.push(format!("{what} was served by {}", row.served_by.join(", ")));
        }
        if row.backend == "vendor:cupti" && row.against != "control" {
            failures.push(format!(
                "{what} is measured against {}, not the control: there is no injection \
                 library beside the binary, or no control series",
                row.against
            ));
        }
        if row.cost().is_none() {
            failures.push(format!(
                "{what} has neither a share nor a dilation to judge"
            ));
        }
        if lossy(row.losses, row.events_total) {
            failures.push(format!(
                "{what} lost {} of {} records: a tracer that drops records under the load \
                 understates its own cost",
                row.losses, row.events_total
            ));
        }
    }
    failures
}

/// The share of its records a row may lose and still set a band: one in a
/// hundred. A tracer that drops records under the load skips the work they
/// would have cost, so its row understates its price by about the share it
/// lost; under this one, by less than the floor of every regime here. Above
/// it, the row is refused rather than read as a cost. Every loss is shown
/// either way, in the table and in the record.
pub const MAX_LOSS_SHARE: f64 = 0.01;

/// Whether `losses` of `events` records is more than a row may lose.
pub fn lossy(losses: u64, events: u64) -> bool {
    losses > 0 && losses as f64 > events as f64 * MAX_LOSS_SHARE
}

/// What is wrong with one arm's series on one workload, whatever its row: a
/// run whose threads shared the launching thread's cpu (the placement did
/// not hold), and a run whose injection library is not the arm's -- a
/// control or a CUPTI run that never loaded it would measure a cost of
/// zero, and a bare run that did would not be bare.
pub fn series_failures(arm: &Arm, workload: &str, series: &Series) -> Vec<String> {
    let mut failures = Vec::new();
    let what = format!("{} on {workload}", arm.label());
    if series.sharing > 0 {
        failures.push(format!(
            "{what}: {} other threads were allowed on the launching thread's cpu",
            series.sharing
        ));
    }
    let wanted = match arm {
        Arm::Bare => Some(-1),
        Arm::Control(_) => Some(1),
        Arm::Served(method) if method.backend.as_deref() == Some("vendor:cupti") => Some(1),
        // `nsys` injects its own library; a shim or a poll none or whatever
        // the tool sets, which the row does not depend on.
        _ => None,
    };
    if let Some(wanted) = wanted {
        let off = series
            .injection
            .iter()
            .filter(|&&seen| seen != wanted)
            .count();
        if off > 0 || series.injection.len() != series.elapsed.len() {
            failures.push(format!(
                "{what}: {off} of {} runs did not {} the injection library ({:?})",
                series.elapsed.len(),
                if wanted == 1 { "load" } else { "go without" },
                series.injection
            ));
        }
    }
    failures
}

/// The worst regime of each (source, backend): what its band must hold.
pub fn worst_by_pair(rows: &[GpuRow]) -> BTreeMap<(String, String), (f64, String, &'static str)> {
    let mut worst: BTreeMap<(String, String), (f64, String, &'static str)> = BTreeMap::new();
    for row in rows {
        let Some((cost, from)) = row.cost() else {
            continue;
        };
        let key = (row.source.clone(), row.backend.clone());
        if worst.get(&key).is_none_or(|(kept, _, _)| cost > *kept) {
            worst.insert(key, (cost, row.workload.clone(), from));
        }
    }
    worst
}

// ------------------------------------------------------------- documents

pub fn percent_cell(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:+.2} %"),
        None => "-".into(),
    }
}

pub fn share_cell(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:.2} %"),
        None => "-".into(),
    }
}

pub fn ms_cell(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:+.1} ms"),
        None => "-".into(),
    }
}

/// The device's state, read by `nvidia-smi` -- an independent reader, and
/// never one that changes anything: the clocks, whether they are locked or
/// the application clocks set, the performance state, and every other
/// process holding a context on it.
pub fn device_state() -> Value {
    let query = |args: &[&str]| -> Value {
        match Command::new("nvidia-smi").args(args).output() {
            Ok(output) if output.status.success() => {
                Value::from(String::from_utf8_lossy(&output.stdout).trim().to_owned())
            }
            Ok(output) => Value::from(format!(
                "nvidia-smi failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Err(error) => Value::from(format!("no nvidia-smi: {error}")),
        }
    };
    json!({
        "device": query(&[
            "--query-gpu=name,driver_version,pstate,clocks.sm,clocks.max.sm,clocks.mem,\
             clocks.applications.graphics,clocks.default_applications.graphics,\
             persistence_mode,clocks_event_reasons.active",
            "--format=csv",
        ]),
        "other_tenants": query(&[
            "--query-compute-apps=pid,process_name,used_memory",
            "--format=csv",
        ]),
    })
}

/// When the table was written: seconds since the epoch, the stamp of its
/// directory.
pub fn stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Seconds since `started`, for the bench's notes.
pub fn since(started: Instant) -> String {
    format!("{:.0} s", started.elapsed().as_secs_f64())
}

/// Prints one note about a row as the bench goes.
pub fn note_row(row: &GpuRow) {
    say(format!(
        "{} via {} on {}: share {}, dilation {} / cpu {} / launching thread {} / others {} \
         against {} (resolves {:.2} %), init {}, exit {}, losses {}",
        row.source,
        row.backend,
        row.workload,
        share_cell(row.share),
        percent_cell(row.dilation),
        percent_cell(row.cpu_dilation),
        percent_cell(row.thread_cpu_dilation),
        ms_cell(row.others_cpu_ms),
        row.against,
        row.resolution,
        ms_cell(row.init_ms),
        ms_cell(row.exit_ms),
        row.losses
    ));
}
