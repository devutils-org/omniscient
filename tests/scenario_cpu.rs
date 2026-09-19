//! Scenarios for the `cpu` family.
//!
//! `tests/family_cpu.rs` checks that each source measures what it claims.
//! These check that the *tool* answers a question and that the answer is
//! right: every scenario here starts from a workload with a property this file
//! chose -- a thousand million instructions retired, three hundred milliseconds
//! of cpu burned, fifty sleeps, a core with one tenant -- and asks whether the
//! number that comes back is that property.
//!
//! The four classes of `BRIEF.md`, and where each one lives below:
//!
//! - **A, the capability sweep**: every mode and emit the catalog declares for
//!   this family, driven from `omn list cpu --format json` so that the sweep is
//!   a projection of the catalog rather than a list someone kept up to date.
//! - **B, the backend comparison**: `cpu` has no source with two *implemented*
//!   backends -- which is established here rather than assumed -- so the
//!   comparison is with the refusal, plus the one the family does own: a core
//!   and its only tenant, read out of two different files.
//! - **C, the loss path**: a profile driven past what the kernel will deliver,
//!   a poll asked for a tick the kernel cannot resolve, a target that exits
//!   mid-window, and a profile that misses every task but one.
//! - **D, the refusal**: the three answers -- a rejection, a machine that
//!   cannot serve it, a build that does not implement it -- and whether each
//!   names the thing that is actually missing.
//!
//! What they found is in `docs/scenarios/cpu.md`, including the defects.

use std::collections::BTreeMap;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

// ---------------------------------------------------------------------------
// The workloads
// ---------------------------------------------------------------------------

/// The variable that turns this test binary into a workload instead of a
/// suite.
///
/// A scenario needs a program whose cpu cost is known by construction, and the
/// only program on this machine whose instruction count this repository
/// controls is one it compiles itself. So the test binary re-executes itself:
/// `omn` launches it with this variable set, the harness runs the single test
/// below, and that test does the work and exits.
const WORKLOAD_VAR: &str = "OMN_CPU_SCENARIO_WORKLOAD";

/// The workload, when this binary is launched as one.
///
/// It is a `#[test]` because that is how a libtest binary is asked to run one
/// named thing. Under an ordinary `cargo test` the variable is unset and this
/// does nothing; it is only work when `omn` launched it.
#[test]
fn workload_runs_only_when_this_binary_was_launched_as_a_workload() {
    let Ok(spec) = std::env::var(WORKLOAD_VAR) else {
        return;
    };
    let (kind, argument) = spec.split_once(':').expect("a workload is `kind:number`");
    let number: u64 = argument.parse().expect("a workload's number");
    match kind {
        // Exactly two instructions per iteration: a decrement and the branch
        // that reads its flag.
        "retire" => retire(number),
        "sleep" => std::thread::sleep(Duration::from_millis(number)),
        // A chosen amount of cpu, by the thread's own clock rather than by the
        // wall clock: this machine runs eight streams' suites at once, and a
        // workload that spun for 250 ms of wall time would consume whatever
        // was left over rather than the number this file chose.
        "burn" => burn(Duration::from_millis(number)),
        // Half on a cpu, half blocked: what P2 of phase two needs.
        "split" => {
            burn(Duration::from_millis(number));
            std::thread::sleep(Duration::from_millis(number));
        }
        "spin" => spin_for(Duration::from_millis(number)),
        // Each sleep takes the task off the cpu exactly once, so the number of
        // switches away from it is the number in this file.
        "sleeps" => {
            for _ in 0..number {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        other => panic!("no workload called `{other}`"),
    }
}

/// A loop of `iterations` turns that retires exactly two instructions per
/// turn, and touches nothing else.
///
/// This is the ground truth the counting scenarios are checked against: the
/// number is in this function rather than in anything the tool computed.
fn retire(iterations: u64) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: the loop decrements a register it owns and branches on the flag
    // that decrement sets.
    unsafe {
        std::arch::asm!(
            "2:",
            "dec {n}",
            "jnz 2b",
            n = inout(reg) iterations => _,
            options(nostack),
        );
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: as above.
    unsafe {
        std::arch::asm!(
            "2:",
            "subs {n}, {n}, #1",
            "b.ne 2b",
            n = inout(reg) iterations => _,
            options(nostack),
        );
    }
}

/// Busy on a cpu for a wall-clock duration, blocking on nothing.
fn spin_for(duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

/// Consumes a chosen amount of cpu time, measured by the kernel's own clock
/// for this thread.
///
/// This is the property the scenarios that measure cpu time are checked
/// against, and it is deliberately not a wall-clock duration: a contended
/// machine gives a spinning task less cpu than wall time, and a scenario whose
/// ground truth moved with the load would be measuring the machine.
fn burn(cpu: Duration) {
    let consumed = || {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `now` is a valid timespec for the kernel to write into.
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
        Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
    };
    let until = consumed() + cpu;
    while consumed() < until {
        std::hint::spin_loop();
    }
}

/// The `--` part of a command line: this binary, launched as a workload.
///
/// The redirection is not decoration. `omn` gives the program it launches its
/// own standard output, so a workload that printed would interleave with the
/// JSON this suite parses. `exec` keeps it one process, so the counters follow
/// the task they were opened on.
fn workload_program() -> Vec<String> {
    let exe = std::env::current_exe().expect("this test binary has a path");
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "exec \"$1\" --exact --test-threads 1 \
         workload_runs_only_when_this_binary_was_launched_as_a_workload >/dev/null 2>&1"
            .to_string(),
        "sh".to_string(),
        exe.to_string_lossy().into_owned(),
    ]
}

// ---------------------------------------------------------------------------
// Running the tool
// ---------------------------------------------------------------------------

/// What a run of `omn` came back with.
///
/// The distinction is the one `docs/contributing.md` insists on: a machine that
/// cannot serve a request stops a scenario quietly, and a request the tool
/// *refused* is a broken scenario that must fail loudly. Nothing here treats a
/// non-zero exit as a reason to pass.
enum Outcome {
    /// The run measured something: its JSON document where it produced one,
    /// and always the text it wrote, because half of what these scenarios
    /// check is what a rendering looks like to whoever reads it next.
    Measured {
        document: serde_json::Value,
        rendered: String,
    },
    /// The run refused, with this exit code and this message on stderr.
    Refused { code: i32, message: String },
}

impl Outcome {
    fn measured(self) -> serde_json::Value {
        match self {
            Outcome::Measured { document, .. } => document,
            Outcome::Refused { code, message } => {
                panic!("expected a measurement, got a refusal (exit {code}): {message}")
            }
        }
    }

    fn refusal(self) -> (i32, String) {
        match self {
            Outcome::Refused { code, message } => (code, message),
            Outcome::Measured { .. } => {
                panic!("expected a refusal, and the run measured something")
            }
        }
    }
}

fn run(args: &[&str]) -> Outcome {
    run_workload(
        None,
        &args
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>(),
    )
}

/// Runs `omn`, with the workload variable set when a launched workload is part
/// of the command line.
fn run_workload(spec: Option<&str>, args: &[String]) -> Outcome {
    let mut command = Command::new(BINARY);
    command.args(args);
    if let Some(spec) = spec {
        command.env(WORKLOAD_VAR, spec);
    }
    into_outcome(command.output().expect("omn should run"))
}

fn into_outcome(output: Output) -> Outcome {
    let out = String::from_utf8_lossy(&output.stdout).into_owned();
    let message = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        let document = serde_json::from_str(&out).unwrap_or(serde_json::Value::Null);
        return Outcome::Measured {
            document,
            rendered: out,
        };
    }
    Outcome::Refused {
        code: output.status.code().unwrap_or(-1),
        message,
    }
}

/// A command line that measures `source` over the workload `spec`.
fn measure(source: &str, spec: &str, axes: &[&str]) -> Outcome {
    let mut args: Vec<String> = vec![source.to_string()];
    args.extend(axes.iter().map(|axis| (*axis).to_string()));
    args.push("--format".into());
    args.push("json".into());
    args.push("--".into());
    args.extend(workload_program());
    run_workload(Some(spec), &args)
}

/// The catalog, as data. Every sweep below is a projection of this rather than
/// a list kept up to date by hand: a source, a mode or an emit added to
/// `catalog/cpu.yaml` is swept the next time this runs.
fn catalog() -> Vec<serde_json::Value> {
    let document = run(&["list", "cpu", "--format", "json"]).measured();
    document.as_array().expect("a list of entries").clone()
}

fn strings(entry: &serde_json::Value, field: &str) -> Vec<String> {
    entry[field]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|value| value.as_str().expect("a string").to_string())
        .collect()
}

/// The single value of a summary series, whatever the emit called it.
fn last_of(document: &serde_json::Value) -> f64 {
    document["results"][0]["outputs"][0]["series"][0]["last"]
        .as_f64()
        .expect("a measured value")
}

/// Whether a refusal is this machine's limit rather than a refusal of the
/// request. Only the first is a reason for a scenario to stop quietly.
fn machine_cannot_serve(message: &str) -> bool {
    message.contains("no viable backend")
        || message.contains("perf_event_paranoid")
        || message.contains("cannot start collecting")
}

// ---------------------------------------------------------------------------
// Ground truth: the number is in the workload
// ---------------------------------------------------------------------------

/// A thousand million instructions, retired on purpose, counted through the
/// whole tool -- and through the session file a reader would run.
///
/// Five hundred million turns of `dec` and `jnz` is a thousand million retired
/// instructions and nothing else; everything above that is the cost of
/// becoming the workload -- a shell, an `exec`, a dynamic linker and a test
/// harness deciding which test to run. That cost is a constant, so the scenario
/// measures it rather than assuming it: the same session runs with a loop of
/// one turn, and the difference between the two runs is the loop.
///
/// The check is three-sided. The difference must be the thousand million this
/// file asked for, within half the one per cent `cpu.instructions` claims. The
/// constant must be small enough that the absolute count is a measurement of
/// the loop and not of the harness. And the `instructions` term of `cpu.ipc`,
/// which is read in a counter group beside `cycles`, must be the same number as
/// the count read on its own -- two roads, one workload, and the workload is
/// the arbiter.
#[test]
fn a_known_number_of_retired_instructions_is_counted_as_that_number() {
    let _load = may_load_the_machine();
    const ITERATIONS: u64 = 500_000_000;
    const EXPECTED: f64 = 2.0 * ITERATIONS as f64;

    let Some(startup) = example_counted("retire:1") else {
        return;
    };
    let Some(measured) = example_counted(&format!("retire:{ITERATIONS}")) else {
        return;
    };

    let loop_only = measured.counted - startup.counted;
    let error = (loop_only - EXPECTED) / EXPECTED;
    assert!(
        error.abs() < 0.005,
        "a workload that retires {EXPECTED:.0} instructions was counted at {loop_only:.0} \
         ({:+.3} %); the runs were {:.0} and {:.0}",
        error * 100.0,
        measured.counted,
        startup.counted
    );
    // With the constant this small the absolute count is a measurement of the
    // loop too, which is the stronger claim: nothing was subtracted to make it
    // agree.
    assert!(
        startup.counted < EXPECTED * 0.01,
        "becoming the workload costs {:.0} instructions, which is more than a hundredth of \
         the {EXPECTED:.0} being measured: the absolute count is no longer about the loop",
        startup.counted
    );
    let absolute = (measured.counted - EXPECTED) / EXPECTED;
    assert!(
        absolute.abs() < 0.02,
        "the whole run counted {:.0} against the {EXPECTED:.0} the loop retires ({:+.3} %)",
        measured.counted,
        absolute * 100.0
    );

    // The same window, read as a ratio: the group has to have counted the same
    // instructions, and the ratio has to be its own terms.
    let (ipc, instructions, cycles) = measured.ratio;
    assert!(
        (ipc - instructions / cycles).abs() < 1e-9,
        "the published ratio {ipc} is not its own terms {instructions}/{cycles}"
    );
    let between = (instructions - measured.counted) / measured.counted;
    assert!(
        between.abs() < 0.02,
        "the counter read on its own says {:.0} instructions and the one read in a group \
         beside `cycles` says {instructions:.0} ({:+.3} %)",
        measured.counted,
        between * 100.0
    );
}

