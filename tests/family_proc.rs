//! Ground-truth tests for the `proc` family.
//!
//! These two counts are the only ones in the catalog that can be checked
//! exactly: a process holding seventeen descriptors holds seventeen, with no
//! skid, no sampling and no folio size in the way. So they are checked exactly,
//! against a process that says what it holds.
//!
//! The comparison is between two independent readers of the same truth: the
//! measured process counts its own descriptors and threads and reports them,
//! and the collector reads the same kernel directories from outside. Agreement
//! means the collector reads what is there; a test that only checked the
//! collector against itself would prove nothing.

use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

/// Runs `omn` against a launched python program and returns the highest value
/// it measured, plus whatever the program reported about itself.
fn measure(source: &str, program: &str, marker: &str) -> Option<(u64, u64)> {
    let output = Command::new(BINARY)
        .args([
            source,
            "--mode",
            "poll@50ms",
            "--emit",
            "summary",
            "--format",
            "json",
            "--",
            "python3",
            "-c",
            program,
        ])
        .output()
        .expect("omn should run");
    assert!(output.status.success(), "{}", stderr(&output));

    let reported: u64 = stderr(&output)
        .lines()
        .find_map(|line| line.strip_prefix(marker))
        .and_then(|value| value.trim().parse().ok())
        .expect("the program reports its own count");

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let series = run["results"][0]["outputs"][0]["series"]
        .as_array()
        .expect("a summary per process");
    let measured = series.first()?["max"].as_f64()? as u64;
    Some((measured, reported))
}

#[test]
fn the_descriptor_count_matches_what_the_process_says_it_holds() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    // Open a known number of extra descriptors, report the resulting total the
    // process can see for itself, and hold still long enough to be polled.
    let program = "import os, sys, time\n\
                   held = [open('/proc/self/status') for _ in range(17)]\n\
                   # The listing's own handle is in the listing, so it is not held.\n\
                   count = len(os.listdir('/proc/self/fd')) - 1\n\
                   sys.stderr.write('FDS %d\\n' % count)\n\
                   sys.stderr.flush()\n\
                   time.sleep(0.5)\n";

    let (measured, reported) = measure("proc.fd", program, "FDS ").expect("a measurement");
    assert_eq!(
        measured, reported,
        "the process holds {reported} descriptors and the collector counted {measured}"
    );
    assert!(
        reported >= 17,
        "the program opened seventeen files and counted only {reported} descriptors in total"
    );
}

#[test]
fn the_thread_count_matches_what_the_process_says_it_runs() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let program = "import sys, threading, time\n\
                   stop = threading.Event()\n\
                   workers = [threading.Thread(target=stop.wait) for _ in range(5)]\n\
                   for worker in workers:\n\
                   \x20   worker.start()\n\
                   sys.stderr.write('THREADS %d\\n' % threading.active_count())\n\
                   sys.stderr.flush()\n\
                   time.sleep(0.5)\n\
                   stop.set()\n";

    let (measured, reported) = measure("proc.threads", program, "THREADS ").expect("a measurement");
    assert_eq!(
        measured, reported,
        "the process runs {reported} threads and the collector counted {measured}"
    );
    assert_eq!(reported, 6, "five workers and the main thread");
}

#[test]
fn a_count_without_its_limit_is_not_reported_as_a_measurement() {
    if !has_python() {
        return;
    }
    let output = Command::new(BINARY)
        .args([
            "proc.fd",
            "--mode",
            "poll@50ms",
            "--emit",
            "raw",
            "--format",
            "json",
            "--",
            "python3",
            "-c",
            "import time; time.sleep(0.2)",
        ])
        .output()
        .expect("omn should run");
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let event = &run["results"][0]["outputs"][0]["events"][0];
    let attrs = event["attrs"].as_array().expect("attributes");
    for name in ["open", "limit", "headroom"] {
        assert!(
            attrs.iter().any(|attr| attr["name"] == name),
            "a descriptor count means nothing without `{name}`: {event}"
        );
    }
}
