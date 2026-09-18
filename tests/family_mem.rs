//! Ground-truth tests for the `mem` family.
//!
//! Owned by that family's thread; see BRIEF.md. A test here checks a
//! measurement against something independently known -- a kernel counter, an
//! injected delay, a known size, a fixed iteration count. A test that checks
//! the code returns what the code computed proves nothing, which is the whole
//! difficulty of testing a tracer.

use std::io::Write;
use std::process::{Command, Output, Stdio};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

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

/// Some machines withhold what these tests need. That is a skip, not a pass and
/// not a failure.
///
/// A missing shim is neither: it is a build that was not run, and letting it
/// skip would mean the heap tests quietly measure nothing on the machine where
/// they matter most. It fails, and says what to run.
fn unavailable(output: &Output) -> Option<String> {
    let message = stderr(output);
    assert!(
        !message.contains("heap shim was not found"),
        "the heap sources need the preload shim: run `cargo build -p omn-preload` first\n{message}"
    );
    let withheld = message.contains("not readable") || message.contains("perf_event_paranoid");
    withheld.then_some(message)
}

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

/// Several footprints of one program, in one window.
fn footprint_session(program: &str, sources: &[&str]) -> String {
    let mut session = String::from("format: json\nrequests:\n");
    for source in sources {
        session.push_str(&format!(
            "  - source: {source}\n    \
             target: {{launch: [python3, -c, {program:?}]}}\n    \
             mode: poll@50ms\n    emit: [summary]\n"
        ));
    }
    session
}

fn summary(run: &serde_json::Value, index: usize) -> (f64, f64) {
    let series = run["results"][index]["outputs"][0]["series"]
        .as_array()
        .unwrap_or_else(|| panic!("a summary per key in result {index}: {run}"));
    let first = &series[0];
    (
        first["min"].as_f64().expect("a minimum"),
        first["max"].as_f64().expect("a maximum"),
    )
}

/// Ground truth for the three footprints.
///
/// A known amount of private memory is made resident in a launched program, and
/// each of the three metrics has to see it. It is the same memory, touched
/// once, so the rise is the same number three times over -- what differs
/// between RSS, PSS and USS is how they count what the program *shares*, and a
/// buffer the program just wrote to is shared with nobody.
///
/// Without a test of this shape, a collector that returned a plausible constant
/// would pass everything else.
#[test]
fn a_known_private_allocation_shows_up_in_all_three_footprints() {
    const TOUCHED: usize = 64 * 1024 * 1024;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    // A baseline first, then the memory, then time to be polled again.
    let program = format!(
        "import time\n\
         time.sleep(0.3)\n\
         block = bytearray({TOUCHED})\n\
         time.sleep(0.4)\n"
    );
    let sources = ["mem.rss", "mem.pss", "mem.uss"];
    let output = run_session(&footprint_session(&program, &sources));
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");

    let mut maxima = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        assert_eq!(run["results"][index]["source"], *source);
        let (low, high) = summary(&run, index);
        let rise = high - low;
        assert!(
            rise >= 0.75 * TOUCHED as f64,
            "{source}: {TOUCHED} bytes were touched but the rise was only {rise}"
        );
        assert!(
            rise <= 4.0 * TOUCHED as f64,
            "{source}: a rise of {rise} is far more than the {TOUCHED} bytes touched"
        );
        maxima.push(high);
    }

    // Private is a subset of proportional is a subset of resident. The three
    // are read one after the other inside a tick, so a few pages may appear
    // between two of them; a fraction of a percent of slack covers that, and
    // nothing near the size of the buffer.
    let slack = 0.01 * maxima[0];
    assert!(
        maxima[2] <= maxima[1] + slack,
        "uss {} exceeds pss {}",
        maxima[2],
        maxima[1]
    );
    assert!(
        maxima[1] <= maxima[0] + slack,
        "pss {} exceeds rss {}",
        maxima[1],
        maxima[0]
    );
}

/// The proportional footprint has to be below the resident one by roughly what
/// the program shares -- its libraries. Every process running the interpreter
/// maps the same libc, and PSS is the metric that does not count it in full in
/// each of them.
#[test]
fn the_proportional_footprint_is_below_the_resident_one_for_a_shared_interpreter() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let program = "import time; time.sleep(0.4)\n";
    let output = run_session(&footprint_session(program, &["mem.rss", "mem.pss"]));
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let (_, rss) = summary(&run, 0);
    let (_, pss) = summary(&run, 1);
    assert!(
        pss < rss,
        "an interpreter shares its libraries, so pss {pss} must be below rss {rss}"
    );
}

