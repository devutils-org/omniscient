//! Scenario tests for the `sched` family.
//!
//! A scenario is not a demo. Each test here runs a session from `examples/`
//! and checks its answer against a property the workload chose -- a number
//! written into the workload's own source, never a number read off the tool
//! and compared with itself. `docs/scenarios/sched.md` is the prose half of
//! this file: what is measured, what cannot be, and what these runs found
//! wrong.
//!
//! The four classes of `BRIEF.md`, and where each one is:
//!
//! - **the capability sweep**: `examples/sched-sweep.yml`, checked against the
//!   emits the catalog itself declares, read back out of `omn list`.
//! - **the comparison**: this family has no source with two implemented
//!   backends, so the comparison is between two *mechanisms* --
//!   `perf_event_open` and a raw tracepoint counting the same blocks -- and,
//!   for the eighteen backends this build declares and does not have, with the
//!   refusal.
//! - **the loss path**: `examples/sched-wakeup-loss.yml`, which fills a 256 KiB
//!   BPF ring on purpose and must say what fell out of it.
//! - **the refusal**: `examples/sched-refusals.yml.rejected` (exit 2),
//!   `examples/sched-not-implemented.yml` and
//!   `examples/sched-forced-backends.yml` (exit 3).
//!
//! The eBPF tests skip, loudly, on a machine that does not grant `CAP_BPF` and
//! `CAP_PERFMON`: the unprivileged answer is a refusal, not a lesser
//! measurement, so there is nothing to fall back to. A skip names what is
//! missing; a request that cannot mean anything is never a reason to skip.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// Round trips in `examples/sched-round-trips.yml`, and the number every
/// source in it is checked against. `the_examples_still_say_what_these_tests_
/// assert` keeps the two from drifting apart.
const ROUND_TRIPS: u64 = 2000;

/// Affinity rewrites in `examples/sched-migrations.yml`.
const AFFINITY_CHANGES: u64 = 200;

/// Round trips in `examples/sched-half-blocked.yml`, the workload phase two's
/// P2 asks for: one whose time is known to be half on the cpu and half waiting.
const HALF_BLOCKED_ROUND_TRIPS: u64 = 600;

/// Pairs and round trips per pair in `examples/sched-wakeup-loss.yml`.
const LOSS_PAIRS: u64 = 2;
const LOSS_ROUNDS: u64 = 20_000;

/// How far a count may sit from the number the workload chose.
///
/// A pipe round trip makes the driver leave the cpu only if the reply has not
/// already arrived, and whether it has depends on what else the machine is
/// doing. Over five runs on an idle machine the spread was under two percent;
/// the band is wide enough that a busy one does not turn a measurement into a
/// failure, and narrow enough that a collector reporting half of what happened
/// still fails.
const TOLERANCE: f64 = 0.25;

fn omn(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

/// Runs one of this family's example sessions.
fn session(file: &str, extra: &[&str]) -> Output {
    let path = format!("examples/{file}");
    assert!(
        Path::new(&path).exists(),
        "{path} is part of this scenario and is missing"
    );
    let mut args = vec!["-f", path.as_str()];
    args.extend_from_slice(extra);
    omn(&args)
}

/// Everything the run said, on either stream. A refusal is on stderr and a
/// measurement is on stdout, and a test that reads one of them only is a test
/// that cannot tell a refusal from a silence.
fn spoken(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

/// Whether this machine refused the request for want of the privileges the
/// eBPF backends need. That is a machine's limit and the only thing in this
/// file that is allowed to stop a test quietly.
fn unprivileged(text: &str) -> bool {
    text.contains("CAP_BPF") || text.contains("CAP_PERFMON")
}

/// Whether a program the workload needs is on this machine.
fn have(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

// ---------------------------------------------------------------- json shapes

fn results(output: &Output) -> Vec<Value> {
    let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "the run did not produce one json document ({error}):\n{}",
            spoken(output)
        )
    });
    document["results"]
        .as_array()
        .expect("a json run carries an array of results")
        .clone()
}

fn labelled<'a>(results: &'a [Value], label: &str) -> &'a Value {
    results
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("no request labelled `{label}` in this session"))
}

fn events_seen(result: &Value) -> u64 {
    result["events_seen"].as_u64().expect("an event count")
}

fn dropped(result: &Value) -> u64 {
    result["losses"]["dropped"].as_u64().expect("a drop count")
}

/// Adds up one attribute over every `raw` event of a request.
///
/// `sched.offcpu` counts in the kernel, so one event is a total over the
/// interval between two drains rather than one block. The number of blocks is
/// on the event as `blocks`, and summing it is the only way to recover what the
/// workload did -- the event count is a count of drains.
fn total_attribute(result: &Value, attribute: &str) -> u64 {
    outputs(result, "raw")
        .flat_map(|output| {
            output["events"]
                .as_array()
                .expect("a raw output carries events")
                .clone()
        })
        .filter_map(|event| {
            event["attrs"]
                .as_array()?
                .iter()
                .find(|attr| attr["name"] == attribute)
                .and_then(|attr| attr["value"].as_u64())
        })
        .sum()
}

