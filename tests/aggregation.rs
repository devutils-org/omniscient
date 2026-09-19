//! Ground-truth tests for the aggregation and presentation layer.
//!
//! What is checked here is not a collector but what is made of what it
//! collected, so the independently known thing is a *distribution* rather than
//! a value: a program that allocates a hundred blocks of one size and two of
//! another has a known histogram, and three threads allocating three known
//! totals have a known ranking. The tool has to reproduce them exactly.
//!
//! `mem.heap.alloc` is the vehicle because it is the one source with a
//! collector whose every event the test itself caused. Its shim intercepts the
//! interpreter's own allocations too, which is why the sizes below are far
//! larger than anything an interpreter allocates -- the modes sit in buckets
//! nothing else reaches, and the tests assert that by checking the buckets
//! around them are empty.

use std::io::Write;
use std::process::{Command, Output, Stdio};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

const MIB: u64 = 1024 * 1024;

fn run_session(session: &str) -> Output {
    let mut child = Command::new(BINARY)
        .args(["-f", "-", "--input-format", "yaml"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(session.as_bytes())
        .expect("writing the session");
    child.wait_with_output().expect("omn should finish")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

/// A missing shim is a build that was not run, not a machine that cannot serve
/// the request: it fails, and says what to run. Letting it skip would mean
/// these tests quietly check nothing on every machine where `cargo build` was
/// used instead of `make`.
fn assert_ran(output: &Output) {
    let message = stderr(output);
    assert!(
        !message.contains("heap shim was not found"),
        "these tests need the preload shim: run `make` rather than `cargo build`\n{message}"
    );
    assert!(output.status.success(), "{message}");
}

/// A session over a launched python program, written as YAML with the program
/// as a JSON string so that newlines survive.
fn session(source: &str, emit: &str, program: &str, format: &str) -> String {
    format!(
        "format: {format}\nrequests:\n  \
         - source: {source}\n    \
         target: {{launch: [python3, -c, {}]}}\n    \
         mode: trace\n    emit: [{emit}]\n",
        serde_json::Value::from(program)
    )
}

/// Allocates `count` blocks of `bytes` through libc itself, so the sizes are
/// the ones the test chose rather than whatever the interpreter does with a
/// `bytes` object. The memory is never written to, so none of it is resident.
fn allocating(blocks: &[(u64, usize)]) -> String {
    let mut program = String::from(
        "import ctypes\n\
         libc = ctypes.CDLL(None)\n\
         libc.malloc.restype = ctypes.c_void_p\n\
         libc.malloc.argtypes = [ctypes.c_size_t]\n",
    );
    for (bytes, count) in blocks {
        program.push_str(&format!("for _ in range({count}): libc.malloc({bytes})\n"));
    }
    program
}

fn buckets(run: &serde_json::Value) -> Vec<(u64, Option<u64>, u64)> {
    run["results"][0]["outputs"][0]["series"][0]["buckets"]
        .as_array()
        .unwrap_or_else(|| panic!("a histogram with buckets: {run}"))
        .iter()
        .map(|bucket| {
            (
                bucket["low"].as_u64().expect("a low edge"),
                bucket["high"].as_u64(),
                bucket["count"].as_u64().expect("a count"),
            )
        })
        .collect()
}

/// The count in whichever bucket contains `value`, and zero when the histogram
/// has no bucket that far out.
fn count_at(buckets: &[(u64, Option<u64>, u64)], value: u64) -> u64 {
    buckets
        .iter()
        .find(|(low, high, _)| *low <= value && high.is_none_or(|high| value < high))
        .map(|(_, _, count)| *count)
        .unwrap_or(0)
}

/// Ground truth for `hist`, and the reason the emit exists.
///
/// A hundred allocations of 2 MiB and two of 64 MiB are a distribution with two
/// modes and nothing at all between them. The counts are exact because the test
/// made them, and the gap is asserted rather than assumed: every log2 bucket
/// between the two modes must be empty, which is both what "two modes" means
/// and the proof that the interpreter's own allocations are nowhere near them.
///
/// A mean over the same run is 3.3 MiB and describes neither mode. That is the
/// whole argument for the emit, and `io.latency` is the case it was written
/// for: a run whose page cache went cold halfway through is bimodal, and a
/// summary of it is a number that never happened.
#[test]
fn a_known_bimodal_distribution_keeps_both_modes_and_the_empty_gap_between_them() {
    const SMALL: u64 = 2 * MIB;
    const LARGE: u64 = 64 * MIB;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(&session(
        "mem.heap.alloc",
        "hist",
        &allocating(&[(SMALL, 100), (LARGE, 2)]),
        "json",
    ));
    assert_ran(&output);

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let buckets = buckets(&run);

    assert_eq!(
        count_at(&buckets, SMALL),
        100,
        "a hundred allocations of {SMALL} bytes, counted: {buckets:?}"
    );
    assert_eq!(
        count_at(&buckets, LARGE),
        2,
        "two allocations of {LARGE} bytes, counted: {buckets:?}"
    );

    let between: Vec<_> = buckets
        .iter()
        .filter(|(low, high, _)| *low > SMALL && high.is_some_and(|high| high <= LARGE))
        .collect();
    assert!(
        !between.is_empty(),
        "the two modes are four octaves apart and the buckets between them are missing: {buckets:?}"
    );
    assert!(
        between.iter().all(|(_, _, count)| *count == 0),
        "nothing was allocated between the two modes, so the gap must be empty: {between:?}"
    );
}

/// A linear histogram puts a value where its width says, and nowhere else.
///
/// The same hundred 2 MiB allocations, cut at 1 MiB, must all land in the one
/// bucket `[2 MiB, 3 MiB)` -- the bucket whose range contains 2 MiB exactly
/// once, which is what `floor(value / width)` means and what an eBPF map with a
/// fixed stride would compute.
#[test]
fn a_linear_histogram_puts_a_known_size_in_the_bucket_its_width_defines() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(&session(
        "mem.heap.alloc",
        "hist:linear:1MiB",
        &allocating(&[(2 * MIB, 100)]),
        "json",
    ));
    assert_ran(&output);

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let buckets = buckets(&run);
    assert!(
        buckets.contains(&(2 * MIB, Some(3 * MIB), 100)),
        "a hundred 2 MiB allocations belong in [2 MiB, 3 MiB) and nowhere else: {buckets:?}"
    );
    assert_eq!(
        count_at(&buckets, 3 * MIB),
        0,
        "nothing was allocated at 3 MiB: {buckets:?}"
    );
}

/// Ground truth for `topk`: a ranking the test created.
///
/// Three threads allocate 512, 64 and 8 MiB. A series is keyed on an event's
/// primary key and this collector's is the thread, so there are four series --
/// the three workers and the interpreter's own main thread -- and their order
/// is known before the tool runs. `topk:3` must produce exactly the three
/// workers, in that order, and account for the fourth in what it left out.
#[test]
fn topk_reproduces_a_ranking_the_program_created() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let program = "import ctypes, threading\n\
         libc = ctypes.CDLL(None)\n\
         libc.malloc.restype = ctypes.c_void_p\n\
         libc.malloc.argtypes = [ctypes.c_size_t]\n\
         def burn(mib): libc.malloc(mib * 1048576)\n\
         threads = [threading.Thread(target=burn, args=(mib,)) for mib in (512, 64, 8)]\n\
         for t in threads: t.start()\n\
         for t in threads: t.join()\n";

    let output = run_session(&session("mem.heap.alloc", "topk:3", program, "json"));
    assert_ran(&output);

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let ranked = &run["results"][0]["outputs"][0];
    let series = ranked["series"]
        .as_array()
        .unwrap_or_else(|| panic!("a ranking: {run}"));

    assert_eq!(series.len(), 3, "topk:3 returns three of the four series");
    let totals: Vec<f64> = series
        .iter()
        .map(|entry| entry["total"].as_f64().expect("a total"))
        .collect();

    // Each worker's total is its own allocation plus whatever the thread's own
    // startup allocated, which is kilobytes against megabytes.
    for (rank, expected) in [512.0, 64.0, 8.0].into_iter().enumerate() {
        let expected = expected * MIB as f64;
        assert!(
            (expected..expected + MIB as f64).contains(&totals[rank]),
            "the thread ranked {} allocated {expected} bytes, and the ranking says {}: {totals:?}",
            rank + 1,
            totals[rank]
        );
    }

    // The fourth series is the main thread. A top three that does not say a
    // fourth exists is a statement about the whole program.
    let rest = &ranked["rest"];
    assert_eq!(rest["series"].as_u64(), Some(1), "{ranked}");
    assert!(
        rest["events"].as_u64().is_some_and(|events| events > 0),
        "the series left out carried the interpreter's own allocations: {ranked}"
    );
}

/// Two emits over one collection, which is the property that makes asking for
/// both free: the same events reduced twice, not collected twice.
#[test]
fn two_emits_of_one_request_describe_the_same_events() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(&session(
        "mem.heap.alloc",
        "hist, topk, summary",
        &allocating(&[(2 * MIB, 100)]),
        "json",
    ));
    assert_ran(&output);

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let results = &run["results"][0];
    let seen = results["events_seen"].as_u64().expect("a count of events");

    let histogram = &results["outputs"][0]["series"][0];
    let ranked = &results["outputs"][1]["series"][0];
    let summary = &results["outputs"][2]["series"][0];

    assert_eq!(
        histogram["count"].as_u64(),
        Some(seen),
        "every event reached the histogram: {results}"
    );
    assert_eq!(
        ranked["events"].as_u64(),
        Some(seen),
        "and the ranking, which has one series here: {results}"
    );
    assert_eq!(
        histogram["sum"].as_f64(),
        summary["mean"]
            .as_f64()
            .zip(summary["count"].as_f64())
            .map(|(mean, count)| mean * count),
        "the histogram's sum and the summary's mean describe the same values"
    );
}

