//! The triage layer, against workloads whose limit the test chose.
//!
//! `omn latency`, `omn saturate` and `omn diff` name a culprit, and a culprit
//! is only worth naming if it is the right one. So every workload here is the
//! fixture `src/triage/workload.c`, built once per test process, and each of
//! its modes spends its time in a way the test picked: on a cpu, blocked on a
//! disk, waiting for a mutex, queued behind its own threads, sleeping in one run
//! of five, or running one function twice as long as the others.
//!
//! **Where the ground truth comes from.** Before it exits, every mode prints
//! the kernel's accounting of its main thread on stderr, prefixed `omn-truth`:
//! nanoseconds on a cpu and waiting for one from `/proc/thread-self/schedstat`,
//! and its own clock around the part that matters -- the time inside
//! `fdatasync`, inside `pthread_mutex_lock`, whether this run was the slow one.
//! The tool reads none of that line. The verdicts are checked against the mode
//! the test chose, and the amounts beside them against that line, with the
//! slack the reference's own resolution calls for.
//!
//! **What is not asserted is what the machine was doing.** Four streams share
//! this machine. Where a verdict could legitimately change because the machine
//! was loaded -- a workload that waits for nothing on an idle machine waits for
//! a cpu on a busy one -- the test reads the fixture's own figures first and
//! skips, loudly and with the numbers, when the property it would check was not
//! there to be checked.
//!
//! **Every session here runs alone.** One of them pins four threads to one cpu
//! on purpose; [`one_at_a_time`] holds them apart within this binary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// The fixture's source, which is the triage layer's own file.
const FIXTURE_SOURCE: &str = include_str!("../src/triage/workload.c");

/// The on-cpu row's period at the default rate, which is its resolution.
const PERIOD_NS: f64 = 1e9 / 997.0;

fn one_at_a_time() -> MutexGuard<'static, ()> {
    static MACHINE: Mutex<()> = Mutex::new(());
    MACHINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ------------------------------------------------------------------ fixture

/// Where the fixture is built. `examples/triage-*.yml` launch it by this path,
/// relative to the repository root.
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/omn-triage-fixture/workload")
}

/// Builds the fixture once per test process, or says why it cannot.
///
/// Frame pointers everywhere, the leaf functions' included, and no inlining:
/// the differential profile names functions by their leaf frame, and a
/// function folded into its caller has no frame to be named by.
fn fixture() -> Result<PathBuf, String> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let path = fixture_path();
            let dir = path.parent().expect("a directory");
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
            let source = dir.join("workload.c");
            std::fs::write(&source, FIXTURE_SOURCE).map_err(|error| error.to_string())?;
            // Built beside and renamed, so a half-written binary is never at
            // the path the examples launch.
            let partial = dir.join(format!("workload.{}", std::process::id()));
            let built = Command::new("cc")
                .args([
                    "-O1",
                    "-g",
                    "-fno-omit-frame-pointer",
                    "-mno-omit-leaf-frame-pointer",
                    "-fno-inline",
                    "-pthread",
                    "-o",
                ])
                .arg(&partial)
                .arg(&source)
                .output()
                .map_err(|error| format!("no C compiler (`cc`) to build the fixture: {error}"))?;
            if !built.status.success() {
                return Err(format!(
                    "the triage fixture did not build:\n{}",
                    String::from_utf8_lossy(&built.stderr)
                ));
            }
            std::fs::rename(&partial, &path).map_err(|error| error.to_string())?;
            Ok(path)
        })
        .clone()
}

macro_rules! needs_fixture {
    () => {
        match fixture() {
            Ok(path) => path,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                return;
            }
        }
    };
}

// ------------------------------------------------------------------ running

fn spoken(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

fn omn(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

/// One answer, and every `omn-truth` line the workload printed, in order.
struct Answered {
    answer: Value,
    truths: Vec<BTreeMap<String, String>>,
    text: String,
}

impl Answered {
    fn truth(&self, name: &str) -> f64 {
        self.truth_of(0, name)
    }

    fn truth_of(&self, run: usize, name: &str) -> f64 {
        self.truths
            .get(run)
            .and_then(|truth| truth.get(name))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| {
                panic!(
                    "the workload reported no `{name}` for run {run}:\n{}",
                    self.text
                )
            })
    }

    fn verdict(&self) -> &Value {
        &self.answer["verdict"]
    }

    fn limit(&self) -> Option<&str> {
        self.verdict()["limit"].as_str()
    }

    fn resource(&self) -> Option<&str> {
        self.verdict()["resource"].as_str()
    }

    /// The run the verdict reads, for `omn latency`.
    fn median_run(&self) -> &Value {
        let index = self.answer["median"].as_u64().expect("a median run") as usize;
        &self.answer["runs"][index]
    }

    /// A state of the run the verdict reads.
    fn state(&self, name: &str) -> &Value {
        self.median_run()["states"]
            .as_array()
            .expect("states")
            .iter()
            .find(|state| state["state"] == name)
            .unwrap_or_else(|| panic!("no state `{name}` in the answer:\n{}", self.text))
    }

    fn resource_row(&self, resource: &str) -> &Value {
        self.answer["resources"]
            .as_array()
            .expect("resources")
            .iter()
            .find(|row| row["resource"] == resource)
            .unwrap_or_else(|| panic!("no resource `{resource}` in the answer:\n{}", self.text))
    }
}

/// Runs a triage command as JSON, or says why it could not and returns nothing.
///
/// Missing privileges is the one refusal allowed to stop a test, and it is
/// said out loud with what would have been checked. Anything else that is not
/// a clean answer is a failure.
fn answer(args: &[&str], would_have_checked: &str) -> Option<Answered> {
    let mut full: Vec<&str> = vec![args[0], "--format", "json"];
    full.extend(&args[1..]);
    let output = omn(&full);
    let text = spoken(&output);
    if !output.status.success() && (text.contains("CAP_BPF") || text.contains("CAP_PERFMON")) {
        eprintln!(
            "skipped: `omn {}` needs CAP_BPF and CAP_PERFMON, which this binary does not carry \
             (run `make`, which grants them). It would have checked {would_have_checked}",
            args.join(" ")
        );
        return None;
    }
    // An input the method cannot do without, which this machine cannot serve:
    // `cpu.profile` on a stock kernel's `perf_event_paranoid`, or on a cpu with
    // no PMU. The question is refused, rightly, and the refusal is the
    // machine's -- its reasons follow on the lines after.
    if !output.status.success()
        && text.contains("cannot do without")
        && text.contains("has no viable backend on this machine")
    {
        eprintln!(
            "skipped: `omn {}` would have checked {would_have_checked}, and: {text}",
            args.join(" ")
        );
        return None;
    }
    assert_eq!(
        output.status.code(),
        Some(0),
        "`omn {}` did not answer:\n{text}",
        args.join(" ")
    );
    let answer: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "`omn {}` did not produce one json document ({error}):\n{text}",
            args.join(" ")
        )
    });
    let stderr = String::from_utf8_lossy(&output.stderr);
    let truths = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("omn-truth "))
        .map(|line| {
            line.split_whitespace()
                .filter_map(|pair| {
                    let (name, value) = pair.split_once('=')?;
                    Some((name.to_owned(), value.to_owned()))
                })
                .collect()
        })
        .collect();
    Some(Answered {
        answer,
        truths,
        text,
    })
}