/// What `examples/cpu-known-work.yml` reports for one workload: the count, and
/// the ratio with the terms it was built from.
struct KnownWork {
    counted: f64,
    ratio: (f64, f64, f64),
}

/// Runs the example that a reader would run, with this binary as the workload
/// it offers to take.
///
/// `None` when the counters shared the hardware with somebody else's. The
/// kernel time-shares counter groups when more are open than the pmu has, and
/// what comes back is then an extrapolation from the slices the group was on
/// for -- which the run says, in as many words. An extrapolation cannot be
/// checked against ground truth to one per cent, so the scenario stops and
/// says why rather than failing on somebody else's measurement. On this
/// machine that happens when another worktree of this wave is running its own
/// counter tests.
fn example_counted(spec: &str) -> Option<KnownWork> {
    let exe = std::env::current_exe().expect("this test binary has a path");
    let output = Command::new(BINARY)
        .args(["-f", "examples/cpu-known-work.yml", "--format", "json"])
        .env(WORKLOAD_VAR, spec)
        .env(
            "OMN_SCENARIO_WORKLOAD",
            format!(
                "{} --exact --test-threads 1 \
                 workload_runs_only_when_this_binary_was_launched_as_a_workload",
                exe.to_string_lossy()
            ),
        )
        .output()
        .expect("omn should run");
    let document = match into_outcome(output) {
        Outcome::Measured { document, .. } => document,
        Outcome::Refused { message, .. } => {
            assert!(
                machine_cannot_serve(&message),
                "the example was refused rather than unserved: {message}"
            );
            eprintln!("skipped: {message}");
            return None;
        }
    };

    let shared_the_hardware = document["results"]
        .as_array()
        .expect("results")
        .iter()
        .flat_map(|result| {
            result["degradations"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .any(|note| {
            note.as_str()
                .unwrap_or_default()
                .contains("shared the hardware")
        });
    if shared_the_hardware {
        eprintln!(
            "skipped: the counters shared the pmu with another process's, so what came back \
             is scaled from the slices they were on for and is not a count to check ground \
             truth against"
        );
        return None;
    }

    let counted = document["results"][0]["outputs"][0]["series"][0]["last"]
        .as_f64()
        .expect("a counted value");
    let ratio = &document["results"][1];
    // The same contention can leave the ratio's group never scheduled at all,
    // and a group of two counters is crowded out where a single one is only
    // time-shared. The run says so as a loss; there is nothing to check.
    if ratio["outputs"][0]["events"]
        .as_array()
        .is_none_or(|events| events.is_empty())
    {
        // The sibling request above counted the workload in the same window, so
        // the target demonstrably ran: a message that blames the target here is
        // the defect `docs/scenarios/cpu.md` recorded as cpu 9.
        let reasons = ratio["losses"]["reasons"].to_string();
        assert!(
            reasons.contains("could not fit on the pmu"),
            "the ratio's group was crowded off a pmu that a counter beside it was using, \
             and the run blames the target instead: {reasons}"
        );
        eprintln!(
            "skipped: the ratio's counter group was never scheduled onto the pmu, so it \
             reported no window: {reasons}"
        );
        return None;
    }
    let named = |name: &str| -> f64 {
        ratio["outputs"][0]["events"][0]["attrs"]
            .as_array()
            .expect("the event carries attrs")
            .iter()
            .find(|attr| attr["name"] == name)
            .unwrap_or_else(|| panic!("`{name}` is on the event"))["value"]
            .as_f64()
            .expect("numeric")
    };
    Some(KnownWork {
        counted,
        ratio: (named("ipc"), named("instructions"), named("cycles")),
    })
}

/// The count for one launch of the workload, or nothing when this machine will
/// not open the counter, or when the counter shared the hardware and what came
/// back is an extrapolation rather than a count.
fn counted(source: &str, spec: &str, axes: &[&str]) -> Option<f64> {
    match measure(source, spec, axes) {
        Outcome::Measured { document, .. } => {
            let result = &document["results"][0];
            let scaled = result["degradations"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .any(|note| {
                    note.as_str()
                        .unwrap_or_default()
                        .contains("shared the hardware")
                });
            if scaled {
                eprintln!(
                    "skipped: `{source}` shared the pmu with another process's counters, so \
                     its value is scaled from the slices it was on for"
                );
                return None;
            }
            Some(last_of(&document))
        }
        Outcome::Refused { message, .. } => {
            assert!(
                machine_cannot_serve(&message),
                "the request was refused, which is a broken scenario rather than a limited \
                 machine: {message}"
            );
            eprintln!("skipped: {message}");
            None
        }
    }
}

/// Three hundred milliseconds asleep is not three hundred milliseconds of cpu,
/// and three hundred burned on a cpu is.
///
/// The same source, the same emit, the same window: only the workload differs,
/// and it differs by a number this file chose. The number is an amount of cpu
/// rather than a share of the window, so the check does not move when the
/// machine is busy -- which it is, with eight streams' suites running on it.
/// A collector that reported the wall clock, or the window, or a plausible
/// constant fails one half or the other.
#[test]
fn a_sleeping_program_consumes_no_cpu_and_a_burning_one_consumes_what_it_burned() {
    const MILLISECONDS: f64 = 300.0;
    let axes = ["-m", "poll@50ms", "-e", "summary"];

    let Some(asleep) = consumed_cpu("sleep:300", &axes) else {
        return;
    };
    let Some(burning) = consumed_cpu("burn:300", &axes) else {
        return;
    };
    assert!(
        asleep.consumed < 50e6,
        "a program that slept for {MILLISECONDS} ms is reported as having consumed {:.0} ms \
         of cpu",
        asleep.consumed / 1e6
    );
    let error = (burning.consumed / 1e6 - MILLISECONDS) / MILLISECONDS;
    assert!(
        error.abs() < 0.25,
        "a program that burned {MILLISECONDS} ms of cpu is reported as having consumed \
         {:.0} ms of it ({:+.0} %), over a {:.0} ms window",
        burning.consumed / 1e6,
        error * 100.0,
        burning.window / 1e6
    );
}

/// A program that burns a chosen amount of cpu and then blocks is reported as
/// having consumed what it burned, and the window is longer than that.
///
/// This is the workload phase two's P2 asks for -- half on a cpu, half blocked
/// -- and the scenario that says it is what it claims to be. `cpu.oncpu_time`
/// must see the cpu and not the sleep: the amount checked is the one the
/// workload chose, and the window has to be long enough to contain the sleep
/// as well, or the workload did not block at all.
#[test]
fn a_program_that_blocks_after_working_is_credited_only_with_the_work() {
    const HALF_MS: f64 = 250.0;
    let Some(split) = consumed_cpu("split:250", &["-m", "poll@50ms", "-e", "summary"]) else {
        return;
    };
    let error = (split.consumed / 1e6 - HALF_MS) / HALF_MS;
    assert!(
        error.abs() < 0.25,
        "a workload that burns {HALF_MS} ms of cpu and then sleeps {HALF_MS} ms is reported \
         as having consumed {:.0} ms ({:+.0} %)",
        split.consumed / 1e6,
        error * 100.0
    );
    assert!(
        split.window > 1.5 * HALF_MS * 1e6,
        "the window is {:.0} ms, which is not long enough to hold {HALF_MS} ms of work and \
         {HALF_MS} ms of sleep: the workload did not block",
        split.window / 1e6
    );
}

/// What `cpu.oncpu_time` said about one run: the cpu time it summed, and the
/// window it covered.
struct Consumed {
    consumed: f64,
    window: f64,
}

fn consumed_cpu(spec: &str, axes: &[&str]) -> Option<Consumed> {
    let document = match measure("cpu.oncpu_time", spec, axes) {
        Outcome::Measured { document, .. } => document,
        Outcome::Refused { message, .. } => {
            assert!(
                machine_cannot_serve(&message),
                "the request was refused rather than unserved: {message}"
            );
            eprintln!("skipped: {message}");
            return None;
        }
    };
    let result = &document["results"][0];
    let series = &result["outputs"][0]["series"][0];
    // A polled source reports a difference per tick, so the cpu time consumed
    // over the window is the sum of the readings, not the last of them.
    let reads = series["count"].as_f64().expect("a polled series");
    let mean = series["mean"].as_f64().expect("a mean");
    Some(Consumed {
        consumed: mean * reads,
        window: result["observed_ns"].as_f64().expect("a window"),
    })
}

// ---------------------------------------------------------------------------
// Class A -- the capability sweep
// ---------------------------------------------------------------------------

/// One cell of the sweep: a source, a mode and an emit the catalog declares
/// together.
#[derive(Clone)]
struct Combination {
    source: String,
    mode: String,
    emit: String,
}

/// What the sweep found in that cell.
enum Served {
    /// The combination ran and produced an output of the emit that was asked
    /// for.
    Measured,
    /// The combination did not run, with the exit code and the message.
    Unserved(i32, String),
}

/// A target this source accepts, as the catalog declares it, with a workload
/// small enough for a sweep.
fn sweep_target(entry: &serde_json::Value) -> Option<Vec<String>> {
    let accepts = |kind: &str| entry["targets"][kind] == "native";
    if accepts("launch") {
        let mut args = vec!["--".to_string()];
        args.extend(
            [
                "/bin/sh",
                "-c",
                "i=0; while [ $i -lt 30000 ]; do i=$((i+1)); done",
            ]
            .iter()
            .map(|part| (*part).to_string()),
        );
        return Some(args);
    }
    if accepts("system") {
        return Some(vec!["--system".to_string(), "-d".into(), "120ms".into()]);
    }
    if accepts("cgroup") {
        return Some(vec![
            "--cgroup".to_string(),
            own_cgroup()?,
            "-d".into(),
            "120ms".into(),
        ]);
    }
    None
}

/// The cgroup this process is in, which is the only one a test can be sure of
/// being allowed to read.
fn own_cgroup() -> Option<String> {
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    Some(own.lines().next()?.rsplit(':').next()?.to_string())
}

/// A mode as a command line writes it: a bare name is not always enough.
fn mode_argument(mode: &str) -> &'static str {
    match mode {
        "count" => "count",
        "sample" => "sample@199hz",
        "poll" => "poll@50ms",
        "snapshot" => "snapshot",
        "trace" => "trace",
        other => panic!("no mode called `{other}`"),
    }
}

/// Runs every (source, mode, emit) the catalog declares for this family.
///
/// The sweep is a projection of the catalog, like the CLI is: a source, a mode
/// or an emit added to `catalog/cpu.yaml` is swept the next time this runs,
/// with nothing here to keep up to date. Eight at a time, because a sweep that
/// takes ten seconds is one nobody runs.
fn sweep() -> Vec<(Combination, Served)> {
    let mut work = Vec::new();
    for entry in catalog() {
        let source = entry["id"].as_str().expect("an id").to_string();
        let Some(target) = sweep_target(&entry) else {
            continue;
        };
        for mode in strings(&entry, "modes") {
            for emit in strings(&entry, "emits") {
                work.push((
                    Combination {
                        source: source.clone(),
                        mode: mode.clone(),
                        emit,
                    },
                    target.clone(),
                ));
            }
        }
    }

    let queue = std::sync::Mutex::new(work.into_iter());
    let found = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| loop {
                let Some((cell, target)) = queue.lock().expect("the queue").next() else {
                    return;
                };
                let mut args = vec![
                    cell.source.clone(),
                    "-m".into(),
                    mode_argument(&cell.mode).into(),
                    "-e".into(),
                    cell.emit.clone(),
                    "--format".into(),
                    "json".into(),
                ];
                args.extend(target);
                let served = match run_workload(None, &args) {
                    Outcome::Measured { document, .. } => {
                        assert_eq!(
                            document["results"][0]["outputs"][0]["emit"], cell.emit,
                            "{} in {} mode was asked for `{}` and produced something else",
                            cell.source, cell.mode, cell.emit
                        );
                        Served::Measured
                    }
                    Outcome::Refused { code, message } => Served::Unserved(code, message),
                };
                found.lock().expect("the results").push((cell, served));
            });
        }
    });
    found.into_inner().expect("the results")
}