/// `speedscope` is offered for a timeline as well as a profile, and this is
/// what that has to produce: a file speedscope will open. The frame indices
/// must resolve into the shared table and the events must be ordered and
/// closed, because speedscope reports a file that breaks either as an error
/// rather than as a chart.
#[test]
fn a_polled_timeline_becomes_a_speedscope_file_that_resolves() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let session = "format: speedscope\nrequests:\n  \
         - source: mem.rss\n    \
         target: {launch: [python3, -c, \"import time; b = bytearray(32 * 1024 * 1024); time.sleep(0.3)\"]}\n    \
         mode: poll@50ms\n    emit: [timeline]\n";
    let output = run_session(session);
    assert_ran(&output);

    let document: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("speedscope's own JSON");
    let frames = document["shared"]["frames"]
        .as_array()
        .unwrap_or_else(|| panic!("a shared frame table: {document}"));
    let profile = &document["profiles"][0];
    assert_eq!(profile["type"], "evented");

    let mut open = Vec::new();
    let mut last_at = 0;
    for event in profile["events"].as_array().expect("events") {
        let at = event["at"].as_u64().expect("a time");
        assert!(
            at >= last_at,
            "speedscope requires ordered events: {profile}"
        );
        last_at = at;
        let frame = event["frame"].as_u64().expect("a frame index") as usize;
        assert!(frame < frames.len(), "frame {frame} is past the table");
        match event["type"].as_str() {
            Some("O") => open.push(frame),
            Some("C") => assert_eq!(open.pop(), Some(frame), "a close out of order: {profile}"),
            other => panic!("unknown event type {other:?}"),
        }
    }
    assert!(open.is_empty(), "a frame was left open: {profile}");
    assert!(
        frames.iter().any(|frame| frame["name"]
            .as_str()
            .is_some_and(|name| name.contains("MiB"))),
        "a timeline's values are in its frame names, in the source's unit: {frames:?}"
    );
}

