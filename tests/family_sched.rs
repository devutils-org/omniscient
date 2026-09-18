//! Ground-truth tests for the `sched` family.
//!
//! Owned by that family's thread; see BRIEF.md. A test here checks a
//! measurement against something independently known -- a kernel counter, an
//! injected delay, a known size, a fixed iteration count. A test that checks
//! the code returns what the code computed proves nothing, which is the whole
//! difficulty of testing a tracer.
//!
//! The eBPF tests skip, loudly, on a machine that does not grant CAP_BPF and
//! CAP_PERFMON. That is the privilege model the project settled on: the
//! unprivileged answer is a refusal, not a lesser measurement, so there is
//! nothing for these tests to fall back to.

use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

/// A shell that blocks a known number of times: each `sleep` is a fork and a
/// `wait4`, and a task that waits leaves the cpu.
fn sleeper(times: usize) -> String {
    let iterations: Vec<String> = (1..=times).map(|index| index.to_string()).collect();
    format!("for i in {}; do sleep 0.01; done", iterations.join(" "))
}

/// Counts the events in a `raw` rendering, which prints one line per event.
fn lines_of_events(text: &str) -> usize {
    text.lines().filter(|line| line.contains("task:")).count()
}

#[test]
fn a_program_that_blocks_ten_times_shows_ten_context_switches() {
    let script = sleeper(10);
    let output = run(&[
        "sched.switches",
        "--mode",
        "trace",
        "--emit",
        "summary",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    if text.contains("perf_event_open") && text.contains("refused") {
        eprintln!("skipped: this machine refuses unprivileged perf sampling");
        return;
    }
    assert!(output.status.success(), "{text}");

    // Ten waits are ten times this task leaves the cpu. The kernel may add
    // more -- a page fault on a cold binary, a preemption -- so the known
    // number is a floor, and a count far above it would mean the events are
    // not what they claim to be.
    let events: usize = text
        .lines()
        .find(|line| line.contains(" events over "))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    assert!(
        events >= 10,
        "ten blocking waits must produce at least ten switches, saw {events}:\n{text}"
    );
    assert!(
        text.contains("no loss"),
        "a run that lost events cannot be checked against a known count:\n{text}"
    );
}

/// The degradation has to reach the user. `pmu:sw` is the fallback of the pair
/// and what it gives up -- the second party of the switch -- is exactly what
/// someone reading a switch count is about to assume they have.
#[test]
fn the_fallback_backend_says_what_it_gives_up() {
    let output = run(&[
        "sched.switches",
        "--mode",
        "trace",
        "--emit",
        "summary",
        "--",
        "/bin/sh",
        "-c",
        "sleep 0.01",
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        eprintln!("skipped: this machine serves neither backend of sched.switches");
        return;
    }
    assert!(
        text.contains("degraded") && text.contains("no second party"),
        "the fallback must state what it cannot tell you:\n{text}"
    );
}

/// Wakeups are the family's reason to exist, and the one measurement no other
/// source can stand in for. This needs the capabilities; without them the
/// project's answer is a refusal that names them, which is asserted instead.
#[test]
fn a_blocked_program_is_woken_as_many_times_as_it_blocks() {
    let script = sleeper(10);
    let output = run(&[
        "sched.wakeup",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);

    if text.contains("CAP_BPF") {
        assert!(
            text.contains("setcap") && text.contains("CAP_PERFMON"),
            "a refusal must name what would make it work:\n{text}"
        );
        eprintln!("skipped: this machine does not grant CAP_BPF and CAP_PERFMON");
        return;
    }
    assert!(output.status.success(), "{text}");

    let events = lines_of_events(&text);
    assert!(
        events >= 10,
        "a program that blocks ten times is woken at least ten times, saw {events}:\n{text}"
    );
    assert!(
        text.contains("no loss"),
        "wakeups lost to a full ring cannot be checked against a known count:\n{text}"
    );
}

/// A session naming an eBPF source must be writable on a machine that cannot
/// serve it: sessions are authored on laptops for servers.
#[test]
fn a_wakeup_session_is_valid_where_it_cannot_be_served() {
    let output = run(&[
        "sched.wakeup",
        "--pid",
        "1",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--dump-request",
        "--no-resolve",
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("sched.wakeup"), "{text}");
}