fn amount(value: &Value) -> f64 {
    value["amount"]
        .as_f64()
        .unwrap_or_else(|| panic!("no amount in {value}"))
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("triage-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

// ------------------------------------------------------------------ the objective

#[test]
fn a_question_without_an_objective_is_refused_with_the_list() {
    for (command, objectives) in [
        ("latency", &["latency", "tail"][..]),
        ("saturate", &["throughput", "tail", "energy"][..]),
    ] {
        let output = omn(&[command, "--", "true"]);
        let text = spoken(&output);
        assert_eq!(
            output.status.code(),
            Some(2),
            "a refusal is exit 2:\n{text}"
        );
        assert!(text.contains("needs an objective"), "{text}");
        for objective in objectives {
            assert!(
                text.contains(objective),
                "the list should name `{objective}`:\n{text}"
            );
        }
        assert!(
            output.stdout.is_empty(),
            "nothing should have been measured:\n{text}"
        );
    }
}

#[test]
fn an_objective_the_method_cannot_answer_names_the_one_that_can() {
    for (args, points_to) in [
        (
            &["latency", "--objective", "throughput", "--", "true"][..],
            "omn saturate --objective throughput",
        ),
        (
            &["latency", "--objective", "energy", "--", "true"][..],
            "omn saturate --objective energy",
        ),
        (
            &["saturate", "--objective", "latency", "--", "true"][..],
            "omn latency --objective latency",
        ),
    ] {
        let output = omn(args);
        let text = spoken(&output);
        assert_eq!(output.status.code(), Some(2), "{text}");
        assert!(text.contains(points_to), "{text}");
    }
    let output = omn(&["latency", "--objective", "cost", "--", "true"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        spoken(&output).contains("not an objective"),
        "{}",
        spoken(&output)
    );
}

#[test]
fn a_tail_that_cannot_be_observed_is_refused_rather_than_answered() {
    for args in [
        &["latency", "--objective", "tail", "--", "true"][..],
        &[
            "latency",
            "--objective",
            "tail",
            "--runs",
            "2",
            "--",
            "true",
        ][..],
        &["latency", "--objective", "tail", "--pid", "1", "-d", "1s"][..],
        &["latency", "--objective", "latency", "--pid", "1"][..],
    ] {
        let output = omn(args);
        let text = spoken(&output);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`omn {}`: {text}",
            args.join(" ")
        );
        assert!(output.stdout.is_empty(), "{text}");
    }
}

/// The layer calls the engine like any other user: what it asks for is a
/// session `omn validate` accepts and `omn -f` would run.
#[test]
fn the_session_a_question_is_asked_through_is_an_ordinary_session() {
    let dir = scratch("dump");
    for (name, args) in [
        (
            "latency",
            &[
                "latency",
                "--objective",
                "tail",
                "--runs",
                "3",
                "--dump-session",
                "--",
                "true",
            ][..],
        ),
        (
            "saturate",
            &[
                "saturate",
                "--objective",
                "energy",
                "--dump-session",
                "--",
                "true",
            ][..],
        ),
    ] {
        let output = omn(args);
        assert!(output.status.success(), "{}", spoken(&output));
        let file = dir.join(format!("{name}.yml"));
        std::fs::write(&file, &output.stdout).unwrap();
        let checked = omn(&["validate", "-f", file.to_str().unwrap()]);
        assert!(
            checked.status.success(),
            "the dumped session should validate:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            spoken(&checked)
        );
    }
}

/// Every `examples/triage-*.yml` is exactly what its command dumps, so the
/// sessions a reader runs with `omn -f` cannot drift from what the question
/// asks for.
#[test]
fn every_triage_session_is_what_its_command_asks_for() {
    let mut checked = 0;
    for entry in std::fs::read_dir("examples").expect("examples") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with("triage-") || !name.ends_with(".yml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let command = text
            .lines()
            .find_map(|line| line.strip_prefix("#   omn "))
            .unwrap_or_else(|| panic!("{name} does not say which command it is the session of"));
        let mut args: Vec<&str> = command.split_whitespace().collect();
        let separator = args
            .iter()
            .position(|arg| *arg == "--")
            .unwrap_or(args.len());
        args.insert(separator, "--dump-session");
        let output = omn(&args);
        assert!(output.status.success(), "{name}: {}", spoken(&output));
        let dumped: Value = serde_norway::from_slice(&output.stdout).unwrap();
        let shipped: Value = serde_norway::from_str(&text).unwrap();
        assert_eq!(
            dumped, shipped,
            "{name} is not what `omn {command}` asks for"
        );
        checked += 1;
    }
    assert!(checked >= 3, "only {checked} triage sessions found");
}

// ------------------------------------------------------------------ omn latency

#[test]
fn a_cpu_bound_program_is_named_cpu_bound_for_latency() {
    let workload = needs_fixture!();
    let _alone = one_at_a_time();
    let Some(run) = answer(
        &[
            "latency",
            "--objective",
            "latency",
            "--",
            workload.to_str().unwrap(),
            "cpu",
            "300",
        ],
        "that 300 ms of spinning is named cpu-bound, and its on-cpu row the kernel's figure",
    ) else {
        return;
    };
    assert_eq!(run.limit(), Some("cpu"), "{}", run.text);
    // The on-cpu row against the kernel's own accounting of the same thread:
    // a sample count is exact about the samples and a period about the time,
    // and the kernel's figure was read a moment before the exit.
    let on_cpu = amount(run.state("on-cpu"));
    let truth = run.truth("oncpu_ns");
    assert!(
        (on_cpu - truth).abs() <= 3.0 * PERIOD_NS + truth * 0.02,
        "on-cpu {on_cpu} against the kernel's {truth}:\n{}",
        run.text
    );
    let hottest = run.state("on-cpu")["named"][0]["says"]
        .as_str()
        .unwrap_or("");
    assert!(
        hottest.contains("spin_cpu_ms"),
        "the spin is where the samples are: {hottest}"
    );
}

#[test]
fn a_program_blocked_on_its_disk_is_named_io_bound_for_latency() {
    let workload = needs_fixture!();
    let _alone = one_at_a_time();
    let dir = scratch("io");
    let Some(run) = answer(
        &[
            "latency",
            "--objective",
            "latency",
            "--",
            workload.to_str().unwrap(),
            "io",
            "300",
            dir.to_str().unwrap(),
        ],
        "that 300 synchronous writes are named I/O-bound, blocked in fdatasync",
    ) else {
        return;
    };
    assert_eq!(run.limit(), Some("disk"), "{}", run.text);
    let blocked = run.state("blocked: uninterruptible");
    // The program blocks uninterruptibly nowhere but inside fdatasync, which
    // it timed: the row cannot exceed that, and the stack says where it was.
    let synced = run.truth("sync_ns");
    assert!(
        amount(blocked) <= synced + 1e6,
        "{} blocked against {synced} inside fdatasync:\n{}",
        amount(blocked),
        run.text
    );
    let named: Vec<&str> = blocked["named"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|naming| naming["says"].as_str())
        .collect();
    assert!(
        named.iter().any(|says| says.contains("fdatasync")),
        "the blocking stack should end in fdatasync: {named:?}"
    );
}

#[test]
fn a_program_waiting_for_a_mutex_is_named_lock_contended_for_latency() {
    let workload = needs_fixture!();
    let _alone = one_at_a_time();
    let Some(run) = answer(
        &[
            "latency",
            "--objective",
            "latency",
            "--",
            workload.to_str().unwrap(),
            "lock",
            "150",
            "2000",
        ],
        "that a thread taking a mutex another holds 2 ms at a time is named lock-contended",
    ) else {
        return;
    };
    let sleeping = run.state("blocked: sleeping");
    let lock = sleeping["named"]
        .as_array()
        .unwrap()
        .iter()
        .find(|naming| naming["source"] == "sched.mutex");
    let Some(lock) = lock else {
        // The shim is a workspace member nothing depends on: without it the
        // sleep is named a futex, which is right and less precise.
        assert_eq!(run.limit(), Some("futex"), "{}", run.text);
        eprintln!("skipped the mutex half: `sched.mutex` did not run (is the shim built? `make`)");
        return;
    };
    assert_eq!(run.limit(), Some("lock"), "{}", run.text);
    // The shim's wait against the program's own clock around the same calls.
    let waited = lock["amount"].as_f64().unwrap();
    let truth = run.truth("lock_wait_ns");
    assert!(
        (waited - truth).abs() <= truth * 0.05 + 1e6,
        "the shim saw {waited} of mutex wait, the program timed {truth}:\n{}",
        run.text
    );
}

/// One workload whose answer changes with the objective: a program that
/// spins 100 ms every run and sleeps 200 ms more in one run of five. Its
/// typical run is cpu-bound; its slow run is slow because it slept.
#[test]
fn the_same_program_is_cpu_bound_for_latency_and_sleep_bound_for_the_tail() {
    let workload = needs_fixture!();
    let _alone = one_at_a_time();
    let dir = scratch("tail");
    let counter = dir.join("counter");
    let args = |objective: &'static str| -> Vec<String> {
        vec![
            "latency".into(),
            "--objective".into(),
            objective.into(),
            "--runs".into(),
            "5".into(),
            "--".into(),
            workload.to_str().unwrap().into(),
            "tail".into(),
            counter.to_str().unwrap().into(),
            "5".into(),
            "100".into(),
            "200".into(),
        ]
    };

    let _ = std::fs::remove_file(&counter);
    let tail_args = args("tail");
    let Some(tail) = answer(
        &tail_args.iter().map(String::as_str).collect::<Vec<_>>(),
        "that the slowest of five runs is named for the 200 ms it slept",
    ) else {
        return;
    };
    // Which run slept is the workload's to say, not the tool's.
    let slow = (0..5)
        .find(|run| tail.truth_of(*run, "slow") == 1.0)
        .expect("one run of five sleeps");
    assert_eq!(
        tail.answer["slowest"].as_u64(),
        Some(slow as u64),
        "{}",
        tail.text
    );
    assert_eq!(tail.limit(), Some("sleep"), "{}", tail.text);
    // 200 ms requested; the kernel may overshoot a sleep by its timer slack
    // and the wakeup after it, and never undershoot.
    let excess = tail.verdict()["amount"].as_f64().unwrap();
    assert!(
        (199e6..=210e6).contains(&excess),
        "the slowest run slept {excess} ns more than the median:\n{}",
        tail.text
    );

    let _ = std::fs::remove_file(&counter);
    let latency_args = args("latency");
    let Some(latency) = answer(
        &latency_args.iter().map(String::as_str).collect::<Vec<_>>(),
        "that the median of the same five runs is named cpu-bound",
    ) else {
        return;
    };
    // The median run spins 100 ms and sleeps nothing, so what limits it is a
    // cpu: its own work on an idle machine, and waiting for one on a machine
    // whose cpus the rest of the suite holds -- which is the tool being right
    // about the run it measured, and failed this test at a load of 20. What
    // the median run is never limited by is the sleep the tail's run made.
    match latency.limit() {
        Some("cpu") => {}
        Some("cpu-queue") => eprintln!(
            "the median run waited for a cpu longer than it ran on one: this machine's cpus \
             were busy (load {}), and the verdict says so:\n{}",
            std::fs::read_to_string("/proc/loadavg")
                .unwrap_or_default()
                .trim(),
            latency.text
        ),
        other => panic!(
            "the median of the same five runs is limited by {other:?}, not by a cpu:\n{}",
            latency.text
        ),
    }
}

/// The loss path: a profile taken fast enough to lose samples carries its
/// shortfall into the verdict, which marks its amount a lower bound.
#[test]
fn a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound() {
    let workload = needs_fixture!();
    let _alone = one_at_a_time();
    let Some(run) = answer(
        &[
            "latency",
            "--objective",
            "latency",
            "--hz",
            "100000",
            "--",
            workload.to_str().unwrap(),
            "cpu",
            "300",
        ],
        "that a lossy profile's shortfall reaches the verdict",
    ) else {
        return;
    };
    let on_cpu = run.state("on-cpu");
    if on_cpu["short"].is_null() {
        eprintln!(
            "skipped: a 100 kHz profile lost nothing on this machine this time ({} samples' \
             worth), so there was no shortfall to carry",
            amount(on_cpu)
        );
        return;
    }
    assert_eq!(run.limit(), Some("cpu"), "{}", run.text);
    assert_eq!(run.verdict()["lower_bound"], true, "{}", run.text);
    assert!(
        run.verdict()["sentence"].as_str().unwrap().contains(">="),
        "{}",
        run.text
    );
}

/// The capability sweep, without touching the machine's capabilities: a
/// catalog that does not carry the lock sources gives an answer without them
/// and says so, and one that does not carry the run queue gives none.
#[test]
fn an_input_this_machine_cannot_serve_is_dropped_by_name_or_fails_the_question() {
    let workload = needs_fixture!();
    let _alone = one_at_a_time();
    let catalog = scratch("catalog");
    for entry in std::fs::read_dir("catalog").unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, catalog.join(path.file_name().unwrap())).unwrap();
    }
    let sched = std::fs::read_to_string("catalog/sched.yaml").unwrap();
    let without = |ids: &[&str]| -> String {
        // Entries are separated by a blank line and start with `- id:`.
        sched
            .split("\n\n")
            .filter(|entry| {
                !ids.iter()
                    .any(|id| entry.contains(&format!("- id: {id}\n")))
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    std::fs::write(
        catalog.join("sched.yaml"),
        without(&["sched.mutex", "sched.futex"]),
    )
    .unwrap();
    let dir = catalog.to_str().unwrap();
    let Some(run) = answer(
        &[
            "latency",
            "--objective",
            "latency",
            "--catalog",
            dir,
            "--",
            workload.to_str().unwrap(),
            "cpu",
            "100",
        ],
        "that a missing optional input is dropped by name",
    ) else {
        return;
    };
    let dropped: Vec<&str> = run.answer["dropped"]
        .as_array()
        .map(|all| all.iter().filter_map(|one| one["label"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(dropped, ["futex", "mutex"], "{}", run.text);
    assert_eq!(run.limit(), Some("cpu"), "{}", run.text);

    std::fs::write(catalog.join("sched.yaml"), without(&["sched.runqlat"])).unwrap();
    let output = omn(&[
        "latency",
        "--objective",
        "latency",
        "--catalog",
        dir,
        "--",
        "true",
    ]);
    let text = spoken(&output);
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("runnable") && text.contains("cannot do without"),
        "{text}"
    );
}

// ------------------------------------------------------------------ omn saturate

/// Little's law against a queue whose length the workload set: four threads
/// spinning on one cpu keep three of them waiting whenever one runs. The
/// control is one thread on the same cpu, and what is asserted is the
/// difference, which a neighbour on that cpu adds to both.
#[test]
fn four_threads_on_one_cpu_queue_three_deep_by_littles_law() {
    let workload = needs_fixture!();
    let _alone = one_at_a_time();
    let queue = |threads: &str| {
        answer(
            &[
                "saturate",
                "--objective",
                "throughput",
                "--",
                workload.to_str().unwrap(),
                "queue",
                threads,
                "250",
            ],
            "a run queue three deep, by Little's law",
        )
    };
    let (Some(four), Some(one)) = (queue("4"), queue("1")) else {
        return;
    };
    let length = |run: &Answered| {
        run.resource_row("cpu")["saturation"]["value"]
            .as_f64()
            .unwrap_or_else(|| panic!("no run queue length:\n{}", run.text))
    };
    let (deep, control) = (length(&four), length(&one));
    assert!(
        (2.5..=3.5).contains(&(deep - control)),
        "four threads on one cpu should queue three deeper than one: {deep:.3} against \
         {control:.3}\n{}",
        four.text
    );
    assert_eq!(four.resource(), Some("cpu"), "{}", four.text);
    // And L = λW is the identity it claims to be, term by term.
    let little = &four.resource_row("cpu")["little"][0];
    let product =
        little["per_second"].as_f64().unwrap() * little["mean_ns"].as_f64().unwrap() / 1e9;
    assert!((product - deep).abs() < 1e-6 * deep.max(1.0), "{little}");
}

#[test]
fn a_contended_mutex_is_what_the_workload_queues_for() {
    let workload = needs_fixture!();
    let _alone = one_at_a_time();
    let Some(run) = answer(
        &[
            "saturate",
            "--objective",
            "throughput",
            "--",
            workload.to_str().unwrap(),
            "lock",
            "150",
            "2000",
        ],
        "that a mutex held 2 ms at a time by one thread and waited on by another is the queue",
    ) else {
        return;
    };
    assert_eq!(run.resource(), Some("lock"), "{}", run.text);
    // The holder holds it almost all the time, by construction: 2 ms held for
    // every 50 us released.
    let held = run.resource_row("lock")["utilization"]["value"]
        .as_f64()
        .unwrap_or(0.0);
    assert!(
        held > 0.5,
        "the mutex should be held most of the time: {held}\n{}",
        run.text
    );
}

/// The same program as `a_program_blocked_on_its_disk_is_named_io_bound_for_latency`:
/// for its latency the disk is the culprit, for its throughput nothing is --
/// one synchronous write at a time queues behind nobody.
#[test]
fn a_program_io_bound_for_latency_queues_for_nothing_for_throughput() {
    let workload = needs_fixture!();
    let _alone = one_at_a_time();
    let dir = scratch("io-throughput");
    let Some(run) = answer(
        &[
            "saturate",
            "--objective",
            "throughput",
            "--",
            workload.to_str().unwrap(),
            "io",
            "300",
            dir.to_str().unwrap(),
        ],
        "that a program waiting on one write at a time is saturated by nothing",
    ) else {
        return;
    };
    // Whether it waited for a cpu is the machine's to decide, and the
    // workload's own figure says whether it did.
    let waited = run.truth("runq_ns") / run.truth("wall_ns");
    if waited >= 0.05 {
        eprintln!(
            "skipped: the workload itself spent {:.1} % of its life waiting for a cpu, so this \
             machine was too busy for it to queue for nothing ({})",
            waited * 100.0,
            run.verdict()["sentence"]
        );
        return;
    }
    assert_eq!(run.resource(), None, "{}", run.text);
    assert!(
        run.verdict()["sentence"]
            .as_str()
            .unwrap()
            .contains("concurrency"),
        "{}",
        run.text
    );
    let blocked = run.resource_row("disk")["saturation"]["value"]
        .as_f64()
        .unwrap();
    assert!(
        blocked > 0.5,
        "its tasks should be blocked on the disk most of the time: {blocked}"
    );
}

#[test]
fn energy_is_credited_to_the_workload_by_its_cpu_time_and_says_what_is_not_metered() {
    let workload = needs_fixture!();
    let _alone = one_at_a_time();
    let Some(run) = answer(
        &[
            "saturate",
            "--objective",
            "energy",
            "--",
            workload.to_str().unwrap(),
            "cpu",
            "200",
        ],
        "that energy is credited by cpu time and the unmetered parts are named",
    ) else {
        return;
    };
    let domains = &run.answer["energy"]["domains"];
    if domains.as_object().is_none_or(|all| all.is_empty()) {
        eprintln!("skipped: no energy counter on this machine:\n{}", run.text);
        return;
    }
    assert_eq!(run.resource(), Some("cpu"), "{}", run.text);
    let share = run.answer["energy"]["cpu_share"].as_f64().unwrap_or(-1.0);
    assert!((0.0..=1.0).contains(&share), "{share}");
    let sentence = run.verdict()["sentence"].as_str().unwrap();
    assert!(
        sentence.contains("TEMPORAL") && sentence.contains("disk"),
        "{sentence}"
    );
}

// ------------------------------------------------------------------ omn diff

/// One program run twice with one function's cost doubled: that function, and
/// only it, is the difference. And the program run twice unchanged has none.
#[test]
fn the_function_whose_cost_doubled_is_the_only_difference() {
    let workload = needs_fixture!();
    let _alone = one_at_a_time();
    let dir = scratch("diff");
    let profile = |factor: &str, name: &str| -> Option<PathBuf> {
        let out = dir.join(name);
        let output = omn(&[
            "cpu.profile",
            "-m",
            "sample@997hz",
            "-e",
            "stack",
            "--stack",
            "fp",
            "-k",
            "task",
            "--format",
            "json",
            "-o",
            out.to_str().unwrap(),
            "--",
            workload.to_str().unwrap(),
            "diff",
            factor,
        ]);
        let text = spoken(&output);
        if !output.status.success()
            && (text.contains("CAP_PERFMON")
                || text.contains("has no viable backend on this machine"))
        {
            eprintln!("skipped: this machine does not profile here: {text}");
            return None;
        }
        assert!(output.status.success(), "{text}");
        Some(out)
    };
    let (Some(once), Some(twice), Some(again)) = (
        profile("1", "once"),
        profile("2", "twice"),
        profile("1", "again"),
    ) else {
        return;
    };
    let diff = |before: &Path, after: &Path| -> Value {
        let output = omn(&[
            "diff",
            "--format",
            "json",
            before.to_str().unwrap(),
            after.to_str().unwrap(),
        ]);
        assert!(output.status.success(), "{}", spoken(&output));
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let doubled = diff(&once, &twice);
    assert_eq!(
        doubled["difference"],
        serde_json::json!(["beta"]),
        "{doubled:#}"
    );
    let beta = doubled["own"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["function"] == "beta")
        .unwrap();
    let ratio = beta["ratio"].as_f64().unwrap();
    assert!(
        (1.6..=2.4).contains(&ratio),
        "beta's cost should double: x{ratio}"
    );

    let unchanged = diff(&once, &again);
    assert_eq!(
        unchanged["difference"],
        serde_json::json!([]),
        "{unchanged:#}"
    );

    // The folded file carries both weights per stack, which is what a
    // differential flame graph is drawn from.
    let output = omn(&[
        "diff",
        "--format",
        "folded",
        once.to_str().unwrap(),
        twice.to_str().unwrap(),
    ]);
    let folded = String::from_utf8_lossy(&output.stdout);
    let line = folded
        .lines()
        .find(|line| {
            line.split(' ')
                .next()
                .is_some_and(|stack| stack.ends_with(";beta"))
        })
        .unwrap_or_else(|| panic!("no beta stack:\n{folded}"));
    let weights: Vec<f64> = line
        .rsplitn(3, ' ')
        .take(2)
        .map(|w| w.parse().unwrap())
        .collect();
    assert!(weights[0] > weights[1] * 1.5, "{line}");
}

#[test]
fn a_profile_that_is_not_one_is_refused_by_name() {
    let dir = scratch("not-a-profile");
    let file = dir.join("empty.folded");
    std::fs::write(&file, "").unwrap();
    let output = omn(&["diff", file.to_str().unwrap(), file.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "{}", spoken(&output));
    assert!(
        spoken(&output).contains("holds no stack"),
        "{}",
        spoken(&output)
    );
}

// ------------------------------------------------------------------ the escalation
//
// A session that holds its detailed sources with `window: on <layer>` and
// decides, from a cheap pass it runs for the whole of it, which to attach.
// Ground truth is the `phases` mode: cpu-bound, then lock-bound, with the
// switch stamped on CLOCK_MONOTONIC and every acquisition of the second phase
// written down as its start and its wait on the same clock.

/// The helper thread holds the mutex this long at a time.
const HOLD_US: u64 = 2_000;

/// The detailed sources, held for their layers.
const PROFILE: &str =
    "  - {label: profile, source: cpu.profile, target: *program, mode: sample@997hz, \
                       emit: [stack], keys: [task], stack: fp, window: on cpu}\n";
const FUTEX: &str =
    "  - {label: futex, source: sched.futex, target: *program, mode: trace, emit: [raw], \
                     keys: [task], window: on lock}\n";
const MUTEX: &str = "  - {label: mutex, source: sched.mutex, target: *program, mode: trace, \
                     emit: [summary], keys: [task], window: on lock}\n";

/// A session with the cheap pass over `program`, and `held` beside it.
fn escalation_session(program: &[String], held: &str) -> String {
    let argv = program
        .iter()
        .map(|arg| format!("'{arg}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "escalate: {{objective: latency}}\n\
         requests:\n\
         \x20 - {{label: blocked, source: sched.offcpu, target: &program {{launch: [{argv}]}}, \
         mode: trace, emit: [summary], keys: [task]}}\n\
         \x20 - {{label: on-cpu, source: cpu.oncpu_time, target: *program, mode: poll@50ms, \
         emit: [summary]}}\n\
         {held}"
    )
}

/// One escalating run: its results, what it cost, and what the program said.
struct Escalated {
    results: Value,
    cost: Value,
    truth: BTreeMap<String, String>,
    text: String,
}

impl Escalated {
    fn result(&self, label: &str) -> &Value {
        self.results["results"]
            .as_array()
            .expect("results")
            .iter()
            .find(|result| result["label"] == label)
            .unwrap_or_else(|| panic!("no result `{label}`:\n{}", self.text))
    }

    fn notes(&self, label: &str) -> Vec<String> {
        self.result(label)["degradations"]
            .as_array()
            .map(|notes| {
                notes
                    .iter()
                    .filter_map(|note| note.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn stopped(&self, label: &str) -> String {
        self.result(label)["stopped_because"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    fn truth(&self, name: &str) -> u64 {
        self.truth
            .get(name)
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("the workload reported no `{name}`:\n{}", self.text))
    }

    /// When the escalation attached `label`: from the moment it began the
    /// attach to the moment the collector was live, on CLOCK_MONOTONIC.
    fn attached(&self, label: &str) -> Option<(u64, u64)> {
        self.notes(label).iter().find_map(|note| {
            let (_, after) = note.split_once("(CLOCK_MONOTONIC ")?;
            let (from, rest) = after.split_once(" to ")?;
            let (to, _) = rest.split_once(" ns)")?;
            Some((from.parse().ok()?, to.parse().ok()?))
        })
    }

    fn cost_of(&self, label: &str) -> &Value {
        self.cost
            .as_array()
            .expect("cost.json is a list")
            .iter()
            .find(|cost| cost["label"] == label)
            .unwrap_or_else(|| panic!("no cost for `{label}`"))
    }
}

/// Runs `session` through `omn -f`, or says why it could not and returns
/// nothing. Missing privileges is the one refusal allowed to stop a test.
fn escalate(
    name: &str,
    session: &str,
    extra: &[&str],
    would_have_checked: &str,
) -> Option<Escalated> {
    let dir = scratch(name);
    let file = dir.join("session.yml");
    std::fs::write(&file, session).unwrap();
    let out = dir.join("out");
    let mut args = vec![
        "-f",
        file.to_str().unwrap(),
        "--format",
        "json",
        "-o",
        out.to_str().unwrap(),
    ];
    args.extend(extra);
    let output = omn(&args);
    let text = spoken(&output);
    if !output.status.success() && (text.contains("CAP_BPF") || text.contains("CAP_PERFMON")) {
        eprintln!(
            "skipped: the escalation's cheap pass needs CAP_BPF and CAP_PERFMON, which this \
             binary does not carry (run `make`, which grants them). It would have checked \
             {would_have_checked}"
        );
        return None;
    }
    assert_eq!(
        output.status.code(),
        Some(0),
        "the run failed:\n{session}\n{text}"
    );
    let results: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("not one json document ({error}):\n{text}"));
    let cost: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("cost.json")).unwrap()).unwrap();
    let truth = String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("omn-truth "))
        .map(|line| {
            line.split_whitespace()
                .filter_map(|pair| {
                    let (name, value) = pair.split_once('=')?;
                    Some((name.to_owned(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Escalated {
        results,
        cost,
        truth,
        text,
    })
}

/// The `phases` program: `cpu_ms` of spinning, then `locks` acquisitions of
/// the mutex, each written to the returned log.
fn phases(name: &str, cpu_ms: u64, locks: u64) -> Option<(Vec<String>, PathBuf)> {
    phases_holding(name, cpu_ms, locks, HOLD_US)
}

/// The same program, with each lock held for `hold_us`.
fn phases_holding(
    name: &str,
    cpu_ms: u64,
    locks: u64,
    hold_us: u64,
) -> Option<(Vec<String>, PathBuf)> {
    let fixture = match fixture() {
        Ok(path) => path,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return None;
        }
    };
    let log = scratch(&format!("{name}-log")).join("waits.txt");
    let program = vec![
        fixture.to_string_lossy().into_owned(),
        "phases".to_owned(),
        cpu_ms.to_string(),
        locks.to_string(),
        hold_us.to_string(),
        log.to_string_lossy().into_owned(),
    ];
    Some((program, log))
}

/// Every acquisition the program timed: `(start_ns, wait_ns)`.
fn acquisitions(log: &Path) -> Vec<(u64, u64)> {
    std::fs::read_to_string(log)
        .expect("the program wrote its waits")
        .lines()
        .filter_map(|line| {
            let (start, wait) = line.split_once(' ')?;
            Some((start.parse().ok()?, wait.parse().ok()?))
        })
        .collect()
}

/// The ground truth: cpu-bound for a second, then lock-bound for a second.
///
/// The run must name `cpu` in the first phase and attach the profile there,
/// name `lock` after the switch -- attaching `sched.futex` and detaching the
/// profile, in that order of release -- and the futex waits it then measured
/// for the main thread must be the program's own waits inside
/// `pthread_mutex_lock` over exactly the window the escalation gave them.
///
/// The slack is the reference's: each futex wait lies inside one
/// `pthread_mutex_lock` call, whose clock also covers the compare-and-swap
/// before it (5 % and 2 ms), and one call can straddle the attach (one hold).
#[test]
fn a_program_cpu_bound_then_lock_bound_escalates_to_the_lock_layer_and_its_waits_match_its_own_clock(
) {
    let _machine = one_at_a_time();
    let Some((program, log)) = phases("ground", 1000, 1000) else {
        return;
    };
    let session = escalation_session(&program, &format!("{PROFILE}{FUTEX}{MUTEX}"));
    let Some(run) = escalate(
        "ground",
        &session,
        &[],
        "that the lock layer is attached after the switch and its waits match the program's",
    ) else {
        return;
    };
    let switch_ns = run.truth("switch_ns");
    let main = run.truth("tid");
    let at = |ns: u64| (ns as f64 - switch_ns as f64) / 1e6;

    // The cpu layer, in the first phase.
    let (_, profile_live) = run
        .attached("profile")
        .unwrap_or_else(|| panic!("the profile was never attached:\n{}", run.text));
    let profile_closed = profile_live + run.result("profile")["observed_ns"].as_u64().unwrap();
    assert!(
        profile_live < switch_ns,
        "the cpu layer is attached in the cpu phase: live {:.1} ms from the switch",
        at(profile_live)
    );
    assert!(
        run.notes("profile")
            .iter()
            .any(|note| note.contains("named the `cpu` layer") && note.contains("for latency")),
        "{:?}",
        run.notes("profile")
    );

    // The lock layer, after the switch, with the cpu layer let go first.
    let (futex_from, futex_live) = run
        .attached("futex")
        .unwrap_or_else(|| panic!("the lock layer was never attached:\n{}", run.text));
    assert!(
        futex_from > switch_ns,
        "the lock layer is named from the lock phase's own numbers: attached {:.1} ms from the \
         switch",
        at(futex_from)
    );
    assert!(
        profile_closed >= switch_ns && profile_closed <= futex_from,
        "the profile ran past the switch, and was let go before the lock layer was attached: \
         closed {:.1} ms, futex from {:.1} ms",
        at(profile_closed),
        at(futex_from)
    );
    let stopped = run.stopped("profile");
    assert!(
        stopped.contains("by the escalation")
            && stopped.contains("`lock` layer")
            && stopped.contains("one detailed layer at a time"),
        "{stopped}"
    );

    // The waits, against the program's own clock over the same window.
    let waits = acquisitions(&log);
    let after_live: u64 = waits
        .iter()
        .filter(|(start, _)| *start >= futex_live)
        .map(|(_, wait)| wait)
        .sum();
    let since_attach: u64 = waits
        .iter()
        .filter(|(start, _)| *start >= futex_from)
        .map(|(_, wait)| wait)
        .sum();
    let measured: u64 = run.result("futex")["outputs"][0]["events"]
        .as_array()
        .expect("raw futex rows")
        .iter()
        .filter(|event| event["keys"][0]["task"]["tid"].as_u64() == Some(main))
        .map(|event| {
            event["attrs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|attr| attr["name"] == "total_ns")
                .and_then(|attr| attr["value"].as_u64())
                .unwrap_or(0)
        })
        .sum();
    eprintln!(
        "switch at 0; profile live {:.1} ms, closed {:.1} ms; futex from {:.1} ms, live {:.1} ms; \
         the program waited {:.2} ms after the attach and {:.2} ms after it was live; futex \
         measured {:.2} ms",
        at(profile_live),
        at(profile_closed),
        at(futex_from),
        at(futex_live),
        since_attach as f64 / 1e6,
        after_live as f64 / 1e6,
        measured as f64 / 1e6
    );
    assert!(
        after_live > 100_000_000,
        "the lock layer was attached with at least 100 ms of waits left to measure: {:.2} ms",
        after_live as f64 / 1e6
    );
    let slack = 2_000_000 + HOLD_US * 1_000;
    assert!(
        measured as f64 >= after_live as f64 * 0.95 - slack as f64,
        "futex saw less than the program waited once it was live: {measured} < {after_live}"
    );
    assert!(
        measured <= since_attach + slack,
        "futex saw more than the program waited from the attach on: {measured} > {since_attach}"
    );

    // The layer this machine cannot serve on a launch is said, not skipped.
    let mutex = run.stopped("mutex");
    assert!(
        mutex.contains("never attached") && mutex.contains("preload"),
        "{mutex}"
    );
    assert_eq!(run.result("mutex")["events_seen"], 0);
    let summary = run.notes("blocked").join("\n");
    assert!(
        summary.contains("named `cpu` at")
            && summary.contains("then `lock` at")
            && summary.contains("attaching `futex`")
            && summary.contains("letting go of `profile`")
            && summary.contains("not served: `mutex`"),
        "{summary}"
    );
}

/// The budget holds, measured: every collection's cost is under the ceiling
/// it was given, or it was let go for exceeding it and says so -- and the
/// run's whole cost, every collector's cpu against the program's, is under it
/// too.
#[test]
fn an_escalating_run_holds_its_budget_and_names_what_it_let_go() {
    let _machine = one_at_a_time();
    let Some((program, _)) = phases("budget", 800, 1500) else {
        return;
    };
    let session = escalation_session(&program, &format!("{PROFILE}{FUTEX}"));
    let Some(run) = escalate(
        "budget",
        &session,
        &["--budget", "5%"],
        "the run's cost against a 5 % budget",
    ) else {
        return;
    };
    let mut spent_ns = 0u64;
    for cost in run.cost.as_array().unwrap() {
        let label = cost["label"].as_str().unwrap();
        spent_ns += cost["overhead"]["attributed_cpu_ns"].as_u64().unwrap_or(0);
        let shed = cost["stopped_because"]
            .as_str()
            .is_some_and(|why| why.contains("to hold the budget"));
        assert!(
            cost["within_budget"] != false || shed,
            "`{label}` exceeded its budget and was not let go for it: {cost}"
        );
    }
    let program_ns = run.cost_of("blocked")["overhead"]["observed_cpu_ns"]
        .as_u64()
        .expect("the program's cpu");
    let percent = spent_ns as f64 * 100.0 / program_ns as f64;
    eprintln!(
        "the run spent {:.2} ms of cpu against the program's {:.2} ms: {percent:.3} %",
        spent_ns as f64 / 1e6,
        program_ns as f64 / 1e6
    );
    assert!(
        percent <= 5.0,
        "the run's own cost is over its budget: {percent:.3} %"
    );
    // What it let go to stay there is named: the cpu layer, either at the
    // switch or earlier because its own first readings -- which carry what
    // attaching it cost while the program ran -- were over the ceiling.
    let profile = run.stopped("profile");
    assert!(
        profile.contains("one detailed layer at a time") || profile.contains("to hold the budget"),
        "{profile}"
    );
    assert!(
        run.cost_of("profile")["overhead"]["wall_ns"]
            .as_u64()
            .unwrap()
            < run.cost_of("blocked")["overhead"]["wall_ns"]
                .as_u64()
                .unwrap(),
        "a detailed layer is paid for over its window, not over the run"
    );
}

/// A detailed layer that costs more than its ceiling, with nothing cheaper
/// in its chain, is let go and the run goes on: the cheap pass keeps reading
/// and the escalation keeps deciding. Without the escalation, a run over its
/// budget with nowhere to go ends.
#[test]
fn a_detailed_layer_over_its_budget_is_let_go_and_the_escalation_goes_on() {
    let _machine = one_at_a_time();
    // Held for 20 ms rather than 2: the lock layer is named when sleeping is
    // over half of a reading, and each wakeup is followed by a wait in the run
    // queue that a loaded machine lengthens. Against a 2 ms hold that wait
    // pushed queue plus cpu over half and nothing was named (the flaky tally,
    // #15); against 20 ms it is a few percent of every cycle. A hundred of
    // them is the same two seconds of lock phase.
    let Some((program, _)) = phases_holding("shed", 1000, 100, 20_000) else {
        return;
    };
    let greedy =
        "  - {label: profile, source: cpu.profile, target: *program, mode: sample@20000hz, \
                  emit: [stack], keys: [task], stack: fp, budget: 0.2%, window: on cpu}\n";
    let session = escalation_session(&program, &format!("{greedy}{FUTEX}"));
    let Some(run) = escalate(
        "shed",
        &session,
        &[],
        "that a profile over its 0.2 % ceiling is let go and the run goes on",
    ) else {
        return;
    };
    let stopped = run.stopped("profile");
    assert!(
        stopped.contains("to hold the budget") && stopped.contains("ceiling of 0.2"),
        "the profile says it was let go for the budget, and against which ceiling: {stopped}"
    );
    assert!(
        run.attached("futex").is_some(),
        "the escalation went on deciding, and named the lock layer after the shed:\n{}",
        run.text
    );
    assert!(
        run.notes("blocked")
            .iter()
            .any(|note| note.contains("`profile` was let go to hold the budget")),
        "{:?}",
        run.notes("blocked")
    );
}

/// A layer the run names and this machine cannot serve is reported with the
/// reason, and the cheap answer stands: nothing else is attached in its
/// place, and the layer the run left is let go all the same.
///
/// On a launched program the lock layer's `sched.mutex` resolves to the
/// preload shim, which is given at exec and cannot be attached mid-run, and
/// glibc's probes are refused for a launch by the catalog: this machine
/// cannot serve that layer mid-run, which is the property.
#[test]
fn a_layer_this_run_cannot_serve_is_reported_and_the_cheap_answer_stands() {
    let _machine = one_at_a_time();
    let Some((program, _)) = phases("unserved", 700, 700) else {
        return;
    };
    let session = escalation_session(&program, &format!("{PROFILE}{MUTEX}"));
    let Some(run) = escalate(
        "unserved",
        &session,
        &[],
        "that a lock layer this run cannot serve is reported with the reason",
    ) else {
        return;
    };
    let notes = run.notes("blocked").join("\n");
    assert!(
        notes.contains("the `lock` layer, named at")
            && notes.contains("could not be served")
            && notes.contains("preload")
            && notes.contains("The cheap answer stands")
            && notes.contains("blocked sleeping"),
        "{notes}"
    );
    assert!(run.stopped("mutex").contains("never attached"));
    assert!(
        run.stopped("profile").contains("`lock` layer"),
        "the cpu layer is let go when the lock layer is named, served or not: {}",
        run.stopped("profile")
    );
}

/// A held request this machine cannot serve at all -- the resolver finds no
/// backend for it -- is a layer this machine lacks, not a failed run: the
/// session runs, the request is set aside with the resolver's reason, and when
/// the cheap pass names its layer the escalation attaches what it can of it
/// and says what it could not, in the resolver's words.
///
/// `cpu.flow` is Intel PT or CoreSight, and this machine is AMD: every
/// backend of its chain is rejected by a probe. On a machine that has one the
/// property cannot be created here, and the test says so.
#[test]
fn a_held_request_that_does_not_resolve_is_a_layer_this_machine_lacks_not_a_failed_run() {
    let _machine = one_at_a_time();
    let probe = omn(&[
        "cpu.flow",
        "-m",
        "trace",
        "--pid",
        "1",
        "-d",
        "1s",
        "-e",
        "raw",
        "--dump-resolved",
    ]);
    if probe.status.success() {
        eprintln!(
            "skipped: this machine serves `cpu.flow`, so no held request of it can be one it \
             lacks; it would have checked that an unresolvable held request does not fail the \
             run"
        );
        return;
    }
    let Some((program, _)) = phases("unresolved", 900, 300) else {
        return;
    };
    let flow = "  - {label: flow, source: cpu.flow, target: *program, mode: trace, emit: [raw], \
                window: on cpu}\n";
    let session = escalation_session(&program, &format!("{PROFILE}{flow}"));
    let Some(run) = escalate(
        "unresolved",
        &session,
        &[],
        "that a held request the resolver cannot serve is set aside and reported",
    ) else {
        return;
    };
    assert!(
        run.text
            .contains("`flow` is held for the escalation and this machine cannot serve it"),
        "the run says, before it starts, what it set aside:\n{}",
        run.text
    );
    let notes = run.notes("blocked").join("\n");
    assert!(
        notes.contains("`cpu` at")
            && notes.contains("not served")
            && notes.contains("`flow`: this machine cannot serve it")
            && notes.contains("no viable backend"),
        "the cpu layer is named, its profile attached, and the flow it could not serve said \
         with the resolver's reason:\n{notes}"
    );
    assert!(
        run.attached("profile").is_some(),
        "what the machine can serve of the layer is attached:\n{}",
        run.text
    );
}

/// A run that escalated and a run that did not are different documents. A
/// program over before two readings could name anything attaches no detailed
/// source, and every held request says it measured nothing -- rather than
/// reading as a quiet one.
#[test]
fn a_run_too_short_to_decide_attaches_nothing_and_says_so() {
    let _machine = one_at_a_time();
    let fixture = needs_fixture!();
    let program = vec![
        fixture.to_string_lossy().into_owned(),
        "cpu".to_owned(),
        "150".to_owned(),
    ];
    let session = escalation_session(&program, &format!("{PROFILE}{FUTEX}"));
    let Some(run) = escalate(
        "short",
        &session,
        &[],
        "that a run too short to decide attaches nothing",
    ) else {
        return;
    };
    let notes = run.notes("blocked").join("\n");
    assert!(notes.contains("named no layer"), "{notes}");
    for label in ["profile", "futex"] {
        let stopped = run.stopped(label);
        assert!(
            stopped.contains("never attached") && stopped.contains("did not name its layer"),
            "{label}: {stopped}"
        );
        assert_eq!(run.result(label)["events_seen"], 0, "{label}");
        assert_eq!(
            run.cost_of(label)["overhead"]["attributed_cpu_ns"],
            0,
            "{label}"
        );
    }
}

/// A join whose `from` side is held for the escalation is allowed, and what
/// fell outside that source's window is counted there rather than matched.
///
/// The ground truth is the workload's shape: its first second is spent on a
/// cpu, and the lock layer -- `sched.futex` -- cannot be attached before the
/// switch to the lock phase. So every reading `cpu.oncpu_time` took in that
/// first second, one every 50 ms, was taken while the join's `from` source was
/// not watching: at least ten of them must be outside its window, and the
/// join's four counts must add up to every reading there was.
#[test]
fn a_join_from_a_held_source_counts_what_fell_outside_its_window() {
    let _machine = one_at_a_time();
    let Some((program, _log)) = phases("join-across", 1000, 1000) else {
        return;
    };
    let session = format!(
        "{}joins:\n  - {{id: waits-then-cpu, from: sched.futex, to: cpu.oncpu_time, via: task, \
         match: eq, window: 60s}}\n",
        escalation_session(&program, FUTEX)
    );
    let Some(run) = escalate(
        "join-across",
        &session,
        &[],
        "that a join from the held lock layer counts the cpu phase outside its window",
    ) else {
        return;
    };
    assert!(
        run.attached("futex").is_some(),
        "the lock layer was attached:\n{}",
        run.text
    );
    let join = run.results["joins"]
        .as_array()
        .and_then(|joins| joins.iter().find(|join| join["rule"] == "waits-then-cpu"))
        .unwrap_or_else(|| panic!("no join in the results:\n{}", run.text));
    let count = |field: &str| join[field].as_u64().unwrap_or(0);
    let outside = count("outside_window");
    assert!(
        outside >= 10,
        "a second of cpu before the lock layer could be attached is twenty readings at 50 ms, \
         and only {outside} were outside the futex window: {join}"
    );
    let readings = run.result("on-cpu")["events_seen"].as_u64().unwrap();
    assert_eq!(
        count("matched") + count("unmatched") + count("keyless") + outside,
        readings,
        "every reading is matched, unmatched, keyless or outside the window: {join}"
    );
}

/// An escalation that cannot mean anything is refused before anything runs,
/// with the reason -- exit 2, the validator's code.
#[test]
fn an_escalation_that_cannot_decide_anything_is_refused_before_the_launch() {
    let dir = scratch("refused");
    let run = |name: &str, yaml: &str| {
        let file = dir.join(format!("{name}.yml"));
        std::fs::write(&file, yaml).unwrap();
        omn(&["-f", file.to_str().unwrap()])
    };
    let held_rss =
        "  - {label: rss, source: mem.rss, target: {launch: [sleep, '5']}, mode: poll@100ms, \
         emit: [summary], window: on cpu}\n";
    for (name, yaml, says) in [
        (
            "no-block",
            format!("requests:\n{held_rss}"),
            "held for an escalation",
        ),
        (
            "no-objective",
            format!("escalate: {{}}\nrequests:\n{held_rss}"),
            "needs an objective",
        ),
        (
            "throughput",
            format!("escalate: {{objective: throughput}}\nrequests:\n{held_rss}"),
            "omn saturate --objective throughput",
        ),
        (
            "no-cheap-pass",
            format!("escalate: {{objective: latency}}\nrequests:\n{held_rss}"),
            "does not run `sched.offcpu` or `cpu.oncpu_time`",
        ),
        (
            "decomposition-across",
            format!(
                "escalate: {{objective: latency}}\nrequests:\n{held_rss}decompose:\n  - {{id: \
                 across, whole: lifetime, parts: [{{state: resident, from: mem.rss, value: \
                 bytes}}]}}\n"
            ),
            "decomposition `across` reads `mem.rss`",
        ),
    ] {
        let started = std::time::Instant::now();
        let output = run(name, &yaml);
        let text = spoken(&output);
        assert_eq!(output.status.code(), Some(2), "{name}: {text}");
        assert!(text.contains(says), "{name}: {text}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "{name}: refused before the five-second program ran"
        );
    }

    let output = omn(&[
        "sched.futex",
        "--mode",
        "trace",
        "--window",
        "on lock",
        "--",
        "true",
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", spoken(&output));
    assert!(spoken(&output).contains("held for an escalation"));
}
