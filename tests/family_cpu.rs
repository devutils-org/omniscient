//! End-to-end checks on the `cpu` family.
//!
//! The unit tests in `src/collect/cpu/counters.rs` check the counter against a
//! loop whose instruction count is known by construction. These check the same
//! thing through the binary, where the request, the resolver, the launch and
//! the aggregation are all in the way -- and check that a request this family
//! cannot serve is refused with the reason rather than answered with the wrong
//! mechanism.

use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A shell loop of `iterations` turns, which is a program whose work scales
/// linearly with a number this test chooses.
fn counting_loop(iterations: u32) -> String {
    format!("i=0; while [ $i -lt {iterations} ]; do i=$((i+1)); done")
}

/// The count of `cpu.instructions` for one run of that loop, or nothing when
/// this machine will not open the counter.
fn instructions_for(iterations: u32) -> Option<f64> {
    let output = run(&[
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
        &counting_loop(iterations),
    ]);
    if !output.status.success() {
        eprintln!("skipped: {}", stderr(&output));
        return None;
    }
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("the json output parses");
    let summary = &document["results"][0]["outputs"][0]["series"][0]["last"];
    Some(summary.as_f64().expect("a counted value"))
}

/// Ground truth through the whole tool: twice the work is twice the
/// instructions. The absolute count depends on this machine's shell, but the
/// ratio depends only on the counter being a counter.
#[test]
fn twice_the_work_retires_about_twice_the_instructions() {
    let Some(once) = instructions_for(100_000) else {
        return;
    };
    let Some(twice) = instructions_for(200_000) else {
        return;
    };
    let ratio = twice / once;
    assert!(
        (1.9..2.1).contains(&ratio),
        "doubling the loop changed the instruction count by a factor of {ratio:.3} \
         ({once} then {twice})"
    );
}

/// `cpu.ipc` is derived, and a derived number that hides its terms cannot be
/// checked. Both counters have to be on the event.
#[test]
fn a_ratio_is_published_with_the_counters_it_came_from() {
    let output = run(&[
        "cpu.ipc",
        "-m",
        "count",
        "-e",
        "raw",
        "--format",
        "json",
        "--",
        "/bin/sh",
        "-c",
        &counting_loop(50_000),
    ]);
    if !output.status.success() {
        eprintln!("skipped: {}", stderr(&output));
        return;
    }
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("the json output parses");
    let event = &document["results"][0]["outputs"][0]["events"][0];
    let attrs = event["attrs"].as_array().expect("the event carries attrs");
    let named = |name: &str| -> f64 {
        attrs
            .iter()
            .find(|attr| attr["name"] == name)
            .unwrap_or_else(|| panic!("`{name}` is on the event"))["value"]
            .as_f64()
            .expect("numeric")
    };
    let (ipc, instructions, cycles) = (named("ipc"), named("instructions"), named("cycles"));
    assert!((ipc - instructions / cycles).abs() < 1e-9);
    assert!(
        (0.01..20.0).contains(&ipc),
        "an ipc of {ipc} is outside anything a cpu can do"
    );
}

/// Counting mode is not a cheap sampling mode, and the refusal has to say which
/// mechanism is missing rather than return a count labelled as a sample.
#[test]
fn sampling_a_counter_is_refused_with_the_mechanism_it_would_need() {
    let output = run(&[
        "cpu.cycles",
        "-m",
        "sample",
        "-e",
        "hist",
        "--",
        "/bin/true",
    ]);
    assert!(!output.status.success(), "{}", stdout(&output));
    let message = stderr(&output);
    assert!(message.contains("sampling path"), "{message}");
    assert!(message.contains("--mode count"), "{message}");
}

/// The record kept beside the numbers has to name the counter that produced
/// them: two runs through different backends are not comparable.
#[test]
fn the_resolved_record_names_the_backend_that_counts() {
    let output = run(&[
        "cpu.instructions",
        "-m",
        "count",
        "-e",
        "summary",
        "--dump-resolved",
        "--format",
        "json",
        "--",
        "/bin/true",
    ]);
    if !output.status.success() {
        eprintln!("skipped: {}", stderr(&output));
        return;
    }
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("the json output parses");
    let resolved = &document[0];
    assert_eq!(resolved["backend"]["id"], "pmu");
    assert_eq!(
        resolved["backend"]["requirements"][0]["needs"],
        "pmu:PERF_COUNT_HW_INSTRUCTIONS"
    );
    // The counter's meaning depends on the machine as much as on the event.
    assert!(resolved["environment"]["smt"].is_string());
    assert!(resolved["environment"]["cpu_governor"].is_string());
}
