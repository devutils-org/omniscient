//! `gpu.kernel` through CUPTI, held to what the workload did.
//!
//! The workloads are `tests/gpu_workload.py`: PTX loaded through `libcuda.so.1`
//! with ctypes, no toolkit and no compiler. Each one writes what it did --
//! kernels launched, copies made, streams used, and for `selftimed` each
//! kernel's own span read off the device's `%globaltimer` -- to the file
//! `OMN_GPU_TRUTH` names, and every assertion here compares the tool's answer
//! with that file.
//!
//! The device is shared: other test binaries, other worktrees and the desktop
//! may hold it while these run. So nothing here is a threshold tuned on a
//! quiet machine. Counts are exact, because a busy device runs the same
//! kernels; times are held to separations -- a kernel's span inside its
//! recorded duration, its start inside the host's launch window -- and a
//! failure says who else held the device.
//!
//! Only the machine skips: no `libcuda`, no device, or no CUPTI, each said.
//! A missing or broken injection library fails, because it is this build's
//! (`ready`). The refusal of a `pid` target is checked everywhere.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");
const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// One device: the tests of this binary take turns on it, so that what one
/// launches is not in another's window.
static DEVICE: Mutex<()> = Mutex::new(());

fn device() -> MutexGuard<'static, ()> {
    DEVICE.lock().unwrap_or_else(|held| held.into_inner())
}

fn omn(args: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(BINARY);
    command.current_dir(ROOT).args(args);
    for (name, value) in environment {
        command.env(name, value);
    }
    command.output().expect("omn should run")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Who else is on the device, for a failure message.
fn tenants() -> String {
    Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,process_name,used_memory",
            "--format=csv,noheader",
        ])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .map(|text| {
            if text.is_empty() {
                "no other compute process".to_owned()
            } else {
                text
            }
        })
        .unwrap_or_else(|error| format!("nvidia-smi did not run: {error}"))
}

/// Whether this machine can run the scenarios, probed once.
///
/// Only the machine may skip them: no `libcuda`, no device the driver will
/// open, or no CUPTI for the probe to find. Everything else -- the injection
/// library missing or broken, `OMN_CUPTI_INJECTION` wrong, the library failing
/// a CUDA call in the workload -- is this build's, and fails every scenario
/// rather than skipping them. So the question is asked in two steps that
/// cannot be confused: the workload alone, with no `omn` and no injection,
/// answers whether there is a device; then the workload under `omn` answers
/// whether the tool works, and the only refusal that is the machine's is the
/// resolver's "no viable backend" (no CUPTI).
fn ready() -> Result<(), String> {
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            let bare = Command::new("python3")
                .current_dir(ROOT)
                .args(["tests/gpu_workload.py", "memory", "1", "1"])
                .env_remove("CUDA_INJECTION64_PATH")
                .output()
                .expect("python3 should run");
            if !bare.status.success() {
                let complaint = stderr(&bare);
                let machine = [
                    "libcuda.so.1 is not loadable here",
                    "cuInit refused with CUDA error",
                    "cuDeviceGet refused with CUDA error",
                    "cuCtxCreate refused with CUDA error",
                ];
                if let Some(line) = complaint
                    .lines()
                    .find(|line| machine.iter().any(|reason| line.contains(reason)))
                {
                    return Err(format!("no NVIDIA GPU to trace here: {line}"));
                }
                panic!("the GPU workload failed on its own, without omn: {complaint}");
            }
            let truth = scratch("probe");
            let output = omn(
                &[
                    "trace",
                    "gpu.kernel",
                    "--emit",
                    "summary",
                    "--format",
                    "json",
                    "--",
                    "python3",
                    "tests/gpu_workload.py",
                    "memory",
                    "1",
                    "1",
                ],
                &[("OMN_GPU_TRUTH", truth.to_str().unwrap())],
            );
            if output.status.success() {
                return Ok(());
            }
            let complaint = stderr(&output);
            if let Some(line) = complaint
                .lines()
                .find(|line| line.contains("has no viable backend on this machine"))
            {
                let why: Vec<&str> = complaint
                    .lines()
                    .skip_while(|candidate| *candidate != line)
                    .take(3)
                    .collect();
                return Err(format!(
                    "no CUPTI beside this GPU to trace with: {}",
                    why.join(" ")
                ));
            }
            panic!(
                "the workload runs on this device alone, and gpu.kernel failed on it: that is \
                 this build's failure, not the machine's (is the injection library built? \
                 `make` builds it beside omn):\n{complaint}"
            );
        })
        .clone()
}

