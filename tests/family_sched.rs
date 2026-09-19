//! Ground-truth tests for the `sched` family.
//!
//! Owned by that family; see docs/contributing.md. A test here checks a
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
        // A machine that cannot serve the request and a request that cannot
        // mean anything are different answers, and only the first is a reason
        // to stop quietly. Skipping on any failure is how a test passes in
        // green for hours without ever checking what it claims to check.
        let complaint = String::from_utf8_lossy(&output.stderr);
        assert!(
            complaint.contains("no viable backend")
                || complaint.contains("cannot start collecting")
                || complaint.contains("perf_event_paranoid")
                || complaint.contains("no collector is written"),
            "this is not a machine that cannot serve the request, it is a request that was \
             refused:\n{complaint}"
        );
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

/// Where `sleep` is. The launcher execs the path it is given and does not
/// search `PATH`, because a measurement of "whichever program that name found"
/// is not a measurement of anything.
fn sleep_binary() -> &'static str {
    for candidate in ["/usr/bin/sleep", "/bin/sleep"] {
        if std::path::Path::new(candidate).exists() {
            return candidate;
        }
    }
    panic!("no `sleep` on this machine, and the off-cpu tests are about a known delay");
}

/// Whether the run was refused for want of the capabilities, which is the
/// project's answer on an unprivileged machine and not a failure. The refusal
/// still has to say what would make it work, and that is asserted rather than
/// assumed.
fn refused_for_capabilities(text: &str) -> bool {
    if !text.contains("CAP_BPF") {
        return false;
    }
    assert!(
        text.contains("setcap") && text.contains("CAP_PERFMON"),
        "a refusal must name what would make it work:\n{text}"
    );
    eprintln!("skipped: this machine does not grant CAP_BPF and CAP_PERFMON");
    true
}

/// Sums one attribute over every event in a `raw` rendering.
fn sum_of(text: &str, attribute: &str) -> u64 {
    let needle = format!("{attribute}=");
    text.lines()
        .filter(|line| line.contains("sched.offcpu"))
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix(&needle))
                .and_then(|value| value.parse::<u64>().ok())
        })
        .sum()
}