fn outputs<'a>(result: &'a Value, emit: &'a str) -> impl Iterator<Item = &'a Value> {
    result["outputs"]
        .as_array()
        .expect("a result carries outputs")
        .iter()
        .filter(move |output| output["emit"] == emit)
}

/// The size of the largest series of an emit that reports one count per key.
fn biggest_series(result: &Value, emit: &str) -> u64 {
    outputs(result, emit)
        .flat_map(|output| {
            output["series"]
                .as_array()
                .expect("a summary carries series")
                .clone()
        })
        .filter_map(|series| series["count"].as_u64())
        .max()
        .unwrap_or(0)
}

/// Every emit this run rendered, per request.
fn emits(result: &Value) -> Vec<String> {
    result["outputs"]
        .as_array()
        .expect("a result carries outputs")
        .iter()
        .filter_map(|output| output["emit"].as_str().map(str::to_owned))
        .collect()
}

fn close_to(measured: u64, expected: u64) -> bool {
    let expected = expected as f64;
    let low = expected * (1.0 - TOLERANCE);
    let high = expected * (1.0 + TOLERANCE);
    (measured as f64) >= low && (measured as f64) <= high
}

// ------------------------------------------------- the numbers and the files

/// The constants above are the workload's ground truth only while the workload
/// still contains them. A scenario whose test and whose session disagree about
/// how many round trips there are is checking nothing, and would do it in
/// green.
#[test]
fn the_examples_still_say_what_these_tests_assert() {
    let expected = [
        ("sched-round-trips.yml", ROUND_TRIPS.to_string()),
        ("sched-migrations.yml", AFFINITY_CHANGES.to_string()),
        (
            "sched-half-blocked.yml",
            HALF_BLOCKED_ROUND_TRIPS.to_string(),
        ),
        ("sched-wakeup-loss.yml", format!("\"{LOSS_ROUNDS}\"")),
        ("sched-wakeup-loss.yml", format!("\"{LOSS_PAIRS}\"")),
    ];
    for (file, number) in expected {
        let path = format!("examples/{file}");
        let text = std::fs::read_to_string(&path).expect("an example this family owns");
        assert!(
            text.contains(&number),
            "{path} no longer contains {number}, which tests/scenario_sched.rs asserts against"
        );
    }
}

/// Every session this family ships has to be a session the tool understands.
///
/// The one whose point is to be refused is checked here too, and separately,
/// because it is named `.yml.rejected` rather than `.yml`. That is not a
/// flourish: `tests/cli.rs::the_shipped_examples_validate` walks `examples/`
/// and requires everything with a format's extension to validate, which a
/// session written to be rejected cannot do and should not have to. Keeping it
/// out of that walk is the smallest way to hold both rules; two lines in the
/// shared test would be the better one, and that file is not this stream's.
#[test]
fn every_session_this_family_ships_is_understood() {
    let refused = omn(&[
        "validate",
        "-f",
        "examples/sched-refusals.yml.rejected",
        "--input-format",
        "yaml",
    ]);
    assert_eq!(
        exit_code(&refused),
        2,
        "the refusals session must be rejected by the validator, for its own reasons:\n{}",
        spoken(&refused)
    );

    let expectations = [
        ("sched-blocking-stack.yml", 0),
        ("sched-forced-backends.yml", 0),
        ("sched-half-blocked.yml", 0),
        ("sched-migrations.yml", 0),
        ("sched-not-implemented.yml", 0),
        ("sched-round-trips.yml", 0),
        ("sched-sweep.yml", 0),
        ("sched-wakeup-loss.yml", 0),
        ("sched-wakeup.yml", 0),
    ];
    for (file, expected) in expectations {
        let path = format!("examples/{file}");
        let output = omn(&["validate", "-f", &path]);
        assert_eq!(
            exit_code(&output),
            expected,
            "`omn validate -f {path}` said something else:\n{}",
            spoken(&output)
        );
    }
    let shipped = std::fs::read_dir("examples")
        .expect("the examples directory")
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("sched-") && name.ends_with(".yml")
        })
        .count();
    assert_eq!(
        shipped,
        expectations.len(),
        "a sched example was added or removed without being checked here"
    );
}

// ------------------------------------------------------------- ground truth

