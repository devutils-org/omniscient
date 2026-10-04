//! Inside a kernel: `gpu.counters` and `gpu.profile` through CUPTI, held to
//! what the workload did.
//!
//! The workloads are `tests/gpu_inside.py` -- `hotloop`, a kernel whose time
//! is on one known source line -- and the memory- and compute-bound loads of
//! `tests/gpu_workload.py` it hands everything else to: PTX loaded through
//! `libcuda.so.1` with ctypes, no toolkit and no compiler.
//!
//! **The device is shared**, and the device counts what its SMs and memory
//! do whoever's work it is: another process's kernels in a launch's window
//! are counted with the launch's (measured, a copy kernel's instruction count
//! went from 3.2 million to 286 million on every launch of a run). The
//! library's witness finds those launches and the collector leaves them out;
//! the verdicts the brief asks for -- instructions reproducible on both
//! workloads, DRAM traffic reproducible on the memory-bound one and not
//! dominated on the compute-bound one -- are held on the launches it found
//! undisturbed, and a failure message names the device's tenants.
//!
//! Only the machine skips: no `libcuda`, no device, or no CUPTI, each said.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");
const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// One device: the tests of this binary take turns on it.
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

/// Whether this machine can run the scenarios, probed once: the workload
/// alone says whether there is a device, then the workload under `omn`
/// whether there is CUPTI. Anything else is this build's, and fails.
fn ready() -> Result<(), String> {
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            let bare = Command::new("python3")
                .current_dir(ROOT)
                .args(["tests/gpu_inside.py", "hotloop", "1", "10"])
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
                    return Err(format!("no NVIDIA GPU to look inside here: {line}"));
                }
                panic!("the GPU workload failed on its own, without omn: {complaint}");
            }
            let output = omn(
                &[
                    "trace",
                    "gpu.profile",
                    "--emit",
                    "raw",
                    "--format",
                    "json",
                    "--",
                    "python3",
                    "tests/gpu_inside.py",
                    "hotloop",
                    "1",
                    "10",
                ],
                &[],
            );
            if output.status.success() {
                return Ok(());
            }
            let complaint = stderr(&output);
            if let Some(line) = complaint
                .lines()
                .find(|line| line.contains("has no viable backend on this machine"))
            {
                return Err(format!(
                    "no CUPTI beside this GPU to look inside with: {line}"
                ));
            }
            panic!(
                "the workload runs on this device alone, and gpu.profile failed on it: that is \
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
    let directory = std::env::temp_dir().join(format!("omn-gpu-inside-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    directory.join(format!("{name}.json"))
}

/// One run of a workload under one inside source.
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

    fn facts(&self) -> String {
        self.result["notes"]
            .as_array()
            .map(|facts| {
                facts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    /// The event of one counter of one kernel.
    fn counter(&self, kernel: &str, counter: &str) -> &Value {
        self.events()
            .into_iter()
            .find(|event| {
                attr(event, "kernel").as_str() == Some(kernel)
                    && attr(event, "counter").as_str() == Some(counter)
            })
            .unwrap_or_else(|| panic!("no {counter} for {kernel}: {:#}", self.result))
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

fn number(event: &Value, name: &str) -> f64 {
    attr(event, name)
        .as_f64()
        .unwrap_or_else(|| panic!("no number `{name}` on {event}"))
}

fn run(source: &str, script: &str, workload: &[&str], environment: &[(&str, &str)]) -> Run {
    let truth = scratch(&format!("{source}-{}", workload.join("-")));
    let _ = std::fs::remove_file(&truth);
    let mut args = vec![
        "trace", source, "--emit", "raw", "--format", "json", "--", "python3", script,
    ];
    args.extend_from_slice(workload);
    let mut environment = environment.to_vec();
    environment.push(("OMN_GPU_TRUTH", truth.to_str().unwrap()));
    let output = omn(&args, &environment);
    assert!(
        output.status.success(),
        "{workload:?} under {source} failed: {}",
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

const INSTRUCTIONS: &str = "smsp__inst_executed.sum";
const DRAM_READ: &str = "dram__bytes_read.sum";

/// The two workloads disagree about the same counter: DRAM bytes repeat on
/// the memory-bound kernel, which moves 64 MiB a launch, and are whatever
/// else touched video memory on the compute-bound one, which reads nothing.
/// Instructions repeat on both, bit for bit, on every launch nothing else
/// disturbed.
///
/// The device is shared, and another process's kernels in a launch's window
/// add hundreds of millions of instructions to it (measured, while another
/// worktree looped GPU programs that each lived under a second: up to every
/// launch of twelve, by the same amount each time, so the disturbed count
/// repeated too). The library's witness -- the blocks launched in the
/// window against the launch's grid -- finds those launches and the
/// collector leaves them out; what is held here is that every launch is
/// either measured or counted as disturbed, and that the verdicts hold on
/// the measured ones. A run with fewer than four measured launches of a
/// kernel has nothing to judge: the next of three attempts is taken, and
/// when none had enough the test **skips**, through the skip gate, naming
/// the device's tenants -- never a green pass that asserted nothing.
#[test]
fn a_counter_s_verdict_is_held_to_two_workloads_that_disagree_about_it() {
    ready_or_skip!();
    let _device = device();
    let mut disturbed = Vec::new();
    for attempt in 1..=3 {
        let memory = run(
            "gpu.counters",
            "tests/gpu_workload.py",
            &["memory", "12", "64"],
            &[],
        );
        let compute = run(
            "gpu.counters",
            "tests/gpu_workload.py",
            &["compute", "12"],
            &[],
        );
        let others = || {
            format!(
                "on the device now: {}; the runs said:\n{}\n{}",
                tenants(),
                memory.facts(),
                compute.facts()
            )
        };
        for run in [&memory, &compute] {
            assert_eq!(
                run.result["losses"]["dropped"], 0,
                "{:#}",
                run.result["losses"]
            );
            for event in run.events() {
                assert_eq!(
                    number(event, "launches") + number(event, "disturbed"),
                    12.0,
                    "{event}"
                );
                assert!(event["ts_err_ns"].as_u64().unwrap() > 0);
            }
        }

        let measured = |run: &Run, kernel: &str| number(run.counter(kernel, DRAM_READ), "launches");
        if measured(&memory, "copy") < 4.0 || measured(&compute, "spin") < 4.0 {
            disturbed.push(format!(
                "attempt {attempt}: {} and {} launches of 12 undisturbed; {}",
                measured(&memory, "copy"),
                measured(&compute, "spin"),
                others()
            ));
            continue;
        }
        let copy_dram = memory.counter("copy", DRAM_READ);
        let spin_dram = compute.counter("spin", DRAM_READ);
        // At least the bytes the kernel reads, every launch: 64 MiB.
        assert!(
            number(copy_dram, "min") >= (64u64 << 20) as f64,
            "{copy_dram}"
        );
        let spread = |event: &Value| attr(event, "spread").as_f64().unwrap_or(f64::INFINITY);
        assert!(
            spread(copy_dram) * 10.0 < spread(spin_dram),
            "the memory-bound kernel's DRAM bytes should spread far less than the \
             compute-bound one's: {copy_dram} against {spin_dram}; {}",
            others()
        );
        assert_eq!(
            attr(copy_dram, "verdict"),
            "reproducible here",
            "{copy_dram}; {}",
            others()
        );
        assert_eq!(
            attr(spin_dram, "verdict"),
            "not dominated by this workload",
            "{spin_dram}"
        );

        for (run, kernel) in [(&memory, "copy"), (&compute, "spin")] {
            let instructions = run.counter(kernel, INSTRUCTIONS);
            assert_eq!(
                attr(instructions, "verdict"),
                "reproducible here",
                "{instructions}; {}",
                others()
            );
            // Bit for bit, on every launch the witness found undisturbed.
            assert_eq!(
                number(instructions, "min"),
                number(instructions, "max"),
                "an instruction count repeats bit for bit: {instructions}; {}",
                others()
            );
        }
        return;
    }
    eprintln!(
        "skipped: the device was never quiet enough to judge a counter: in three attempts \
         other work disturbed all but three launches of a kernel, so no verdict was the \
         workload's; on the device now: {}\n  {}",
        tenants(),
        disturbed.join("\n  ")
    );
}

/// Counters replay every launch, so a run that would also time kernels is
/// refused before the program runs, and so is one with PC sampling, which
/// CUPTI cannot run in the same process.
#[test]
fn counters_beside_kernel_times_or_pc_sampling_are_refused() {
    let session = scratch("refused").with_extension("yml");
    let launch = "[python3, tests/gpu_workload.py, compute, \"1\"]";
    for other in ["gpu.kernel", "gpu.profile"] {
        std::fs::write(
            &session,
            format!(
                "name: refused\nformat: json\nrequests:\n  \
                 - {{label: counters, source: gpu.counters, target: {{launch: {launch}}}, \
                 mode: trace, emit: [raw]}}\n  \
                 - {{label: other, source: {other}, target: {{launch: {launch}}}, \
                 mode: trace, emit: [raw]}}\n"
            ),
        )
        .unwrap();
        let output = omn(&["-f", session.to_str().unwrap()], &[]);
        let complaint = stderr(&output);
        if ready().is_err() {
            // No device or no CUPTI: the resolver refuses first, which is
            // the machine's answer and still a refusal.
            assert!(!output.status.success(), "{complaint}");
            continue;
        }
        assert!(!output.status.success(), "{complaint}");
        assert!(
            complaint.contains(&format!("gpu.counters cannot be asked with {other}")),
            "{complaint}"
        );
    }
}

/// Every inside source needs the program launched under it.
#[test]
fn a_pid_target_is_refused_with_the_reason() {
    for source in ["gpu.counters", "gpu.profile"] {
        let output = omn(&["trace", source, "--pid", "1", "--duration", "1s"], &[]);
        assert!(!output.status.success());
        let complaint = stderr(&output);
        // Where CUPTI resolves, the refusal is the pid's, not the machine's.
        let reason = complaint.contains("CUDA_INJECTION64_PATH")
            || (ready().is_err() && complaint.contains("no viable backend"));
        assert!(reason, "{source}: {complaint}");
    }
}

/// The hot loop's line holds most of the samples, and more than every other
/// line together; the lines that hold nothing worth a sample hold almost
/// nothing. Every sample counted is CUPTI's.
#[test]
fn the_known_hot_loop_comes_first() {
    ready_or_skip!();
    let _device = device();
    let run = run(
        "gpu.profile",
        "tests/gpu_inside.py",
        &["hotloop", "6", "100000"],
        &[],
    );
    let hot = run.truth["hot_line"].as_u64().unwrap();
    let file = run.truth["file"].as_str().unwrap();
    let mut by_line: std::collections::BTreeMap<u64, f64> = Default::default();
    let mut total = 0.0;
    for event in run.events() {
        assert_eq!(attr(event, "kernel"), "hotloop", "{event}");
        assert_eq!(attr(event, "file"), file, "{event}");
        assert!(event["ts_err_ns"].as_u64().unwrap() > 0);
        let samples = number(event, "samples");
        *by_line
            .entry(attr(event, "line").as_u64().unwrap())
            .or_default() += samples;
        total += samples;
    }
    assert!(total > 0.0, "{:#}", run.result);
    let first = by_line
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(line, _)| *line);
    assert_eq!(
        first,
        Some(hot),
        "{by_line:?}; on the device: {}",
        tenants()
    );
    assert!(
        by_line[&hot] > total / 2.0,
        "the hot line should hold most samples: {by_line:?}"
    );
    for cold in run.truth["cold_lines"].as_array().unwrap() {
        let cold = by_line.get(&cold.as_u64().unwrap()).copied().unwrap_or(0.0);
        assert!(cold * 100.0 < by_line[&hot], "{by_line:?}");
    }
    let facts = run.facts();
    assert!(
        facts.contains("100.0 % of them on a source line"),
        "{facts}"
    );
    assert!(facts.contains("a sample every 2^12 cycles"), "{facts}");
}

/// The program counter holding the most samples, and every offset that
/// holds any.
fn offsets(run: &Run) -> (u64, std::collections::BTreeMap<u64, f64>) {
    let mut by_pc: std::collections::BTreeMap<u64, f64> = Default::default();
    for event in run.events() {
        *by_pc.entry(number(event, "pc") as u64).or_default() += number(event, "samples");
    }
    let hottest = by_pc
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(pc, _)| *pc)
        .expect("samples");
    (hottest, by_pc)
}

/// A kernel compiled without line information is placed by offset, and the
/// run says which kernel had no lines. The offsets are the hot loop's: the
/// same PTX compiled with line information samples hottest at the same
/// offset, and that offset is on the hot line.
#[test]
fn a_kernel_without_line_information_is_placed_by_offset_and_named() {
    ready_or_skip!();
    let _device = device();
    let bare = run(
        "gpu.profile",
        "tests/gpu_inside.py",
        &["hotloop-bare", "3", "100000"],
        &[],
    );
    let events = bare.events();
    assert!(!events.is_empty(), "{:#}", bare.result);
    for event in &events {
        assert!(attr(event, "line").is_null(), "{event}");
        let source = attr(event, "source").as_str().unwrap();
        assert_eq!(
            source,
            format!("+0x{:x}", number(event, "pc") as u64),
            "{event}"
        );
    }
    let facts = bare.facts();
    assert!(facts.contains("no line information in hotloop"), "{facts}");

    let lined = run(
        "gpu.profile",
        "tests/gpu_inside.py",
        &["hotloop", "3", "100000"],
        &[],
    );
    let (bare_hottest, _) = offsets(&bare);
    let (lined_hottest, _) = offsets(&lined);
    assert_eq!(
        bare_hottest,
        lined_hottest,
        "the same code samples hottest at the same offset, lines or not; on the device: {}",
        tenants()
    );
    let hot = lined.truth["hot_line"].as_u64().unwrap();
    let on_hot_line = lined.events().into_iter().any(|event| {
        number(event, "pc") as u64 == lined_hottest && attr(event, "line").as_u64() == Some(hot)
    });
    assert!(
        on_hot_line,
        "offset {lined_hottest:#x} is not on line {hot}"
    );
}

/// The samples are held to the kernel's own account of its cycles. Every SM
/// takes one sample every `2^n` of its cycles, so a launch of `selftimed`
/// -- one block per SM, each block reading `clock64` before and after its
/// work and writing `%smid` -- should take its blocks' cycles times the SMs
/// it ran on, over `2^n`. The workload launches once more than it records
/// (a warm-up of the same length), so the reference is scaled by that.
/// Measured: 1.001 of the reference at `2^12`, 1.001 at `2^14`; held within
/// 10 %, over the samples of `selftimed` alone (another process's kernels
/// are CUPTI's `non_user` count, not the program's).
#[test]
fn the_samples_are_the_kernel_s_own_cycles_over_the_period() {
    ready_or_skip!();
    let _device = device();
    for period in [12u32, 14] {
        let run = run(
            "gpu.profile",
            "tests/gpu_inside.py",
            &["selftimed", "10", "100000"],
            &[("OMN_CUPTI_PC_PERIOD", &period.to_string())],
        );
        let samples: f64 = run
            .events()
            .into_iter()
            .filter(|event| attr(event, "kernel") == "selftimed")
            .map(|event| number(event, "samples"))
            .sum();
        let launches = run.truth["selftimed"].as_array().unwrap();
        let cycles: f64 = launches
            .iter()
            .map(|launch| number_of(launch, "max_block_cycles") * number_of(launch, "sms"))
            .sum();
        let recorded = launches.len() as f64;
        let reference = cycles / f64::from(1u32 << period) * (recorded + 1.0) / recorded;
        let ratio = samples / reference;
        assert!(
            (0.9..=1.1).contains(&ratio),
            "2^{period}: {samples} samples against {reference:.0} from the kernel's own cycles \
             ({ratio:.3}); on the device: {}; {}",
            tenants(),
            run.facts()
        );
        assert_eq!(
            run.result["losses"]["dropped"], 0,
            "{:#}",
            run.result["losses"]
        );
    }
}

fn number_of(value: &Value, name: &str) -> f64 {
    value[name]
        .as_f64()
        .unwrap_or_else(|| panic!("no number `{name}` in {value}"))
}

/// The sampling period CUPTI applied is read back, and checked by its
/// effect: the same work sampled every 2^14 cycles takes about a quarter of
/// the samples it takes every 2^12 -- asserted as fewer than half.
#[test]
fn the_sampling_period_asked_is_the_one_applied_by_its_effect() {
    ready_or_skip!();
    let _device = device();
    let samples = |period: &str| {
        let run = run(
            "gpu.profile",
            "tests/gpu_inside.py",
            &["hotloop", "4", "100000"],
            &[("OMN_CUPTI_PC_PERIOD", period)],
        );
        let facts = run.facts();
        assert!(
            facts.contains(&format!("a sample every 2^{period} cycles")),
            "{facts}"
        );
        run.events()
            .iter()
            .map(|event| number(event, "samples"))
            .sum::<f64>()
    };
    let dense = samples("12");
    let sparse = samples("14");
    assert!(
        sparse > 0.0 && sparse * 2.0 < dense,
        "2^14 cycles took {sparse} samples, 2^12 took {dense}; on the device: {}",
        tenants()
    );
}

/// PC sampling and kernel times on one launch: one socket, both read, the
/// kernel count exact.
#[test]
fn a_profile_and_kernel_times_share_one_launch() {
    ready_or_skip!();
    let _device = device();
    let session = scratch("profile-and-kernel").with_extension("yml");
    let launch = "[python3, tests/gpu_inside.py, hotloop, \"3\", \"50000\"]";
    std::fs::write(
        &session,
        format!(
            "name: both\nformat: json\nrequests:\n  \
             - {{label: times, source: gpu.kernel, target: {{launch: {launch}}}, mode: trace, \
             emit: [raw]}}\n  \
             - {{label: inside, source: gpu.profile, target: {{launch: {launch}}}, mode: trace, \
             emit: [raw]}}\n"
        ),
    )
    .unwrap();
    let output = omn(&["-f", session.to_str().unwrap()], &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let document: Value = serde_json::from_slice(&output.stdout).expect("json results");
    let results = document["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let times = &results[0];
    assert_eq!(times["events_seen"], 3, "{times:#}");
    assert_eq!(times["losses"]["undecoded"], 0, "{:#}", times["losses"]);
    let inside = &results[1];
    assert!(inside["events_seen"].as_u64().unwrap() > 0, "{inside:#}");
    assert_eq!(inside["losses"]["undecoded"], 0, "{:#}", inside["losses"]);
}
