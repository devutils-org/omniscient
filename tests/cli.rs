//! End-to-end checks on the binary.
//!
//! These exist to pin the contract the rest of the tool is written against: the
//! exit codes, the implicit `trace` subcommand, and the promise that a dumped
//! request is a runnable session file. The unit tests cover the reasoning; this
//! covers the surface.

use std::io::Write;
use std::process::{Command, Output, Stdio};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// A request was refused because it cannot mean what it asks for.
const EXIT_REJECTED: i32 = 2;
/// The request is valid, but nothing collects it yet.
const EXIT_NOT_IMPLEMENTED: i32 = 3;

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn run_with_stdin(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(BINARY)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(stdin.as_bytes())
        .expect("writing to stdin");
    child.wait_with_output().expect("omn should finish")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_source_needs_no_subcommand() {
    let implicit = run(&[
        "io.latency",
        "--system",
        "-m",
        "trace",
        "-e",
        "hist",
        "--dump-request",
    ]);
    let explicit = run(&[
        "trace",
        "io.latency",
        "--system",
        "-m",
        "trace",
        "-e",
        "hist",
        "--dump-request",
    ]);
    assert!(implicit.status.success(), "{}", stderr(&implicit));
    assert_eq!(stdout(&implicit), stdout(&explicit));
}

#[test]
fn an_impossible_request_is_refused_before_anything_is_attached() {
    let output = run(&[
        "mem.bandwidth",
        "--pid",
        "1",
        "-m",
        "count",
        "-e",
        "rate",
        "--dump-request",
    ]);
    assert_eq!(output.status.code(), Some(EXIT_REJECTED));
    assert!(
        stderr(&output).contains("scoped to the package"),
        "{}",
        stderr(&output)
    );
    assert!(
        stdout(&output).is_empty(),
        "a refused request must print no request"
    );
}

#[test]
fn a_valid_request_with_no_collector_names_the_ones_that_exist() {
    let output = run(&["io.latency", "--system", "-m", "trace", "-e", "hist"]);
    assert_eq!(output.status.code(), Some(EXIT_NOT_IMPLEMENTED));
    assert!(stderr(&output).contains("mem.rss"), "{}", stderr(&output));
}

#[test]
fn a_request_this_machine_cannot_serve_is_refused_with_the_reason() {
    // `io.latency --pid` needs in-program filtering, so it needs the block
    // tracepoints; without readable tracefs there is no backend that can.
    let output = run(&[
        "io.latency",
        "--pid",
        "1",
        "-m",
        "trace",
        "-e",
        "hist",
        "--dump-request",
    ]);
    if output.status.success() {
        return; // this machine can serve it, which is also a valid outcome
    }
    assert_eq!(output.status.code(), Some(EXIT_REJECTED));
    assert!(
        stderr(&output).contains("no viable backend"),
        "{}",
        stderr(&output)
    );

    // The same request still writes down, for a machine that can serve it.
    let written = run(&[
        "io.latency",
        "--pid",
        "1",
        "-m",
        "trace",
        "-e",
        "hist",
        "--no-resolve",
        "--dump-request",
    ]);
    assert!(written.status.success(), "{}", stderr(&written));
}

#[test]
fn a_dumped_request_is_a_session_file_the_tool_reads_back() {
    let dumped = run(&[
        "io.latency",
        "--pid",
        "4242",
        "-m",
        "trace",
        "-e",
        "hist,topk:5",
        "-k",
        "blk,task",
        "--filter",
        "bytes > 4KiB",
        "-d",
        "30s",
        "--format",
        "yaml",
        "--no-resolve",
        "--dump-request",
    ]);
    assert!(dumped.status.success(), "{}", stderr(&dumped));

    let replayed = run_with_stdin(
        &[
            "-f",
            "-",
            "--input-format",
            "yaml",
            "--format",
            "yaml",
            "--no-resolve",
            "--dump-request",
        ],
        &stdout(&dumped),
    );
    assert!(replayed.status.success(), "{}", stderr(&replayed));
    assert_eq!(
        stdout(&replayed),
        stdout(&dumped),
        "a session must survive a trip through the file surface unchanged"
    );
}

#[test]
fn the_same_session_reads_from_yaml_json_and_toml() {
    let yaml = run_with_stdin(
        &[
            "-f",
            "-",
            "--input-format",
            "yaml",
            "--format",
            "json",
            "--dump-request",
        ],
        "requests: [{source: io.latency, target: system, mode: trace, emit: [hist]}]",
    );
    let json = run_with_stdin(
        &[
            "-f",
            "-",
            "--input-format",
            "json",
            "--format",
            "json",
            "--dump-request",
        ],
        r#"{"requests":[{"source":"io.latency","target":"system","mode":"trace","emit":["hist"]}]}"#,
    );
    let toml = run_with_stdin(
        &["-f", "-", "--input-format", "toml", "--format", "json", "--no-resolve", "--dump-request"],
        "[[requests]]\nsource = \"io.latency\"\ntarget = \"system\"\nmode = \"trace\"\nemit = [\"hist\"]\n",
    );
    assert!(yaml.status.success(), "{}", stderr(&yaml));
    assert_eq!(stdout(&yaml), stdout(&json));
    assert_eq!(stdout(&json), stdout(&toml));
}

#[test]
fn the_shipped_examples_validate() {
    for example in [
        "examples/web-latency.yml",
        "examples/saturation.toml",
        "examples/rss-run.yml",
    ] {
        let output = run(&["validate", "-f", example]);
        assert!(output.status.success(), "{example}: {}", stderr(&output));
    }
}

#[test]
fn the_catalog_listing_is_machine_readable() {
    let output = run(&["list", "io", "--format", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let entries: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid JSON");
    let entries = entries.as_array().expect("a list of entries");
    assert!(entries.len() >= 10);
    assert!(entries.iter().all(|entry| entry["family"] == "io"));
}

#[test]
fn every_declared_format_is_listed_with_whether_its_encoder_exists() {
    let output = run(&["formats", "--format", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let info: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid JSON");
    let outputs = info["output"].as_array().expect("output formats");
    let folded = outputs
        .iter()
        .find(|format| format["name"] == "folded")
        .expect("folded is declared");
    assert_eq!(folded["implemented"], false);
    assert_eq!(folded["emits"].as_array().unwrap().len(), 1);
}

#[test]
fn a_result_format_is_refused_for_a_request_dump() {
    let output = run(&[
        "cpu.profile",
        "--pid",
        "1",
        "-m",
        "sample",
        "-e",
        "stack",
        "--format",
        "folded",
        "--dump-request",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("describes results"),
        "{}",
        stderr(&output)
    );
}

/// Ground truth.
///
/// Every other test here checks that the tool is coherent with itself, which
/// proves nothing about whether the number is right. This one makes a known
/// amount of memory exist and asks whether the measurement saw it: without a
/// test of this shape, a collector that returns a plausible constant passes
/// everything else.
#[test]
fn a_known_allocation_shows_up_in_the_measured_series() {
    const TOUCHED: usize = 64 * 1024 * 1024;

    let pid = std::process::id().to_string();
    let measuring = Command::new(BINARY)
        .args([
            "mem.rss",
            "--pid",
            &pid,
            "--mode",
            "poll@50ms",
            "--duration",
            "1200ms",
            "--emit",
            "summary",
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // Let it take a baseline before the memory exists.
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Touching one byte per page is what makes the pages resident: allocating
    // alone would move nothing, which is the source's first caveat.
    let mut block = vec![0u8; TOUCHED];
    for page in block.chunks_mut(4096) {
        page[0] = 1;
    }
    std::hint::black_box(&block);

    let output = measuring.wait_with_output().expect("omn should finish");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(block);

    let results: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json results");
    let series = results["outputs"][0]["series"]
        .as_array()
        .expect("a summary per key");
    let ours = series
        .iter()
        .find(|entry| entry["key"] == format!("task:{pid}"))
        .expect("the series for the measured process");

    let rise = ours["max"].as_f64().unwrap() - ours["min"].as_f64().unwrap();
    assert!(
        rise >= 0.75 * TOUCHED as f64,
        "touched {TOUCHED} bytes but the measured rise was only {rise}"
    );
    assert!(
        rise <= 4.0 * TOUCHED as f64,
        "measured rise of {rise} is far more than the {TOUCHED} bytes touched"
    );
}

#[test]
fn a_run_writes_its_numbers_and_the_record_of_how_they_were_obtained() {
    let directory = std::env::temp_dir().join(format!("omn-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);

    let output = run(&[
        "mem.rss",
        "--pid",
        &std::process::id().to_string(),
        "--mode",
        "poll@50ms",
        "--duration",
        "150ms",
        "--emit",
        "summary",
        "--out",
        directory.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    let resolved = std::fs::read_to_string(directory.join("resolved.json")).expect("resolved.json");
    let resolved: serde_json::Value = serde_json::from_str(&resolved).unwrap();
    assert_eq!(resolved["request"]["source"], "mem.rss");
    assert!(resolved["backend"]["id"]
        .as_str()
        .unwrap()
        .starts_with("fs:proc:"));
    assert!(!resolved["environment"]["kernel_release"]
        .as_str()
        .unwrap()
        .is_empty());
    assert!(std::fs::read_to_string(directory.join("results.txt")).is_ok());

    let _ = std::fs::remove_dir_all(&directory);
}