/// Ground truth for the aggregating path: the delay is the kernel's own timer,
/// the tolerance is the one the brief sets, and the number comes back through
/// a hash map the program filled rather than through a stream of events.
#[test]
fn a_program_that_sleeps_a_known_time_is_off_cpu_for_that_time() {
    const SLEPT_NS: u64 = 400_000_000;
    let output = run(&[
        "sched.offcpu",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--",
        sleep_binary(),
        "0.4",
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    if refused_for_capabilities(&text) {
        return;
    }
    assert!(output.status.success(), "{text}");

    let blocked_ns = sum_of(&text, "total_ns");
    let margin = SLEPT_NS / 20;
    assert!(
        blocked_ns.abs_diff(SLEPT_NS) <= margin,
        "a 400 ms sleep must be reported as 400 ms off cpu within 5 %, saw {} ms:\n{text}",
        blocked_ns / 1_000_000
    );
    assert!(
        text.contains("no loss"),
        "blocked time lost to a full map cannot be checked against a known delay:\n{text}"
    );
    // A timer is an interruptible wait. Reporting it as uninterruptible would
    // turn a program doing nothing into a program stuck on a device.
    assert!(
        text.contains("state=sleeping"),
        "the state a task blocked in must reach the user:\n{text}"
    );
}

/// Preemption is not blocked time, and this is the assertion that says so with
/// a number. A program that never waits for anything is pushed off the cpu
/// hundreds of times in three hundred milliseconds; if those switches were
/// counted, this would report something close to the whole run.
#[test]
fn a_program_that_only_spins_reports_almost_no_blocked_time() {
    let output = run(&[
        "sched.offcpu",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--duration",
        "300ms",
        "--",
        "/bin/sh",
        "-c",
        "while :; do :; done",
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    if refused_for_capabilities(&text) {
        return;
    }
    assert!(output.status.success(), "{text}");

    let blocked_ns = sum_of(&text, "total_ns");
    assert!(
        blocked_ns < 50_000_000,
        "a program that spun for 300 ms without waiting for anything reported {} ms blocked, \
         which means run queue wait is being counted as a block:\n{text}",
        blocked_ns / 1_000_000
    );
}

/// The in-kernel aggregation is the point of the backend, and what it buys is
/// that the number of things userspace reads does not grow with the number of
/// switches. A shell waiting on twenty children blocks twenty times and must
/// produce one entry per drain interval, not one per block.
#[test]
fn the_kernel_aggregates_so_that_many_blocks_are_a_few_events() {
    let output = run(&[
        "sched.offcpu",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--",
        "/bin/sh",
        "-c",
        "i=0; while [ $i -lt 20 ]; do sleep 0.005; i=$((i+1)); done",
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    if refused_for_capabilities(&text) {
        return;
    }
    assert!(output.status.success(), "{text}");

    let events = text
        .lines()
        .filter(|line| line.contains("sched.offcpu") && line.contains("total_ns="))
        .count();
    let blocks = sum_of(&text, "blocks");
    assert!(
        blocks >= 20,
        "twenty waits are at least twenty blocks, saw {blocks}:\n{text}"
    );
    assert!(
        (events as u64) < blocks,
        "{blocks} blocks came back as {events} events, so nothing was aggregated:\n{text}"
    );
}

/// Fills a file with bytes no filesystem can compress away, and puts it on the
/// device before returning.
///
/// A file of zeros is not a file to read: btrfs stores 32 MiB of them in
/// almost nothing, so reading it back with `O_DIRECT` reaches the device a
/// quarter of the time and the block count comes out a quarter of what the
/// arithmetic says. Measured here before it was understood -- 133 waits for
/// 512 reads -- which is the same lesson as every other surprising number in
/// this project: the collector was right and the premise was wrong.
fn write_incompressible(path: &std::path::Path, bytes: usize) {
    use std::io::Write;
    // xorshift64, so the bytes are the same on every run and on every machine:
    // a test that reads different data each time is a test with a different
    // amount of device work each time.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut blob = Vec::with_capacity(bytes);
    while blob.len() < bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        blob.extend_from_slice(&state.to_ne_bytes());
    }
    let mut file = std::fs::File::create(path).expect("a file to read back");
    file.write_all(&blob).expect("the whole file written");
    // Without this the reads can be served from pages the filesystem has not
    // written out yet, and there is no device wait to measure.
    file.sync_all().expect("the file on the device");
}

/// Ground truth for the state, and a second, independent count of the blocks.
///
/// `dd` reading with `O_DIRECT` bypasses the page cache, so every read is a
/// trip to the device and the task waits for it uninterruptibly. The number of
/// reads is arithmetic -- the file size over the block size -- and it is known
/// before the run, which makes it something to check the block count against
/// rather than something derived from what the collector reported.
#[test]
fn reads_that_must_reach_the_device_are_uninterruptible_waits() {
    const BLOCK_BYTES: usize = 64 * 1024;
    const READS: usize = 512;

    let blob = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("offcpu-direct.blob");
    write_incompressible(&blob, BLOCK_BYTES * READS);

    let output = run(&[
        "sched.offcpu",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--",
        "/usr/bin/dd",
        &format!("if={}", blob.display()),
        "of=/dev/null",
        "bs=64k",
        "iflag=direct",
        "status=none",
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_file(&blob);

    if refused_for_capabilities(&text) {
        return;
    }
    // A filesystem that will not serve O_DIRECT is a machine that cannot
    // answer the question, which is the one reason to stop quietly. A run that
    // was refused for any other reason is not.
    if text.contains("the launched program exited with exit status 1") {
        assert!(
            text.contains("sched.offcpu"),
            "the run itself failed, which is not the filesystem's doing:\n{text}"
        );
        eprintln!("skipped: this filesystem does not serve O_DIRECT, so there is no device wait");
        return;
    }
    assert!(output.status.success(), "{text}");

    let blocks = sum_of(&text, "blocks");
    assert!(
        blocks >= READS as u64 * 9 / 10,
        "{READS} reads that must reach the device are {READS} waits, saw {blocks}:\n{text}"
    );
    assert!(
        blocks <= READS as u64 + 64,
        "{READS} reads produced {blocks} blocks, so something other than the reads is being \
         counted:\n{text}"
    );
    assert!(
        text.contains("state=uninterruptible"),
        "a task waiting on a device is in uninterruptible sleep, and calling it anything else \
         turns a disk problem into a program doing nothing:\n{text}"
    );
}