/// The matrix in `format.rs` is a claim about what a file can mean, and it is
/// enforced before anything is collected rather than after: a collapsed-stack
/// file holding a histogram parses and means nothing.
#[test]
fn a_format_refuses_an_emit_it_cannot_express_before_collecting_anything() {
    let output = run_session(&session("mem.heap.alloc", "hist", "pass", "folded"));
    assert!(
        !output.status.success(),
        "`--emit hist --format folded` was accepted: {}",
        stdout(&output)
    );
    let message = stderr(&output);
    assert!(
        message.contains("cannot carry") && message.contains("hist"),
        "{message}"
    );
}

/// Until a collector captures one, `--emit stack` has to say that no event
/// carried a stack.
///
/// The aggregation is written and the encoders are written; what is missing is
/// the capture. An empty profile and a program that did nothing render
/// identically, so the count of what carried no stack is the whole difference
/// between "not built yet" and "nothing happened". When stacks arrive this test
/// changes shape; it must not quietly keep passing.
#[test]
fn a_stack_emit_with_no_captured_stacks_says_so_rather_than_showing_an_empty_profile() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(&session(
        "mem.heap.alloc",
        "stack",
        &allocating(&[(2 * MIB, 4)]),
        "json",
    ));
    assert_ran(&output);

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let stacks = &run["results"][0]["outputs"][0];
    let seen = run["results"][0]["events_seen"]
        .as_u64()
        .expect("a count of events");
    assert!(seen > 0, "the program allocated: {run}");
    assert_eq!(
        stacks["unattributed"].as_u64(),
        Some(seen),
        "no collector captures a stack yet, and every event has to be counted as such: {run}"
    );
    assert_eq!(
        stacks["stacks"].as_array().map(Vec::len),
        Some(0),
        "{stacks}"
    );
}

/// A profile format handed a run with no profile in it refuses.
///
/// An empty collapsed-stack file is a valid file: it opens as an empty flame
/// graph, which reads as a program that spent no time anywhere. Refusing with
/// the reason is the difference between a missing mechanism and a measurement.
#[test]
fn a_collapsed_stack_file_is_refused_rather_than_written_empty() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(&session("mem.heap.alloc", "stack", "pass", "folded"));
    assert!(
        !output.status.success(),
        "an empty folded file was written: {:?}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("no event carried one"),
        "{}",
        stderr(&output)
    );
}
