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

use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Output, Stdio};

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
        ("sched-lock-shared-file.yml", LOCK_WRITERS.to_string()),
        ("sched-lock-shared-file.yml", LOCK_LINES.to_string()),
        (
            "sched-futex-known-wait.yml",
            format!("HOLD = {}", FUTEX_HOLD_MS as f64 / 1000.0),
        ),
        (
            "sched-mutex-known-hold.yml",
            format!("\"{MUTEX_THREADS}\", \"{MUTEX_PER_THREAD}\", \"{MUTEX_HOLD_US}\""),
        ),
        (
            "sched-mutex-uncontended.yml",
            format!(
                "\"1\", \"{}\", \"{MUTEX_HOLD_US}\"",
                MUTEX_THREADS * MUTEX_PER_THREAD
            ),
        ),
        (
            "sched-mutex-with-heap.yml",
            format!("\"2\", \"{}\"", MUTEX_WITH_HEAP / 2),
        ),
        (
            "sched-mutex-loss.yml",
            format!("\"{STORM_THREADS}\", \"{STORM_PER_THREAD}\", \"0\", private"),
        ),
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
        ("sched-futex-known-wait.yml", 0),
        ("sched-irq-against-procfs.yml", 0),
        ("sched-lock-shared-file.yml", 0),
        ("sched-numa-balance.yml", 0),
        ("sched-sweep-machine.yml", 0),
        ("sched-forced-backends.yml", 0),
        ("sched-half-blocked.yml", 0),
        ("sched-migrations.yml", 0),
        ("sched-mutex-known-hold.yml", 0),
        ("sched-mutex-loss.yml", 0),
        ("sched-mutex-uncontended.yml", 0),
        ("sched-mutex-with-heap.yml", 0),
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

    // The driver's switches, not the run's: `sched.switches` follows every
    // task of the program now, the responder too, and a switch count holds
    // the preemptions a loaded machine adds besides the blocks the loop made
    // (2643 of the driver's under a full suite, for 2000). Every round trip
    // blocks the driver once, which is the floor; counting each switch twice
    // is what the ceiling stops.
    let switches = driver_switches(&results);
    assert!(
        switches as f64 >= ROUND_TRIPS as f64 * (1.0 - TOLERANCE) && switches <= 2 * ROUND_TRIPS,
        "{ROUND_TRIPS} round trips are {ROUND_TRIPS} switches of the driver out of the cpu \
         and the preemptions besides, saw {switches}"
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

    let switches = driver_switches(&results);
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
        switches as f64 >= ROUND_TRIPS as f64 * (1.0 - TOLERANCE) && close_to(blocks, ROUND_TRIPS),
        "{ROUND_TRIPS} round trips: perf saw {switches} switches, the tracepoint {blocks} blocks"
    );
}

