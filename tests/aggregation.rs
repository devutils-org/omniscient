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
    // Within the rounding of one division and one product, not exactly: a
    // mean times a count is not the sum it came from in floating point, and on
    // a run of 216 MB it missed by one ulp every time.
    let sum = histogram["sum"].as_f64().expect("a histogram sum");
    let expected =
        summary["mean"].as_f64().expect("a mean") * summary["count"].as_f64().expect("a count");
    assert!(
        (sum - expected).abs() <= 4.0 * f64::EPSILON * sum.abs().max(1.0),
        "the histogram's sum and the summary's mean describe the same values: {sum} against \
         {expected}"
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

// ---------------------------------------------------------------------------
// The distribution that says it is short -- finding 37.
//
// Every test above measures a distribution the workload defined. These two
// measure the one thing a distribution cannot say for itself: that it is
// missing part of its own tail. The events a lossy run loses are the slow
// ones, because whatever stalled the machine is what stopped the measuring,
// so a percentile taken off it is a lower bound and nothing used to say so.
//
// The loss here is real rather than substituted. `mem.heap.alloc` reports its
// own drops -- the shim writes an `OP_DROPPED` record carrying the backlog
// when allocations outrun the pipe -- so the run that fills it is a run whose
// shortfall was counted by something other than the layer under test.
// ---------------------------------------------------------------------------

/// A block size below glibc's mmap threshold, so two hundred thousand of them
/// are arena allocations rather than two hundred thousand mappings, and far
/// from anything an interpreter allocates. It owns the log2 bucket
/// `[64 KiB, 128 KiB)`.
const BLOCK: u64 = 100_000;

/// Enough allocations to outrun the pipe. The shim's own notes name a
/// two-hundred-thousand allocation storm as what it was hardened against.
const BURST: usize = 200_000;

/// Allocates and immediately frees `count` blocks of `bytes`.
///
/// Freeing each block keeps the storm inside one arena: the point is the rate
/// at which records are produced, and two hundred thousand live blocks of this
/// size would be twenty gigabytes of heap for no reason.
fn storming(bytes: u64, count: usize) -> String {
    format!(
        "import ctypes\n\
         libc = ctypes.CDLL(None)\n\
         libc.malloc.restype = ctypes.c_void_p\n\
         libc.malloc.argtypes = [ctypes.c_size_t]\n\
         libc.free.argtypes = [ctypes.c_void_p]\n\
         for _ in range({count}): libc.free(libc.malloc({bytes}))\n"
    )
}

/// The first series of the first output, which is where the shortfall lives.
fn shortfall(run: &serde_json::Value) -> &serde_json::Value {
    &run["results"][0]["outputs"][0]["series"][0]["short"]
}

/// Ground truth for the mark: a run that lost events it can count, against a
/// number of allocations the test made and therefore knows.
///
/// Two hundred thousand allocations of one size go in; fewer than two hundred
/// thousand come out, because the shim drops what it cannot push through the
/// pipe. That gap is the defect finding 37 names -- the histogram of the short
/// run and the histogram of a complete one are the same picture -- and the
/// fraction beside the value is what closes it.
///
/// Three things are checked against something other than the emit layer: the
/// count the test created, the drop count the *shim* reported, and the
/// arithmetic between them. The conservation check is the strongest of the
/// three: every allocation this program made either reached a bucket or was
/// counted as a loss, so the bucket and the drops together cannot come to less
/// than the storm.
#[test]
fn a_distribution_built_from_a_run_that_dropped_events_says_how_short_it_is() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(&session(
        "mem.heap.alloc",
        "hist",
        &storming(BLOCK, BURST),
        "json",
    ));
    assert_ran(&output);
    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");

    let result = &run["results"][0];
    let dropped = result["losses"]["dropped"].as_u64().expect("a drop count");
    let seen = result["events_seen"].as_u64().expect("a count of events");
    let counted = count_at(&buckets(&run), BLOCK);

    // A drop is a machine under pressure, and a machine fast enough to drain
    // the pipe as fast as python can fill it is not a machine that cannot
    // serve this -- it is a run with nothing to mark. Saying so with the
    // numbers is the difference between a test that skipped and a test that
    // quietly checked nothing.
    if dropped == 0 {
        eprintln!(
            "skipped: {BURST} allocations of {BLOCK} bytes produced {seen} events over {} ns \
             and the shim dropped none of them, so this run has no shortfall to mark. \
             {counted} of the {BURST} reached the [64 KiB, 128 KiB) bucket.",
            result["observed_ns"]
        );
        assert_eq!(
            shortfall(&run),
            &serde_json::Value::Null,
            "and a run that lost nothing must not be marked: {result}"
        );
        return;
    }

    assert!(
        counted < BURST as u64,
        "the run dropped {dropped} records and the bucket still holds all {BURST} \
         allocations, which cannot both be true: {result}"
    );
    assert!(
        counted + dropped >= BURST as u64,
        "{counted} allocations arrived and {dropped} records were counted as lost, which \
         does not account for the {BURST} this program made: the gap is a loss nothing \
         counted"
    );

    let short = shortfall(&run);
    assert_eq!(
        short["dropped"].as_u64(),
        Some(dropped),
        "the mark has to carry the loss the shim reported, not one of its own: {short}"
    );
    assert_eq!(short["records"].as_u64(), Some(seen));
    let fraction = short["fraction"].as_f64().expect("a fraction");
    let expected = dropped as f64 / (dropped + seen) as f64;
    assert!(
        (fraction - expected).abs() < 1e-12,
        "{dropped} of {} records is {expected}, and the mark says {fraction}",
        dropped + seen
    );
    assert!(
        short["note"]
            .as_str()
            .is_some_and(|note| note.contains("lower bound")),
        "the direction of the bias is the half a reader acts on: {short}"
    );

    // And the same run rendered for a human, which is where a p99 is actually
    // read off. A second run rather than a second format of the first: the
    // tool writes one format per session, and the storm is cheap.
    let output = run_session(&session(
        "mem.heap.alloc",
        "hist",
        &storming(BLOCK, BURST),
        "text",
    ));
    assert_ran(&output);
    let text = stdout(&output);
    if text.contains("0 dropped") {
        eprintln!("skipped: the second storm dropped nothing:\n{text}");
        return;
    }
    assert!(
        text.contains("n>=") && text.contains("lower bound"),
        "the mark is what a reader of the text output sees, and it is not there:\n{text}"
    );
}