/// Every combination the catalog offers either runs or says what is missing,
/// and the emits that cannot be reached at all are the ones this family has
/// written down.
///
/// A combination the catalog offers and the build cannot serve is a promise
/// broken where nobody looks: `omn list cpu.cycles` says it emits `stack`, and
/// it does -- in `sample` mode, which no counting source implements. So the
/// sweep asserts two different things. Every refusal must name a mechanism
/// rather than fail vaguely; and the set of emits that no declared mode can
/// reach must not grow beyond the ones `docs/scenarios/cpu.md` records, which
/// is what makes a new gap a failing test rather than a discovery years later.
#[test]
fn every_mode_and_emit_the_catalog_declares_is_served_or_says_what_is_missing() {
    let _load = may_load_the_machine();
    // Source and emit, where no mode the catalog declares can produce it here.
    // Each one is in `docs/scenarios/cpu.md` with the mechanism it waits on.
    const UNREACHABLE: [(&str, &str); 18] = [
        ("cpu.cycles", "hist"),
        ("cpu.cycles", "topk"),
        ("cpu.cycles", "stack"),
        ("cpu.instructions", "hist"),
        ("cpu.instructions", "topk"),
        ("cpu.instructions", "stack"),
        ("cpu.branch_misses", "topk"),
        ("cpu.branch_misses", "stack"),
        ("cpu.cache_misses", "topk"),
        ("cpu.cache_misses", "stack"),
        ("cpu.context_switches", "topk"),
        ("cpu.context_switches", "stack"),
        ("cpu.oncpu_time", "hist"),
        ("cpu.oncpu_time", "rate"),
        // This machine has no issue-slot counter, so nothing of `cpu.topdown`
        // is reachable here. A machine that has one reaches both.
        ("cpu.topdown", "summary"),
        ("cpu.topdown", "raw"),
        // Intel PT and CoreSight are absent here and have no collector
        // anywhere: `cpu.flow` is vocabulary in both senses.
        ("cpu.flow", "raw"),
        ("cpu.flow", "timeline"),
    ];

    let found = sweep();
    assert!(
        found.len() > 40,
        "the sweep ran {} combinations, which is too few to be the catalog",
        found.len()
    );

    let mut reachable: BTreeMap<(String, String), bool> = BTreeMap::new();
    for (cell, served) in &found {
        let key = (cell.source.clone(), cell.emit.clone());
        let entry = reachable.entry(key).or_insert(false);
        match served {
            Served::Measured => *entry = true,
            Served::Unserved(code, message) => {
                assert!(
                    (1..=3).contains(code),
                    "{} in {} mode emitting {} exited {code}: {message}",
                    cell.source,
                    cell.mode,
                    cell.emit
                );
                // A refusal that does not name the source it refused cannot be
                // acted on, and a refusal with no reason at all is worse than
                // no message.
                assert!(
                    message.contains(&cell.source),
                    "a refusal of {} in {} mode emitting {} does not name it: {message}",
                    cell.source,
                    cell.mode,
                    cell.emit
                );
            }
        }
    }

    let unreachable: Vec<(String, String)> = reachable
        .iter()
        .filter(|(_, served)| !**served)
        .map(|(key, _)| key.clone())
        .collect();
    for (source, emit) in &unreachable {
        assert!(
            UNREACHABLE.contains(&(source.as_str(), emit.as_str())),
            "`{source}` declares `{emit}` and no mode it declares can produce it here. \
             That is a promise the catalog makes and this build does not keep; if it is \
             intended, record it in docs/scenarios/cpu.md and add it to this list"
        );
    }
    for (source, emit) in &unreachable {
        let why: Vec<String> = found
            .iter()
            .filter(|(cell, _)| &cell.source == source && &cell.emit == emit)
            .map(|(cell, served)| match served {
                Served::Measured => format!("{}: served", cell.mode),
                Served::Unserved(code, message) => format!(
                    "{} ({code}): {}",
                    cell.mode,
                    message.lines().next().unwrap_or_default()
                ),
            })
            .collect();
        eprintln!("UNREACHABLE {source} {emit} -- {}", why.join(" | "));
    }
    eprintln!(
        "swept {} combinations: {} of {} (source, emit) pairs are reachable here",
        found.len(),
        reachable.len() - unreachable.len(),
        reachable.len()
    );
}

/// The same measurement, asked for in four output formats, is the same number
/// in all four.
///
/// `--format` is the axis nobody exercises: a format that claims an emit it
/// cannot carry, or that rounds a counter into a string, is invisible until
/// someone parses it. The workload retires a number this file chose, so each
/// format can be checked against the workload rather than against the others.
#[test]
fn a_counted_window_is_the_same_number_in_every_format_that_carries_it() {
    let _load = may_load_the_machine();
    const ITERATIONS: u64 = 500_000_000;
    const EXPECTED: f64 = 2.0 * ITERATIONS as f64;
    let spec = format!("retire:{ITERATIONS}");

    let Some(json) = counted("cpu.instructions", &spec, &["-m", "count", "-e", "summary"]) else {
        return;
    };
    assert!(
        ((json - EXPECTED) / EXPECTED).abs() < 0.02,
        "json reported {json:.0} for a loop that retires {EXPECTED:.0}"
    );

    for format in ["text", "yaml", "toml", "ndjson"] {
        let mut args: Vec<String> = ["cpu.instructions", "-m", "count", "-e", "raw", "--format"]
            .iter()
            .map(|part| (*part).to_string())
            .collect();
        args.push(format.to_string());
        args.push("--".into());
        args.extend(workload_program());
        let rendered = match run_workload(Some(&spec), &args) {
            Outcome::Measured { rendered, .. } => rendered,
            Outcome::Refused { code, message } => {
                panic!("`--format {format}` was refused (exit {code}): {message}")
            }
        };
        // Every one of these renders the counted value in full, so the count
        // the workload retired has to be among the numbers in the document. A
        // format that dropped the counter, or rounded it away, fails here.
        assert!(
            carries_a_number_near(&rendered, EXPECTED, 0.02),
            "`--format {format}` carries no number within 2 % of the {EXPECTED:.0} \
             instructions the workload retired:\n{}",
            &rendered[..rendered.len().min(400)]
        );
    }
}

/// Whether some number in a rendered document is `expected`, within
/// `tolerance` of it.
///
/// Reading a rendering rather than a parse tree is the point: each of these
/// formats is someone else's input, and what matters is that the number
/// survived the trip.
fn carries_a_number_near(text: &str, expected: f64, tolerance: f64) -> bool {
    text.split(|c: char| !c.is_ascii_digit())
        .filter_map(|digits| digits.parse::<f64>().ok())
        .any(|number| ((number - expected) / expected).abs() < tolerance)
}

/// A shell that spins until it is killed, optionally on one core.
///
/// A copy of `/bin/sh` under a name nothing else on this machine has: a
/// per-core or machine-wide measurement has only `comm` to tell the workload
/// from everything else that is running, and a process called `sh` cannot be
/// told from any other shell.
struct Spinner {
    child: std::process::Child,
}

impl Spinner {
    fn start(name: &str, cpu: Option<usize>) -> Option<Self> {
        let spinner = Self::started(name, cpu)?;
        // Long enough to be running, rather than still being started, when the
        // measurement opens.
        std::thread::sleep(Duration::from_millis(150));
        Some(spinner)
    }

    /// One spinner, without waiting for it: a caller starting a machine's
    /// worth of them waits once at the end instead of once each.
    fn started(name: &str, cpu: Option<usize>) -> Option<Self> {
        assert!(name.len() <= 15, "`{name}` is longer than a comm");
        let path = std::env::temp_dir().join(name);
        if !path.exists() {
            std::fs::copy("/bin/sh", &path).ok()?;
            let mut mode = std::fs::metadata(&path).ok()?.permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
            std::fs::set_permissions(&path, mode).ok()?;
        }
        let shell = path.to_str()?;
        let mut command = match cpu {
            Some(cpu) => {
                let mut pinned = Command::new("taskset");
                pinned.args(["-c", &cpu.to_string(), shell]);
                pinned
            }
            None => Command::new(shell),
        };
        let child = command.args(["-c", "while :; do :; done"]).spawn().ok()?;
        Some(Self { child })
    }

