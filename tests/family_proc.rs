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
        eprintln!("skipped: this machine has no python3 to launch");
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

/// Runs `omn` with `args` and returns its JSON results.
fn run_json(args: &[&str]) -> (serde_json::Value, Output) {
    let output = Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run");
    assert!(output.status.success(), "{}", stderr(&output));
    let run = serde_json::from_str(&stdout(&output)).expect("json results");
    (run, output)
}

fn attr(event: &serde_json::Value, name: &str) -> Option<serde_json::Value> {
    event["attrs"]
        .as_array()?
        .iter()
        .find(|attr| attr["name"] == name)
        .map(|attr| attr["value"].clone())
}

/// `proc.tasks` on a launched program is one event per thread per poll: the
/// fullest poll holds exactly the threads the program says it runs.
#[test]
fn every_thread_of_a_process_is_one_event_of_a_poll() {
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
    let (run, output) = run_json(&[
        "proc.tasks",
        "--mode",
        "poll@50ms",
        "--emit",
        "raw",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        program,
    ]);
    let reported: usize = stderr(&output)
        .lines()
        .find_map(|line| line.strip_prefix("THREADS "))
        .and_then(|value| value.trim().parse().ok())
        .expect("the program reports its threads");
    let events = run["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("events")
        .clone();
    let mut per_poll = std::collections::BTreeMap::<u64, usize>::new();
    for event in &events {
        *per_poll
            .entry(event["ts_ns"].as_u64().unwrap())
            .or_default() += 1;
    }
    let fullest = per_poll.values().copied().max().unwrap_or(0);
    assert_eq!(reported, 6, "five workers and the main thread");
    assert_eq!(
        fullest, reported,
        "the program runs {reported} threads and the fullest poll listed {fullest}"
    );
    let pids: std::collections::BTreeSet<_> = events
        .iter()
        .filter_map(|event| attr(event, "pid"))
        .map(|v| v.to_string())
        .collect();
    assert_eq!(
        pids.len(),
        1,
        "every thread is of the one process: {pids:?}"
    );
}

/// `busy` is the share of a cpu: a thread that spins reads one, a thread that
/// waits reads zero, both against what the program chose to do.
#[test]
fn a_spinning_thread_is_busy_and_a_waiting_one_is_not() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    // The main thread spins for 800 ms; one worker waits the whole time.
    let program = "import threading, time\n\
                   stop = threading.Event()\n\
                   worker = threading.Thread(target=stop.wait)\n\
                   worker.start()\n\
                   end = time.monotonic() + 0.8\n\
                   while time.monotonic() < end:\n\
                   \x20   pass\n\
                   stop.set()\n";
    let (run, _) = run_json(&[
        "proc.tasks",
        "--mode",
        "poll@100ms",
        "--emit",
        "raw",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        program,
    ]);
    let events = run["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("events")
        .clone();
    let polled: Vec<_> = events
        .iter()
        .filter(|event| {
            attr(event, "since")
                .and_then(|v| v.as_str().map(str::to_owned))
                .as_deref()
                == Some("poll")
        })
        .collect();
    let share = |main: bool| -> Vec<f64> {
        polled
            .iter()
            .filter(|event| (attr(event, "tid") == attr(event, "pid")) == main)
            .filter_map(|event| attr(event, "busy")?.as_f64())
            .collect()
    };
    let spinning = share(true);
    let waiting = share(false);
    assert!(
        spinning.len() >= 4,
        "too few polls of the main thread: {spinning:?}"
    );
    // The first and the last interval may straddle the program's start and
    // end, so the claim is on the median: within two ticks of a 100 ms poll.
    let mut sorted = spinning.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    assert!(
        (0.8..=1.2).contains(&median),
        "a spinning thread read a median share of {median}: {spinning:?}"
    );
    assert!(
        waiting.iter().all(|share| *share <= 0.2),
        "a waiting thread read {waiting:?}"
    );
}

/// `proc.tasks --system` lists a process this test started, once, with the
/// name, the parent and the state procfs gives it.
#[test]
fn the_machine_lists_a_process_this_test_started() {
    let mut child = Command::new("sleep").arg("5").spawn().expect("sleep");
    let filter = format!("pid == {}", child.id());
    let (run, _) = run_json(&[
        "proc.tasks",
        "--system",
        "--mode",
        "snapshot",
        "--emit",
        "raw",
        "--filter",
        &filter,
        "--format",
        "json",
    ]);
    child.kill().ok();
    child.wait().ok();
    let events = run["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("events")
        .clone();
    assert_eq!(events.len(), 1, "one event for the process: {events:?}");
    let event = &events[0];
    assert_eq!(attr(event, "comm").unwrap(), "sleep");
    assert_eq!(
        attr(event, "ppid").unwrap().as_u64(),
        Some(std::process::id() as u64)
    );
    assert_eq!(attr(event, "state").unwrap(), "S");
    assert_eq!(attr(event, "threads").unwrap().as_u64(), Some(1));
}

/// `proc.tasks --tid` is that one thread, under the process it belongs to.
/// The kernel serves `/proc/<tid>/task` with the whole thread group, which is
/// what a `--tid` run once listed -- every thread, each labelled with the tid
/// as its pid.
#[test]
fn a_thread_target_is_that_thread_under_its_own_process() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let program = "import sys, threading\n\
                   stop = threading.Event()\n\
                   ids = []\n\
                   def work():\n\
                   \x20   ids.append(threading.get_native_id())\n\
                   \x20   stop.wait()\n\
                   workers = [threading.Thread(target=work) for _ in range(3)]\n\
                   for worker in workers:\n\
                   \x20   worker.start()\n\
                   while len(ids) < 3:\n\
                   \x20   pass\n\
                   print(ids[-1], flush=True)\n\
                   sys.stdin.read()\n\
                   stop.set()\n";
    let mut child = Command::new("python3")
        .args(["-c", program])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("python3");
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.as_mut().unwrap()),
        &mut line,
    )
    .unwrap();
    let tid = line.trim().to_owned();
    let output = Command::new(BINARY)
        .args([
            "proc.tasks",
            "--tid",
            &tid,
            "--mode",
            "snapshot",
            "--emit",
            "raw",
            "--format",
            "json",
        ])
        .output()
        .expect("omn should run");
    drop(child.stdin.take());
    child.wait().ok();
    assert!(output.status.success(), "{}", stderr(&output));
    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let events = run["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("events")
        .clone();
    assert_eq!(events.len(), 1, "one thread, one event: {events:?}");
    assert_eq!(attr(&events[0], "tid").unwrap().to_string(), tid);
    assert_eq!(
        attr(&events[0], "pid").unwrap().as_u64(),
        Some(u64::from(child.id()))
    );
}