/// The driver's switches in `examples/sched-round-trips.yml`: the series of
/// the task `sched.offcpu` saw block, which is the driver alone -- its target
/// is the launched shell's process, and the responder is a process of its own
/// that `sched.switches`, following what the shell starts, counts beside it.
fn driver_switches(results: &[Value]) -> u64 {
    let mut blocked = std::collections::BTreeMap::<u64, u64>::new();
    for event in raw_events(labelled(results, "blocks")) {
        let tid = event["tid"].as_u64().expect("a block names its task");
        *blocked.entry(tid).or_default() += event["blocks"].as_u64().unwrap_or(0);
    }
    let (driver, _) = blocked
        .into_iter()
        .max_by_key(|(_, blocks)| *blocks)
        .expect("the driver blocked");
    outputs(labelled(results, "switches-out"), "summary")
        .flat_map(|output| output["series"].as_array().expect("series").clone())
        .find(|series| series["key"] == format!("task:{driver}"))
        .and_then(|series| series["count"].as_u64())
        .unwrap_or(0)
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
    // workload reusable for the `sched.wakeup -> sched.offcpu` crossing: one
    // for each of the two tasks, the driver and the responder, which are the
    // two heaviest wakees. Not the run's total, which a busy machine inflates:
    // a wakeup made from an interrupt that landed on the driver's cpu is
    // credited to the driver as its waker, and a full suite put 3862 and 5496
    // there for 1200 while the pair still had its 600 each (filed).
    let pair: u64 = outputs(labelled(&results, "released-by"), "topk")
        .flat_map(|output| {
            output["series"]
                .as_array()
                .expect("a ranking carries series")
                .iter()
                .take(2)
                .filter_map(|series| series["events"].as_u64())
                .collect::<Vec<_>>()
        })
        .sum();
    assert!(
        close_to(pair, 2 * HALF_BLOCKED_ROUND_TRIPS),
        "a round trip wakes each of the two tasks once, so {HALF_BLOCKED_ROUND_TRIPS} of them \
         are {} between the pair, saw {pair}",
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
///
/// It failed in two full runs of three at the tenth wave's close and passed
/// alone, and neither the symbolizer nor the exit was the cause: the program
/// walked every blocking stack on the machine before narrowing to the target,
/// and a loaded machine filled the stack map before the target blocked. The
/// deterministic version of that is
/// `a_targets_blocking_stack_is_named_while_another_process_floods_the_stack_map`.
#[test]
fn a_blocking_stack_names_the_call_that_blocked() {
    // The workload says when it is ready, and the test waits for it to say so.
    // Without that this attaches 400 ms to a python that has not finished
    // starting: measured under the rest of this suite, the run saw four blocks
    // of an interpreter still loading and none of the sleeps it was written
    // for. Lengthening the window would have hidden the race rather than
    // removing it.
    const SLEEPS: &str =
        "import sys, time\nprint('ready', flush=True)\nfor _ in range(20): time.sleep(0.05)";
    if !have("python3") {
        eprintln!(
            "skipped: this machine has no `python3`, which the sleeping workload is written in"
        );
        return;
    }

    // Attached to a process this run did not start.
    let mut sleeper = Command::new("python3")
        .args(["-c", SLEEPS])
        .stdout(Stdio::piped())
        .spawn()
        .expect("the sleeping workload should start");
    let mut ready = String::new();
    std::io::BufReader::new(sleeper.stdout.take().expect("a piped stdout"))
        .read_line(&mut ready)
        .expect("the workload announces itself before it sleeps");
    assert_eq!(ready.trim(), "ready");
    let pid = sleeper.id().to_string();
    // Six hundred milliseconds of a workload that sleeps for a thousand. The
    // window is wider than the assertion needs on a quiet machine, because a
    // loaded one spends some of it watching the interpreter block on something
    // that is not the sleep -- and the assertion is that the sleep is *there*,
    // not that it is all that is.
    let output = omn(&[
        "sched.offcpu",
        "--pid",
        &pid,
        "--emit",
        "stack",
        "-d",
        "600ms",
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

/// A request missing an axis nothing can fill in is exit 2, like every other
/// request that cannot mean anything.
///
/// Two requests, both missing one required axis, both unanswerable for the same
/// reason -- and they used to answer with two different codes. The missing emit
/// is raised by the validator and reached `EXIT_REJECTED`; the missing mode is
/// a `ParseError` from `cli.rs` and fell through `main`'s generic failure code,
/// so a script branching on it sent its reader to look at the machine. That is
/// defect 6 of `docs/scenarios/sched.md`, and it applied to every source with
/// two modes -- `sched.irq` and `sched.numa_balance` answered the same way.
#[test]
fn an_under_specified_request_is_rejected_rather_than_failed() {
    let under_specified: [(&[&str], &str); 3] = [
        (
            &["sched.switches", "--emit", "summary", "--", "/bin/true"],
            "needs --mode",
        ),
        (
            &["sched.switches", "--mode", "trace", "--", "/bin/true"],
            "without an emit",
        ),
        (
            &["sched.irq", "--emit", "summary", "--system", "-d", "50ms"],
            "needs --mode",
        ),
    ];
    for (args, expected) in under_specified {
        let output = omn(args);
        let text = spoken(&output);
        assert_eq!(
            exit_code(&output),
            2,
            "`{}` cannot mean anything as written, and that is exit 2 whichever layer \
             notices it:\n{text}",
            args.join(" ")
        );
        assert!(
            text.contains(expected),
            "`{}` was refused without saying `{expected}`:\n{text}",
            args.join(" ")
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
        text.contains("no collector is written for `sched.mutex` on backend `bpf:uprobe`"),
        "the refusal must name the source and the backend pinned:\n{text}"
    );
    assert!(
        text.contains("The request itself is valid"),
        "the refusal must say the request was understood, or a reader goes looking \
         for a machine problem that is not there:\n{text}"
    );

    // And the four that used to answer this way no longer do. A source that
    // gains a collector and leaves a "not built" message behind is the same
    // lie as one that never had it, pointing the other way.
    for source in ["sched.lock", "sched.irq", "sched.numa_balance"] {
        let output = omn(&[
            source, "--mode", "trace", "--emit", "summary", "--system", "-d", "50ms",
        ]);
        let text = spoken(&output);
        if unprivileged(&text) {
            eprintln!("skipped: `{source}` needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
            continue;
        }
        assert!(
            !text.contains("no collector is written"),
            "`{source}` has a collector and still says it does not:\n{text}"
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

    // Every one of them, not only the first. The validator has always answered
    // for every rejection at once; this session's five pinned backends used to
    // be reported one run at a time, so fixing the file took five runs -- same
    // shape of file, same kind of answer, two behaviours. That is defect 8 of
    // `docs/scenarios/sched.md`.
    for (source, backend) in [
        ("sched.offcpu", "bpf:tp:stacks"),
        ("sched.offcpu", "bpf:tp"),
        ("sched.switches", "bpf:raw_tp"),
        ("sched.wakeup", "bpf:tp"),
        ("sched.migrations", "bpf:tp"),
    ] {
        assert!(
            text.contains(&format!(
                "no collector is written for `{source}` on backend `{backend}`"
            )),
            "a session with five unserved requests has to name all five:\n{text}"
        );
    }

    // And the machine is not what is missing: the same backend's requirements
    // all probe available, which is what makes exit 1 the wrong answer.
    let listed = spoken(&omn(&["list", "sched.offcpu"]));
    assert!(
        listed.contains("no collector yet"),
        "`omn list` should mark the backends this build does not have:\n{listed}"
    );
}

// ===========================================================================
// The sixth wave: lock contention, futex waits, interrupts and NUMA balancing
//
// Four sources whose catalog entries were written to the raw tracepoint path
// in wave three and whose collectors landed in wave six. Each has a ground
// truth of its own kind, and they are deliberately three different kinds: a
// difference the workload constructed, a delay the workload injected, and a
// counter the kernel keeps for its own reasons.
// ===========================================================================

use std::path::PathBuf;

/// Writers in `examples/sched-lock-shared-file.yml`, and lines each one writes.
///
/// The count is in the session's own comment, and
/// `the_examples_still_say_what_these_tests_assert` keeps the two together.
const LOCK_WRITERS: usize = 12;
const LOCK_LINES: u64 = 400_000;

/// How many waits on the shared inode's rwsem this scenario expects before it
/// believes twelve writers were serialised on one lock at all.
///
/// Measured over two seconds on the development machine: 2233 of them, 5.8
/// seconds of aggregate waiting at a mean of 2.6 ms each. The bar is two
/// hundred, which is wide enough that a busy machine or a different filesystem
/// does not turn a measurement into a failure, and far above the zero the
/// private-file run produces.
const LOCK_EXCLUSIVE_WAITS: u64 = 200;

/// How many times more of them the shared run must have than the private one.
///
/// The private run produced none at all, which is the point: an inode's rwsem
/// is per inode, so twelve writers on twelve inodes never contend it however
/// fast they run. The ratio rather than an exact zero because a machine doing
/// something else may contend some other file's rwsem in the same window, and
/// a scenario that failed on that would be asserting what the machine was
/// doing.
const LOCK_SEPARATION: u64 = 20;

/// How long `examples/sched-futex-known-wait.yml` holds its lock while the
/// other thread waits for it, in milliseconds. The session says the same
/// number.
const FUTEX_HOLD_MS: u64 = 500;

/// What counts as the workload's own wait rather than the interpreter's.
///
/// The two threads also wait on the interpreter's internal locks, for
/// microseconds at a time. Half of the injected wait is four orders of
/// magnitude above those, so it separates them without being a threshold
/// anyone has to calibrate.
const LONG_WAIT_NS: u64 = FUTEX_HOLD_MS * 1_000_000 / 2;

/// The name the lock workload's processes carry, which the session filters on.
const LOCK_PROBE: &str = "omnlockprobe";

/// One machine, two scenarios that start processes under the same name.
///
/// `sched.lock` has no per-process target -- the catalog gives it `--system`,
/// `--pid` and `--tid`, and the workload is twelve separate processes, so a
/// machine-wide run narrowed by `comm` is the only shape that sees all of
/// them. Two of these running at once each measure the other's writers, and
/// the attribution assertion catches it, which is how this lock came to be
/// here rather than a guess that it might be needed.
static LOCK_WORKLOAD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds that lock for the length of a scenario, poisoning and all: a test
/// that panicked while holding it has already failed, and the next one should
/// still run rather than failing for a reason of somebody else's.
fn one_lock_workload_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    LOCK_WORKLOAD
        .lock()
        .unwrap_or_else(|held| held.into_inner())
}

fn scratch(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// A copy of the shell under a name nothing else on the machine has.
///
/// The session filters on `comm`, and `comm` is the basename of what was
/// exec'd. A workload that ran as `sh` could not be told apart from every other
/// shell on the machine, and a `--system` measurement of the machine's own
/// contention is exactly what this scenario must not accidentally assert.
fn lock_probe() -> Option<PathBuf> {
    let path = scratch(LOCK_PROBE);
    if path.exists() {
        return Some(path);
    }
    let shell = Path::new("/bin/sh");
    if !shell.exists() {
        return None;
    }
    std::fs::copy(shell, &path).ok()?;
    let mut permissions = std::fs::metadata(&path).ok()?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
    }
    std::fs::set_permissions(&path, permissions).ok()?;
    Some(path)
}

/// Starts the writers and returns them, with the pids they were given.
///
/// `shared` decides the only thing that differs between the two runs this
/// scenario compares: whether all of them append to one file or each to its
/// own. Same program, same number of processes, same number of writes, same
/// bytes.
fn lock_writers(probe: &Path, shared: bool, directory: &Path) -> Vec<std::process::Child> {
    let _ = std::fs::create_dir_all(directory);
    (0..LOCK_WRITERS)
        .filter_map(|index| {
            let file = if shared {
                directory.join("shared")
            } else {
                directory.join(format!("own.{index}"))
            };
            let script = format!(
                "exec 3>>{}; i=0; while [ $i -lt {LOCK_LINES} ]; do \
                 echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx >&3; i=$((i+1)); done",
                file.display()
            );
            Command::new(probe)
                .arg("-c")
                .arg(script)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()
        })
        .collect()
}

fn stop(writers: Vec<std::process::Child>) {
    for mut writer in writers {
        let _ = writer.kill();
        let _ = writer.wait();
    }
}

/// How many lock waits the run attributed, in waits rather than in events.
///
/// One event stands for as many waits as its `count` says: the histogram is
/// built in the kernel, so the event count is a count of `(task, kind, stack,
/// bucket)` rows and not of waits.
fn waits_attributed(result: &Value) -> u64 {
    total_attribute(result, "count")
}

/// Every `raw` event of a request, as attribute maps.
fn raw_events(result: &Value) -> Vec<std::collections::BTreeMap<String, Value>> {
    outputs(result, "raw")
        .flat_map(|output| {
            output["events"]
                .as_array()
                .expect("a raw output carries events")
                .clone()
        })
        .map(|event| {
            event["attrs"]
                .as_array()
                .expect("an event carries attributes")
                .iter()
                .map(|attr| {
                    (
                        attr["name"].as_str().expect("a name").to_owned(),
                        attr["value"].clone(),
                    )
                })
                .collect()
        })
        .collect()
}

// -------------------------------------------------- sched.lock: ground truth

/// Twelve processes appending to one file wait for a kernel lock; the same
/// twelve appending to twelve files do not.
///
/// The property is constructed rather than observed, which is what makes it
/// ground truth. A write to a regular file holds that file's inode lock for the
/// duration of the write, so the first run serialises twelve writers on one
/// lock and the second serialises nothing. Everything else about the two runs
/// is identical: the same program, the same number of processes, the same
/// number of writes of the same size, started the same way.
///
/// What is asserted is the separation and not a threshold. The measured ratio
/// on an idle machine was fifty-four; the bar is five, because a machine with
/// something else on it is still a machine where sharing a lock costs more than
/// not sharing one, and a collector that reported the machine's own contention
/// rather than the workload's would fail either way.
#[test]
fn sharing_one_file_is_the_only_difference_between_these_two_runs() {
    let _serialised = one_lock_workload_at_a_time();
    let Some(probe) = lock_probe() else {
        eprintln!("skipped: /bin/sh could not be copied, and this workload needs a program with a name of its own to filter on");
        return;
    };
    let directory = scratch("sched-lock-shared-file");

    let mut measured = Vec::new();
    for shared in [true, false] {
        let _ = std::fs::remove_dir_all(&directory);
        let writers = lock_writers(&probe, shared, &directory);
        assert_eq!(
            writers.len(),
            LOCK_WRITERS,
            "the workload could not be started, so there is nothing to measure"
        );
        let pids: Vec<u32> = writers.iter().map(|writer| writer.id()).collect();
        // Long enough for every writer to be past `exec` and into its loop.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let output = session("sched-lock-shared-file.yml", &["--format", "json"]);
        let text = spoken(&output);
        stop(writers);
        if unprivileged(&text) {
            eprintln!("skipped: `sched.lock` needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
            let _ = std::fs::remove_dir_all(&directory);
            return;
        }
        assert_eq!(exit_code(&output), 0, "{text}");
        let results = results(&output);
        let result = labelled(&results, "contention");

        // Attribution, and it is exact rather than approximate: every wait this
        // run reported has to belong to a process this test started. A
        // `--system` measurement that leaked the machine's own contention into
        // the answer would show up here and nowhere else.
        let mut exclusive = (0u64, 0u64);
        let mut every = (0u64, 0u64);
        for event in raw_events(result) {
            assert_eq!(
                event["comm"].as_str(),
                Some(LOCK_PROBE),
                "the session filters on comm and an event carries another: {event:?}"
            );
            let pid = event["pid"].as_u64().expect("a pid on every event") as u32;
            assert!(
                pids.contains(&pid),
                "the run attributed a wait to pid {pid}, which is not one of the {} it started",
                pids.len()
            );
            let count = event["count"].as_u64().expect("a weight");
            let total = event["total_ns"].as_u64().expect("an exact total");
            every = (every.0 + count, every.1 + total);
            // The rwsem taken for writing is the file's own inode lock, which
            // is the lock the workload constructed a fight over.
            if event["kind"]
                .as_str()
                .is_some_and(|kind| kind.contains("write"))
            {
                exclusive = (exclusive.0 + count, exclusive.1 + total);
            }
        }
        measured.push((exclusive, every));
    }
    let _ = std::fs::remove_dir_all(&directory);

    let (shared, private) = (measured[0], measured[1]);
    eprintln!(
        "{LOCK_WRITERS} writers appending {LOCK_LINES} lines each.\n  \
         one shared file:   {} waits on the inode rwsem ({:.2} s), {} waits on any lock\n  \
         twelve own files:  {} waits on an inode rwsem ({:.2} s), {} waits on any lock",
        shared.0 .0,
        shared.0 .1 as f64 / 1e9,
        shared.1 .0,
        private.0 .0,
        private.0 .1 as f64 / 1e9,
        private.1 .0,
    );

    assert!(
        shared.0 .0 >= LOCK_EXCLUSIVE_WAITS,
        "twelve processes appending to one file serialise on that file's rwsem, and this run \
         found {} waits on one where it expected at least {LOCK_EXCLUSIVE_WAITS}",
        shared.0 .0
    );
    assert!(
        shared.0 .0 >= private.0 .0.max(1) * LOCK_SEPARATION,
        "one shared file produced {} waits on an inode rwsem and twelve private files produced \
         {}; the rwsem is per inode, so the only difference between these two runs is the one \
         that should show here",
        shared.0 .0,
        private.0 .0
    );
    assert!(
        shared.0 .1 > private.0 .1,
        "the shared run waited {} ns on inode rwsems and the private run {} ns",
        shared.0 .1,
        private.0 .1
    );
}

/// A count of lock waits is a throughput measurement wearing a contention
/// measurement's name, and this scenario is where that was found.
///
/// The private-file run -- the one with *no* contention over the thing the
/// workload shares -- produces two hundred times more lock waits in total than
/// the shared one. Nothing is wrong with either number. Serialising twelve
/// writers on one inode makes them slow; slow processes make fewer syscalls;
/// and it is the syscalls that reach the filesystem's own spinlocks. The
/// private run is two hundred times faster and pays for it in two hundred
/// times more brief contention somewhere else.
///
/// This test asserts the trap rather than working around it, because it is the
/// reason `kind` is part of the key: a reader who ranks two runs by how many
/// lock waits each had will order them backwards, and there is nothing in a
/// count that says so. It is written up in `docs/scenarios/sched.md`.
#[test]
fn more_lock_waits_is_not_more_contention() {
    let _serialised = one_lock_workload_at_a_time();
    let Some(probe) = lock_probe() else {
        eprintln!("skipped: /bin/sh could not be copied");
        return;
    };
    let directory = scratch("sched-lock-throughput");
    let mut totals = Vec::new();
    for shared in [true, false] {
        let _ = std::fs::remove_dir_all(&directory);
        let writers = lock_writers(&probe, shared, &directory);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let output = session("sched-lock-shared-file.yml", &["--format", "json"]);
        let text = spoken(&output);
        stop(writers);
        if unprivileged(&text) {
            eprintln!("skipped: `sched.lock` needs CAP_BPF and CAP_PERFMON");
            let _ = std::fs::remove_dir_all(&directory);
            return;
        }
        assert_eq!(exit_code(&output), 0, "{text}");
        let results = results(&output);
        totals.push(waits_attributed(labelled(&results, "contention")));
    }
    let _ = std::fs::remove_dir_all(&directory);

    eprintln!(
        "lock waits of any kind: {} with the contention, {} without it -- the run that is \
         serialised is the one with fewer",
        totals[0], totals[1]
    );
    assert!(
        totals[0] > 0 && totals[1] > 0,
        "both runs contend something; this one found {totals:?}"
    );
    assert!(
        totals[1] > totals[0],
        "the finding this test records is that the uncontended run has *more* lock waits \
         ({} against {}), because it runs faster and reaches more locks. If that has stopped \
         being true on this machine, the note in docs/scenarios/sched.md needs the new numbers \
         rather than this test needing a smaller assertion",
        totals[1],
        totals[0]
    );
}

/// The loss path: a run that could not time a wait says which wait and why.
///
/// Every reason this source can lose a wait for is a different fact about the
/// run and is answered differently -- a map too small for the machine, an
/// interval inside another interval, a bucket the kernel would not let this cpu
/// touch -- and a loss reported under the wrong one sends a reader to change
/// something that was never the problem. The first version of this collector
/// reported all of them as "the map is full", on a machine where the map held
/// sixteen thousand entries and nine were in use.
///
/// The losses themselves are the kernel's to produce and this test does not
/// pretend to force them: a lock tracepoint fires inside the code a map update
/// takes a lock in, so re-entrant refusals happen on a busy machine and not to
/// order. What is asserted is what holds either way -- the count and the words
/// agree, and there is never a number without a sentence.
#[test]
fn a_run_that_could_not_time_a_wait_says_which_wait_and_why() {
    let _serialised = one_lock_workload_at_a_time();
    let Some(probe) = lock_probe() else {
        eprintln!("skipped: /bin/sh could not be copied");
        return;
    };
    let directory = scratch("sched-lock-loss");
    let _ = std::fs::remove_dir_all(&directory);
    let writers = lock_writers(&probe, true, &directory);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let output = session("sched-lock-shared-file.yml", &["--format", "json"]);
    let text = spoken(&output);
    stop(writers);
    let _ = std::fs::remove_dir_all(&directory);
    if unprivileged(&text) {
        eprintln!("skipped: `sched.lock` needs CAP_BPF and CAP_PERFMON");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let result = &results(&output)[0];

    let lost = dropped(result);
    let reasons = result["losses"]["reasons"]
        .as_object()
        .expect("a loss carries its reasons");
    if lost == 0 {
        // Stated rather than hidden: this run lost nothing, so what it
        // establishes is the negative -- a clean run claims no loss.
        eprintln!(
            "the contended run lost nothing this time, so what is checked is the negative: \
             {} events, no drops, no reasons",
            events_seen(result)
        );
        assert!(
            reasons.is_empty(),
            "a run that dropped nothing must not name a reason it dropped something: {reasons:?}"
        );
        return;
    }

    assert!(
        !reasons.is_empty(),
        "{lost} waits were dropped and the run gave no reason for any of them"
    );
    // Each of the things this source can fail to record has its own
    // sentence. A reason that is none of them is a number with prose around it.
    // The last is the kernel's: a run of the program skipped because one was
    // already running on that cpu, which this contended workload does provoke.
    let known = [
        "than the in-flight map holds",
        "without ending its first",
        "already being updated on this cpu",
        "before it began",
        "than the histogram holds",
        "already running on that cpu",
    ];
    for reason in reasons.keys() {
        assert!(
            known.iter().any(|shape| reason.contains(shape)),
            "a loss was reported under a reason this scenario does not recognise: {reason}"
        );
    }
    // And it reaches the terminal, which is where a reader is.
    let rendered = spoken(&session("sched-lock-shared-file.yml", &[]));
    assert!(
        rendered.contains("LOSS") || rendered.contains("no loss"),
        "a run has to say what it did not see:\n{rendered}"
    );
}

// ------------------------------------------------- sched.futex: ground truth

/// A lock held for a known time is the wait the other thread reports.
///
/// The workload injects the delay: one thread takes a lock and sleeps 250 ms
/// holding it, and a second asks for the same lock 50 ms later. Its first wait
/// is 200 ms and its next two are 250 ms each, and all three numbers are in the
/// session's own source rather than in anything the tool computed.
///
/// The check is against the injected delay in two ways, because they fail
/// differently. The bucket is a fact about the histogram's layout -- a
/// kernel-side bucket and a userspace one have to agree, and off by one is a
/// factor of two. The exact total is a fact about the clock: `total_ns` is kept
/// beside the count precisely because a log2 bucket is a factor of two wide and
/// a sum built from its bounds would inherit that.
#[test]
fn a_lock_held_for_a_known_time_is_the_wait_the_other_thread_reports() {
    if !have("python3") {
        eprintln!(
            "skipped: this workload needs two threads and a lock, and the program that provides \
             them here is python3, which this machine does not have"
        );
        return;
    }
    let output = session("sched-futex-known-wait.yml", &["--format", "json"]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: `sched.futex` needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let results = results(&output);
    let result = labelled(&results, "waits");

    // Every wait this run reported. The one that matters is the long one:
    // the workload's threads also wait on the interpreter's own locks, for
    // microseconds at a time.
    let mut long_waits: Vec<(u64, u64, u64)> = Vec::new();
    let mut processes: std::collections::BTreeSet<u64> = Default::default();
    let mut bucketed = 0;
    for event in raw_events(result) {
        processes.insert(event["pid"].as_u64().expect("a pid"));
        let low = event["wait_ns"].as_u64().expect("a bucketed wait");
        let tid = event["tid"].as_u64().expect("a tid");
        let count = event["count"].as_u64().expect("a weight");
        let total = event["total_ns"].as_u64().expect("an exact total");

        // The bucket has to contain the exact total, on every event and
        // whatever this machine was doing. The histogram is built in the
        // kernel and re-bucketed by the emit layer, and off by one there is a
        // factor of two in every number this source reports -- so it is
        // checked against the value rather than against a bucket the workload
        // was expected to land in.
        if count == 1 {
            assert!(
                low <= total && total < low.saturating_mul(2).max(1),
                "a wait of {total} ns is reported in the bucket that starts at {low} ns"
            );
            bucketed += 1;
        }
        if total >= LONG_WAIT_NS {
            long_waits.push((tid, low, total));
        }
    }
    assert!(
        bucketed > 0,
        "no event carried a single wait, so the kernel's bucketing was never crossed against \
         the emit layer's:\n{text}"
    );

    // The target narrowed to one process, so nothing else on the machine may
    // appear -- and `sys_enter` fires on every syscall of every process, so
    // this is the assertion that the filter ran at all.
    assert_eq!(
        processes.len(),
        1,
        "the run watched one launched program and reported waits from {} processes",
        processes.len()
    );

    // Exactly one, by construction rather than by luck: the main thread takes
    // the lock before the waiter exists, so the waiter cannot arrive early and
    // find it free, and nothing else in the program waits for anything.
    assert_eq!(
        long_waits.len(),
        1,
        "the workload makes one thread wait about {FUTEX_HOLD_MS} ms and nothing else in it \
         waits at all; this run reports {} waits longer than {} ms: {long_waits:?}\n{text}",
        long_waits.len(),
        LONG_WAIT_NS / 1_000_000,
    );

    let (waiter, bucket, measured) = long_waits[0];
    let expected_ns = FUTEX_HOLD_MS * 1_000_000;
    eprintln!(
        "a lock held {FUTEX_HOLD_MS} ms: thread {waiter} waited {measured} ns against \
         {expected_ns} ns injected, reported in the bucket starting at {bucket} ns"
    );
    assert!(
        close_to(measured, expected_ns),
        "the workload held the lock for {expected_ns} ns and the run reports a wait of \
         {measured} ns"
    );
    // And the wait can only be shorter than the hold, never longer: it begins
    // when the waiter asks, which is after the lock was taken.
    assert!(
        measured <= expected_ns + expected_ns / 10,
        "a thread waited {measured} ns for a lock held {expected_ns} ns"
    );
}

// --------------------------------------------------- sched.irq: ground truth

/// Every interrupt this run timed is one the kernel also counted.
///
/// `/proc/interrupts` is the ground truth, and it is a good one for the reason
/// `docs/contributing.md` asks for: the kernel maintains those counters itself,
/// in a different place and for a different reason, so they are not this tool
/// checking its own arithmetic. What this source adds is the only thing that
/// file does not have -- how long each handler ran.
///
/// The check is an inequality, deliberately. The tool attaches after the first
/// read of the file and detaches before the second, so what it counted is
/// strictly inside what the file counted, always and on any machine. An
/// equality would be an assertion about how quiet the machine was at the edges
/// of the window, which is exactly the shape of test findings 11 and 35 are
/// about.
///
/// Shared interrupt lines are left out and the reason is not incidental: the
/// tracepoint fires once per handler on a line, and `/proc/interrupts` counts
/// once per interrupt, so on a line with two handlers the tool legitimately
/// counts twice what the file does.
#[test]
fn every_interrupt_this_run_timed_is_one_the_kernel_also_counted() {
    let Some(before) = interrupt_counts() else {
        eprintln!(
            "skipped: /proc/interrupts is not readable, and it is this scenario's ground truth"
        );
        return;
    };
    let output = session("sched-irq-against-procfs.yml", &["--format", "json"]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!(
            "skipped: `sched.irq` needs CAP_BPF and CAP_PERFMON, which this machine does not grant"
        );
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let after = interrupt_counts().expect("/proc/interrupts was readable a moment ago");
    let results = results(&output);
    let result = labelled(&results, "handlers");

    let mut measured: std::collections::BTreeMap<u64, (u64, u64, String)> = Default::default();
    for event in raw_events(result) {
        let irq = event["irq"].as_u64().expect("an interrupt number");
        let entry = measured.entry(irq).or_insert((0, 0, String::new()));
        entry.0 += event["count"].as_u64().expect("a weight");
        entry.1 += event["total_ns"].as_u64().expect("an exact total");
        entry.2 = event["name"].as_str().expect("a handler name").to_owned();
    }
    assert!(
        !measured.is_empty(),
        "a machine with a disk and a network card takes interrupts, and this run timed none:\n{text}"
    );

    let mut compared = 0;
    for (irq, (count, total_ns, name)) in &measured {
        let Some(after) = after.get(irq) else {
            continue;
        };
        // A line several devices share counts once per interrupt in the file
        // and once per handler here, so the two are not comparable.
        if after.shared {
            continue;
        }
        let delta = after
            .count
            .saturating_sub(before.get(irq).map_or(0, |row| row.count));
        assert!(
            *count <= delta,
            "irq {irq} ({name}): this run timed {count} handler runs and the kernel's own \
             counter moved by {delta} over a window that contains this run entirely"
        );
        assert!(
            *total_ns > 0 || *count == 0,
            "irq {irq} ({name}): {count} handler runs took no time at all"
        );
        // The names have to be the same string, or `--filter name` selects
        // something other than what `/proc/interrupts` calls it.
        assert!(
            after.actions.contains(name),
            "irq {irq}: this run calls the handler `{name}` and /proc/interrupts calls it `{}`",
            after.actions
        );
        compared += 1;
    }
    assert!(
        compared > 0,
        "every interrupt this run timed was on a shared line, so nothing could be compared \
         against the kernel's own counter"
    );

    // And the busiest line has to be most of what the kernel saw, not a
    // fraction of it: an inequality alone would be satisfied by a collector
    // that reported one interrupt in a hundred.
    let (irq, (count, _, name)) = measured
        .iter()
        .filter(|(irq, _)| after.get(irq).is_some_and(|row| !row.shared))
        .max_by_key(|(_, (count, _, _))| *count)
        .expect("a busiest line among the comparable ones");
    let delta = after[irq]
        .count
        .saturating_sub(before.get(irq).map_or(0, |row| row.count));
    eprintln!(
        "irq {irq} ({name}): {count} handler runs timed against {delta} the kernel counted over \
         a window that contains this run"
    );
    assert!(
        count * 2 >= delta,
        "irq {irq} ({name}) fired {delta} times by the kernel's own count and this run timed \
         {count} of them, which is less than half"
    );
}

/// One interrupt line of `/proc/interrupts`.
struct InterruptLine {
    count: u64,
    /// The handler names at the end of the line, as one string.
    actions: String,
    /// Whether more than one device shares this line.
    shared: bool,
}

/// `/proc/interrupts`, summed across cpus, by interrupt number.
fn interrupt_counts() -> Option<std::collections::BTreeMap<u64, InterruptLine>> {
    let text = std::fs::read_to_string("/proc/interrupts").ok()?;
    let mut lines = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some((number, rest)) = line.split_once(':') else {
            continue;
        };
        let Ok(irq) = number.trim().parse::<u64>() else {
            // The named lines -- `NMI`, `LOC`, `RES` -- are not interrupt
            // numbers and no handler fires for them.
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let counts: Vec<u64> = fields
            .iter()
            .map_while(|field| field.parse::<u64>().ok())
            .collect();
        // What follows the per-cpu counts is the chip, the hardware interrupt,
        // the trigger and then the handler names, comma-separated.
        let tail = fields[counts.len()..].join(" ");
        let actions = tail
            .split_once("Edge")
            .or_else(|| tail.split_once("Level"))
            .or_else(|| tail.split_once("Fasteoi"))
            .map(|(_, actions)| actions.trim().to_owned())
            .unwrap_or(tail);
        lines.insert(
            irq,
            InterruptLine {
                count: counts.iter().sum(),
                shared: actions.contains(','),
                actions,
            },
        );
    }
    Some(lines)
}

// ------------------------------------ sched.numa_balance: the machine's limit

/// A machine with one NUMA node reports no balancing decisions, and says that
/// the empty series is the machine.
///
/// This is the one source in the wave whose property cannot be created here,
/// and the difference between a source that found nothing and a machine that
/// could never produce anything is exactly what a reader needs. So the test is
/// not a skip: it asserts that the tool says which of the two it is, with the
/// number that settles it.
#[test]
fn a_machine_with_one_numa_node_says_the_empty_series_is_the_machine() {
    let nodes = std::fs::read_dir("/sys/devices/system/node")
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .strip_prefix("node")
                        .is_some_and(|rest| rest.parse::<u32>().is_ok())
                })
                .count()
        })
        .unwrap_or(0);

    let output = session("sched-numa-balance.yml", &["--format", "json"]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: `sched.numa_balance` needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let results = results(&output);
    let result = labelled(&results, "decisions");
    let degradations: Vec<String> = result["degradations"]
        .as_array()
        .map(|notes| {
            notes
                .iter()
                .filter_map(|note| note.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    if nodes <= 1 {
        eprintln!(
            "this machine has {nodes} NUMA node, so the balancer has nothing to decide and this \
             source can produce no measurement here; what is checked is that the run says so"
        );
        assert_eq!(
            events_seen(result),
            0,
            "a machine with one NUMA node cannot produce a balancing decision, and this run \
             reported some:\n{text}"
        );
        assert!(
            degradations
                .iter()
                .any(|note| note.contains("NUMA node") && note.contains("never")),
            "an empty series from a machine that cannot produce one and an empty series from a \
             balancer with nothing to do look identical; the run has to say which: \
             {degradations:?}"
        );
    } else {
        // The claim only holds one way. On a machine with somewhere to move a
        // task to, the run must not tell a reader that the series is empty
        // because of the machine.
        assert!(
            !degradations
                .iter()
                .any(|note| note.contains("NUMA node") && note.contains("never")),
            "this machine has {nodes} NUMA nodes and the run claims it has nowhere to balance \
             to: {degradations:?}"
        );
        eprintln!(
            "{nodes} NUMA nodes: the balancer made {} decisions over this window",
            events_seen(result)
        );
    }
}

// ------------------------------------------------- the sweep and the refusals

/// The machine-wide half of the capability sweep.
///
/// Its companion is `every_emit_each_collected_source_declares_renders`, and
/// the two are separate files because one run observes one target: a session
/// mixing a launched program with the machine is refused before anything
/// starts.
#[test]
fn every_emit_the_machine_wide_sources_declare_renders() {
    let output = session("sched-sweep-machine.yml", &["--format", "json"]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: every source in this sweep needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let results = results(&output);

    for (label, source) in [
        ("lock-every-emit", "sched.lock"),
        ("irq-traced", "sched.irq"),
        ("numa-traced", "sched.numa_balance"),
    ] {
        let declared = declared_emits(source);
        let rendered = emits(labelled(&results, label));
        for emit in &declared {
            assert!(
                rendered.contains(emit),
                "`{source}` declares `{emit}` and the sweep did not render it; \
                 rendered {rendered:?}, declared {declared:?}"
            );
        }
    }

    // `count` narrows what any source may emit -- one measurement over one
    // window is not a distribution and not a ranking -- and the three it does
    // allow still have to render.
    for label in ["irq-counted", "numa-counted"] {
        let rendered = emits(labelled(&results, label));
        for emit in ["rate", "raw", "summary"] {
            assert!(
                rendered.contains(&emit.to_owned()),
                "`{label}` did not render `{emit}`: {rendered:?}"
            );
        }
    }
}

/// A source whose targets the catalog narrows has to refuse the others before
/// anything attaches, and say which the entry declares.
///
/// These are exit 2 -- a request that cannot mean anything -- rather than exit
/// 1 or 3, and the difference is the whole of class D: a reader who takes a
/// rejection for a machine limit goes looking for a kernel option, and one who
/// takes it for a missing collector goes looking for a newer build.
#[test]
fn the_targets_and_modes_these_entries_do_not_declare_are_rejected_by_name() {
    let cases = [
        // `sys_enter` fires on every syscall on the machine, so a machine-wide
        // futex histogram is mostly idle thread pools waiting on purpose. The
        // entry declares no system target and the validator says so.
        (
            vec![
                "sched.futex",
                "--system",
                "-m",
                "trace",
                "-e",
                "hist",
                "-d",
                "50ms",
            ],
            "no `system` support",
        ),
        // An interrupt handler belongs to no process.
        (
            vec![
                "sched.irq",
                "--pid",
                "1",
                "-m",
                "trace",
                "-e",
                "hist",
                "-d",
                "50ms",
            ],
            "no `pid` support",
        ),
        // A lock wait is a duration between two events; counting them would
        // report a number of waits under a unit of nanoseconds.
        (
            vec![
                "sched.lock",
                "--system",
                "-m",
                "count",
                "-e",
                "summary",
                "-d",
                "50ms",
            ],
            "cannot be observed in `count` mode",
        ),
        // The balancer's decisions are the machine's.
        (
            vec![
                "sched.numa_balance",
                "--pid",
                "1",
                "-m",
                "count",
                "-e",
                "summary",
                "-d",
                "50ms",
            ],
            "no `pid` support",
        ),
    ];
    for (args, expected) in cases {
        let output = omn(&args);
        let text = spoken(&output);
        assert_eq!(
            exit_code(&output),
            2,
            "`{}` asks for something the catalog does not offer, which is a rejection and not a \
             machine limit:\n{text}",
            args.join(" ")
        );
        assert!(
            text.contains(expected),
            "`{}` was rejected without saying `{expected}`:\n{text}",
            args.join(" ")
        );
    }
}

// ------------------------------------------- the queue wait inside a block

/// A block includes the wait for a cpu after the wakeup, and now says how much.
///
/// `sched.offcpu` times a block from the switch that took the task off the cpu
/// to the switch that gave it one back, so everything between the wakeup and
/// that second switch is run queue latency inside a number called blocked time
/// -- and `sched.runqlat` reports that same wait. The seventh wave's compute
/// crossing measured the double count: 100 sleeps of 5 ms behind a spinning
/// shell reported 582 ms off cpu of which 77 ms was queue wait, and any
/// decomposition adding the two sources counted those 77 ms twice.
///
/// What is asserted here is arithmetic and separation, not a threshold. The
/// machine decides how long a queue wait is; the tool decides whether it says
/// so, and whether its three numbers are consistent with each other.
#[test]
fn a_block_says_how_much_of_it_was_waiting_for_a_cpu() {
    if !have("python3") {
        eprintln!("skipped: no python3 to sleep with");
        return;
    }
    // One core, two spinners and a sleeper on it: the sleeper's wakeups then
    // have to queue behind work that is already runnable. `taskset` is the
    // contention, and it is created rather than waited for.
    let Some(core) = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .map(|_| "1".to_owned())
    else {
        eprintln!("skipped: no cpu to pin to");
        return;
    };
    if !have("taskset") {
        eprintln!("skipped: no taskset to create the contention with");
        return;
    }
    let mut spinners = Vec::new();
    for _ in 0..2 {
        spinners.push(
            Command::new("taskset")
                .args(["-c", &core, "python3", "-c", "\nwhile True: pass\n"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("a spinner starts"),
        );
    }

    let output = omn(&[
        "sched.offcpu",
        "-m",
        "trace",
        "-e",
        "raw",
        "-d",
        "2s",
        "--format",
        "json",
        "--",
        "taskset",
        "-c",
        &core,
        "python3",
        "-c",
        "\nimport time\nfor _ in range(100): time.sleep(0.005)\n",
    ]);
    for mut spinner in spinners {
        let _ = spinner.kill();
        let _ = spinner.wait();
    }

    if unprivileged(&spoken(&output)) {
        eprintln!("skipped: this machine does not grant CAP_BPF and CAP_PERFMON");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{}", spoken(&output));
    let results = results(&output);
    let result = &results[0];

    let total: u64 = total_attribute(result, "total_ns");
    let queued: u64 = total_attribute(result, "queued_ns");
    let blocked: u64 = total_attribute(result, "blocked_ns");
    assert!(total > 0, "the sleeper blocked at all: {}", spoken(&output));

    // 1. The three numbers are one measurement, not three: the split is
    //    arithmetic and cannot drift, whatever the machine did.
    assert_eq!(
        blocked + queued,
        total,
        "blocked + queued must be the whole block: {blocked} + {queued} != {total}"
    );
    // 2. The wait is inside the block, which is the whole claim. A queue wait
    //    larger than the block it is part of would mean the wakeup stamp
    //    belonged to a different block.
    assert!(
        queued <= total,
        "the queue wait cannot exceed the block it is inside: {queued} > {total}"
    );
    // 3. Under contention the wait is not nothing. This is the one assertion
    //    the machine has a say in, so it asserts only that the number exists at
    //    all -- two spinners on one core cannot produce a hundred instant
    //    wakeups, and if this machine somehow did, the separation above is
    //    still the part that matters.
    if queued == 0 {
        eprintln!(
            "skipped the contention half: this machine woke every sleep onto a free cpu \
             (total {total} ns over {} events)",
            events_seen(result)
        );
        return;
    }
    assert!(
        queued < total,
        "a sleep of 5 ms is not all queue wait: {queued} of {total}"
    );
}

/// A block whose wakeup this run did not see reports an unknown queue wait as
/// zero, and says how many blocks that is.
///
/// The distinction is the point: a zero that means "no wait" and a zero that
/// means "not seen" are different facts, and a decomposition subtracting the
/// second is subtracting a guess. A task already blocked when the programs
/// attach is the ordinary case -- it is every sleeping thread on the machine.
#[test]
fn a_block_whose_wakeup_was_not_seen_says_so_rather_than_reporting_no_wait() {
    let output = omn(&[
        "sched.offcpu",
        "--system",
        "-m",
        "trace",
        "-e",
        "raw",
        "-d",
        "500ms",
        "--format",
        "json",
    ]);
    if unprivileged(&spoken(&output)) {
        eprintln!("skipped: this machine does not grant CAP_BPF and CAP_PERFMON");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{}", spoken(&output));
    let results = results(&output);
    let result = &results[0];
    if events_seen(result) == 0 {
        eprintln!("skipped: nothing on this machine blocked in the window");
        return;
    }

    // Machine-wide over half a second, some thread was already asleep when the
    // programs attached and woke without this run seeing the wakeup. Where that
    // happened the run says so in words rather than reporting a wait of zero.
    let said = result["degradations"].to_string();
    let rows_with_no_wait = outputs(result, "raw")
        .flat_map(|output| output["events"].as_array().cloned().unwrap_or_default())
        .filter(|event| {
            event["attrs"]
                .as_array()
                .map(|attrs| {
                    attrs
                        .iter()
                        .any(|attr| attr["name"] == "queued_ns" && attr["value"] == 0)
                })
                .unwrap_or(false)
        })
        .count();
    if rows_with_no_wait > 0 {
        assert!(
            said.contains("without this run having seen the wakeup"),
            "{rows_with_no_wait} rows report no queue wait and nothing says whether that is a \
             measurement or a gap: {said}"
        );
    }
}

/// A bucketed source stamps each row where it happened, not where it was read.
///
/// These sources build their histograms in the kernel and emit one event per
/// row per drain. Stamped at the drain, every event of a window carried one
/// instant — the window's *end* — and a join that opened on such a source
/// opened there, so everything that happened in the same window as the first
/// episode fell outside the state it was meant to open. That is finding 44,
/// met first in `mem.reclaim` by the seventh wave's machine crossing and fixed
/// across every bucketed source in the eighth.
///
/// What is asserted is the shape and not a threshold: with a drain every
/// hundred milliseconds over a second, stamping at the drain gives about ten
/// distinct instants however busy the machine is, and stamping at each row's
/// own beginning gives about as many as there are rows. A source this machine
/// gives too few rows to tell the two apart skips with its own count.
#[test]
fn a_bucketed_source_does_not_stamp_every_row_of_a_window_with_one_instant() {
    for source in ["sched.runqlat", "sched.lock", "sched.irq"] {
        let output = omn(&[
            source, "--system", "-m", "trace", "-e", "raw", "-d", "1s", "--format", "json",
        ]);
        if unprivileged(&spoken(&output)) {
            eprintln!("skipped: this machine does not grant CAP_BPF and CAP_PERFMON");
            return;
        }
        assert_eq!(exit_code(&output), 0, "{source}: {}", spoken(&output));
        let results = results(&output);
        let result = &results[0];

        let stamps: Vec<u64> = outputs(result, "raw")
            .flat_map(|output| output["events"].as_array().cloned().unwrap_or_default())
            .filter_map(|event| event["ts_ns"].as_u64())
            .collect();
        if stamps.len() < 40 {
            eprintln!(
                "skipped {source}: {} events is too few to tell one instant per window from one \
                 per row",
                stamps.len()
            );
            continue;
        }
        let distinct: std::collections::BTreeSet<u64> = stamps.iter().copied().collect();
        assert!(
            distinct.len() > stamps.len() / 4,
            "{source}: {} events carry only {} distinct instants, which is one per drain rather \
             than one per row",
            stamps.len(),
            distinct.len()
        );
    }
}

// ===========================================================================
// The ninth wave: `sched.mutex`, through the preload shim
//
// The shim that interposed `malloc` interposes `pthread_mutex_lock` now, and
// every scenario here runs one C program whose property the command line
// chose: how many threads, how many acquisitions, how long each hold. The
// program also times its own waits and holds with the clock the tool uses and
// prints them, which gives each run a second reference the tool's interval is
// nested inside: the shim is *in* the call the program timed, so the tool's
// wait can only be shorter than the program's, and its hold only longer.
// ===========================================================================

/// Threads, acquisitions per thread and hold of
/// `examples/sched-mutex-known-hold.yml`.
const MUTEX_THREADS: u64 = 2;
const MUTEX_PER_THREAD: u64 = 500;
const MUTEX_HOLD_US: u64 = 200;

/// Acquisitions in `examples/sched-mutex-with-heap.yml`, over two threads.
const MUTEX_WITH_HEAP: u64 = 600;

/// Threads and acquisitions per thread of `examples/sched-mutex-loss.yml`.
const STORM_THREADS: u64 = 8;
const STORM_PER_THREAD: u64 = 200_000;

const MUTEX_FIXTURE_SOURCE: &str = r#"
/* BEGIN FIXTURE -- omn's mutex fixture. Threads contend one pthread mutex a
 * number of times the command line chose, holding it for a time the command
 * line chose, and the program times its own waits and holds with the clock
 * the tool uses. It reports on stderr, because stdout is where `omn` writes.
 *
 *   mutex <threads> <acquisitions per thread> <hold us> [shared|private]
 *
 * `shared`, the default, has every thread take one mutex. `private` gives each
 * thread its own, so nothing is contended and the only limit on the rate is
 * the program itself -- which is what the loss scenario needs. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static pthread_mutex_t shared_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_barrier_t start;
static long per_thread;
static unsigned long long hold_ns;

static unsigned long long now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (unsigned long long)t.tv_sec * 1000000000ull + (unsigned long long)t.tv_nsec;
}

struct tally { pthread_mutex_t *lock; unsigned long long wait_ns, hold_ns, min_hold_ns; long taken; };

static void *worker(void *argument) {
    struct tally *tally = argument;
    pthread_mutex_t *lock = tally->lock;
    tally->min_hold_ns = ~0ull;
    /* Every thread starts together, so the contention is the workload's and
     * not an accident of how long thread creation took. */
    pthread_barrier_wait(&start);
    for (long i = 0; i < per_thread; i++) {
        unsigned long long asked = now_ns();
        pthread_mutex_lock(lock);
        unsigned long long acquired = now_ns();
        /* A spin rather than a sleep: a sleep's timer slack would make the
         * hold whatever the kernel chose, and this is the number the tool is
         * checked against. */
        while (now_ns() - acquired < hold_ns) {
        }
        unsigned long long released = now_ns();
        pthread_mutex_unlock(lock);
        tally->wait_ns += acquired - asked;
        tally->hold_ns += released - acquired;
        if (released - acquired < tally->min_hold_ns)
            tally->min_hold_ns = released - acquired;
        tally->taken++;
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc != 4 && argc != 5) {
        fprintf(stderr, "usage: %s <threads> <acquisitions per thread> <hold us> [shared|private]\n",
                argv[0]);
        return 2;
    }
    int threads = atoi(argv[1]);
    per_thread = atol(argv[2]);
    hold_ns = strtoull(argv[3], NULL, 10) * 1000ull;
    if (threads < 1 || threads > 64 || per_thread < 1) {
        fprintf(stderr, "threads must be 1 to 64 and acquisitions at least 1\n");
        return 2;
    }
    int private = argc == 5 && strcmp(argv[4], "private") == 0;
    pthread_t ids[64];
    struct tally tallies[64];
    static pthread_mutex_t own[64];
    memset(tallies, 0, sizeof tallies);
    for (int t = 0; t < threads; t++) {
        pthread_mutex_init(&own[t], NULL);
        tallies[t].lock = private ? &own[t] : &shared_lock;
    }
    pthread_barrier_init(&start, NULL, (unsigned)threads);
    for (int t = 0; t < threads; t++)
        pthread_create(&ids[t], NULL, worker, &tallies[t]);
    unsigned long long wait = 0, hold = 0, min_hold = ~0ull;
    long taken = 0;
    for (int t = 0; t < threads; t++) {
        pthread_join(ids[t], NULL);
        wait += tallies[t].wait_ns;
        hold += tallies[t].hold_ns;
        taken += tallies[t].taken;
        if (tallies[t].min_hold_ns < min_hold)
            min_hold = tallies[t].min_hold_ns;
    }
    fprintf(stderr, "fixture: mutex=%llu private=%d threads=%d acquisitions=%ld wait_ns=%llu hold_ns=%llu min_hold_ns=%llu\n",
            (unsigned long long)(unsigned long)&shared_lock, private, threads, taken, wait, hold, min_hold);
    return 0;
}
/* END FIXTURE */
"#;

/// Where the fixture is built. The examples launch it by this path, relative
/// to the repository root, so it is not `CARGO_TARGET_TMPDIR`.
fn mutex_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/omn-shim-fixture/mutex")
}

/// Builds the fixture once per test process, or says why it cannot.
fn mutex_fixture() -> Result<PathBuf, String> {
    static BUILT: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let path = mutex_fixture_path();
            let dir = path.parent().expect("a directory");
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
            let source = dir.join("mutex.c");
            std::fs::write(&source, MUTEX_FIXTURE_SOURCE).map_err(|error| error.to_string())?;
            // Built beside and renamed, so a half-written binary is never at
            // the path the examples launch.
            let partial = dir.join(format!("mutex.{}", std::process::id()));
            let built = Command::new("cc")
                .args(["-O1", "-pthread", "-o"])
                .arg(&partial)
                .arg(&source)
                .output()
                .map_err(|error| {
                    format!("no C compiler (`cc`) to build the fixture with: {error}")
                })?;
            if !built.status.success() {
                return Err(format!(
                    "the mutex fixture did not build:\n{}",
                    String::from_utf8_lossy(&built.stderr)
                ));
            }
            std::fs::rename(&partial, &path).map_err(|error| error.to_string())?;
            Ok(path)
        })
        .clone()
}

macro_rules! needs_mutex_fixture {
    () => {
        if let Err(reason) = mutex_fixture() {
            eprintln!("skipped: {reason}");
            return;
        }
    };
}

/// The `fixture: key=value ...` line the fixture printed on stderr, which the
/// launched program shares with the tool.
fn fixture_said(output: &Output) -> std::collections::BTreeMap<String, u64> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("fixture: "))
        .next_back()
        .unwrap_or_else(|| panic!("the fixture printed nothing:\n{stderr}"))
        .split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.parse().expect("a number")))
        .collect()
}

/// One unsigned attribute of every raw event.
fn raw_uints(result: &Value, name: &str) -> Vec<u64> {
    raw_events(result)
        .iter()
        .map(|event| {
            event
                .get(name)
                .and_then(Value::as_u64)
                .unwrap_or_else(|| panic!("an event without `{name}`: {event:?}"))
        })
        .collect()
}

/// Two threads, one mutex, 500 acquisitions each held 200 us.
///
/// Two things are known independently of the tool, and each is checked:
///
/// - **the count**, exactly: a thousand acquisitions of one mutex, and no
///   other mutex in the program;
/// - **every hold at least 200 us**, because the fixture spins until it is.
///
/// And against the program's own clock, both ways: the tool's total wait is at
/// most the program's, and its total hold is at least the program's and not
/// much more. That the waits are *long* is the next test's separation, against
/// the same program with one thread.
///
/// What is deliberately not asserted is a floor on the waits. The argument for
/// one looks sound -- the threads start together, so whichever finishes last
/// was waiting through every hold of the other, 100 ms at least -- and it held
/// on an idle machine, where one thread waited once for 101 ms. Inside the
/// full suite it did not: 54 ms, and the fixture's own clock said 55 ms. A
/// thread that is runnable with no cpu to run on, before its first lock or
/// between an unlock and the next, is neither waiting nor holding, and on a
/// loaded machine that is where the other 46 ms went. The tool and the
/// program agreed; the expectation was about the scheduler.
#[test]
fn every_acquisition_of_a_contended_mutex_is_one_event_with_its_wait_and_hold() {
    needs_mutex_fixture!();
    let output = session("sched-mutex-known-hold.yml", &["--format", "json"]);
    let text = spoken(&output);
    assert_eq!(exit_code(&output), 0, "{text}");
    let said = fixture_said(&output);
    let results = results(&output);
    let result = labelled(&results, "contended");
    let acquisitions = MUTEX_THREADS * MUTEX_PER_THREAD;
    let hold_ns = MUTEX_HOLD_US * 1_000;

    assert_eq!(
        said["acquisitions"], acquisitions,
        "the fixture did its part"
    );
    assert_eq!(
        dropped(result),
        0,
        "a thousand records fit the pipe:\n{text}"
    );
    let addresses = raw_uints(result, "addr");
    assert_eq!(
        addresses.len() as u64,
        acquisitions,
        "the program took its mutex {acquisitions} times and the run reports {} acquisitions",
        addresses.len()
    );
    assert!(
        addresses.iter().all(|&address| address == said["mutex"]),
        "the program has one mutex, at {:#x}, and the run reports others: {:?}",
        said["mutex"],
        addresses
            .iter()
            .filter(|&&address| address != said["mutex"])
            .take(5)
            .collect::<Vec<_>>()
    );

    let holds = raw_uints(result, "hold_ns");
    let shortest = *holds.iter().min().expect("a hold");
    assert!(
        shortest >= hold_ns,
        "every hold is at least {hold_ns} ns by construction, and one is reported as {shortest} ns"
    );
    let waits = raw_uints(result, "wait_ns");
    let (waited, held): (u64, u64) = (waits.iter().sum(), holds.iter().sum());
    eprintln!(
        "{acquisitions} acquisitions: the tool saw {waited} ns waiting and {held} ns holding; \
         the program timed {} ns and {} ns",
        said["wait_ns"], said["hold_ns"]
    );

    assert!(
        waited <= said["wait_ns"],
        "the shim times inside the call the program timed, so its wait ({waited} ns) cannot \
         exceed the program's ({} ns)",
        said["wait_ns"]
    );
    assert!(
        held >= said["hold_ns"] && held <= said["hold_ns"] + said["hold_ns"] / 10,
        "the shim's holds enclose the program's by a few instructions each: {held} ns against \
         the program's {} ns",
        said["hold_ns"]
    );
}

/// The same thousand holds of 200 us with one thread: the holds are the same,
/// the waits vanish.
///
/// This is the separation `sched.futex` cannot make, because an uncontended
/// mutex makes no syscall: the futex source sees nothing for this program, and
/// this one sees every acquisition with a wait of tens of nanoseconds.
#[test]
fn a_mutex_nobody_else_wants_is_held_as_long_and_waited_for_not_at_all() {
    needs_mutex_fixture!();
    let alone = session("sched-mutex-uncontended.yml", &["--format", "json"]);
    assert_eq!(exit_code(&alone), 0, "{}", spoken(&alone));
    let contended = session("sched-mutex-known-hold.yml", &["--format", "json"]);
    assert_eq!(exit_code(&contended), 0, "{}", spoken(&contended));

    let alone_said = fixture_said(&alone);
    let alone_results = results(&alone);
    let alone_result = labelled(&alone_results, "alone");
    let contended_results = results(&contended);
    let contended_result = labelled(&contended_results, "contended");

    let acquisitions = MUTEX_THREADS * MUTEX_PER_THREAD;
    let alone_holds = raw_uints(alone_result, "hold_ns");
    assert_eq!(alone_holds.len() as u64, acquisitions);
    assert!(alone_holds
        .iter()
        .all(|&hold| hold >= MUTEX_HOLD_US * 1_000));

    let alone_waited: u64 = raw_uints(alone_result, "wait_ns").iter().sum();
    let contended_waited: u64 = raw_uints(contended_result, "wait_ns").iter().sum();
    eprintln!(
        "{acquisitions} acquisitions of a 200 us hold: {alone_waited} ns of waiting alone, \
         {contended_waited} ns with a second thread"
    );
    assert!(
        alone_waited <= alone_said["wait_ns"],
        "nested inside the program's own timing: {alone_waited} ns against {} ns",
        alone_said["wait_ns"]
    );
    assert!(
        alone_waited.saturating_mul(100) < contended_waited,
        "one thread alone waits for nothing and two wait for each other's holds; the run \
         reports {alone_waited} ns against {contended_waited} ns, which is not a separation"
    );
}

/// A mutex source and a heap source on one launch read one pipe, and neither
/// decodes the other's records as its own.
#[test]
fn locks_and_allocations_of_one_program_are_read_from_one_pipe() {
    needs_mutex_fixture!();
    let output = session("sched-mutex-with-heap.yml", &["--format", "json"]);
    let text = spoken(&output);
    assert_eq!(exit_code(&output), 0, "{text}");
    assert!(
        !text.contains("disagree"),
        "two preload sources of one launch must share the program's environment:\n{text}"
    );
    let results = results(&output);
    let locks = labelled(&results, "locks");
    let heap = labelled(&results, "heap");
    assert_eq!(
        events_seen(locks),
        MUTEX_WITH_HEAP,
        "the program took its mutex {MUTEX_WITH_HEAP} times:\n{text}"
    );
    for (label, result) in [("locks", locks), ("heap", heap)] {
        assert_eq!(
            result["losses"]["undecoded"].as_u64(),
            Some(0),
            "`{label}` failed to decode a record another source on the pipe wrote:\n{text}"
        );
        assert_eq!(dropped(result), 0, "`{label}` lost records:\n{text}");
    }
    assert!(
        events_seen(heap) > 0,
        "the fixture allocates its thread stacks' bookkeeping, and the heap source saw none:\n{text}"
    );
}

/// More records than the pipe can hold, and every one of them accounted for.
///
/// The shim drops rather than blocking the program, and counts what it drops;
/// so however much is lost, kept plus dropped is exactly what the program did.
#[test]
fn a_mutex_storm_the_pipe_cannot_carry_is_counted_to_the_last_record() {
    needs_mutex_fixture!();
    let output = session("sched-mutex-loss.yml", &["--format", "json"]);
    let text = spoken(&output);
    assert_eq!(exit_code(&output), 0, "{text}");
    let said = fixture_said(&output);
    let results = results(&output);
    let result = labelled(&results, "storm");
    let expected = STORM_THREADS * STORM_PER_THREAD;
    assert_eq!(said["acquisitions"], expected, "the fixture did its part");
    let (kept, lost) = (events_seen(result), dropped(result));
    eprintln!("{expected} acquisitions: {kept} kept, {lost} dropped and counted");
    if lost == 0 {
        eprintln!(
            "note: the pipe kept up with {expected} acquisitions on this machine, so the loss \
             path was not exercised this run; the count still has to be exact"
        );
    }
    assert_eq!(
        kept + lost,
        expected,
        "kept and dropped must add up to what the program did: {kept} + {lost} against \
         {expected}\n{text}"
    );
}

/// The two requests the preload backend cannot serve, each refused with the
/// reason rather than answered with nothing.
#[test]
fn a_mutex_request_the_shim_cannot_serve_is_refused_by_name() {
    // A process already running: a preloaded library is given at exec, so
    // the shim is passed over for it by name, and since the tenth wave glibc's
    // probes serve it instead of the run being refused.
    let running = omn(&[
        "--dump-request",
        "sched.mutex",
        "--mode",
        "trace",
        "--emit",
        "summary",
        "--pid",
        &std::process::id().to_string(),
    ]);
    let text = spoken(&running);
    if !unprivileged(&text) {
        assert!(
            text.contains("backend   bpf:usdt"),
            "a running process is read through glibc's probes:\n{text}"
        );
    }

    // Stacks, which the uprobe backend could take and the shim cannot.
    let stacks = omn(&[
        "sched.mutex",
        "--mode",
        "trace",
        "--emit",
        "stack",
        "--",
        "/bin/true",
    ]);
    let text = spoken(&stacks);
    assert_eq!(exit_code(&stacks), 3, "{text}");
    assert!(
        text.contains("carries no stacks") && text.contains("sched.futex"),
        "the refusal must say what does carry them:\n{text}"
    );
}

// ===========================================================================
// The tenth wave: `sched.mutex` over glibc's USDT probes
//
// The preload shim reaches a program only at exec. A process already running is
// read through `libc:mutex_entry`, `mutex_acquired` and `mutex_release`, and
// these scenarios hold the same fixture to the same property from outside: a
// hold the fixture chose, measured by a run that attached after it started.
// ===========================================================================

/// The attribute `name` of a raw event, as a number.
fn raw_number(event: &Value, name: &str) -> Option<u64> {
    event["attrs"]
        .as_array()?
        .iter()
        .find(|attr| attr["name"] == name)
        .and_then(|attr| attr["value"].as_u64())
}

/// A running fixture, killed when dropped.
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Every acquisition of a process already running is measured from outside,
/// and each hold is at least the hold the fixture chose: the release probe
/// fires after the fixture's own sleep, so the tool's hold nests around it.
#[test]
fn a_running_process_is_measured_through_glibcs_probes() {
    needs_mutex_fixture!();
    const HOLD_US: u64 = 2_000;
    let child = Command::new(mutex_fixture_path())
        .args(["2", "100000", &HOLD_US.to_string(), "shared"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the fixture starts");
    let running = Running(child);
    // Long enough for both threads to be inside their loop before the probes
    // go in, so what is measured is the steady state.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let pid = running.0.id().to_string();
    let output = omn(&[
        "sched.mutex",
        "--pid",
        &pid,
        "-d",
        "600ms",
        "-e",
        "raw",
        "--format",
        "json",
    ]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: `sched.mutex` over USDT needs CAP_BPF and CAP_PERFMON");
        return;
    }
    if text.contains("is not among the probes this process carries") {
        eprintln!("skipped: this machine's glibc publishes no mutex probes:\n{text}");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let result = &results(&output)[0];
    assert_eq!(
        result["backend"], "bpf:usdt",
        "a pid target is not the shim's: {text}"
    );
    let events = result["outputs"][0]["events"]
        .as_array()
        .expect("a raw emit carries its events");
    let unlocked: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["attrs"]
                .as_array()
                .is_some_and(|attrs| attrs.iter().any(|attr| attr["value"] == "unlock"))
        })
        .collect();
    // Two threads taking turns on a 2 ms hold for 600 ms is about three
    // hundred acquisitions; a tenth of that is the separation from nothing.
    assert!(
        unlocked.len() >= 30,
        "{} acquisitions ended in an unlock over 600 ms of a 2 ms hold:\n{text}",
        unlocked.len()
    );
    for event in &unlocked {
        let hold = raw_number(event, "hold_ns").expect("an unlocked hold has a length");
        assert!(
            hold >= HOLD_US * 1000,
            "a hold of {hold} ns is shorter than the {HOLD_US} us the fixture slept inside it"
        );
    }
    // And the two threads contend: one of them waited for most of a hold.
    let longest_wait = unlocked
        .iter()
        .filter_map(|event| raw_number(event, "wait_ns"))
        .max()
        .unwrap_or(0);
    assert!(
        longest_wait >= HOLD_US * 1000 / 2,
        "two threads sharing a {HOLD_US} us hold never waited half of one: the longest \
         wait was {longest_wait} ns"
    );
}

/// The shim still serves a launch, where it is the cheaper of the two, and the
/// probes serve a pid, which the shim cannot.
#[test]
fn a_launch_goes_to_the_shim_and_a_pid_to_the_probes() {
    let launched = omn(&[
        "--dump-request",
        "sched.mutex",
        "-e",
        "summary",
        "--",
        "true",
    ]);
    let pid = omn(&[
        "--dump-request",
        "sched.mutex",
        "-e",
        "summary",
        "--pid",
        "1",
    ]);
    let (launched, pid) = (spoken(&launched), spoken(&pid));
    if unprivileged(&pid) {
        eprintln!("skipped: the USDT backend needs CAP_BPF and CAP_PERFMON");
        return;
    }
    assert!(
        launched.contains("preload"),
        "a launch resolves onto the shim:\n{launched}"
    );
    assert!(
        pid.contains("bpf:usdt"),
        "a pid resolves onto glibc's probes:\n{pid}"
    );
}

/// The probe program hands up a mutex and a thread, not a stack, and says so.
#[test]
fn a_stack_over_the_probes_is_refused_by_name() {
    let output = omn(&[
        "sched.mutex",
        "--pid",
        &std::process::id().to_string(),
        "-e",
        "stack",
        "-d",
        "100ms",
    ]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: the USDT backend needs CAP_BPF and CAP_PERFMON");
        return;
    }
    assert_ne!(exit_code(&output), 0, "{text}");
    assert!(text.contains("carries no stacks"), "{text}");
}

// ------------------------------------------------- drains that lose nothing

/// A program that counts its own syscalls, so the tool's total can be checked
/// against a number the tool had no part in.
///
/// Eight threads call `getppid` -- which glibc does not cache and nothing else
/// in this program makes -- as fast as they can for a fixed wall time, each
/// counting its own calls, and the program prints the sum when every thread has
/// joined. As fast as they can because the defect this is written against is a
/// race between the program's increment and the drain's take: the more often
/// the exit half is between its lookup and its add when a drain walks the map,
/// the more of them are lost. Over a second and a half on a 100 ms tick, that
/// is fifteen drains against rows being written by eight cpus at once.
const DRAIN_FIXTURE_SOURCE: &str = r#"
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define THREADS 8

static double run_for;
static unsigned long long calls[THREADS];

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void *hammer(void *slot) {
    unsigned long long *mine = slot;
    double until = now() + run_for;
    while (now() < until) {
        for (int i = 0; i < 1000; i++) {
            syscall(SYS_getppid);
        }
        *mine += 1000;
    }
    return NULL;
}

int main(int argc, char **argv) {
    run_for = argc > 1 ? atof(argv[1]) : 1.5;
    pthread_t threads[THREADS];
    for (int i = 0; i < THREADS; i++) {
        pthread_create(&threads[i], NULL, hammer, &calls[i]);
    }
    unsigned long long total = 0;
    for (int i = 0; i < THREADS; i++) {
        pthread_join(threads[i], NULL);
        total += calls[i];
    }
    fprintf(stderr, "fixture: calls=%llu threads=%d\n", total, THREADS);
    return 0;
}
"#;

/// Builds a C fixture into `target/omn-drains/`, beside and renamed so a
/// half-written binary is never at the path a test launches.
fn build_c(name: &str, source: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/omn-drains")
        .join(name);
    let dir = path.parent().expect("a directory");
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let source_path = dir.join(format!("{name}.c"));
    std::fs::write(&source_path, source).map_err(|error| error.to_string())?;
    let partial = dir.join(format!("{name}.{}", std::process::id()));
    let built = Command::new("cc")
        .args(["-O1", "-fno-omit-frame-pointer", "-pthread", "-o"])
        .arg(&partial)
        .arg(&source_path)
        .output()
        .map_err(|error| format!("no C compiler (`cc`) to build the fixture with: {error}"))?;
    if !built.status.success() {
        return Err(format!(
            "the `{name}` fixture did not build:\n{}",
            String::from_utf8_lossy(&built.stderr)
        ));
    }
    std::fs::rename(&partial, &path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn drain_fixture() -> Result<PathBuf, String> {
    static BUILT: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_c("getppid", DRAIN_FIXTURE_SOURCE))
        .clone()
}

/// Every call the program made is in a row or in a counted loss, over several
/// runs -- the drain never takes a row out from under the increment that was
/// about to land in it.
///
/// The eighth wave measured the race on the fentry histograms: a hundred
/// thousand reads, 55192 counted, 44807 gone. `proc.syscall` and every other
/// drained program in `sched` and `mem` had the same shape; they now share the
/// two-map drain of `bpf::map::Rows`. Against the single-map drain this test
/// fails -- the numbers are in `docs/families/drains.md`.
///
/// Exact, not close. The program's count is its own, the filter is pushed into
/// the kernel, and a call that is neither in a row nor in `losses` is the
/// defect. A loss the tool counted is allowed, and printed, because it is the
/// invariant being kept rather than broken -- but a run that lost nothing is
/// what a clean drain should give, and the spread over the runs is printed so a
/// reader can see which one this machine gave.
#[test]
fn every_call_a_program_made_is_in_a_row_or_a_counted_loss_on_a_short_tick() {
    const RUNS: usize = 3;
    let fixture = match drain_fixture() {
        Ok(path) => path,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let fixture = fixture.to_string_lossy().into_owned();
    for run in 0..RUNS {
        let output = omn(&[
            "proc.syscall",
            "--mode",
            "trace",
            "--filter",
            "syscall == 'getppid'",
            "--emit",
            "raw",
            "--format",
            "json",
            "-d",
            "5s",
            "--",
            &fixture,
            "1.5",
        ]);
        let text = spoken(&output);
        if unprivileged(&text) {
            eprintln!("skipped: proc.syscall needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
            return;
        }
        assert_eq!(exit_code(&output), 0, "{text}");
        let made = fixture_said(&output)["calls"];
        let results = results(&output);
        let result = &results[0];
        let counted = total_attribute(result, "count");
        let losses = &result["losses"];
        let lost: u64 = ["dropped", "undecoded", "throttled"]
            .iter()
            .filter_map(|field| losses[*field].as_u64())
            .sum();
        eprintln!("run {run}: made {made}, counted {counted}, losses {losses}");
        assert!(
            counted <= made,
            "run {run}: {counted} calls counted for {made} made -- an increment landed in \
             another row's element; losses {losses}"
        );
        assert!(
            counted + lost >= made,
            "run {run}: the program made {made} calls, {counted} are in rows and {lost} are \
             counted as lost: {} vanished without a trace; losses {losses}",
            made - counted - lost
        );
    }
}

// ---------------------------------------- a blocking stack on a loaded machine

/// A program that blocks from a different user stack almost every time: a
/// random walk twenty calls deep through two mutually recursive functions,
/// then a 20 us sleep at the leaf, on eight threads. Tens of thousands of
/// distinct stacks a second -- what a machine running a test suite, a build and
/// three other agents looks like to a machine-wide stack walk.
const STACK_NOISE_SOURCE: &str = r#"
#include <pthread.h>
#include <stdlib.h>
#include <unistd.h>

static __attribute__((noinline)) void b(unsigned *seed, int depth);
static __attribute__((noinline)) void a(unsigned *seed, int depth) {
    if (depth == 0) { usleep(20); return; }
    if (rand_r(seed) & 1) a(seed, depth - 1); else b(seed, depth - 1);
    __asm__ volatile("" ::: "memory");
}
static __attribute__((noinline)) void b(unsigned *seed, int depth) {
    if (depth == 0) { usleep(20); return; }
    if (rand_r(seed) & 1) a(seed, depth - 1); else b(seed, depth - 1);
    __asm__ volatile("" ::: "memory");
}
static void *run(void *arg) {
    unsigned seed = (unsigned)(long)arg;
    for (;;) a(&seed, 20);
    return 0;
}
int main(int argc, char **argv) {
    pthread_t thread;
    for (long i = 1; i <= 8; i++) pthread_create(&thread, 0, run, (void *)i);
    sleep(argc > 1 ? atoi(argv[1]) : 10);
    return 0;
}
"#;

/// The target's blocking stack is named while another process floods the
/// kernel's stack map -- which is what `a_blocking_stack_names_the_call_that_blocked`
/// was failing on in full runs.
///
/// The stack map holds one stack per bucket, 4096 of them, and refuses a
/// second stack in a bucket rather than replacing the first. The program
/// walked the stack of every task on the machine that blocked and narrowed to
/// the target afterwards, so on a loaded machine the map was full of other
/// processes' stacks within a few hundred milliseconds, and the target's block
/// came back with none -- reported as "no user stack to walk", which was not
/// the reason either. It passed alone and failed in two full runs of three,
/// depending on whether the target blocked before the rest of the machine had
/// filled the map.
///
/// Here the rest of the machine is a program this test controls, and the
/// target waits 400 ms before it blocks in earnest -- long after the noise has
/// filled every bucket. The noise is a separate process, so a narrowed run
/// must not walk its stacks at all: the target's stack has to be named, and no
/// block of it may be reported as crowded out.
#[test]
fn a_targets_blocking_stack_is_named_while_another_process_floods_the_stack_map() {
    const SLEEPER: &str = "import sys, time\nprint('ready', flush=True)\ntime.sleep(0.4)\n\
                           for _ in range(10): time.sleep(0.05)";
    if !have("python3") {
        eprintln!(
            "skipped: this machine has no `python3`, which the sleeping workload is written in"
        );
        return;
    }
    static BUILT: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    let noise = match BUILT.get_or_init(|| build_c("stack-noise", STACK_NOISE_SOURCE)) {
        Ok(path) => path.clone(),
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let mut noise = Command::new(noise)
        .arg("10")
        .spawn()
        .expect("the stack noise should start");

    let mut sleeper = Command::new("python3")
        .args(["-c", SLEEPER])
        .stdout(Stdio::piped())
        .spawn()
        .expect("the sleeping workload should start");
    let mut ready = String::new();
    std::io::BufReader::new(sleeper.stdout.take().expect("a piped stdout"))
        .read_line(&mut ready)
        .expect("the workload announces itself before it sleeps");
    let pid = sleeper.id().to_string();
    let output = omn(&["sched.offcpu", "--pid", &pid, "--emit", "stack", "-d", "1s"]);
    let _ = sleeper.wait();
    let _ = noise.kill();
    let _ = noise.wait();

    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: sched.offcpu needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    assert!(
        text.contains("clock_nanosleep"),
        "the workload blocked in `clock_nanosleep` while another process flooded the stack \
         map, and its stack does not say so:\n{text}"
    );
    assert!(
        !text.contains("their stack not kept"),
        "a narrowed run lost the target's stacks to another process's:\n{text}"
    );
}

// ================================================= finding 48: a waker is a task

/// The half-blocked pair of `examples/sched-half-blocked.yml`, with the two
/// pids said by the workload itself on stderr rather than inferred from the
/// wakeups: the driver is the launched shell (`$$`), the responder the
/// subshell it put in the background (`$!`).
const PAIR_SCRIPT: &str = r#"D=$(mktemp -d); mkfifo "$D/a" "$D/b"
{ exec 4<"$D/a" 5>"$D/b"
  while read -r _ <&4; do
    j=0; while [ $j -lt 100 ]; do j=$((j + 1)); done
    echo y >&5
  done; } &
echo "omn-pair driver=$$ responder=$!" >&2
exec 3>"$D/a" 6<"$D/b"
i=0
while [ $i -lt 600 ]; do
  j=0; while [ $j -lt 100 ]; do j=$((j + 1)); done
  echo x >&3; read -r _ <&6
  i=$((i + 1))
done
exec 3>&- 6<&-; rm -rf "$D""#;

/// The cpu the pair and the load share: the last one this test may use.
fn shared_cpu() -> usize {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::cpu_set_t>();
    assert_eq!(unsafe { libc::sched_getaffinity(0, size, &mut set) }, 0);
    (0..libc::CPU_SETSIZE as usize)
        .rev()
        .find(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
        .expect("a cpu this test may run on")
}

fn pin_to(cpu: usize) {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_SET(cpu, &mut set) };
    let size = std::mem::size_of::<libc::cpu_set_t>();
    assert_eq!(unsafe { libc::sched_setaffinity(0, size, &set) }, 0);
}

/// Interrupt load on one cpu for as long as `running` holds, and the tids of
/// the threads it runs: the load finding 48 was reproduced with, made to land
/// where the pair is. A thread sleeping 50 us at a time is woken by its
/// hrtimer's hard interrupt on its own cpu; a direct read is completed by the
/// disk's interrupt, which a per-cpu queue delivers to the reading cpu; a
/// loopback datagram is delivered by the network softirq. None of these
/// threads is ever woken by the pair -- they share nothing with it but a cpu.
fn interrupt_load(
    cpu: usize,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> (Vec<std::thread::JoinHandle<()>>, Vec<u64>) {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::Ordering;
    let (tids, tid_of) = std::sync::mpsc::channel::<u64>();
    let mut threads = Vec::new();

    let receiver = std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback socket");
    receiver
        .set_read_timeout(Some(std::time::Duration::from_millis(50)))
        .expect("a timeout");
    let address = receiver.local_addr().expect("an address");
    let (flag, said) = (running.clone(), tids.clone());
    threads.push(std::thread::spawn(move || {
        said.send(unsafe { libc::gettid() } as u64).unwrap();
        let mut buffer = [0u8; 64];
        while flag.load(Ordering::Relaxed) {
            let _ = receiver.recv(&mut buffer);
        }
    }));
    let (flag, said) = (running.clone(), tids.clone());
    threads.push(std::thread::spawn(move || {
        pin_to(cpu);
        said.send(unsafe { libc::gettid() } as u64).unwrap();
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback socket");
        while flag.load(Ordering::Relaxed) {
            let _ = sender.send_to(&[7u8; 64], address);
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
    }));

    // A direct read needs a real block device under it, which the target
    // directory has and /tmp may not.
    let path = scratch("finding-48-direct");
    std::fs::write(&path, vec![0u8; 1 << 20]).expect("a file to read back");
    let flag = running;
    threads.push(std::thread::spawn(move || {
        pin_to(cpu);
        tids.send(unsafe { libc::gettid() } as u64).unwrap();
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT)
            .open(&path)
        else {
            return;
        };
        let layout = std::alloc::Layout::from_size_align(4096, 4096).expect("a page");
        let page = unsafe { std::alloc::alloc(layout) };
        let mut offset = 0i64;
        while flag.load(Ordering::Relaxed) {
            use std::os::fd::AsRawFd;
            let read = unsafe { libc::pread(file.as_raw_fd(), page.cast(), 4096, offset) };
            if read < 0 {
                break;
            }
            offset = (offset + 4096) % (1 << 20);
        }
        unsafe { std::alloc::dealloc(page, layout) };
    }));
    let load = (0..threads.len()).map(|_| tid_of.recv().unwrap()).collect();
    (threads, load)
}

/// Finding 48, against its reproduction. The half-blocked pair runs pinned to
/// one cpu beside a load pinned to the same cpu that raises interrupts, and a
/// wakeup made from one of them used to be credited to whichever task it
/// landed on. Measured once with the preempt count left unread: 1954 wakeups
/// of the load's threads and 15 310 of third parties in all (a kworker's
/// 12 417 among them) credited to the driver in one run; read, 0 and 10.
///
/// What the workload knows about itself: the driver wakes the responder once
/// per round trip and the responder wakes the driver once, 600 each; and the
/// load's threads share nothing with the pair but the cpu, so the driver never
/// wakes one of them. Every wakeup of a load thread credited to the driver is
/// an interrupt it was only the landing place of, and over several runs there
/// is none, while the pair keeps its 600 each and every wakeup the program saw
/// from an interrupt names no task as its waker and was kept as the target's
/// wakee.
#[test]
fn a_wakeup_from_an_interrupt_is_not_credited_to_the_task_it_landed_on() {
    const RUNS: usize = 3;
    if !have("taskset") {
        eprintln!("skipped: the pair is pinned with `taskset`, which this machine does not have");
        return;
    }
    let cpu = shared_cpu();
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (load, load_tids) = interrupt_load(cpu, running.clone());
    let cpu_list = cpu.to_string();
    let mut seen = Vec::new();
    for run in 0..RUNS {
        let output = omn(&[
            "sched.wakeup",
            "-m",
            "trace",
            "-e",
            "raw",
            "--format",
            "json",
            "--",
            "taskset",
            "-c",
            &cpu_list,
            "/bin/sh",
            "-c",
            PAIR_SCRIPT,
        ]);
        let text = spoken(&output);
        if unprivileged(&text) {
            running.store(false, std::sync::atomic::Ordering::Relaxed);
            eprintln!("skipped: sched.wakeup needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
            return;
        }
        assert_eq!(exit_code(&output), 0, "{text}");
        let said = text
            .lines()
            .find_map(|line| line.strip_prefix("omn-pair "))
            .unwrap_or_else(|| panic!("the workload says its pair:\n{text}"));
        let pid = |name: &str| -> u64 {
            said.split_whitespace()
                .find_map(|field| field.strip_prefix(&format!("{name}=")))
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("`{name}` in `{said}`"))
        };
        let (driver, responder) = (pid("driver"), pid("responder"));

        let results = results(&output);
        let events = raw_events(&results[0]);
        let number = |event: &std::collections::BTreeMap<String, Value>, name: &str| {
            event[name]
                .as_u64()
                .unwrap_or_else(|| panic!("`{name}` in {event:?}"))
        };
        let (mut to_responder, mut to_driver, mut to_the_load) = (0u64, 0u64, 0u64);
        let (mut third_parties, mut from_interrupts) = (0u64, 0u64);
        let mut strangers = std::collections::BTreeMap::<String, u64>::new();
        for event in &events {
            let (waker, wakee) = (number(event, "waker_pid"), number(event, "wakee_pid"));
            if event["waker_context"] != "task" {
                from_interrupts += 1;
                assert_eq!(
                    (number(event, "waker_tid"), waker),
                    (0, 0),
                    "an interrupt's wakeup names no task as its waker: {event:?}"
                );
                assert_eq!(
                    wakee, driver,
                    "an interrupt's wakeup is kept only as the target's wakee: {event:?}"
                );
                continue;
            }
            if waker == driver && wakee == responder {
                to_responder += 1;
            } else if waker == responder && wakee == driver {
                to_driver += 1;
            } else if waker == driver && wakee != driver {
                third_parties += 1;
                if load_tids.contains(&number(event, "wakee_tid")) {
                    to_the_load += 1;
                }
                let name = format!("{wakee}:{}", event["comm"].as_str().unwrap_or("?"));
                *strangers.entry(name).or_default() += 1;
            }
        }
        seen.push(format!(
            "run {run}: {to_responder} to the responder, {to_driver} to the driver, \
             {to_the_load} to the load, {third_parties} to third parties in all \
             {strangers:?}, {from_interrupts} from interrupts"
        ));
        assert!(
            close_to(to_responder, HALF_BLOCKED_ROUND_TRIPS)
                && close_to(to_driver, HALF_BLOCKED_ROUND_TRIPS),
            "{HALF_BLOCKED_ROUND_TRIPS} round trips wake each of the pair once: {seen:?}"
        );
        assert_eq!(
            to_the_load, 0,
            "the load shares a cpu with the driver and nothing else; a wakeup of it credited \
             to the driver is an interrupt the driver was only the landing place of: {seen:?}"
        );
    }
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    for thread in load {
        let _ = thread.join();
    }
    eprintln!("{}", seen.join("\n"));
}

/// The other half of finding 48: an interrupt's wakeup is recognised, not only
/// kept away from the wrong task. A `nanosleep` ends when its hrtimer expires,
/// and an hrtimer expires in the timer's hard interrupt, so each of a known
/// number of sleeps is ended by an interrupt -- whatever task it landed on.
#[test]
fn a_sleep_is_ended_by_the_timer_interrupt_and_not_by_the_task_it_landed_on() {
    const SLEEPS: u64 = 20;
    if !have("python3") {
        eprintln!("skipped: the sleeper is a python3 loop, and this machine has no python3");
        return;
    }
    let script = format!("import time\nfor _ in range({SLEEPS}): time.sleep(0.002)");
    let output = omn(&[
        "sched.wakeup",
        "-m",
        "trace",
        "-e",
        "raw",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        &script,
    ]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: sched.wakeup needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let results = results(&output);
    let events = raw_events(&results[0]);
    let by_timer: Vec<_> = events
        .iter()
        .filter(|event| event["waker_context"] == "hardirq")
        .collect();
    assert!(
        by_timer.len() as u64 >= SLEEPS,
        "{SLEEPS} sleeps end in {SLEEPS} timer interrupts, saw {} of {} wakeups:\n{text}",
        by_timer.len(),
        events.len()
    );
    for event in by_timer {
        assert_eq!(
            event["waker_tid"], 0,
            "an interrupt names no task: {event:?}"
        );
    }
    assert!(
        text.contains("issued from an interrupt"),
        "the run says what an interrupt's waker is:\n{text}"
    );
}

/// Stream S's finding for this source: a predicate the resolver credits to
/// the kernel ran nowhere. It runs over each drained wakeup now, and says so.
#[test]
fn a_wakeup_filter_that_matches_nothing_keeps_nothing_and_says_what_it_removed() {
    let output = omn(&[
        "sched.wakeup",
        "-m",
        "trace",
        "-e",
        "raw",
        "--format",
        "json",
        "--filter",
        "comm == \"nosuchcomm\"",
        "--",
        "/bin/sh",
        "-c",
        "sleep 0.02; sleep 0.02",
    ]);
    let text = spoken(&output);
    if unprivileged(&text) {
        eprintln!("skipped: sched.wakeup needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let results = results(&output);
    assert!(
        raw_events(&results[0]).is_empty(),
        "a filter nothing matches keeps nothing:\n{text}"
    );
    assert!(
        text.contains("the filter kept 0 wakeups and removed"),
        "the run says what the filter removed:\n{text}"
    );
}

// ====================================== sched.switches: every thread followed

/// Threads in the switch fixture, and the sleeps each one takes.
const SWITCH_THREADS: usize = 4;
const SWITCH_SLEEPS: u64 = 100;

/// A program whose threads each sleep a known number of times and then report
/// the kernel's own count of their switches, read from inside the thread.
const SWITCH_FIXTURE_SOURCE: &str = r#"
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/syscall.h>

#define THREADS 4
#define SLEEPS 100

static void *sleeper(void *unused) {
    (void)unused;
    for (int i = 0; i < SLEEPS; i++) usleep(500);
    /* Read last: every switch before this is in the kernel's count. */
    FILE *status = fopen("/proc/thread-self/status", "r");
    char line[256];
    unsigned long voluntary = 0, involuntary = 0;
    while (status && fgets(line, sizeof line, status)) {
        sscanf(line, "voluntary_ctxt_switches: %lu", &voluntary);
        sscanf(line, "nonvoluntary_ctxt_switches: %lu", &involuntary);
    }
    if (status) fclose(status);
    fprintf(stderr, "omn-switches tid=%ld switches=%lu\n", (long)syscall(SYS_gettid),
            voluntary + involuntary);
    return 0;
}

int main(void) {
    pthread_t threads[THREADS];
    for (int i = 0; i < THREADS; i++) pthread_create(&threads[i], 0, sleeper, 0);
    for (int i = 0; i < THREADS; i++) pthread_join(threads[i], 0);
    return 0;
}
"#;

/// Ground truth for `sched.switches` following threads: each of the fixture's
/// threads counts its own switches in the kernel's `/proc` counters and says
/// the number, and the tool's series for that thread is that number -- up to
/// the edges, the status read itself and the thread's exit, a switch or two
/// the kernel's count read too early to hold. Before the switch to
/// `perf::ThreadSampler` the event was not inherited, and no thread the
/// program started had a series at all.
#[test]
fn every_thread_a_program_starts_has_its_switches_counted() {
    const EDGES: u64 = 4;
    static BUILT: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    let fixture = match BUILT
        .get_or_init(|| build_c("switchers", SWITCH_FIXTURE_SOURCE))
        .clone()
    {
        Ok(path) => path,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let fixture = fixture.to_string_lossy().into_owned();
    let output = omn(&[
        "sched.switches",
        "-m",
        "trace",
        "-e",
        "summary",
        "--format",
        "json",
        "--",
        &fixture,
    ]);
    let text = spoken(&output);
    assert_eq!(exit_code(&output), 0, "{text}");
    let said: Vec<(String, u64)> = text
        .lines()
        .filter_map(|line| line.strip_prefix("omn-switches "))
        .map(|line| {
            let field = |name: &str| {
                line.split_whitespace()
                    .find_map(|field| field.strip_prefix(name))
                    .unwrap_or_else(|| panic!("`{name}` in `{line}`"))
                    .to_owned()
            };
            (
                format!("task:{}", field("tid=")),
                field("switches=").parse().expect("a count"),
            )
        })
        .collect();
    assert_eq!(said.len(), SWITCH_THREADS, "every thread reports:\n{text}");

    let results = results(&output);
    let series: std::collections::BTreeMap<String, u64> = outputs(&results[0], "summary")
        .flat_map(|output| output["series"].as_array().expect("series").clone())
        .filter_map(|series| {
            Some((
                series["key"].as_str()?.to_owned(),
                series["count"].as_u64()?,
            ))
        })
        .collect();
    for (task, kernel) in &said {
        assert!(
            *kernel >= SWITCH_SLEEPS,
            "{SWITCH_SLEEPS} sleeps are at least {SWITCH_SLEEPS} switches, and {task} counted \
             {kernel}"
        );
        let measured = series.get(task).copied().unwrap_or(0);
        assert!(
            measured >= *kernel && measured <= kernel + EDGES,
            "{task} counted {kernel} switches of its own and the tool saw {measured}: \
             {series:?}\n{text}"
        );
    }
    assert_eq!(
        dropped(&results[0]),
        0,
        "a lost switch cannot be checked against a known number:\n{text}"
    );
}

/// P8's `sched` half: the catalog declares `system: native` for
/// `sched.switches`, and the machine is now served, one event per cpu. Ground
/// truth is a launched sleeper beside the machine-wide run: the machine's
/// count holds at least the sleeper's own switches, under the sleeper's pid.
#[test]
fn a_machine_wide_switch_count_holds_a_known_sleepers_switches() {
    const SLEEPS: u64 = 50;
    let sleeper = Command::new("/bin/sh")
        .args(["-c", "while :; do sleep 0.004; done"])
        .spawn()
        .expect("a shell");
    let pid = sleeper.id();
    let output = omn(&[
        "sched.switches",
        "--system",
        "-d",
        "150ms",
        "-m",
        "trace",
        "-e",
        "raw",
        "--format",
        "json",
    ]);
    // Killed only after the run, so it was sleeping for the whole window
    // however long `omn` took to start on a loaded machine.
    let mut sleeper = sleeper;
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    let text = spoken(&output);
    if text.contains("CAP_PERFMON") || text.contains("perf_event_paranoid") {
        eprintln!("skipped: a per-cpu switch event needs CAP_PERFMON here:\n{text}");
        return;
    }
    assert_eq!(exit_code(&output), 0, "{text}");
    let results = results(&output);
    let events = raw_events(&results[0]);
    let mine = events
        .iter()
        .filter(|event| event.get("pid").and_then(Value::as_u64) == Some(u64::from(pid)))
        .count() as u64;
    // 150 ms of a loop of 4 ms sleeps, each a fork and a wait of the shell:
    // at least one switch of the shell per `sleep` it waited for, of which the
    // window holds a fifth or more.
    assert!(
        mine >= SLEEPS / 5,
        "the shell waited for {} `sleep`s in the window, each a switch, and the machine-wide \
         count holds {mine} of its switches out of {}",
        SLEEPS / 5,
        events.len()
    );
}

/// Stream S's finding for `sched.offcpu` and `sched.migrations`: a predicate
/// the resolver credits to the kernel ran nowhere, and a machine-wide run
/// narrowed to a name nothing has reported every task on the machine. It runs
/// over each drained row now: nothing passes a name nothing has, the run says
/// what it removed, and the name the launched shell has keeps its blocks.
#[test]
fn a_filter_over_the_drained_sources_keeps_what_it_names_and_says_what_it_removed() {
    for (source, removed) in [
        ("sched.offcpu", "the filter kept 0 blocks and removed"),
        (
            "sched.migrations",
            "the filter kept 0 migrations and removed",
        ),
    ] {
        let output = omn(&[
            source,
            "--system",
            "-d",
            "300ms",
            "-m",
            "trace",
            "-e",
            "raw",
            "--format",
            "json",
            "--filter",
            "comm == \"nosuchcomm\"",
        ]);
        let text = spoken(&output);
        if unprivileged(&text) {
            eprintln!("skipped: {source} needs CAP_BPF and CAP_PERFMON, which this machine does not grant");
            return;
        }
        assert_eq!(exit_code(&output), 0, "{text}");
        let results = results(&output);
        assert!(
            raw_events(&results[0]).is_empty(),
            "{source}: a filter nothing matches keeps nothing:\n{text}"
        );
        // A machine that neither blocked nor migrated for 300 ms does not
        // exist, so there was something to remove.
        assert!(
            text.contains(removed),
            "{source} says what its filter removed:\n{text}"
        );
    }

    let output = omn(&[
        "sched.offcpu",
        "-m",
        "trace",
        "-e",
        "raw",
        "--format",
        "json",
        "--filter",
        "comm == \"sh\"",
        "--",
        "/bin/sh",
        "-c",
        "sleep 0.05; sleep 0.05; true",
    ]);
    let text = spoken(&output);
    assert_eq!(exit_code(&output), 0, "{text}");
    let results = results(&output);
    let blocks: u64 = raw_events(&results[0])
        .iter()
        .map(|event| event["blocks"].as_u64().unwrap_or(0))
        .sum();
    assert!(
        blocks >= 2,
        "the shell waits for each of its two children, so the name it has keeps at least two \
         blocks, and kept {blocks}:\n{text}"
    );
}
