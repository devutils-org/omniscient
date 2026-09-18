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
fn unavailable(output: &Output) -> Option<String> {
    let message = stderr(output);
    let withheld = message.contains("no viable backend")
        || message.contains("not readable")
        || message.contains("perf_event_paranoid");
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