macro_rules! ready_or_skip {
    () => {
        if let Err(reason) = ready() {
            eprintln!("skipped: {reason}");
            return;
        }
    };
}

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("omn-gpu-kernel-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    directory.join(format!("{name}.json"))
}

/// One run of a workload under `gpu.kernel`: the result and the truth file.
struct Run {
    result: Value,
    truth: Value,
}

impl Run {
    fn events(&self) -> Vec<&Value> {
        self.result["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|output| output["emit"] == "raw")
            .and_then(|output| output["events"].as_array())
            .map(|events| events.iter().collect())
            .unwrap_or_default()
    }

    fn of_op(&self, op: &str) -> Vec<&Value> {
        let mut events: Vec<&Value> = self
            .events()
            .into_iter()
            .filter(|event| attr(event, "op").as_str() == Some(op))
            .collect();
        events.sort_by_key(|event| event["ts_ns"].as_u64().unwrap());
        events
    }

    fn dropped(&self) -> u64 {
        self.result["losses"]["dropped"].as_u64().unwrap()
    }
}

fn attr<'a>(event: &'a Value, name: &str) -> &'a Value {
    event["attrs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attr| attr["name"] == name)
        .map(|attr| &attr["value"])
        .unwrap_or(&Value::Null)
}

fn trace(name: &str, workload: &[&str], environment: &[(&str, &str)]) -> Run {
    let truth = scratch(name);
    let _ = std::fs::remove_file(&truth);
    let mut args = vec![
        "trace",
        "gpu.kernel",
        "--emit",
        "raw",
        "--format",
        "json",
        "--",
        "python3",
        "tests/gpu_workload.py",
    ];
    args.extend_from_slice(workload);
    let mut environment = environment.to_vec();
    environment.push(("OMN_GPU_TRUTH", truth.to_str().unwrap()));
    let output = omn(&args, &environment);
    assert!(
        output.status.success(),
        "{workload:?} under gpu.kernel failed: {}",
        stderr(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("json results");
    let truth: Value = serde_json::from_str(
        &std::fs::read_to_string(&truth).expect("the workload writes its truth file"),
    )
    .unwrap();
    Run {
        result: document["results"][0].clone(),
        truth,
    }
}

/// Every workload's own count of kernels, copies and memsets is the tool's
/// count, exactly, and nothing was lost.
fn holds_counts(run: &Run, workload: &str) {
    let expected: u64 = run.truth["kernels"]
        .as_object()
        .unwrap()
        .values()
        .map(|count| count.as_u64().unwrap())
        .sum();
    let kernels = run.of_op("kernel");
    assert_eq!(
        kernels.len() as u64,
        expected,
        "{workload}: {} kernels reported, {expected} launched; {} dropped; device: {}",
        kernels.len(),
        run.dropped(),
        tenants()
    );
    for (name, count) in run.truth["kernels"].as_object().unwrap() {
        let named = kernels
            .iter()
            .filter(|event| attr(event, "kernel").as_str() == Some(name))
            .count() as u64;
        assert_eq!(
            named,
            count.as_u64().unwrap(),
            "{workload}: kernel `{name}`"
        );
    }
    assert_eq!(
        run.of_op("memcpy").len() as u64,
        run.truth["memcpy"].as_u64().unwrap(),
        "{workload}: memcpy count"
    );
    assert_eq!(
        run.of_op("memset").len() as u64,
        run.truth["memset"].as_u64().unwrap(),
        "{workload}: memset count"
    );
    assert_eq!(run.dropped(), 0, "{workload}: {}", run.result["losses"]);
    for event in run.events() {
        assert!(
            event["ts_err_ns"].as_u64().unwrap() > 0,
            "{workload}: a converted timestamp claims to be exact: {event}"
        );
        assert!(attr(event, "duration_ns").as_u64().is_some());
    }
}

#[test]
fn a_pid_target_is_refused_and_the_entry_says_why() {
    let output = omn(
        &[
            "trace",
            "gpu.kernel",
            "--pid",
            &std::process::id().to_string(),
            "--emit",
            "summary",
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let complaint = stderr(&output);
    assert!(complaint.contains("per pid"), "{complaint}");
    // The refusal itself carries the entry's reason, not only `omn list`.
    assert!(
        complaint.contains("CUDA_INJECTION64_PATH") && !complaint.contains("declares no"),
        "{complaint}"
    );
    let listing = omn(&["list", "gpu.kernel"], &[]);
    let text = String::from_utf8_lossy(&listing.stdout);
    assert!(text.contains("CUDA_INJECTION64_PATH"), "{text}");
    assert!(text.contains("refused for that reason"), "{text}");
}

#[test]
fn the_launch_bound_load_is_counted_kernel_for_kernel() {
    ready_or_skip!();
    let _device = device();
    let run = trace("launches", &["launches", "20000"], &[]);
    holds_counts(&run, "launches 20000");
}

/// A program that forks after `cuInit`: the child inherits the library's
/// `atexit` handler and a copy of its state without its threads. The child
/// exits as it would without `omn`, sends nothing, and every kernel is the
/// parent's, counted once.
#[test]
fn a_child_forked_after_cuinit_exits_and_reports_nothing() {
    ready_or_skip!();
    let _device = device();
    let run = trace("forked", &["forked", "500"], &[]);
    holds_counts(&run, "forked 500");
    let child = run.truth["child"].as_u64().unwrap();
    let pids: std::collections::BTreeSet<u64> = run
        .events()
        .iter()
        .map(|event| attr(event, "pid").as_u64().unwrap())
        .collect();
    assert_eq!(pids.len(), 1, "one process ran kernels: {pids:?}");
    assert!(
        !pids.contains(&child),
        "the child {child} is credited: {pids:?}"
    );
}

#[test]
fn the_compute_bound_load_is_counted_kernel_for_kernel() {
    ready_or_skip!();
    let _device = device();
    let run = trace("compute", &["compute", "50"], &[]);
    holds_counts(&run, "compute 50");
}

/// The memory-bound load also moves a chosen number of bytes per copy and
/// per memset, and the copies are named by their direction.
#[test]
fn the_memory_bound_load_is_counted_with_its_bytes() {
    ready_or_skip!();
    let _device = device();
    let run = trace("memory", &["memory", "10", "8"], &[]);
    holds_counts(&run, "memory 10 8");
    let copies = run.of_op("memcpy");
    let bytes: Vec<u64> = copies
        .iter()
        .map(|event| attr(event, "bytes").as_u64().unwrap())
        .collect();
    let expected: Vec<u64> = run.truth["memcpy_bytes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_u64().unwrap())
        .collect();
    assert_eq!(bytes, expected);
    let directions: Vec<&str> = copies
        .iter()
        .map(|event| attr(event, "kernel").as_str().unwrap())
        .collect();
    assert_eq!(directions, ["[memcpy HtoD]", "[memcpy DtoH]"]);
    assert_eq!(
        attr(run.of_op("memset")[0], "bytes").as_u64(),
        run.truth["memset_bytes"][0].as_u64()
    );
}

/// Managed memory migrates between every two kernels; the migrations are not
/// kernels, and are not counted as any.
#[test]
fn the_managed_memory_load_counts_kernels_and_not_migrations() {
    ready_or_skip!();
    let _device = device();
    let run = trace("managed", &["managed", "10", "8"], &[]);
    holds_counts(&run, "managed 10 8");
}

/// Kernels on distinct streams stay on distinct streams, each with exactly
/// the launches the workload put there.
#[test]
fn the_multi_stream_load_keeps_its_streams_apart() {
    ready_or_skip!();
    let _device = device();
    let run = trace("streams", &["streams", "4", "25"], &[]);
    holds_counts(&run, "streams 4 25");
    let mut per_stream = std::collections::BTreeMap::<u64, u64>::new();
    for event in run.of_op("kernel") {
        *per_stream
            .entry(attr(event, "stream").as_u64().unwrap())
            .or_default() += 1;
    }
    assert_eq!(
        per_stream.len() as u64,
        run.truth["streams"].as_u64().unwrap(),
        "{per_stream:?}"
    );
    let each = run.truth["per_stream"].as_u64().unwrap();
    assert!(
        per_stream.values().all(|count| *count == each),
        "{per_stream:?}"
    );
    // And they ran at once: four blocks a kernel leave the device room for
    // all four streams, so some kernel starts before one on another stream
    // has ended. A trace that serialised them would show none.
    let mut spans: Vec<(u64, u64, u64)> = run
        .of_op("kernel")
        .iter()
        .map(|event| {
            let start = event["ts_ns"].as_u64().unwrap();
            let end = start + attr(event, "duration_ns").as_u64().unwrap();
            (start, end, attr(event, "stream").as_u64().unwrap())
        })
        .collect();
    spans.sort();
    let overlapping = spans
        .windows(2)
        .filter(|pair| pair[1].0 < pair[0].1 && pair[1].2 != pair[0].2)
        .count();
    assert!(
        overlapping > 0,
        "no two kernels on distinct streams overlapped; device: {}",
        tenants()
    );
}

/// Each kernel's recorded duration against the kernel's own span, read off
/// `%globaltimer` by its blocks; and its converted start and end inside the
/// host's `CLOCK_MONOTONIC` window around the launch.
///
/// The span is a lower bound on the duration: CUPTI's kernel starts before
/// the first block's first instruction and ends after the last block's last.
/// Measured on a 2080 Ti the difference is 1.0 to 1.3 us on a 1 ms kernel;
/// the bound asserted is ten microseconds, or 1 % of the span on a device
/// slowed by another tenant -- a separation, not that measurement.
#[test]
fn each_kernel_lasts_its_own_span_and_starts_inside_its_launch() {
    ready_or_skip!();
    let _device = device();
    let run = trace("selftimed", &["selftimed", "20", "100000"], &[]);
    holds_counts(&run, "selftimed 20 100000");
    let kernels = run.of_op("kernel");
    let spans = run.truth["selftimed"].as_array().unwrap();
    // The first launch is the workload's warm-up, outside the record.
    assert_eq!(kernels.len(), spans.len() + 1);
    for (event, truth) in kernels[1..].iter().zip(spans) {
        let duration = attr(event, "duration_ns").as_u64().unwrap() as i64;
        let error = event["ts_err_ns"].as_u64().unwrap() as i64;
        let start = event["ts_ns"].as_u64().unwrap() as i64;
        let span = truth["span_ns"].as_i64().unwrap();
        let before = truth["host_before_ns"].as_i64().unwrap();
        let after = truth["host_after_ns"].as_i64().unwrap();
        let slack = 10_000i64.max(span / 100);
        assert!(
            duration + error >= span && duration - span <= slack,
            "duration {duration} ns against the kernel's own span {span} ns (error {error}); \
             device: {}",
            tenants()
        );
        assert!(
            start + error >= before && start + duration <= after + error,
            "kernel [{start}, {}] outside the host's window [{before}, {after}] (error \
             {error}); device: {}",
            start + duration,
            tenants()
        );
    }
}

/// CUPTI drops when it has no buffer, and the run says how many. With two
/// buffers of 64 KiB under 50 000 launches it must drop, and what was kept
/// and what was dropped add up to what was launched, exactly: the count is
/// CUPTI's own, not an estimate. With the default buffers the same load
/// drops nothing.
#[test]
fn a_launch_bound_load_that_overflows_the_buffers_says_how_many_it_lost() {
    ready_or_skip!();
    let _device = device();
    let starved = trace(
        "starved",
        &["launches", "50000"],
        &[
            ("OMN_CUPTI_BUFFERS", "2"),
            ("OMN_CUPTI_BUFFER_BYTES", "65536"),
        ],
    );
    let kept = starved.of_op("kernel").len() as u64;
    let dropped = starved.dropped();
    assert!(dropped > 0, "two 64 KiB buffers held 50 000 launches");
    assert_eq!(kept + dropped, 50_000, "kept {kept}, dropped {dropped}");
    let reasons = starved.result["losses"]["reasons"].to_string();
    assert!(reasons.contains("CUPTI had no buffer"), "{reasons}");

    let fed = trace("fed", &["launches", "50000"], &[]);
    assert_eq!(fed.dropped(), 0, "{}", fed.result["losses"]);
    assert_eq!(fed.of_op("kernel").len(), 50_000);
}

/// A program that never initialises CUDA reports no activity, and says why
/// rather than reporting an idle device.
#[test]
fn a_program_that_never_touches_the_gpu_says_so() {
    ready_or_skip!();
    let run = trace("idle", &["idle", "10"], &[]);
    assert!(run.events().is_empty());
    let notes = run.result["degradations"].to_string();
    assert!(
        notes.contains("never loaded the CUDA injection library"),
        "{notes}"
    );
}

/// Two `gpu.kernel` requests on one launch -- a summary and a ranking -- give
/// the program one injection library and one socket, and each reads every
/// record: the same counts, not half each.
#[test]
fn two_requests_on_one_launch_each_read_every_record() {
    ready_or_skip!();
    let _device = device();
    let session = scratch("two-requests").with_extension("yml");
    let launch = "[python3, tests/gpu_workload.py, memory, \"3\", \"1\"]";
    std::fs::write(
        &session,
        format!(
            "name: two readers\nformat: json\nrequests:\n  \
             - {{label: one, source: gpu.kernel, target: {{launch: {launch}}}, mode: trace, \
             emit: [summary]}}\n  \
             - {{label: two, source: gpu.kernel, target: {{launch: {launch}}}, mode: trace, \
             emit: [raw]}}\n"
        ),
    )
    .unwrap();
    let output = omn(&["-f", session.to_str().unwrap()], &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let document: Value = serde_json::from_slice(&output.stdout).expect("json results");
    let results = document["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for result in results {
        assert_eq!(result["events_seen"], 6, "{}", result["label"]);
    }
}