/// The negative, and it matters as much as the mark: the same workload, small
/// enough to lose nothing, gains nothing.
///
/// A mark that appears on every run is a mark readers learn to skip, and then
/// the run that needed it is the one nobody reads it on. So a clean run is
/// checked for the absence of the whole vocabulary -- the field in the
/// structured output, the `>=` in the text -- rather than for a mark that says
/// zero.
#[test]
fn a_clean_run_of_the_same_workload_gains_nothing() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(&session(
        "mem.heap.alloc",
        "hist,summary",
        &storming(BLOCK, 200),
        "json",
    ));
    assert_ran(&output);
    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let result = &run["results"][0];

    // Two hundred allocations cannot fill the pipe. If this machine somehow
    // dropped one the test has nothing to say about a clean run, and says so
    // rather than asserting the opposite.
    let dropped = result["losses"]["dropped"].as_u64().expect("a drop count");
    if dropped > 0 {
        eprintln!(
            "skipped: 200 allocations dropped {dropped} records on this machine, so there is \
             no clean run here to check: {}",
            result["losses"]
        );
        return;
    }

    for output in result["outputs"].as_array().expect("two outputs") {
        for series in output["series"].as_array().expect("a series") {
            assert_eq!(
                series.get("short"),
                None,
                "a run that lost nothing carries no shortfall: {series}"
            );
        }
    }

    let output = run_session(&session(
        "mem.heap.alloc",
        "hist,summary",
        &storming(BLOCK, 200),
        "text",
    ));
    assert_ran(&output);
    let text = stdout(&output);
    if !text.contains("no loss") {
        eprintln!("skipped: the second clean run was not clean:\n{text}");
        return;
    }
    assert!(
        !text.contains(">="),
        "nothing was lost and every number is exact, so no value may be written as a bound:\n\
         {text}"
    );
    assert!(
        !text.contains("short by"),
        "a clean run must not explain a shortfall it does not have:\n{text}"
    );
}