/// The anchor of this family's scenarios: one workload, one number, four
/// sources that must each find it.
///
/// The pipe ping-pong in `examples/sched-round-trips.yml` performs a number of
/// round trips written into its own loop. Each round trip is one block of the
/// driver, so it is one switch out, one off-cpu block, one run queue wait --
/// and two wakeups, because the driver is at one end of every hand-off in both
/// directions.
///
/// None of those four is compared with any of the others here. Each is
/// compared with the number in the workload.
#[test]
fn one_known_number_of_round_trips_is_found_by_four_sources() {
    let output = session("sched-round-trips.yml", &["--format", "json"]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: this machine does not grant CAP_BPF and CAP_PERFMON, which every sched eBPF backend needs");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let results = results(&output);

    let switches = events_seen(labelled(&results, "switches-out"));
    assert!(
        close_to(switches, ROUND_TRIPS),
        "{ROUND_TRIPS} round trips are {ROUND_TRIPS} switches out of the cpu, saw {switches}"
    );

    // Not the event count: `sched.offcpu` aggregates in the kernel, so its
    // events are drains and the blocks are on them.
    let blocks = total_attribute(labelled(&results, "blocks"), "blocks");
    assert!(
        close_to(blocks, ROUND_TRIPS),
        "{ROUND_TRIPS} round trips are {ROUND_TRIPS} blocks, saw {blocks}"
    );

    let wakeups = events_seen(labelled(&results, "wakeups"));
    assert!(
        close_to(wakeups, 2 * ROUND_TRIPS),
        "a round trip wakes the peer and is woken back, so {ROUND_TRIPS} of them are {} wakeups, saw {wakeups}",
        2 * ROUND_TRIPS
    );

    // One series per woken task, and the driver's is the one the loop counted.
    let waits = biggest_series(labelled(&results, "run-queue-waits"), "summary");
    assert!(
        close_to(waits, ROUND_TRIPS),
        "every wakeup of the driver is followed by one run queue wait, so {ROUND_TRIPS} \
         round trips are {ROUND_TRIPS} waits, saw {waits}"
    );

    for label in ["switches-out", "blocks", "wakeups", "run-queue-waits"] {
        assert_eq!(
            dropped(labelled(&results, label)),
            0,
            "`{label}` lost events, so its count cannot be checked against a known number:\n{text}"
        );
    }
}

/// The closest this family comes to a backend comparison, and the direction of
/// the gap is the whole of the caveat.
///
/// `sched.switches` counts switches out of the cpu through `perf_event_open`;
/// `sched.offcpu` counts blocks through a raw tracepoint in the kernel. A task
/// leaves the cpu for two reasons -- it was waiting, or the scheduler took the
/// cpu away -- and only the second mechanism can tell them apart. So perf
/// always counts at least as many, never fewer, and what it counts on top is
/// preemption.
///
/// That is not a rounding difference. On an idle machine this workload
/// preempts almost never and the two agree to half a percent; measured inside
/// this suite, with thirteen other tests running, perf saw 2196 and the
/// tracepoint 1972 -- a tenth of the answer, and a tenth that a reader of the
/// switch count alone would take for blocking.
#[test]
fn perf_counts_the_blocks_a_raw_tracepoint_counts_and_preemption_besides() {
    let output = session("sched-round-trips.yml", &["--format", "json"]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: sched.offcpu needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let results = results(&output);

    let switches = events_seen(labelled(&results, "switches-out"));
    let blocks = total_attribute(labelled(&results, "blocks"), "blocks");
    // The slack is the handful of blocks still in flight when the run ends,
    // which have no duration yet and are therefore in no total.
    assert!(
        blocks <= switches + 8,
        "every block is a switch out, so the tracepoint cannot count more than perf: \
         {blocks} blocks against {switches} switches"
    );
    // And both are still the number the workload chose, which is what stops
    // this from being two mechanisms agreeing about nothing.
    assert!(
        close_to(switches, ROUND_TRIPS) && close_to(blocks, ROUND_TRIPS),
        "{ROUND_TRIPS} round trips: perf saw {switches} switches, the tracepoint {blocks} blocks"
    );
}

/// The workload phase two needs: one whose time is half on the cpu and half
/// waiting, by construction rather than by measurement.
///
/// The driver and the responder spin the same number of shell iterations before
/// answering each other, so the driver is on the cpu for its own spin and
/// blocked for the responder's. Neither half is read off the tool: they are
/// equal because the same loop bound is written twice.
///
/// This is the shape a cpu profile alone gets wrong. Half the program's time is
/// in a place a profile cannot see, and `sched.offcpu` beside `cpu.oncpu_time`
/// has to find both halves and have them add up.
#[test]
fn a_workload_that_is_half_blocked_measures_as_half_blocked() {
    let output = session("sched-half-blocked.yml", &["--format", "json"]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: sched.offcpu needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let results = results(&output);

    let blocked_ns = total_attribute(labelled(&results, "blocked"), "total_ns");
    let blocks = total_attribute(labelled(&results, "blocked"), "blocks");
    assert!(
        close_to(blocks, HALF_BLOCKED_ROUND_TRIPS),
        "{HALF_BLOCKED_ROUND_TRIPS} round trips are {HALF_BLOCKED_ROUND_TRIPS} blocks, \
         saw {blocks}"
    );

    // `cpu.oncpu_time` reports a delta per poll, so the total is the count of
    // polls times their mean.
    let on_cpu = labelled(&results, "on-cpu");
    let on_cpu_ns: f64 = outputs(on_cpu, "summary")
        .flat_map(|output| {
            output["series"]
                .as_array()
                .expect("a summary carries series")
                .clone()
        })
        .filter_map(|series| Some(series["count"].as_f64()? * series["mean"].as_f64()?))
        .sum();
    assert!(
        on_cpu_ns > 0.0,
        "the on-cpu half of the workload was not measured at all:\n{text}"
    );

    let blocked_share = blocked_ns as f64 / (blocked_ns as f64 + on_cpu_ns);
    assert!(
        (0.35..=0.65).contains(&blocked_share),
        "the two halves are equal in the workload and came back {:.0} % blocked: \
         {:.1} ms blocked against {:.1} ms on cpu",
        blocked_share * 100.0,
        blocked_ns as f64 / 1e6,
        on_cpu_ns / 1e6
    );

    // And the wakeups are still two per round trip, which is what makes this
    // workload reusable for the `sched.wakeup -> sched.offcpu` crossing.
    let wakeups = events_seen(labelled(&results, "released-by"));
    assert!(
        close_to(wakeups, 2 * HALF_BLOCKED_ROUND_TRIPS),
        "a round trip is two wakeups, so {HALF_BLOCKED_ROUND_TRIPS} of them are {}, saw {wakeups}",
        2 * HALF_BLOCKED_ROUND_TRIPS
    );
}

/// A migration is usually a symptom nobody chose, so the workload chooses it:
/// a task whose affinity no longer admits the core it is running on is moved
/// at once, and the loop rewrites its affinity a known number of times.
#[test]
fn a_known_number_of_affinity_changes_is_a_known_number_of_migrations() {
    if !have("taskset") {
        eprintln!("skipped: this machine has no `taskset`, which the migration workload uses to rewrite its own affinity");
        return;
    }
    let output = session("sched-migrations.yml", &["--format", "json"]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: sched.migrations needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let migrations = total_attribute(labelled(&results(&output), "migrations"), "migrations");
    assert!(
        close_to(migrations, AFFINITY_CHANGES),
        "{AFFINITY_CHANGES} affinity rewrites move the task {AFFINITY_CHANGES} times, \
         saw {migrations}"
    );
}

/// The measurement no cpu profiler can stand in for: not that the shell
/// blocked, but who released it.
///
/// The workload runs a known number of children and waits for each. A child
/// exiting is what wakes the shell, and no two children are the same task, so
/// the run has to contain that many wakeups of the shell by that many distinct
/// wakers. Anything less and the causal chain is not being recorded.
#[test]
fn each_child_that_exits_is_named_as_the_one_that_released_the_shell() {
    const CHILDREN: usize = 5;

    let output = session("sched-wakeup.yml", &[]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: sched.wakeup needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");

    // The shell is the wakee of its own releases; every other wakee in the run
    // is something this process woke from softirq context, which the source's
    // own caveat warns about.
    let mut wakers: Vec<&str> = Vec::new();
    let mut shell: Option<&str> = None;
    for line in text.lines().filter(|line| line.contains("sched.wakeup")) {
        let field = |name: &str| {
            line.split_whitespace()
                .find_map(|word| word.strip_prefix(name))
        };
        let (Some(wakee), Some(waker), Some(comm)) =
            (field("wakee_tid="), field("waker_tid="), field("comm="))
        else {
            continue;
        };
        if comm != "sh" {
            continue;
        }
        shell = Some(wakee);
        if shell == Some(wakee) {
            wakers.push(waker);
        }
    }
    assert!(
        shell.is_some(),
        "no wakeup of the launched shell was recorded at all:\n{text}"
    );
    wakers.sort_unstable();
    wakers.dedup();
    assert!(
        wakers.len() >= CHILDREN,
        "{CHILDREN} children exiting are {CHILDREN} distinct wakers of the shell, saw {}: \
         {wakers:?}\n{text}",
        wakers.len()
    );
}

/// Where a program blocked, not how long -- whoever started the program.
///
/// The workload sleeps from one known call, so the leaf of the blocking stack
/// is known too: `clock_nanosleep`, which is what `time.sleep` becomes. That
/// name is ground truth twice over: it is in the workload's own source, and it
/// is a libc symbol this tool has to find in the target's own mappings to
/// print.
///
/// Both targets, because for a long time only one of them worked. A launched
/// program came back with twelve bare addresses: a symbolizer snapshots
/// `/proc/<pid>/maps` when it is built, and for a program this tool launches
/// there is no single moment when that file says anything useful -- at `start`
/// the child has not execed, at the first tick it is inside `execve` with its
/// old image gone and its new one not yet mapped, and by the time the block
/// that matters is drained the program has exited and the file is empty. The
/// fix reads the maps every tick and keeps the last read that had anything in
/// it, so what a stack is resolved against is the most recent mapping the
/// target had while it was alive.
#[test]
fn a_blocking_stack_names_the_call_that_blocked() {
    const SLEEPS: &str = "import time\nfor _ in range(20): time.sleep(0.05)";
    if !have("python3") {
        eprintln!(
            "skipped: this machine has no `python3`, which the sleeping workload is written in"
        );
        return;
    }

    // Attached to a process this run did not start.
    let mut sleeper = Command::new("python3")
        .args(["-c", SLEEPS])
        .spawn()
        .expect("the sleeping workload should start");
    let pid = sleeper.id().to_string();
    let output = omn(&[
        "sched.offcpu",
        "--pid",
        &pid,
        "--emit",
        "stack",
        "-d",
        "400ms",
    ]);
    let _ = sleeper.wait();

    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: sched.offcpu needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    assert!(
        text.contains("clock_nanosleep"),
        "the workload blocked in `clock_nanosleep` and the stack does not say so:\n{text}"
    );

    // The same call, launched by this run instead -- and blocking once, then
    // exiting. That is the shape the defect needs and the shape a reader is
    // most likely to bring: a program whose one block ends as it finishes, so
    // the drain that carries the stack happens after the program is gone and
    // there are no mappings left to read. A workload that blocks every fifty
    // milliseconds for a second symbolizes either way, because some drain
    // catches it alive.
    let launched = omn(&[
        "sched.offcpu",
        "--emit",
        "stack",
        "--",
        "python3",
        "-c",
        "import time\ntime.sleep(0.3)",
    ]);
    let code = exit_code(&launched);
    let launched = spoken(&launched);
    assert_eq!(code, 0, "{launched}");
    assert!(
        launched.contains("clock_nanosleep"),
        "the same workload launched by this run blocked in the same call, and its stack \
         comes back unsymbolized:\n{launched}"
    );
    // Not one name out of twelve frames either: a snapshot taken mid-`execve`
    // resolves the odd address that happens to fall in what was left.
    let frames: Vec<&str> = launched
        .lines()
        .find(|line| line.contains("clock_nanosleep"))
        .map(|line| line.split(';').collect())
        .unwrap_or_default();
    let named = frames
        .iter()
        .filter(|frame| !frame.trim().starts_with("0x"))
        .count();
    assert!(
        named * 2 > frames.len(),
        "{named} of {} frames of a launched program's blocking stack carry a name: \n{launched}",
        frames.len()
    );
}

/// Asking for stacks costs the stacks and not a symbol table nobody reads.
///
/// `--emit stack` used to cost about 0.65 s per process, and machine-wide it
/// did not finish: every symbolizer parsed `/proc/kallsyms` -- 395 318 lines on
/// this machine -- and `sched.offcpu` builds one per process it sees. The table
/// was then never consulted, because this source asks the kernel for
/// `BPF_F_USER_STACK` and says so in as many words.
///
/// The reference is the same request without stacks, over the same workload:
/// the difference between the two is what symbolizing costs, and it is the only
/// number here that does not depend on how fast this machine is. The minimum of
/// three runs each, because the machine is shared.
#[test]
fn asking_for_stacks_costs_the_stacks_and_not_the_kernels_symbol_table() {
    use std::time::{Duration, Instant};

    // Two thirds of a second was the defect, per process. A quarter of one is
    // well inside it and well outside what reading a few dozen mappings and one
    // shell's symbols costs.
    const SLACK: Duration = Duration::from_millis(250);
    const ROUNDS: usize = 3;

    let fastest = |axes: &[&str]| -> Option<Duration> {
        let mut best: Option<Duration> = None;
        for _ in 0..ROUNDS {
            let started = Instant::now();
            let output = omn(axes);
            let elapsed = started.elapsed();
            let text = spoken(&output);
            if unprivileged(&text) {
                return None;
            }
            assert_eq!(exit_code(&output), 0, "{text}");
            best = Some(best.map_or(elapsed, |held| held.min(elapsed)));
        }
        best
    };

    let sleeper = ["--", "/usr/bin/sleep", "0.2"];
    let mut raw = vec!["sched.offcpu", "--emit", "raw"];
    raw.extend(sleeper);
    let mut stacks = vec!["sched.offcpu", "--emit", "stack"];
    stacks.extend(sleeper);

    let Some(without) = fastest(&raw) else {
        eprintln!("skipped: sched.offcpu needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    };
    let Some(with) = fastest(&stacks) else {
        eprintln!("skipped: sched.offcpu needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    };
    assert!(
        with <= without + SLACK,
        "the same 200 ms workload took {with:?} with stacks and {without:?} without: the \
         stacks are paying for something they do not use"
    );

    // And machine-wide, where the same cost was paid once per process that
    // blocked. This used to not return at all; what is asserted is that it
    // returns, inside a bound no working run comes near.
    let started = Instant::now();
    let output = omn(&["sched.offcpu", "--system", "--emit", "stack", "-d", "200ms"]);
    let wide = started.elapsed();
    let text = spoken(&output);
    assert_eq!(exit_code(&output), 0, "{text}");
    assert!(
        wide < Duration::from_secs(20),
        "a 200 ms machine-wide run with stacks took {wide:?}"
    );
    eprintln!(
        "stacks cost {with:?} against {without:?} without them, and a machine-wide 200 ms \
         window with stacks finished in {wide:?}"
    );
}

/// `count` and `trace` are two answers, not one answer under two names.
///
/// `Collector::stop` says what `count` is: "the single measurement of a `count`
/// window, which has no series and whose closing read **is** the result".
/// `sched.migrations` declared both modes, took the mode, and discarded it, so
/// the two produced the same events over the same windows -- and a mode that
/// changes nothing is worse than one that is refused, because the refusal is
/// visible.
///
/// The workload is the one `examples/sched-migrations.yml` uses and this file
/// already has ground truth for: a task whose affinity no longer admits the
/// core it is on is moved at once, and the loop rewrites it
/// `AFFINITY_CHANGES` times. It is driven from the command line rather than
/// from the session, because `--mode` beside `-f` is accepted and discarded
/// (cpu 5 in `docs/scenarios/cpu.md`) and a scenario that asked the session for
/// `count` would be handed `trace` and would check nothing.
///
/// So the two modes must report the same total -- the workload's -- in two
/// shapes: a series of drains, and one window.
#[test]
fn counting_migrations_is_one_window_and_tracing_them_is_a_series() {
    if !have("taskset") {
        eprintln!("skipped: this machine has no `taskset`, which the migration workload uses to rewrite its own affinity");
        return;
    }
    // The example's loop, stretched over several drains. The pauses do not
    // touch the ground truth -- the task's affinity is still rewritten
    // `AFFINITY_CHANGES` times, and each rewrite still moves it once -- and
    // they are what makes the two shapes distinguishable: a run shorter than
    // one drain interval is one drain, and one drain of `trace` looks like the
    // single window of `count` however the mode is implemented.
    let loop_body = format!(
        "i=0\nwhile [ $i -lt {AFFINITY_CHANGES} ]; do\n  taskset -pc $(( (i % 4) * 5 )) $$ \
         >/dev/null 2>&1\n  i=$((i + 1))\n  if [ $(( i % 40 )) -eq 0 ]; then sleep 0.15; fi\n\
         done\n"
    );
    let moved_by = |mode: &str| -> Option<Value> {
        let output = omn(&[
            "sched.migrations",
            "--mode",
            mode,
            "--emit",
            "raw",
            "--keys",
            "task",
            "--format",
            "json",
            "--",
            "/bin/sh",
            "-c",
            &loop_body,
        ]);
        let text = spoken(&output);
        if unprivileged(&text) {
            return None;
        }
        assert_eq!(exit_code(&output), 0, "{text}");
        Some(results(&output)[0].clone())
    };

    let (Some(traced), Some(counted)) = (moved_by("trace"), moved_by("count")) else {
        eprintln!("skipped: sched.migrations needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    };

    // Same workload, same total: the mode changes the shape of the answer and
    // not the answer.
    for (mode, result) in [("trace", &traced), ("count", &counted)] {
        let moved = total_attribute(result, "migrations");
        assert!(
            close_to(moved, AFFINITY_CHANGES),
            "{AFFINITY_CHANGES} affinity rewrites move the task {AFFINITY_CHANGES} times, \
             and `{mode}` saw {moved}"
        );
    }

    // And a different shape. `count` reports one row per (task, cpu) over the
    // whole window; `trace` reports one per drain, so a pair that moved in two
    // intervals is two rows.
    let windows = |result: &Value| -> Vec<u64> {
        outputs(result, "raw")
            .flat_map(|output| {
                output["events"]
                    .as_array()
                    .expect("a raw output carries events")
                    .clone()
            })
            .filter_map(|event| {
                event["attrs"]
                    .as_array()?
                    .iter()
                    .find(|attr| attr["name"] == "window_ns")
                    .and_then(|attr| attr["value"].as_u64())
            })
            .collect()
    };
    let counted_windows = windows(&counted);
    let traced_windows = windows(&traced);
    assert!(
        !counted_windows.is_empty() && !traced_windows.is_empty(),
        "one of the two modes reported no rows at all"
    );

    // `sched.migrations` drains every 250 ms, so a workload that finishes
    // inside one interval is one drain in either mode and there is nothing here
    // to tell apart.
    let observed = counted["observed_ns"]
        .as_u64()
        .expect("the run reports its window");
    if observed < 750_000_000 {
        eprintln!(
            "skipped: the workload finished in {} ms, which is fewer than three of this \
             source's 250 ms drains, so `trace` has no series to be",
            observed / 1_000_000
        );
        return;
    }
    assert!(
        traced_windows.len() > counted_windows.len(),
        "`trace` reported {} rows and `count` reported {}: the two modes are producing the \
         same events, which is the defect `docs/scenarios/sched.md` records as sched 4",
        traced_windows.len(),
        counted_windows.len()
    );

    // One measurement: every counted row covers the same window, and that
    // window is the run's rather than a drain's. The run reports its own
    // length, which is what the claim is checked against.
    let counted_window = counted_windows[0];
    assert!(
        counted_windows
            .iter()
            .all(|window| *window == counted_window),
        "the counted rows report different windows, so they are not one measurement: \
         {counted_windows:?}"
    );
    assert!(
        counted_window * 10 >= observed * 9,
        "a counted row covers {counted_window} ns of a run that lasted {observed} ns: a \
         `count` window is the whole run"
    );
    let drains = *traced_windows.iter().max().expect("a traced window");
    assert!(
        drains < observed,
        "every traced row covers the whole run ({drains} ns of {observed} ns), so `trace` \
         is not reporting a series either"
    );
    eprintln!(
        "{AFFINITY_CHANGES} migrations, reported as {} rows over one {counted_window} ns \
         window by `count` and as {} rows of at most {drains} ns by `trace`",
        counted_windows.len(),
        traced_windows.len()
    );
}

// ------------------------------------------------------- the capability sweep

/// Every emit the catalog declares has to render, on events shaped the way this
/// family's really are.
///
/// The declared set is read back out of `omn list`, which is the catalog
/// projected, rather than written down here: a source that gains an emit gains
/// it in this test on the same commit. The two exceptions are the combinations
/// this build refuses rather than serves, and both are refused by name
/// elsewhere in this file.
#[test]
fn every_emit_each_collected_source_declares_renders() {
    let output = session("sched-sweep.yml", &["--format", "json"]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: four of the five sources in the sweep need CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let results = results(&output);

    let covered = [
        ("wakeup-every-emit", "sched.wakeup", &[][..]),
        ("runqlat-every-emit", "sched.runqlat", &[]),
        // `stack` is served only for a target the run did not launch, and has
        // a scenario of its own; `examples/sched-blocking-stack.yml`.
        ("offcpu-every-emit", "sched.offcpu", &["stack"]),
        ("switches-every-emit", "sched.switches", &[]),
        ("migrations-traced", "sched.migrations", &[]),
    ];
    for (label, source, excused) in covered {
        let declared = declared_emits(source);
        let rendered = emits(labelled(&results, label));
        for emit in &declared {
            if excused.contains(&emit.as_str()) {
                continue;
            }
            assert!(
                rendered.contains(emit),
                "`{source}` declares `{emit}` and the sweep did not render it; \
                 rendered {rendered:?}, declared {declared:?}"
            );
        }
    }
}

/// The emits one source declares, as `omn list` projects the catalog.
fn declared_emits(source: &str) -> Vec<String> {
    let output = omn(&["list", source]);
    let text = spoken(&output);
    text.lines()
        .find_map(|line| line.trim().strip_prefix("emits "))
        .unwrap_or_else(|| panic!("`omn list {source}` printed no emits line:\n{text}"))
        .split(',')
        .map(|emit| emit.trim().to_owned())
        .collect()
}

/// A format is another way for an emit to be wrong. The sweep has to carry the
/// same emits through `json` as through the terminal, because a format that
/// quietly drops one is a promise broken where nobody looks.
#[test]
fn the_sweep_carries_every_emit_through_a_second_format() {
    let terminal = session("sched-sweep.yml", &[]);
    if unprivileged(&spoken(&terminal)) {
        eprintln!(
            "skipped: the sweep needs CAP_BPF and CAP_PERFMON, which this machine does not grant"
        );
        return;
    }
    let structured = session("sched-sweep.yml", &["--format", "json"]);
    assert_eq!(exit_code(&structured), 0, "{}", spoken(&structured));

    let rendered = spoken(&terminal);
    for result in results(&structured) {
        let label = result["label"].as_str().expect("a label");
        for emit in emits(&result) {
            // `topk` prints its own heading with the count in it, and `hist`
            // prints the bucketing it used; the rest are the bare word.
            let heading = match emit.as_str() {
                "topk" => "topk:".to_owned(),
                "hist" => "hist (".to_owned(),
                other => format!("\n  {other}\n"),
            };
            assert!(
                rendered.contains(&heading),
                "`{label}` rendered `{emit}` in json and not in the terminal format"
            );
        }
    }
}

/// A mode the catalog declares and the build refuses has to say what is
/// missing, not merely that it failed.
///
/// `sched.switches` declares `count` and only `pmu:sw` is written for it, which
/// samples. The refusal names the gap and the flag that works around it. The
/// exit code it uses for that is wrong, and is written up in
/// `docs/scenarios/sched.md`; what is checked here is the message, which is
/// what a reader acts on.
#[test]
fn a_declared_mode_the_build_refuses_says_what_is_missing() {
    let output = omn(&[
        "sched.switches",
        "--mode",
        "count",
        "--emit",
        "summary",
        "--",
        "/bin/true",
    ]);
    let text = spoken(&output);
    assert_ne!(exit_code(&output), 0, "`count` mode is not served:\n{text}");
    assert!(
        text.contains("counting support") && text.contains("--mode trace"),
        "the refusal must name the gap and the mode that works:\n{text}"
    );
}

// ------------------------------------------------------------- the loss path

/// A collector that loses three percent and says nothing produces wrong
/// numbers that look right.
///
/// The workload drives more wakeups through the ring than it can hold between
/// drains. What is checked is the invariant: the loss is counted, it reaches
/// the output with a reason that names what filled, and the events plus the
/// drops still account for the round trips the workload performed.
///
/// What is *not* checked, though it is what the scenario is for, is that the
/// event count alone falls short of the workload. Run on its own it does, by
/// a fifth -- the measured runs are in `docs/scenarios/sched.md`. Run inside
/// this suite it sometimes does not, because a contended interpreter lock adds
/// wakeups of its own and the workload's two-per-round-trip stops being the
/// rate. A test cannot demand a shortfall whose size the machine decides, so
/// the number lives in the document and the invariant lives here.
#[test]
fn an_overflowing_wakeup_ring_says_what_it_could_not_hold() {
    if !have("python3") {
        eprintln!("skipped: this machine has no `python3`, and no shell workload reaches the wakeup rate that fills the ring");
        return;
    }
    let output = session("sched-wakeup-loss.yml", &["--format", "json"]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: sched.wakeup needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");

    let results = results(&output);
    let result = labelled(&results, "wakeups-under-overflow");
    let seen = events_seen(result);
    let lost = dropped(result);
    let expected = 2 * LOSS_PAIRS * LOSS_ROUNDS;

    if lost == 0 {
        // A machine that drains faster than this workload can fill is a
        // machine that cannot serve this scenario, and says which. It still has
        // to have counted the workload correctly, or nothing here was measured.
        assert!(
            close_to(seen, expected),
            "nothing was dropped and the count is still wrong: saw {seen}, expected about {expected}\n{text}"
        );
        eprintln!(
            "skipped: this machine drained the 256 KiB wakeup ring faster than {LOSS_PAIRS} pipe \
             ping-pong pairs could fill it, so there was no overflow to report"
        );
        return;
    }

    assert!(
        (seen + lost) as f64 >= expected as f64 * (1.0 - TOLERANCE),
        "the events and the drops together still do not account for the workload: \
         {seen} seen plus {lost} dropped against about {expected}"
    );
    assert!(
        seen > 0,
        "a run that lost events must still carry what it did see:\n{text}"
    );

    // The count alone would leave a reader guessing at a kernel they cannot
    // see. A loss has to name what filled.
    let terminal = spoken(&session("sched-wakeup-loss.yml", &[]));
    assert!(
        terminal.contains("LOSS:"),
        "a run that dropped events did not say so in the terminal:\n{terminal}"
    );
    assert!(
        terminal.contains("ring was full") || terminal.contains("longer than the"),
        "the loss is counted and not explained:\n{terminal}"
    );
}

// -------------------------------------------------------------- the refusals

/// A request the catalog cannot make sense of is a rejection: exit 2, and the
/// reason names the thing that is actually missing.
///
/// Four of them at once, because a session reports all of its rejections
/// together -- which is what makes this the cheapest of the three refusals to
/// check, and the one whose messages a reader is most likely to meet.
#[test]
fn a_request_that_cannot_mean_anything_is_refused_by_name() {
    let output = session("sched-refusals.yml.rejected", &["--input-format", "yaml"]);
    let text = spoken(&output);
    assert_eq!(
        exit_code(&output),
        2,
        "a rejection is exit 2, not a failure and not a missing feature:\n{text}"
    );
    let named = [
        // The emit, and what the source does emit instead.
        ("cannot emit `hist`", "raw, topk, timeline, summary"),
        // The mode, and the one it has.
        ("cannot be observed in `count` mode", "supports trace"),
        // The field, and the fields there are.
        ("no field `cpu` to filter on", "comm, pid, state"),
        // The target, and why a network interface is not one.
        (
            "cannot be measured per iface",
            "not a network interface source",
        ),
    ];
    for (reason, hint) in named {
        assert!(
            text.contains(reason),
            "no rejection said `{reason}`:\n{text}"
        );
        assert!(
            text.contains(hint),
            "the rejection `{reason}` did not go on to say `{hint}`:\n{text}"
        );
    }
}

/// A source this build describes and does not implement is exit 3, not exit 1.
///
/// The difference matters to the reader more than to the script: exit 1 sends
/// them looking for a kernel option or a capability, and there is none to find.
/// The message says the request itself is valid, which is the sentence that
/// stops the search.
#[test]
fn a_source_with_no_collector_is_refused_with_its_own_exit_code() {
    let output = session("sched-not-implemented.yml", &[]);
    let text = spoken(&output);
    assert_eq!(
        exit_code(&output),
        3,
        "`no collector` has an exit code of its own:\n{text}"
    );
    assert!(
        text.contains("no collector is written for `sched.lock`"),
        "the refusal must name the source:\n{text}"
    );
    assert!(
        text.contains("The request itself is valid"),
        "the refusal must say the request was understood, or a reader goes looking \
         for a machine problem that is not there:\n{text}"
    );

    // The family's two machine-wide uncollected sources answer the same way
    // from the command line, which a session file cannot hold beside the three
    // above: one run observes one target.
    for source in ["sched.irq", "sched.numa_balance"] {
        let output = omn(&[
            source, "--mode", "trace", "--emit", "summary", "--system", "-d", "50ms",
        ]);
        assert_eq!(
            exit_code(&output),
            3,
            "`{source}` has no collector and should say so with exit 3:\n{}",
            spoken(&output)
        );
    }
}

/// This family has no source with two implemented backends, so the comparison
/// `--backend force:` exists for is with the refusal.
///
/// Every backend pinned in this session probes available on a machine that can
/// run the rest of these tests: the requirement is there, the kernel is there,
/// and what is missing is the collector. So the answer has to be exit 3 -- the
/// build -- and not exit 1, the machine. It also has to say that the resolver
/// was bypassed, because a pinned backend is not a measurement anyone should
/// quote.
#[test]
fn pinning_a_backend_this_build_lacks_blames_the_build_and_not_the_machine() {
    let output = session("sched-forced-backends.yml", &[]);
    let text = spoken(&output);
    assert_eq!(
        exit_code(&output),
        3,
        "a pinned backend with no collector is exit 3:\n{text}"
    );
    assert!(
        text.contains("is pinned, so the resolver's choice is bypassed"),
        "pinning a backend has to say that it did:\n{text}"
    );
    assert!(
        text.contains("no collector is written for `sched.offcpu` on backend `bpf:tp:stacks`"),
        "the refusal must name the backend that was pinned:\n{text}"
    );

    // And the machine is not what is missing: the same backend's requirements
    // all probe available, which is what makes exit 1 the wrong answer.
    let listed = spoken(&omn(&["list", "sched.offcpu"]));
    assert!(
        listed.contains("no collector yet"),
        "`omn list` should mark the backends this build does not have:\n{listed}"
    );
}