    fn pid(&self) -> String {
        self.child.id().to_string()
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A profile renders through every format that carries stacks, and is refused
/// by the one that does not.
///
/// They are not interchangeable: `folded` is text, `speedscope` is JSON and
/// `pprof` is a gzipped protobuf, and a format that claims an emit it cannot
/// carry is the promise this class exists to find. The workload chooses the
/// number: a shell that never blocks is on a cpu for the whole window, so a
/// request for 997 samples a second over 200 ms has to produce about two
/// hundred of them, in every rendering that carries a count.
#[test]
fn a_profile_renders_in_every_format_that_carries_stacks() {
    let _load = may_load_the_machine();
    let Some(spinner) = Spinner::start("omnprofilefmt", None) else {
        eprintln!("skipped: /bin/sh could not be copied, so there is no named workload");
        return;
    };
    let axes = [
        "--pid",
        &spinner.pid(),
        "-m",
        "sample@997hz",
        "-e",
        "stack",
        "--stack",
        "fp",
        "-d",
        "200ms",
    ];

    let mut args: Vec<String> = vec!["cpu.profile".into()];
    args.extend(axes.iter().map(|axis| (*axis).to_string()));
    args.extend(["--format".to_string(), "json".to_string()]);
    let document = match run_workload(None, &args) {
        Outcome::Measured { document, .. } => document,
        Outcome::Refused { message, .. } => {
            assert!(machine_cannot_serve(&message), "{message}");
            eprintln!("skipped: {message}");
            return;
        }
    };
    let samples = document["results"][0]["events_seen"]
        .as_f64()
        .expect("a sample count");
    assert!(
        (40.0..320.0).contains(&samples),
        "a shell spinning for 200 ms sampled at 997 Hz produced {samples} samples, where \
         the frequency and the window ask for about two hundred -- fewer than forty means \
         it was not sampled, more than the window allows means it was not cpu time"
    );

    // Together rather than one after another. Each of these runs used to cost
    // a second of symbol reading before it sampled anything -- defect 7 of
    // `docs/scenarios/cpu.md`, fixed in wave five -- so what the arrangement
    // buys now is three windows instead of three windows and three seconds.
    std::thread::scope(|scope| {
        for format in ["folded", "speedscope", "pprof"] {
            scope.spawn(move || {
                let mut args: Vec<String> = vec!["cpu.profile".into()];
                args.extend(axes.iter().map(|axis| (*axis).to_string()));
                args.push("--format".into());
                args.push(format.to_string());
                match run_workload(None, &args) {
                    Outcome::Measured { rendered, .. } => {
                        assert!(
                            !rendered.is_empty(),
                            "`--format {format}` produced nothing for a profile of a \
                             spinning program"
                        );
                    }
                    Outcome::Refused { code, message } => {
                        panic!("`--format {format}` was refused (exit {code}): {message}")
                    }
                }
            });
        }
    });

    // `chrome` carries timelines, and a stack is not one. The refusal has to
    // say so rather than render an empty trace.
    let mut args: Vec<String> = vec!["cpu.profile".into()];
    args.extend(axes.iter().map(|axis| (*axis).to_string()));
    args.extend(["--format".to_string(), "chrome".to_string()]);
    let (code, message) = run_workload(None, &args).refusal();
    assert_eq!(code, 2, "{message}");
    assert!(message.contains("chrome"), "{message}");
    assert!(message.contains("stack"), "{message}");
}

/// A profile counts cpu time, not wall time.
///
/// The claim is in `src/collect/cpu/profile.rs`: a request for 997 Hz asks for
/// one sample per 1.003 ms *of cpu*, so a program that spends half its life
/// asleep produces half as many samples as its wall clock would suggest. The
/// workload chooses that half, and `cpu.oncpu_time` measures it over the same
/// window in the same session -- so the scenario has both terms and the
/// prediction is arithmetic: samples = frequency x cpu seconds.
#[test]
fn the_samples_of_a_profile_count_the_cpu_time_and_not_the_wall_clock() {
    let _alone = machine_to_itself();
    const HZ: f64 = 997.0;
    let session = format!(
        r#"
name: half on a cpu, half asleep
format: json
requests:
  - label: where the time went
    source: cpu.profile
    target: {{launch: {LAUNCH}}}
    mode: sample@997hz
    emit: [topk]
    stack: fp
  - label: how much of it was cpu
    source: cpu.oncpu_time
    target: {{launch: {LAUNCH}}}
    mode: poll@50ms
    emit: [summary]
"#,
        LAUNCH = HALF_AND_HALF
    );
    let Some(document) = session_run(&session) else {
        return;
    };
    let results = document["results"].as_array().expect("two results");
    assert_eq!(results.len(), 2);

    let samples = results[0]["events_seen"].as_f64().expect("a sample count");
    let window = results[1]["observed_ns"].as_f64().expect("a window");
    let series = &results[1]["outputs"][0]["series"][0];
    let consumed = series["mean"].as_f64().expect("a mean")
        * series["count"].as_f64().expect("a polled series");

    // The workload's own property first: a program that spins for as long as
    // it sleeps spends half its window on a cpu. Without this the sample count
    // below would be checked against a number nobody chose.
    let share = consumed / window;
    assert!(
        (0.1..0.9).contains(&share),
        "the workload is meant to spend part of its window on a cpu and part asleep, and \
         spent {:.0} % of it on a cpu: neither half is there to be counted",
        share * 100.0
    );

    let expected = HZ * consumed / 1e9;
    let error = (samples - expected) / expected;
    assert!(
        error.abs() < 0.3,
        "a profile at {HZ} Hz over {:.0} ms of cpu time took {samples} samples, where the \
         frequency asks for about {expected:.0} ({:+.0} %). A profile counting wall clock \
         over the same run would have taken about {:.0}",
        consumed / 1e6,
        error * 100.0,
        HZ * window / 1e9
    );
}

/// A shell that spends about as long spinning as it spends asleep.
///
/// The iteration count is calibrated for a machine that does this loop in
/// about 250 ms; a slower machine spins for longer and the scenario reads the
/// share it measured rather than the one it assumed, which is why every check
/// above is against the measured cpu time and not against the 250.
const HALF_AND_HALF: &str =
    r#"[/bin/sh, -c, "i=0; while [ $i -lt 90000 ]; do i=$((i+1)); done; sleep 0.25"]"#;

/// Runs a session from standard input, as `-f -` reads it.
fn session_run(session: &str) -> Option<serde_json::Value> {
    use std::io::Write;
    let mut child = Command::new(BINARY)
        .args(["-f", "-", "--input-format", "yaml"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("omn should run");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(session.as_bytes())
        .expect("writing the session");
    let output = child.wait_with_output().expect("omn should finish");
    match into_outcome(output) {
        Outcome::Measured { document, .. } => Some(document),
        Outcome::Refused { message, .. } => {
            assert!(
                machine_cannot_serve(&message),
                "the session was refused rather than unserved: {message}"
            );
            eprintln!("skipped: {message}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Class B -- the backend comparison
// ---------------------------------------------------------------------------

/// Every backend of every source in this family, pinned with
/// `--backend force:` and asked to measure.
///
/// This is the class the catalog exists for: where a chain has two implemented
/// backends, the same workload measured through each has to agree or explain
/// the difference. `cpu` has none -- every chain here has exactly one backend
/// with a collector -- so the comparison is with the refusal instead, and this
/// is what establishes the "none" rather than asserting it from a table
/// someone maintained.
///
/// It is also the test that tells whoever adds the second collector that the
/// comparison is now owed: the day a chain has two, this fails and names it.
#[test]
fn every_backend_either_measures_or_says_it_is_not_built() {
    let _load = may_load_the_machine();
    // Every (source, backend) of the family, with a target that backend
    // serves: a backend may serve fewer than its source does, and asking it
    // for one it does not is a different refusal from the one under test.
    let mut work: Vec<(String, String, Vec<String>)> = Vec::new();
    for entry in catalog() {
        let source = entry["id"].as_str().expect("an id").to_string();
        let modes = strings(&entry, "modes");
        let emits = strings(&entry, "emits");
        let (Some(mode), Some(emit)) = (modes.first(), emits.first()) else {
            continue;
        };
        for backend in entry["backends"].as_array().expect("a chain") {
            let id = backend["id"].as_str().expect("a backend id").to_string();
            let scoped = if backend["targets"].is_object() {
                let mut narrowed = entry.clone();
                narrowed["targets"] = backend["targets"].clone();
                narrowed
            } else {
                entry.clone()
            };
            let Some(target) = sweep_target(&scoped) else {
                continue;
            };
            let mut args = vec![
                source.clone(),
                "--backend".into(),
                format!("force:{id}"),
                "-m".into(),
                mode_argument(mode).into(),
                "-e".into(),
                emit.clone(),
                "--format".into(),
                "json".into(),
            ];
            args.extend(target);
            work.push((source.clone(), id, args));
        }
    }

    let queue = std::sync::Mutex::new(work.into_iter());
    let implemented: std::sync::Mutex<BTreeMap<String, Vec<String>>> =
        std::sync::Mutex::new(BTreeMap::new());
    let declared_only: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| loop {
                let Some((source, id, args)) = queue.lock().expect("the queue").next() else {
                    return;
                };
                match run_workload(None, &args) {
                    Outcome::Measured { .. } => implemented
                        .lock()
                        .expect("the results")
                        .entry(source)
                        .or_default()
                        .push(id),
                    Outcome::Refused { code, message } => {
                        if code == 3 {
                            // The answer this class wants from an
                            // unimplemented backend: not a failure, and it
                            // names both halves of what is missing.
                            assert!(
                                message.contains(&source) && message.contains(&id),
                                "`{source}` on `{id}` exits 3 without naming what is not \
                                 written: {message}"
                            );
                            declared_only
                                .lock()
                                .expect("the results")
                                .push(format!("{source} {id}"));
                        } else {
                            // Anything else has to be this machine rather than
                            // this build: a requirement that is missing here.
                            assert!(
                                machine_cannot_serve(&message) || message.contains("no viable"),
                                "`{source}` on `{id}` was refused for a reason that is \
                                 neither a missing collector nor a missing requirement: \
                                 {message}"
                            );
                        }
                    }
                }
            });
        }
    });

    let implemented = implemented.into_inner().expect("the results");
    let declared_only = declared_only.into_inner().expect("the results");
    assert!(
        !declared_only.is_empty(),
        "no backend of this family is declared without a collector, which cannot be right"
    );
    for (source, backends) in &implemented {
        assert!(
            backends.len() < 2,
            "`{source}` now has two implemented backends ({}), so this family owes a real \
             backend comparison: measure one workload through each and state the \
             relationship in docs/scenarios/cpu.md",
            backends.join(" and ")
        );
    }
    eprintln!(
        "{} sources measure through exactly one backend; {} backends are declared and not \
         built: {}",
        implemented.len(),
        declared_only.len(),
        declared_only.join(", ")
    );
}

/// The one comparison this family does own: two mechanisms, one busy core.
///
/// `cpu.util --cpu N` reads `/proc/stat`, which counts every task that ran on
/// that core. `cpu.oncpu_time --pid` reads `/proc/<pid>/stat`, which counts one
/// process wherever it ran. Different files, different collectors, different
/// keys -- and a core with exactly one spinning process pinned to it makes them
/// the same number. That is the ground truth: the scenario created the only
/// tenant of that core, so both readings have to be one whole cpu.
///
/// It takes two runs rather than one session, because a session observes one
/// target and these are two. That is a limit worth knowing before phase two
/// tries to join a per-core series to a per-process one.
#[test]
fn a_core_with_one_tenant_reads_the_same_through_two_files() {
    let _alone = machine_to_itself();
    let _one_at_a_time = CONTENDED_CPU
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cpu = a_core_of_its_own();
    let Some(spinner) = Spinner::start("omnonetenant", Some(cpu)) else {
        eprintln!("skipped: this machine has no `taskset`, so no core can be given one tenant");
        return;
    };

    let core = run(&[
        "cpu.util",
        "--cpu",
        &cpu.to_string(),
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "400ms",
        "--format",
        "json",
    ]);
    let Outcome::Measured { document: core, .. } = core else {
        eprintln!("skipped: this machine will not report per-core utilisation");
        return;
    };
    let process = run(&[
        "cpu.oncpu_time",
        "--pid",
        &spinner.pid(),
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "400ms",
        "--format",
        "json",
    ]);
    let Outcome::Measured {
        document: process, ..
    } = process
    else {
        eprintln!("skipped: this machine will not report a process's cpu time");
        return;
    };

    let busy = core["results"][0]["outputs"][0]["series"][0]["mean"]
        .as_f64()
        .expect("a share");
    let window = process["results"][0]["observed_ns"]
        .as_f64()
        .expect("a window");
    let series = &process["results"][0]["outputs"][0]["series"][0];
    let consumed = series["mean"].as_f64().expect("a mean")
        * series["count"].as_f64().expect("a polled series");
    let share_of_the_window = consumed / window;

    assert!(
        busy > 0.9,
        "a core with a spinning process pinned to it reads {busy:.3} busy in /proc/stat"
    );
    assert!(
        share_of_the_window > 0.2,
        "the pinned process is reported as having consumed {share_of_the_window:.3} of the \
         window, so it was not spinning on that core at all"
    );
    // The invariant, whoever else is on the core: a core is at least as busy
    // as the tasks pinned to it. A `busy` computed over the wrong denominator,
    // or read off the wrong line of /proc/stat, breaks this.
    assert!(
        busy + 0.05 >= share_of_the_window,
        "the core reads {busy:.3} busy while one of its tasks alone consumed \
         {share_of_the_window:.3} of the window: one of the two files is being read wrong"
    );
    // And when this scenario is the core's only tenant -- which it is on a
    // machine that is not running seven other streams' suites -- the two are
    // the same number.
    if share_of_the_window > 0.9 {
        assert!(
            (busy - share_of_the_window).abs() < 0.15,
            "the core was busy {busy:.3} and its only tenant consumed \
             {share_of_the_window:.3}"
        );
    } else {
        eprintln!(
            "the pinned tenant got {share_of_the_window:.3} of a core that was \
             {busy:.3} busy: something else is running there, so only the inequality \
             could be checked"
        );
    }
}

/// The core a scenario gives one tenant.
///
/// The second to last rather than the last, which is where `family_sched.rs`
/// pins its spinners -- and where every other worktree of this wave pins
/// theirs, since eight of them are running the same suite on this machine at
/// once. Two suites pinning to the same core is one suite measuring the
/// other's load, and the failure reads like a wrong number.
fn a_core_of_its_own() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    cpus.saturating_sub(2)
}

/// Only one scenario may contend a core at a time.
///
/// `cargo test` runs the tests of one binary in parallel threads, and a
/// scenario that gives a core exactly one tenant cannot share it with another
/// scenario's spinners.
static CONTENDED_CPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The machine itself.
///
/// `cargo test` runs the tests of one binary in parallel threads, and most of
/// these scenarios are load: a sweep runs eight measurements at once, and a
/// profile at a hundred thousand hertz is a busy cpu of its own. A scenario
/// that measures a *share* -- how much of its window a program spent on a cpu,
/// how busy a core was -- cannot run beside that, because a spinning workload
/// on a loaded machine really does get less cpu than its wall clock, and the
/// failure reads like a wrong number rather than like two tests at once.
///
/// So the lock is the other way round from the usual reading: a scenario that
/// needs a quiet machine takes it alone, and everything that is merely load
/// takes it shared.
static MACHINE: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Held by a scenario whose answer depends on how much cpu its workload got.
fn machine_to_itself() -> std::sync::RwLockWriteGuard<'static, ()> {
    // A scenario that failed poisons the lock, and a poisoned lock would turn
    // one failure into every failure. The guard is about the machine, not
    // about shared state, so there is nothing to protect from a panic.
    MACHINE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Held by a scenario that is load and does not mind company.
fn may_load_the_machine() -> std::sync::RwLockReadGuard<'static, ()> {
    MACHINE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Class C -- the loss path
// ---------------------------------------------------------------------------

/// A profile asked for more samples than the kernel will deliver says what it
/// did not take.
///
/// The ceiling is `kernel.perf_event_max_sample_rate`, and asking for a
/// hundred thousand samples a second crosses it on any machine: the kernel
/// stops delivering for a while and says so with `PERF_RECORD_THROTTLE`, and
/// the ring overflows while the drain is behind. Both are losses whose size
/// nobody knows, and the run has to say so -- in the text a person reads and in
/// the JSON a harness reads, because a run that lost a third of its samples and
/// renders as a clean profile is the failure this whole project is against.
#[test]
fn a_profile_driven_past_the_kernels_ceiling_says_what_it_did_not_take() {
    let _load = may_load_the_machine();
    let Some(spinner) = Spinner::start("omnprofileloss", None) else {
        eprintln!("skipped: /bin/sh could not be copied, so there is no workload to profile");
        return;
    };
    let axes = [
        "cpu.profile",
        "--pid",
        &spinner.pid(),
        "-m",
        "sample@100000hz",
        "-e",
        "topk",
        "--stack",
        "fp",
        "-d",
        "150ms",
    ];

    let mut as_json: Vec<&str> = axes.to_vec();
    as_json.extend(["--format", "json"]);
    let Outcome::Measured { document, .. } = run(&as_json) else {
        eprintln!("skipped: this machine will not sample at all");
        return;
    };
    let losses = &document["results"][0]["losses"];
    let reasons = losses["reasons"]
        .as_object()
        .expect("the losses carry their reasons");
    let throttled = losses["throttled"].as_u64().unwrap_or(0);
    let named: u64 = reasons
        .iter()
        .filter(|(reason, _)| reason.contains("throttled"))
        .filter_map(|(_, count)| count.as_u64())
        .sum();
    assert!(
        throttled + named > 0,
        "a hundred thousand samples a second was not throttled on this machine, so this \
         scenario measured nothing: {losses}"
    );
    // The field, not just the sentence. `losses.throttled` is what a consumer
    // filters on, and a throttled run that leaves it at zero reads as clean to
    // everything but a human.
    assert!(
        throttled > 0,
        "the reasons say this run was throttled {named} times and `losses.throttled` is \
         zero: {losses}"
    );
    // And the periods must not be counted as samples. How many samples were not
    // taken inside a throttled window is not knowable from userspace, so a
    // `dropped` count that nothing explains as a ring overflow is a precision
    // nobody has.
    let dropped = losses["dropped"].as_u64().unwrap_or(0);
    if dropped > 0 {
        assert!(
            reasons
                .keys()
                .any(|reason| reason.contains("ring overflowed")),
            "{dropped} samples are counted as dropped and no ring overflowed, so the \
             {throttled} throttled periods are being reported as a number of samples: {losses}"
        );
    }

    // The same run, rendered for a person. A loss that only a JSON reader sees
    // is a loss most readers will not see.
    let Outcome::Measured { rendered, .. } = run(&axes) else {
        eprintln!("skipped: this machine will not sample at all");
        return;
    };
    assert!(
        rendered.contains("LOSS"),
        "the text rendering of a throttled run does not say it lost anything:\n{rendered}"
    );
    assert!(
        rendered.contains("perf_event_max_sample_rate"),
        "the run does not name the ceiling it hit, which is the one thing a reader can \
         act on:\n{rendered}"
    );
    eprintln!(
        "the kernel throttled this profile over {throttled} periods, counted apart from \
               the {dropped} samples the ring reported losing"
    );
}

/// A poll faster than the kernel's own accounting gets the kernel's
/// resolution, and only one of the two sources that read it says so.
///
/// The request names the cadence, so the scenario knows what it asked for:
/// five hundred readings of a millisecond each over half a second. `/proc/stat`
/// credits cpu time in ticks of ten milliseconds, so four hundred and fifty of
/// those readings find a counter that has not moved -- and a share of a window
/// where nothing moved is not zero, it is nothing, which is why
/// `src/collect/cpu/util.rs` emits no event for it.
///
/// That is the right arithmetic and it is the whole point of the scenario: the
/// readings that do arrive are still right. The core has exactly one tenant, so
/// its utilisation is one whatever the cadence, and a collector that had
/// reported those empty windows as idle would say something else.
///
/// What the scenario checks after that is the disclosure, from both of the
/// sources that read the same tick. `cpu.oncpu_time` states the granularity and
/// how far from the truth a reading can be, predicted from the interval;
/// `cpu.util` states what it actually delivered -- how many of the reads found
/// the file unmoved -- which is the same disclosure with the measurement in it.
/// Neither said anything at all until the defect this scenario recorded was
/// fixed.
#[test]
fn a_poll_faster_than_the_kernels_tick_gets_the_kernels_resolution() {
    const REQUESTED: f64 = 500.0;

    let _alone = machine_to_itself();
    let _one_at_a_time = CONTENDED_CPU
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cpu = a_core_of_its_own();
    let Some(tenant) = Spinner::start("omnfastpoll", Some(cpu)) else {
        eprintln!("skipped: this machine has no `taskset`, so no core can be given one tenant");
        return;
    };

    let Outcome::Measured { document, .. } = run(&[
        "cpu.util",
        "--cpu",
        &cpu.to_string(),
        "-m",
        "poll@1ms",
        "-e",
        "summary",
        "-d",
        "500ms",
        "--format",
        "json",
    ]) else {
        eprintln!("skipped: this machine will not report per-core utilisation");
        return;
    };

    let result = &document["results"][0];
    let delivered = result["events_seen"].as_f64().expect("a count of readings");
    let busy = result["outputs"][0]["series"][0]["mean"]
        .as_f64()
        .expect("a share");

    // The shortfall, in the run's own words. A series that carries a tenth of
    // the cadence it was asked for and says nothing is the failure this whole
    // wave is against, and the number has to be the one that happened rather
    // than one derived from the interval.
    let disclosures = result["degradations"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|note| note.as_str().map(str::to_owned));
    let Some(shortfall) = disclosures
        .into_iter()
        .find(|note| note.contains("unmoved"))
    else {
        // A machine so loaded that a round of reads takes longer than the tick
        // never reads twice inside one, and then there is no shortfall to
        // disclose -- the run says that instead, as late ticks. That is a
        // machine this scenario cannot be run on, not a missing disclosure.
        let late = result["losses"]["late_ticks"].as_u64().unwrap_or(0);
        assert!(
            late > 0,
            "{delivered} of {REQUESTED:.0} readings arrived, no tick ran late, and the run \
             says nothing about the ones that did not arrive: {}",
            result["degradations"]
        );
        eprintln!(
            "skipped: {late} of the {REQUESTED:.0} one-millisecond rounds took longer than a \
             millisecond, so this machine never polled inside the kernel's tick"
        );
        return;
    };
    assert!(
        shortfall.contains("ticks of 10 ms"),
        "the disclosure does not name the kernel tick that swallowed the readings: {shortfall}"
    );
    let unmoved: f64 = shortfall
        .split_whitespace()
        .next()
        .and_then(|word| word.parse().ok())
        .unwrap_or_else(|| panic!("the disclosure opens with the count: {shortfall}"));
    // The disclosure has to be about the bulk of the reads, not a footnote.
    // Ten reads per tick is nine tenths unmoved; what is asserted is two
    // thirds, which a loaded machine taking fewer reads in total still meets
    // and a disclosure of the odd stray read does not.
    assert!(
        unmoved > delivered * 2.0,
        "the run reported {delivered} readings and calls {unmoved} of its reads unmoved: \
         the disclosure is not about the resolution this source actually delivered"
    );
    assert!(
        unmoved + delivered <= REQUESTED + 1.0,
        "the run accounts for {} reads where {REQUESTED:.0} were asked for",
        unmoved + delivered
    );

    assert!(
        delivered <= REQUESTED + 1.0,
        "a request for one reading a millisecond over half a second produced {delivered} \
         of them, so the series describes a cadence nobody asked for"
    );
    assert!(
        busy > 0.9,
        "a core with one spinning tenant read {busy:.3} busy over {delivered} readings: \
         reading inside the kernel's tick changed the answer, not just its resolution"
    );

    // The same cadence against the same tick, through the other source that
    // reads it. This one has to say what the reader is holding.
    let Outcome::Measured { rendered, .. } = run(&[
        "cpu.oncpu_time",
        "--pid",
        &tenant.pid(),
        "-m",
        "poll@1ms",
        "-e",
        "summary",
        "-d",
        "200ms",
    ]) else {
        eprintln!("skipped: this machine will not read a process's cpu time");
        return;
    };
    assert!(
        rendered.contains("ticks of") && rendered.contains("away from what was consumed"),
        "polling ten times faster than the kernel credits cpu time is not disclosed:\n{rendered}"
    );
    eprintln!(
        "`cpu.util --cpu {cpu} -m poll@1ms` delivered {delivered} of {REQUESTED} readings, \
         and says so: {shortfall}"
    );
}

/// A profile costs its window and not a second more.
///
/// The reference is a counting run of the same length over the same target:
/// same startup, same catalog, same probe, same teardown, differing only in
/// the mechanism that measures. So the difference between the two wall clocks
/// is what profiling costs beyond counting, and nothing here depends on how
/// fast this machine is.
///
/// It used to be a second, flat -- independent of the window, the frequency,
/// the target and whether stacks were asked for at all -- because every
/// sampling collector parsed `/proc/kallsyms` when it built its symbolizer,
/// and then the unwinder asked at the end whether the kernel's symbols were
/// readable, which parsed it whatever the run had collected. Both are now paid
/// only by a run that has kernel frames to name.
///
/// The minimum of two runs each, and the machine to itself while they run: a
/// wall clock measured beside forty-eight spinners is a measurement of the
/// spinners, and the fastest run is the one least disturbed.
#[test]
fn a_profile_costs_its_window_and_not_a_fixed_second() {
    const WINDOW: &str = "300ms";
    const LONGER: &str = "900ms";
    const ROUNDS: usize = 2;
    // A second was the defect. Half of one is far inside it and far outside
    // anything the measurement itself costs, which is twenty milliseconds.
    const SLACK: Duration = Duration::from_millis(500);

    let _alone = machine_to_itself();
    let Some(target) = Spinner::start("omnprofilecost", None) else {
        eprintln!("skipped: /bin/sh could not be copied, so there is no target to measure");
        return;
    };
    let pid = target.pid();

    let fastest = |axes: &[&str]| -> Option<Duration> {
        let mut best = None;
        for _ in 0..ROUNDS {
            let started = Instant::now();
            let outcome = run(axes);
            let elapsed = started.elapsed();
            if !matches!(outcome, Outcome::Measured { .. }) {
                return None;
            }
            best = Some(best.map_or(elapsed, |held: Duration| held.min(elapsed)));
        }
        best
    };

    let Some(counting) = fastest(&[
        "cpu.instructions",
        "--pid",
        &pid,
        "-m",
        "count",
        "-e",
        "summary",
        "-d",
        WINDOW,
    ]) else {
        eprintln!("skipped: this machine will not open a hardware counter");
        return;
    };
    let Some(profiling) = fastest(&[
        "cpu.profile",
        "--pid",
        &pid,
        "-m",
        "sample@99hz",
        "-e",
        "topk",
        "--stack",
        "none",
        "-d",
        WINDOW,
    ]) else {
        eprintln!("skipped: this machine will not sample at all");
        return;
    };

    assert!(
        profiling <= counting + SLACK,
        "a {WINDOW} profile took {profiling:?} where a {WINDOW} count of the same target \
         took {counting:?}: the profile is paying a fixed cost before it samples anything"
    );

    // And the cost is the window's, not a constant: a longer profile must cost
    // the difference between the two windows, rather than the same flat second
    // plus its window.
    let Some(longer) = fastest(&[
        "cpu.profile",
        "--pid",
        &pid,
        "-m",
        "sample@99hz",
        "-e",
        "topk",
        "--stack",
        "none",
        "-d",
        LONGER,
    ]) else {
        eprintln!("skipped: this machine will not sample at all");
        return;
    };
    let overhead = longer.saturating_sub(Duration::from_millis(900));
    assert!(
        overhead <= SLACK,
        "a {LONGER} profile took {longer:?}, which is {overhead:?} more than its own window"
    );
    eprintln!(
        "a {WINDOW} profile took {profiling:?} against {counting:?} for a count of the same \
         window; a {LONGER} profile overran its window by {overhead:?}"
    );
}

/// A counter that never ran says which of the two reasons it was.
///
/// `running_ns == 0` has two causes and they need opposite responses: the
/// target never got a cpu, or the target ran and the counter did not, because
/// the pmu was full and a group is crowded out whole. The code used to pick the
/// first and print it, so a reader whose program was plainly working was told
/// it had not run.
///
/// The two are told apart by `enabled_ns`, which for a per-task event advances
/// only while the task is on a cpu. This scenario drives the half a test can
/// drive: a target that really is asleep for the whole window, whose message
/// must now name the window it was enabled for -- no time at all -- rather than
/// leaving the reader to guess which of the two happened. The other half is
/// checked wherever this machine actually crowds a group out, in
/// `example_counted` above, and against readings the kernel would produce in
/// the unit tests of `src/collect/cpu/counters.rs`.
#[test]
fn a_counter_over_a_target_that_never_ran_says_that_is_what_happened() {
    let Ok(mut asleep) = Command::new("/bin/sleep").arg("5").spawn() else {
        eprintln!("skipped: this machine has no `/bin/sleep`, so nothing here is idle on purpose");
        return;
    };
    let pid = asleep.id().to_string();
    // Long enough that its exec is behind it: a process still starting up is a
    // process that ran.
    std::thread::sleep(Duration::from_millis(250));

    let outcome = run(&[
        "cpu.instructions",
        "--pid",
        &pid,
        "-m",
        "count",
        "-e",
        "summary",
        "-d",
        "300ms",
        "--format",
        "json",
    ]);
    let _ = asleep.kill();
    let _ = asleep.wait();

    let Outcome::Measured { document, .. } = outcome else {
        eprintln!("skipped: this machine will not open a hardware counter");
        return;
    };
    let losses = &document["results"][0]["losses"];
    let reasons = losses["reasons"]
        .as_object()
        .expect("the losses carry their reasons");
    let reason = reasons
        .keys()
        .next()
        .unwrap_or_else(|| {
            panic!("a counter over a process that slept through the window reported no reason: {losses}")
        })
        .clone();

    assert!(
        reason.contains("enabled for no time at all"),
        "the counter over a sleeping target does not say that it was never enabled, so a \
         reader cannot tell this from a pmu that had no room: {reason}"
    );
    assert!(
        reason.contains("not the same as a count of zero"),
        "an absent count is being offered as a count: {reason}"
    );
    assert!(
        !reason.contains("pmu"),
        "a target that slept through the window is being blamed on the hardware: {reason}"
    );
    eprintln!("a target that never ran is told from a pmu with no room: {reason}");
}

/// A program that exits before the window closes ends the run and says so.
///
/// The workload chooses the moment: it sleeps for two hundred milliseconds
/// against a request for five seconds. A run that reported the five seconds it
/// asked for, or that kept polling a pid the kernel had reused, would be
/// reporting a window that did not happen.
#[test]
fn a_target_that_exits_ends_the_window_and_names_the_reason() {
    let _alone = machine_to_itself();
    let mut args: Vec<String> = ["cpu.oncpu_time", "-m", "poll@50ms", "-e", "summary"]
        .iter()
        .map(|part| (*part).to_string())
        .collect();
    args.extend(["-d".to_string(), "5s".to_string()]);
    args.extend(["--format".to_string(), "json".to_string(), "--".to_string()]);
    args.extend(workload_program());

    let started = Instant::now();
    let Outcome::Measured { document, .. } = run_workload(Some("sleep:200"), &args) else {
        eprintln!("skipped: this machine will not read a process's cpu time");
        return;
    };
    let elapsed = started.elapsed();
    let result = &document["results"][0];
    let window = result["observed_ns"].as_f64().expect("a window");

    assert!(
        elapsed < Duration::from_secs(2),
        "the run asked for five seconds against a workload that lives for two hundred \
         milliseconds and took {elapsed:?} to notice"
    );
    assert!(
        (0.15..1.5).contains(&(window / 1e9)),
        "the window reported is {:.0} ms for a workload that lived about 200 ms",
        window / 1e6
    );
    let stopped = result["stopped_because"].as_str().unwrap_or_default();
    assert!(
        stopped.contains("exited"),
        "the run ended because the target exited and says `{stopped}`"
    );
}

/// A profile of a program whose work is in another task finds none of it, and
/// says nothing.
///
/// This is the loss the class exists for: not a number that is missing, but a
/// run that lost everything and renders as a clean profile. One shell, one
/// window, two sources. `cpu.instructions` is a counting event, which the
/// kernel inherits, so it follows the child the shell starts and reports the
/// work. `cpu.profile` is a sampling event, which cannot be inherited -- the
/// kernel refuses to map a ring for an inherited per-task event -- so it
/// samples the shell, which is blocked in `wait` for the whole run.
///
/// `src/perf.rs` has `SampledEvent::sampled_threads` to say it, "for the record
/// that travels with the numbers", and until this defect was fixed nothing
/// called it: the run reported "no loss", and the flame graph of a thread pool
/// was a flame graph of the thread that started it.
///
/// So the invariant this now pins is the one the defect entry asked for: the
/// profile either finds the work or states that it did not see it. It cannot
/// find it -- the kernel refuses to map a ring for an inherited per-task event,
/// which is a property of the mechanism and not of this build -- so what is
/// asserted is the statement, and that it is a *degradation* rather than a
/// loss. There is no drop to count here: the samples were never taken, and a
/// number of them would be a precision nobody has.
#[test]
fn a_profile_that_cannot_see_the_work_says_it_did_not_see_it() {
    let _load = may_load_the_machine();
    const WORK: &str =
        r#"[/bin/sh, -c, "(i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done) & wait"]"#;
    let session = format!(
        r#"
name: the work is in the child
format: json
requests:
  - label: what the program did
    source: cpu.instructions
    target: {{launch: {WORK}}}
    mode: count
    emit: [summary]
  - label: where it did it
    source: cpu.profile
    target: {{launch: {WORK}}}
    mode: sample@997hz
    emit: [topk]
    stack: fp
"#
    );
    let Some(document) = session_run(&session) else {
        return;
    };
    let results = document["results"].as_array().expect("two results");
    let instructions = results[0]["outputs"][0]["series"][0]["last"]
        .as_f64()
        .expect("a counted value");
    let samples = results[1]["events_seen"].as_f64().expect("a sample count");
    let window = results[1]["observed_ns"].as_f64().expect("a window");
    let losses = &results[1]["losses"];

    // The workload's property, established by the source that can see it: this
    // program spent its window doing arithmetic, in a task the profiler was not
    // opened on.
    assert!(
        instructions > 1e8,
        "the workload retired only {instructions:.0} instructions, so it is not the busy \
         program this scenario needs"
    );
    let expected = 997.0 * window / 1e9;
    assert!(
        samples < expected * 0.05,
        "the profile took {samples} samples of a program that was on a cpu for {:.0} ms, \
         which is the work being found: a per-task sampling event has started following \
         the tasks its target starts, and this scenario is measuring something else now",
        window / 1e6
    );

    // The statement, in the record that travels with the numbers. Without it a
    // reader has a flame graph of a shell blocked in `wait` and no way to know
    // that the program's whole window is missing from it.
    let said = results[1]["degradations"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|note| note.as_str().map(str::to_owned))
        .find(|note| note.contains("not inherited"))
        .unwrap_or_else(|| {
            panic!(
                "the profile saw {samples} samples of a program that retired \
                 {instructions:.0} instructions in another task, and says nothing about \
                 what it did not follow: {}",
                results[1]["degradations"]
            )
        });
    assert!(
        said.contains("child process"),
        "the disclosure names the threads it did not sample and not the child processes, \
         which is where this workload's cpu went: {said}"
    );
    // Said as a degradation, not as a loss: nothing was dropped, because
    // nothing was ever delivered to drop.
    assert_eq!(
        losses["dropped"], 0,
        "samples that were never taken are being counted as samples that were lost, which \
         states a number nobody can know: {losses}"
    );
    eprintln!(
        "a program that retired {instructions:.0} instructions over {:.0} ms was profiled \
         with {samples} samples, and the run says why: {said}",
        window / 1e6
    );
}

// ---------------------------------------------------------------------------
// Class D -- the refusal
// ---------------------------------------------------------------------------

/// The three things the tool says no to, and whether each names what is
/// actually missing.
///
/// They are different answers and a reader acts on them differently: a
/// rejection means the request cannot mean anything and no machine will serve
/// it; "no viable backend" means this machine cannot, and the same request
/// belongs on another one; "no collector" means this build does not implement
/// it, and the request is fine. A refusal that sends a reader to the wrong
/// sysctl, the wrong capability or the wrong flag costs more than no message.
///
/// So each carries its own exit code, and this pins both halves: the sentence a
/// person reads and the number a script branches on. The three used to be two
/// -- "no viable backend" exited 2, the same as a request that cannot mean
/// anything, one line under a hint saying the opposite -- which was defect 6 of
/// `docs/scenarios/cpu.md`.
#[test]
fn each_refusal_names_the_thing_that_is_missing() {
    // The request, the exit code, and what the message has to contain.
    let rejections: [(&[&str], &str); 7] = [
        (
            &["cpu.ipc", "-m", "count", "-e", "hist", "--", "/bin/true"],
            "cannot emit `hist`",
        ),
        (
            &["cpu.profile", "-m", "count", "-e", "raw", "--", "/bin/true"],
            "cannot be observed in `count` mode",
        ),
        (
            &[
                "cpu.util",
                "--pid",
                "1",
                "-m",
                "poll@100ms",
                "-e",
                "summary",
            ],
            "cannot be measured per pid",
        ),
        (
            &[
                "cpu.throttle",
                "--system",
                "-m",
                "poll@100ms",
                "-e",
                "summary",
            ],
            "cannot be measured per system",
        ),
        (
            &[
                "cpu.migrations",
                "--cpu",
                "0",
                "-m",
                "count",
                "-e",
                "summary",
            ],
            "not attributable to a single core",
        ),
        (
            &[
                "cpu.instructions",
                "-m",
                "count",
                "-e",
                "summary",
                "--filter",
                "nope == 1",
                "--",
                "/bin/true",
            ],
            "has no field `nope`",
        ),
        (
            &[
                "cpu.instructions",
                "-m",
                "count",
                "-e",
                "summary",
                "-k",
                "mem",
                "--",
                "/bin/true",
            ],
            "does not carry the `mem` key",
        ),
    ];
    for (args, expected) in rejections {
        let (code, message) = run(args).refusal();
        assert_eq!(
            code,
            2,
            "`{}` should be rejected as meaningless: {message}",
            args.join(" ")
        );
        assert!(
            message.contains(expected),
            "`{}` was rejected without saying `{expected}`: {message}",
            args.join(" ")
        );
        // A rejection is about the request, so nothing about it is the
        // machine's fault. Sending a reader to a sysctl or a capability here
        // is the expensive kind of wrong.
        for blamed in ["perf_event_paranoid", "CAP_", "no viable backend"] {
            assert!(
                !message.contains(blamed),
                "a rejection of `{}` blames `{blamed}`: {message}",
                args.join(" ")
            );
        }
    }

    // This machine cannot serve these, and the reason is a named requirement.
    // Both are legitimate requests that another machine runs.
    let unserved: [(&[&str], &str); 2] = [
        (
            &["cpu.topdown", "-m", "count", "-e", "raw", "--", "/bin/true"],
            "slots",
        ),
        (
            &["cpu.flow", "-m", "trace", "-e", "raw", "--", "/bin/true"],
            "intel_pt",
        ),
    ];
    for (args, requirement) in unserved {
        let (code, message) = run(args).refusal();
        assert_eq!(
            code,
            1,
            "`{}` is a valid request this machine cannot serve, which is not the same answer \
             as a request that cannot mean anything and must not share its code: {message}",
            args.join(" ")
        );
        assert!(
            message.contains("no viable backend"),
            "`{}` is not served here and does not say so: {message}",
            args.join(" ")
        );
        assert!(
            message.contains(requirement),
            "`{}` does not name the `{requirement}` it needs: {message}",
            args.join(" ")
        );
        // The difference between "this machine" and "this request", in the
        // one line a reader acts on.
        assert!(
            message.contains("Use --no-resolve"),
            "`{}` does not say the request is valid elsewhere: {message}",
            args.join(" ")
        );
        assert!(
            !message.contains("perf_event_paranoid"),
            "a missing counter is blamed on privileges: {message}"
        );
    }

    // This build does not implement these, which is neither of the above.
    let unbuilt: [(&[&str], &str); 2] = [
        (
            &[
                "cpu.cycles",
                "--backend",
                "force:pmu:sw",
                "-m",
                "count",
                "-e",
                "summary",
                "--",
                "/bin/true",
            ],
            "no collector is written for `cpu.cycles` on backend `pmu:sw`",
        ),
        (
            &[
                "cpu.util",
                "--backend",
                "force:fs:cgroup",
                "--cgroup",
                "/",
                "-m",
                "poll@100ms",
                "-e",
                "summary",
                "-d",
                "100ms",
            ],
            "no collector is written for `cpu.util` on backend `fs:cgroup`",
        ),
    ];
    for (args, expected) in unbuilt {
        let (code, message) = run(args).refusal();
        assert_eq!(
            code,
            3,
            "`{}` is unimplemented rather than failed, and exit {code} says otherwise: \
             {message}",
            args.join(" ")
        );
        assert!(
            message.contains(expected),
            "`{}` does not say what is not written: {message}",
            args.join(" ")
        );
    }
}

/// A mode the catalog offers and this build does not serve exits 3, not 1.
///
/// `cpu.cycles -m sample` is a valid request -- the catalog declares the mode,
/// the machine has the counter -- that this build cannot serve because no
/// collector implements sampling for a counting source. That is the same
/// statement as "no collector is written", and it now carries the same code.
///
/// It used to exit 1, which is what a caller reads as a run that went wrong,
/// and that was defect 4 of `docs/scenarios/cpu.md`. What decides it is where
/// the refusal is raised: this one comes out of the factory, which is a pure
/// function of the resolved request, so nothing about this machine or this run
/// can be involved in it.
#[test]
fn a_mode_the_build_does_not_implement_exits_as_unimplemented() {
    let (code, message) = run(&[
        "cpu.cycles",
        "-m",
        "sample@99hz",
        "-e",
        "hist",
        "--",
        "/bin/true",
    ])
    .refusal();
    assert_eq!(
        code, 3,
        "a mode no collector implements is this build's gap, not this run's failure: {message}"
    );
    // The code is the half a script reads; the message is the half a person
    // reads, and it has to name the mechanism and the way round it.
    assert!(message.contains("sampling path"), "{message}");
    assert!(message.contains("--mode count"), "{message}");
}

/// A workload that goes to sleep a chosen number of times is taken off the cpu
/// that many times.
///
/// `cpu.context_switches` counts switches *away* from the task, and a sleep is
/// exactly one of them. So the workload chooses the number: fifty sleeps is
/// fifty switches, plus whatever it costs to become the workload -- measured
/// the same way as the instruction count, by running the same program with no
/// sleeps at all and subtracting.
#[test]
fn a_workload_that_sleeps_a_chosen_number_of_times_is_switched_that_many_times() {
    let _load = may_load_the_machine();
    const SLEEPS: f64 = 50.0;
    let axes = ["-m", "count", "-e", "summary"];
    let Some(baseline) = counted("cpu.context_switches", "sleeps:0", &axes) else {
        return;
    };
    let Some(sleeping) = counted("cpu.context_switches", "sleeps:50", &axes) else {
        return;
    };
    // The workload guarantees a floor rather than an exact number: fifty
    // sleeps are fifty switches away from the task, and becoming the workload
    // costs a handful more that vary with what the loader and the filesystem
    // did. So the count has to be at least the fifty, and close enough to them
    // that it is those and not the machine.
    assert!(
        sleeping >= SLEEPS,
        "a workload that slept {SLEEPS} times was switched away only {sleeping} times"
    );
    assert!(
        sleeping < SLEEPS * 2.0 + baseline,
        "a workload that slept {SLEEPS} times was switched away {sleeping} times, against \
         {baseline} for the same program with no sleeps at all: that is not the sleeping"
    );
}

/// PSI moves when there are more runnable tasks than cpus to run them, and
/// does not when there are not.
///
/// The ground truth is the scheduler's own arithmetic rather than anything the
/// tool computed: twice as many spinning tasks as this machine has cpus
/// guarantees that at every instant about half of them are runnable and not
/// running, and `cpu.pressure` counts exactly that time. The check is
/// two-sided, because a counter stuck at a plausible value passes a one-sided
/// one: the same measurement on the machine as it was, before the load
/// started, has to be far lower.
#[test]
fn pressure_rises_when_there_are_more_runnable_tasks_than_cpus() {
    let _alone = machine_to_itself();
    let _one_at_a_time = CONTENDED_CPU
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(quiet) = stalled_share() else {
        return;
    };
    if quiet > 0.5 {
        eprintln!(
            "skipped: the machine was already stalled {:.0} % of the window before this \
             scenario loaded it, so its own load cannot be told from somebody else's",
            quiet * 100.0
        );
        return;
    }

    let cpus = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    let load: Vec<Spinner> = (0..cpus * 2)
        .filter_map(|_| Spinner::started("omnpressure", None))
        .collect();
    if load.len() != cpus * 2 {
        eprintln!("skipped: /bin/sh could not be copied, so the machine cannot be loaded");
        return;
    }
    std::thread::sleep(Duration::from_millis(200));
    let Some(loaded) = stalled_share() else {
        return;
    };
    drop(load);

    assert!(
        loaded > 0.2,
        "with {} spinning tasks on {cpus} cpus, the cpus were reported as stalled for \
         {:.1} % of the window",
        cpus * 2,
        loaded * 100.0
    );
    assert!(
        loaded > quiet + 0.1,
        "loading the machine moved the stall share from {:.1} % to {:.1} %, which is not \
         the difference between an idle machine and one with twice as many runnable tasks \
         as cpus",
        quiet * 100.0,
        loaded * 100.0
    );
}

/// The share of the window in which at least one task was waiting for a cpu.
fn stalled_share() -> Option<f64> {
    match run(&[
        "cpu.pressure",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "400ms",
        "--format",
        "json",
    ]) {
        Outcome::Measured { document, .. } => {
            let series = &document["results"][0]["outputs"][0]["series"][0];
            Some(series["max"].as_f64().expect("a share"))
        }
        Outcome::Refused { message, .. } => {
            assert!(
                machine_cannot_serve(&message) || message.contains("pressure"),
                "the request was refused rather than unserved: {message}"
            );
            eprintln!("skipped: {message}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The examples are the scenarios, and they have to run
// ---------------------------------------------------------------------------

/// A run that was late many times explains it once.
///
/// `Losses::reasons` is a map from the reason to how often it happened, and the
/// late-tick reason used to embed the duration that made it late -- so seven
/// late ticks were seven reasons, each counted once, while the throttles and
/// overflows beside them aggregated exactly as the map is meant to. A run long
/// enough to be late a thousand times printed a thousand lines and carried a
/// thousand keys into its JSON, and the one number a reader wants from them --
/// *how* late were they -- was not there, because each was its own key. Defect
/// 8 of `docs/scenarios/cpu.md`.
///
/// Ten microseconds is shorter than any machine reads `/proc/stat`, so the run
/// is late on essentially every round and the scenario needs nothing of the
/// machine it is on.
#[test]
fn a_run_that_was_late_many_times_explains_it_once() {
    let document = run(&[
        "cpu.util",
        "--cpu",
        "0",
        "-m",
        "poll@10us",
        "-e",
        "summary",
        "-d",
        "50ms",
        "--format",
        "json",
    ])
    .measured();
    let losses = &document["results"][0]["losses"];
    let late = losses["late_ticks"].as_u64().unwrap_or(0);
    assert!(
        late > 20,
        "a ten-microsecond poll of /proc/stat cannot be kept, and this run was late \
         {late} times: {losses}"
    );

    let reasons = losses["reasons"].as_object().expect("the reasons");
    assert_eq!(
        reasons.len(),
        1,
        "{late} late ticks have to share one reason: {reasons:?}"
    );
    let (reason, count) = reasons.iter().next().expect("just counted one");
    assert_eq!(
        count.as_u64(),
        Some(late),
        "and the reason has to carry the count it explains: {reasons:?}"
    );
    assert!(
        reason.contains("outran the 10.0us tick"),
        "naming the cadence that was missed: {reason}"
    );

    // The measurement is beside the count rather than in the key, which is
    // what makes "how late were they" answerable at all.
    let overrun = &losses["overrun"];
    assert_eq!(overrun["tick_ns"].as_u64(), Some(10_000));
    assert!(
        overrun["worst_ns"].as_u64().unwrap_or_default() > 10_000,
        "a tick counted as late has to have overrun it: {overrun}"
    );
    assert!(
        overrun["total_ns"].as_u64().unwrap_or_default()
            >= overrun["worst_ns"].as_u64().unwrap_or_default(),
        "the total has to hold the worst one in it: {overrun}"
    );
}

/// A `--duration` written beside `-f` shortens the session, and says so.
///
/// It used to be accepted and discarded: the session named no duration, the
/// command line named 100 ms, the run lasted as long as the workload did, and
/// `--dump-request` -- the thing a reader checks when they want to know what
/// they asked for -- carried no duration at all. Where the session *did* name
/// one, the file won and the flag was silently ignored. Either answer is
/// defensible; being told is not optional, and a flag that leaves no trace is
/// the silent case this repository refuses everywhere else. Defect 5 of
/// `docs/scenarios/cpu.md`.
///
/// The rule that came out of it is the one a `Session` already describes: the
/// command line may replace what a session holds for the whole of itself --
/// the duration, the budget, the format, the output directory -- and every
/// other axis names one request, which a file may have many of, so the parser
/// refuses those rather than accepting and dropping them.
#[test]
fn a_duration_beside_a_session_file_replaces_the_sessions_own() {
    let dumped = run(&[
        "-f",
        "examples/cpu-known-work.yml",
        "-d",
        "100ms",
        "--dump-request",
        "--format",
        "json",
    ]);
    let (document, rendered) = match dumped {
        Outcome::Measured { document, rendered } => (document, rendered),
        Outcome::Refused { code, message } => {
            panic!("dumping a request should not fail (exit {code}): {message}")
        }
    };
    assert!(
        rendered.contains("100ms"),
        "the dumped request has to carry the duration that was asked for:\n{rendered}"
    );
    for request in document["requests"].as_array().expect("requests") {
        assert_eq!(
            request["duration"], "100ms",
            "every request of the session takes the duration the command line named: \
             {request}"
        );
    }

    // And where the session names a duration of its own, the replacement is
    // said out loud rather than happening quietly in either direction.
    let noted = Command::new(BINARY)
        .args([
            "-f",
            "examples/cpu-saturation.yml",
            "-d",
            "100ms",
            "--dump-request",
        ])
        .output()
        .expect("omn should run");
    let notes = String::from_utf8_lossy(&noted.stderr).into_owned();
    assert!(
        notes.contains("--duration 100ms replaced"),
        "a session that names its own duration has to be told which requests were \
         moved:\n{notes}"
    );

    // An axis that describes one request cannot mean anything beside a file
    // that may hold many, and is refused by name rather than dropped.
    for axis in [["-m", "count"], ["-e", "raw"], ["--stack", "fp"]] {
        let refused = Command::new(BINARY)
            .args(["-f", "examples/cpu-known-work.yml"])
            .args(axis)
            .output()
            .expect("omn should run");
        let message = String::from_utf8_lossy(&refused.stderr).into_owned();
        assert_eq!(
            refused.status.code(),
            Some(2),
            "`{}` beside a session file cannot mean anything:\n{message}",
            axis.join(" ")
        );
        assert!(
            message.contains("cannot be used with"),
            "and the refusal has to name both flags:\n{message}"
        );
    }
}

/// A run writing a structured document says that its target shares the stdout.
///
/// A launched program inherits this process's stdout -- its output is the
/// user's, and swallowing it would be worse -- but a structured format is one
/// document, so anything the target prints lands in the middle of it and
/// `omn ... --format json -- ./prog | jq` works exactly until `./prog` says
/// something. The way out existed and the run named it afterwards; nothing
/// warned before. Defect 10 of `docs/scenarios/cpu.md`.
#[test]
fn a_structured_run_that_launches_a_program_says_they_share_the_stdout() {
    let noisy = Command::new(BINARY)
        .args([
            "cpu.instructions",
            "-m",
            "count",
            "-e",
            "summary",
            "--format",
            "json",
            "--",
            "/bin/sh",
            "-c",
            "echo NOISE",
        ])
        .output()
        .expect("omn should run");
    let warning = String::from_utf8_lossy(&noisy.stderr).into_owned();
    assert!(
        warning.contains("same stdout") && warning.contains("-o DIR"),
        "a structured run over a launched program has to say what will happen to its \
         document, and where to put it instead:\n{warning}"
    );
    // It is a warning and not a refusal: the run is legitimate, and redirecting
    // the program's output is the other way out.
    assert!(noisy.status.success(), "{warning}");

    // Text is a report for a person, who can see what came from where, and is
    // what every run in a terminal uses. Warning there would be noise.
    let plain = Command::new(BINARY)
        .args([
            "cpu.instructions",
            "-m",
            "count",
            "-e",
            "summary",
            "--",
            "/bin/sh",
            "-c",
            "echo NOISE",
        ])
        .output()
        .expect("omn should run");
    assert!(
        !String::from_utf8_lossy(&plain.stderr).contains("same stdout"),
        "text is not one document and must not be warned about"
    );
}

/// Every session this family ships is validated, run, and made to answer.
///
/// An example that stopped validating would be bad; one that validates and
/// cannot run is worse, because it is the first thing anyone copies. So each
/// one is run here with a short duration, and the only acceptable outcomes are
/// a measurement or a refusal that names something this machine does not have.
/// A session that is refused because it asks for something meaningless is a
/// broken example and fails.
///
/// Two of them are meant to be refused here and say so in their own headers:
/// `cpu-out-of-reach.yml` needs counters this machine has not got, and
/// `cpu-throttled-cgroup.yml` needs a cgroup with a quota that only root or a
/// delegated session can make.
#[test]
fn every_example_of_this_family_validates_and_runs_or_names_what_it_needs() {
    let _load = may_load_the_machine();
    let mut sessions: Vec<String> = std::fs::read_dir("examples")
        .expect("the examples directory")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            name.starts_with("cpu-")
                .then(|| path.to_string_lossy().into_owned())
        })
        .collect();
    sessions.sort();
    assert!(
        sessions.len() >= 6,
        "only {} cpu examples were found, which is fewer than this family ships",
        sessions.len()
    );

    let queue = std::sync::Mutex::new(sessions.clone().into_iter());
    let unserved: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| loop {
                let Some(path) = queue.lock().expect("the queue").next() else {
                    return;
                };
                let validated = Command::new(BINARY)
                    .args(["validate", "-f", &path])
                    .output()
                    .expect("omn should run");
                assert!(
                    validated.status.success(),
                    "{path} does not validate: {}",
                    String::from_utf8_lossy(&validated.stderr)
                );

                // With the window the file names. `-d` beside `-f` now
                // shortens it and says so -- `a_duration_beside_a_session_file`
                // below is where that is asserted -- but an example is worth
                // running as a reader would run it.
                match run(&["-f", &path, "--format", "json"]) {
                    Outcome::Measured { document, .. } => {
                        let results = document["results"]
                            .as_array()
                            .unwrap_or_else(|| panic!("{path} produced no results"));
                        assert!(!results.is_empty(), "{path} produced no results");
                        for result in results {
                            let outputs = result["outputs"].as_array().expect("outputs");
                            assert!(
                                !outputs.is_empty(),
                                "{path} ran `{}` and emitted nothing",
                                result["source"]
                            );
                        }
                    }
                    Outcome::Refused { code, message } => {
                        assert!(
                            machine_cannot_serve(&message)
                                || message.contains("no viable backend")
                                || message.contains("cgroup"),
                            "{path} is refused for a reason that is not this machine's \
                             (exit {code}): {message}"
                        );
                        unserved
                            .lock()
                            .expect("the results")
                            .push(format!("{path}: {}", message.lines().next().unwrap_or("")));
                    }
                }
            });
        }
    });

    for line in unserved.into_inner().expect("the results") {
        eprintln!("not served here: {line}");
    }
}

/// A cgroup capped at a tenth of a cpu stops a spinning task for nine tenths
/// of every period, and `cpu.throttle` has to report that.
///
/// The ground truth is CFS's own arithmetic rather than anything the tool
/// computed: a quota of 10 ms in a period of 100 ms means a task that wants a
/// whole cpu gets ten milliseconds and is stopped for the other ninety. So the
/// throttled time has to be about nine tenths of the window, and a reading far
/// below it is a quota that was not enforced.
///
/// This machine cannot serve it: the cpu controller is not delegated to a user
/// session's cgroup, so no cgroup that can be created here can have a quota.
/// The scenario is here, and in `examples/cpu-throttled-cgroup.yml`, so that a
/// machine which can serve it runs it -- and the skip names the one thing that
/// is missing rather than passing quietly.
#[test]
fn a_cgroup_capped_at_a_tenth_of_a_cpu_is_stopped_for_nine_tenths_of_the_time() {
    let Some(parent) = own_cgroup() else {
        eprintln!("skipped: this process is in no cgroup v2, so none can be made under it");
        return;
    };
    let root = std::path::Path::new("/sys/fs/cgroup");
    let controllers = std::fs::read_to_string(root.join(&parent[1..]).join("cgroup.controllers"))
        .unwrap_or_default();
    if !controllers.split_whitespace().any(|name| name == "cpu") {
        eprintln!(
            "skipped: the cpu controller is not delegated to `{parent}` (it offers \
             `{}`), so no cgroup made here can carry a quota and nothing can be throttled",
            controllers.trim()
        );
        return;
    }

    let path = root.join(&parent[1..]).join("omn-scenario-throttle");
    if std::fs::create_dir(&path).is_err() && !path.exists() {
        eprintln!("skipped: `{}` could not be created", path.display());
        return;
    }
    let cleanup = CgroupUnderTest(path.clone());
    // A tenth of a cpu, in the kernel's own units.
    if std::fs::write(path.join("cpu.max"), "10000 100000").is_err() {
        eprintln!("skipped: `cpu.max` of the new cgroup could not be written");
        return;
    }
    let Some(spinner) = Spinner::start("omnthrottled", None) else {
        eprintln!("skipped: /bin/sh could not be copied, so there is nothing to throttle");
        return;
    };
    if std::fs::write(path.join("cgroup.procs"), spinner.pid()).is_err() {
        eprintln!("skipped: the spinning task could not be moved into the cgroup");
        return;
    }

    let inner = format!("{parent}/omn-scenario-throttle");
    let Outcome::Measured { document, .. } = run(&[
        "cpu.throttle",
        "--cgroup",
        &inner,
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "600ms",
        "--format",
        "json",
    ]) else {
        eprintln!("skipped: this machine will not read the cgroup's cpu.stat");
        return;
    };
    drop(cleanup);

    let result = &document["results"][0];
    let window = result["observed_ns"].as_f64().expect("a window");
    let series = &result["outputs"][0]["series"][0];
    let stopped = series["mean"].as_f64().expect("a mean")
        * series["count"].as_f64().expect("a polled series");
    let share = stopped / window;
    assert!(
        share > 0.6,
        "a task that wants a whole cpu and may have a tenth of one was reported as stopped \
         for {:.0} % of the window, where the quota says about ninety",
        share * 100.0
    );
}

/// The cgroup a scenario made, removed when it is done with it.
struct CgroupUnderTest(std::path::PathBuf);

impl Drop for CgroupUnderTest {
    fn drop(&mut self) {
        // Only ever after the tasks in it have gone; a cgroup with members
        // refuses to be removed, which is the kernel keeping this honest.
        let _ = std::fs::remove_dir(&self.0);
    }
}