/// The throttled half of finding 37, end to end and against a real kernel
/// refusal to deliver.
///
/// `kernel.perf_event_max_sample_rate` is reached exactly when the event rate
/// is highest, which is what makes a throttle the purest form of the bias: the
/// samples the kernel declines to deliver are the ones from the busiest
/// moment. And unlike a drop it yields no numerator -- a throttle is a window,
/// not a count of what was in it -- so the honest answer is that the number is
/// not knowable, which is the answer `DECISIONS.md` already chose for the loss
/// line. This test is here to hold the tool to saying that rather than
/// dividing a throttle count by something and calling it a rate.
///
/// A sampler asked for a hundred thousand samples a second against a busy loop
/// is what provokes it. Whether it does depends on
/// `kernel.perf_event_max_sample_rate` and `kernel.perf_cpu_time_max_percent`,
/// which are the machine's settings and not this test's, so a run that is not
/// throttled skips with both numbers rather than asserting what the machine
/// was doing.
#[test]
fn a_throttled_sampler_says_it_cannot_tell_how_much_it_lost() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(
        "format: json\nrequests:\n  \
         - source: cpu.profile\n    \
         target: {launch: [python3, -c, \"x=0\\nfor i in range(20000000): x+=i\"]}\n    \
         mode: sample@100000\n    emit: [hist]\n    duration: 600ms\n",
    );
    if !output.status.success() {
        let message = stderr(&output);
        assert!(
            !message.contains("cannot emit") && !message.contains("no collector"),
            "a request that cannot mean anything is not a machine that cannot serve it: \
             {message}"
        );
        eprintln!(
            "skipped: this machine would not sample cpu.profile \
             (perf_event_paranoid={}): {message}",
            sysctl("kernel/perf_event_paranoid")
        );
        return;
    }

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let result = &run["results"][0];
    let throttled = result["losses"]["throttled"]
        .as_u64()
        .expect("a throttle count");
    if throttled == 0 {
        eprintln!(
            "skipped: a hundred thousand samples a second was not throttled here \
             (perf_event_max_sample_rate={}, perf_cpu_time_max_percent={}), and {} samples \
             arrived over {} ns, so there is no throttle to describe",
            sysctl("kernel/perf_event_max_sample_rate"),
            sysctl("kernel/perf_cpu_time_max_percent"),
            result["events_seen"],
            result["observed_ns"],
        );
        return;
    }

    let short = shortfall(&run);
    assert_eq!(
        short["throttled"].as_u64(),
        Some(throttled),
        "the mark carries the kernel's own throttle count: {short}"
    );
    let note = short["note"].as_str().expect("a note");
    assert!(
        note.contains("unknown number") && note.contains("not recorded anywhere"),
        "a throttle has no numerator, and inventing one is the defect: {note}"
    );
    assert!(
        note.contains("lower bound"),
        "the distribution is still short and still one-sided: {note}"
    );

    // A throttled run that also dropped reports the fraction it knows as a
    // floor and the rest as unknown, rather than folding one into the other.
    let dropped = result["losses"]["dropped"].as_u64().expect("a drop count");
    if dropped > 0 {
        assert!(
            note.contains("and by an unknown number more"),
            "{dropped} dropped records are a floor under an unknown total: {note}"
        );
    } else {
        assert_eq!(
            short.get("fraction"),
            None,
            "nothing was dropped, so no fraction exists to print: {short}"
        );
    }
}

/// A kernel tunable, or `?` where this machine does not publish it. Only ever
/// used to say why a scenario could not be run here.
fn sysctl(name: &str) -> String {
    std::fs::read_to_string(format!("/proc/sys/{name}"))
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| "?".to_owned())
}