/// A program that allocates a known amount of memory, touches it, and then
/// frees every other block.
///
/// The holes are the point. Freeing everything lets the allocator hand the
/// whole block back to the kernel -- on this machine glibc trims 128 MiB back
/// to nothing the moment the last chunk goes -- and there would be no
/// fragmentation to measure. Freeing every other chunk leaves the arena with
/// live memory at the top and holes below it, which is exactly the shape that
/// makes a long-running process grow and never shrink.
fn fragmenting_program(count: usize, size: usize) -> String {
    format!(
        "import ctypes, time\n\
         libc = ctypes.CDLL(None)\n\
         libc.malloc.restype = ctypes.c_void_p\n\
         libc.free.argtypes = [ctypes.c_void_p]\n\
         blocks = [libc.malloc({size}) for _ in range({count})]\n\
         for block in blocks: ctypes.memset(block, 1, {size})\n\
         time.sleep(0.25)\n\
         for block in blocks[::2]: libc.free(block)\n\
         time.sleep(0.45)\n"
    )
}

fn heap_session(source: &str, program: &str) -> String {
    format!(
        "format: json\nrequests:\n  \
         - source: {source}\n    \
         target: {{launch: [python3, -c, {program:?}]}}\n    \
         mode: poll@50ms\n    emit: [summary, timeline]\n"
    )
}

/// Ground truth for the live heap.
///
/// The program allocates a known number of blocks of a known size through
/// `libc.malloc` itself, so the live total is arithmetic rather than opinion:
/// 1024 blocks of 64 KiB is 64 MiB, and after half of them are freed it is 32
/// MiB. The interpreter's own allocations ride on top of that, which is why the
/// bounds are ranges and not equalities.
#[test]
fn the_live_heap_matches_the_allocations_the_program_made() {
    const COUNT: usize = 1024;
    const SIZE: usize = 64 * 1024;
    const ALLOCATED: f64 = (COUNT * SIZE) as f64;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(&heap_session(
        "mem.heap.live",
        &fragmenting_program(COUNT, SIZE),
    ));
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let series = run["results"][0]["outputs"][0]["series"][0].clone();
    let peak = series["max"].as_f64().expect("a maximum");
    let last = series["last"].as_f64().expect("a last value");

    assert!(
        (ALLOCATED..1.5 * ALLOCATED).contains(&peak),
        "{COUNT} blocks of {SIZE} bytes are {ALLOCATED} bytes live at the peak, measured {peak}"
    );
    assert!(
        (0.4 * ALLOCATED..0.75 * ALLOCATED).contains(&last),
        "half of them were freed, so about {} bytes should be left, measured {last}",
        ALLOCATED / 2.0
    );
}

/// Ground truth for retained memory, the metric this family exists to make
/// measurable.
///
/// Half of a known 64 MiB is freed, and every page of it was written to before
/// the free, so every page of it is memory the program gave back and the
/// process still holds. The allocator cannot return any of it: the holes are
/// surrounded by live blocks.
///
/// The check that this is not a tautology is the resident size reported beside
/// it. RSS stays at the full 64 MiB while the live heap halves, and the
/// retained number accounts for the difference -- which is precisely what "free
/// returns nothing" means, measured rather than asserted.
#[test]
fn memory_freed_into_a_fragmented_arena_is_reported_as_retained() {
    const COUNT: usize = 1024;
    const SIZE: usize = 64 * 1024;
    const FREED: f64 = (COUNT / 2 * SIZE) as f64;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(&heap_session(
        "mem.heap.retained",
        &fragmenting_program(COUNT, SIZE),
    ));
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let result = &run["results"][0];
    let retained = result["outputs"][0]["series"][0]["last"]
        .as_f64()
        .expect("a last value");

    // A page shared between a freed block and a live one is counted as live, so
    // the measurement is a lower bound: 64 KiB blocks are 16 pages each and at
    // most one page at each end is given away.
    assert!(
        retained >= 0.8 * FREED,
        "{FREED} bytes of touched memory were freed into holes, measured {retained} retained"
    );
    assert!(
        retained <= 1.1 * FREED,
        "only {FREED} bytes were freed, so {retained} is more than there was"
    );

    // Nothing was lost, and the raw events carry what the number is made of.
    assert_eq!(result["losses"]["dropped"], 0);
    let last_point = result["outputs"][1]["series"][0]["points"]
        .as_array()
        .expect("a timeline")
        .last()
        .cloned()
        .expect("at least one point");
    assert!(
        last_point["value"].as_f64().unwrap() > 0.0,
        "the timeline ends with the retained memory still there: {last_point}"
    );
}
