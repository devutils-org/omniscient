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
    // Refused before anything runs: no capability for the eBPF backend, and
    // `pmu:sw` missing because a context switch is kernel mode and this
    // machine lets the process count its own user mode only (paranoid 2).
    if !output.status.success()
        && (text.contains("has no viable backend on this machine")
            || text.contains("which happen only in kernel mode"))
    {
        let reason = text
            .lines()
            .find(|l| l.starts_with("error:"))
            .unwrap_or(&text);
        eprintln!("skipped: {reason}");
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

// ---------------------------------------------------------------------------
// sched.runqlat
// ---------------------------------------------------------------------------

/// Only one test may contend a cpu at a time.
///
/// Two of them do, and `cargo test` runs the tests of one binary in parallel
/// threads. A test that measures what one task alone on a cpu waits for cannot
/// share that cpu with another test's three spinners -- it would measure a
/// contended cpu and call it an idle one. Serialising them is the fix; giving
/// each its own cpu is not, because the interference is the machine's, not the
/// test's.
static CONTENDED_CPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A copy of `/bin/sh` under a name nothing else on the machine has.
///
/// The load has to be findable in a system-wide run, and `comm` is the only
/// thing that distinguishes it. A shell called `sh` cannot be told apart from
/// every other shell on the machine, and a system-wide measurement that cannot
/// name its own workload is measuring the machine. Each test gets its own
/// name, so that one of them can never count the other's load as its own.
/// The name this run's workload answers to, distinct from every other run's on
/// the machine and not only from this suite's other tests.
///
/// `std::env::temp_dir()` is machine-wide. Six worktrees of one fan-out running
/// this suite at once each copied `/bin/sh` to the same path and each spun a
/// task answering to that `comm` -- and the filters below are `--system`. That
/// is the whole of the flake this test was blamed for: it never failed alone,
/// and it failed when the machine was busiest. The pid makes a run's workload
/// its own, and has to fit inside the fifteen characters the kernel keeps.
fn workload_name(stem: &str) -> String {
    let name = format!("{stem}{}", std::process::id() % 100_000);
    assert!(
        name.len() <= 15,
        "`{name}` is longer than a comm; shorten `{stem}`"
    );
    name
}

fn distinctly_named_shell(name: &str) -> Option<std::path::PathBuf> {
    // Fifteen characters is what the kernel keeps of a name, so the file is
    // named inside that: a workload whose `comm` is truncated is one the
    // filter cannot match.
    //
    assert!(name.len() <= 15, "`{name}` is longer than a comm");
    let path = std::env::temp_dir().join(name);
    if !path.exists() {
        std::fs::copy("/bin/sh", &path).ok()?;
        let mut mode = std::fs::metadata(&path).ok()?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&path, mode).ok()?;
    }
    Some(path)
}

/// Busy tasks, all pinned to one cpu, that spin until they are killed.
///
/// The last cpu rather than the first, only so that a suite running its own
/// tests in parallel is disturbed as little as possible.
struct PinnedSpinners(Vec<std::process::Child>);

impl PinnedSpinners {
    /// The cpu these spinners contend, which is the whole of what they measure.
    fn cpu() -> Option<usize> {
        Some(std::thread::available_parallelism().ok()?.get() - 1)
    }

    fn start(count: usize, name: &str) -> Option<Self> {
        let shell = distinctly_named_shell(name)?;
        let cpu = Self::cpu()?;
        let children: Vec<_> = (0..count)
            .filter_map(|_| {
                Command::new("taskset")
                    .args([
                        "-c",
                        &cpu.to_string(),
                        shell.to_str()?,
                        "-c",
                        "while :; do :; done",
                    ])
                    .spawn()
                    .ok()
            })
            .collect();
        if children.len() != count {
            return None;
        }
        // Long enough for every one of them to be running before the
        // measurement opens; a task that had not started yet would spend the
        // first part of the window not competing for anything.
        std::thread::sleep(std::time::Duration::from_millis(200));
        Some(Self(children))
    }
}

impl Drop for PinnedSpinners {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn monotonic_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec for the kernel to write into.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// The kernel's own account of how long a task has waited on a run queue:
/// the second field of `/proc/<tid>/schedstat`, in nanoseconds.
fn run_delay_ns(tid: u32) -> Option<u64> {
    std::fs::read_to_string(format!("/proc/{tid}/schedstat"))
        .ok()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The waits of the pinned tasks, in nanoseconds, by thread, and the span the
/// run was attached for, out of a JSON document.
fn waits_and_span(output: &Output) -> (std::collections::BTreeMap<u64, u64>, u64) {
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one json document");
    let result = &document["results"][0];
    let mut totals = std::collections::BTreeMap::new();
    for output in result["outputs"].as_array().expect("outputs") {
        for event in output["events"].as_array().into_iter().flatten() {
            let attr = |name: &str| {
                event["attrs"]
                    .as_array()?
                    .iter()
                    .find(|attr| attr["name"] == name)?["value"]
                    .as_u64()
            };
            if let (Some(tid), Some(total)) = (attr("tid"), attr("total_ns")) {
                *totals.entry(tid).or_insert(0) += total;
            }
        }
    }
    let window = &result["window"];
    let span =
        window["closed_ns"].as_u64().unwrap_or(0) - window["opened_ns"].as_u64().unwrap_or(0);
    (totals, span)
}

/// Ground truth, from the kernel's own per-task account: each pinned task's
/// waits are its `run_delay`, the time `/proc/<tid>/schedstat` says it spent
/// runnable and not running.
///
/// *N* cpu-bound tasks pinned to one cpu queue behind each other; one task
/// alone on a cpu queues for nothing a spinner does. The reference for each is
/// read before `omn` starts and after it ends, so it brackets the run: what
/// the run counted can be no more than the kernel's delta, and no less than
/// the delta less the part of the bracket the run was not attached for -- no
/// task can have waited longer than that part lasted -- less one wait at each
/// edge, which began before the attach or ended after the detach. That is
/// exact, whatever else the machine runs on that cpu, where the share of a
/// wall-clock window this test asserted before depended on the machine
/// (flaky tally #6). And the separation: three tasks sharing a cpu wait,
/// between them, far more than one alone.
#[test]
fn tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them() {
    /// One wait at each edge of the window: a timeslice among three
    /// spinners, with room for a loaded cpu.
    const EDGE_NS: u64 = 50_000_000;
    let mut waited = Vec::new();

    let _one_at_a_time = CONTENDED_CPU.lock().expect("the cpu lock");
    let name = workload_name("omnshare");
    for spinners in [1usize, 3] {
        let Some(load) = PinnedSpinners::start(spinners, &name) else {
            eprintln!("skipped: this machine has no `taskset`, so no cpu can be contended");
            return;
        };
        let tids: Vec<u32> = load.0.iter().map(|child| child.id()).collect();
        let before: Vec<Option<u64>> = tids.iter().map(|tid| run_delay_ns(*tid)).collect();
        let bracket_opened = monotonic_ns();
        let output = run(&[
            "sched.runqlat",
            "--system",
            "--duration",
            "1s",
            "--emit",
            "raw",
            "--format",
            "json",
            "--filter",
            &format!("comm == '{name}'"),
        ]);
        let bracket = monotonic_ns() - bracket_opened;
        let after: Vec<Option<u64>> = tids.iter().map(|tid| run_delay_ns(*tid)).collect();
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        if refused_for_capabilities(&text) {
            return;
        }
        assert!(output.status.success(), "{text}");
        let (waits, span) = waits_and_span(&output);
        let outside = bracket.saturating_sub(span);

        let mut total = 0;
        for ((tid, before), after) in tids.iter().zip(&before).zip(&after) {
            let (Some(before), Some(after)) = (before, after) else {
                eprintln!(
                    "skipped: /proc/{tid}/schedstat is not readable, and it is the reference"
                );
                return;
            };
            let kernel = after - before;
            let measured = waits.get(&u64::from(*tid)).copied().unwrap_or(0);
            eprintln!(
                "{spinners} on one cpu: task {tid} waited {:.1} ms by the kernel's account over \
                 {:.2} s, and {:.1} ms were counted over the {:.2} s the run was attached",
                kernel as f64 / 1e6,
                bracket as f64 / 1e9,
                measured as f64 / 1e6,
                span as f64 / 1e9,
            );
            assert!(
                measured <= kernel + kernel / 100 + 1_000_000,
                "task {tid} was counted waiting {measured} ns, more than the {kernel} ns the \
                 kernel says it waited over a bracket that contains the run:\n{text}"
            );
            assert!(
                measured + outside + 2 * EDGE_NS >= kernel,
                "task {tid} waited {kernel} ns by the kernel's account, of which at most \
                 {outside} ns fell outside the {span} ns the run was attached for, and only \
                 {measured} ns were counted:\n{text}"
            );
            total += measured;
        }
        waited.push(total);
    }

    let (alone, contended) = (waited[0], waited[1]);
    assert!(
        contended > alone * 3 + 100_000_000,
        "contending for a cpu must show up as waiting for one: {:.1} ms alone, {:.1} ms among \
         three",
        alone as f64 / 1e6,
        contended as f64 / 1e6
    );
}

/// A cpu target is that cpu's run queue: `sched.runqlat` declares `cpu:
/// native`, and until the thirteenth wave its collector refused one. Three
/// tasks pinned to one cpu queue there and nowhere else, so a run on that cpu
/// keeps each of them for what the kernel's own `run_delay` says it waited --
/// the same bracket as the test above -- and keeps no row of another cpu.
#[test]
fn a_cpu_target_is_that_cpus_run_queue() {
    const EDGE_NS: u64 = 50_000_000;
    let _one_at_a_time = CONTENDED_CPU.lock().expect("the cpu lock");
    let name = workload_name("omncpuq");
    let Some(load) = PinnedSpinners::start(3, &name) else {
        eprintln!("skipped: this machine has no `taskset`, so no cpu can be contended");
        return;
    };
    let cpu = PinnedSpinners::cpu().expect("a cpu").to_string();
    let tids: Vec<u32> = load.0.iter().map(|child| child.id()).collect();
    let before: Vec<Option<u64>> = tids.iter().map(|tid| run_delay_ns(*tid)).collect();
    let bracket_opened = monotonic_ns();
    let output = run(&[
        "sched.runqlat",
        "--cpu",
        &cpu,
        "--duration",
        "1s",
        "--emit",
        "raw",
        "--format",
        "json",
    ]);
    let bracket = monotonic_ns() - bracket_opened;
    let after: Vec<Option<u64>> = tids.iter().map(|tid| run_delay_ns(*tid)).collect();
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    if refused_for_capabilities(&text) {
        return;
    }
    assert!(output.status.success(), "{text}");
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one json document");
    let result = &document["results"][0];
    let elsewhere = result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .flat_map(|output| output["events"].as_array().cloned().unwrap_or_default())
        .filter(|event| {
            event["attrs"]
                .as_array()
                .and_then(|attrs| attrs.iter().find(|attr| attr["name"] == "cpu"))
                .and_then(|attr| attr["value"].as_u64())
                != cpu.parse().ok()
        })
        .count();
    assert_eq!(
        elsewhere, 0,
        "{elsewhere} rows of another cpu were kept:\n{text}"
    );
    let (waits, span) = waits_and_span(&output);
    let outside = bracket.saturating_sub(span);
    for ((tid, before), after) in tids.iter().zip(&before).zip(&after) {
        let (Some(before), Some(after)) = (before, after) else {
            eprintln!("skipped: /proc/{tid}/schedstat is not readable, and it is the reference");
            return;
        };
        let kernel = after - before;
        let measured = waits.get(&u64::from(*tid)).copied().unwrap_or(0);
        assert!(
            measured <= kernel + kernel / 100 + 1_000_000 && measured + outside + 2 * EDGE_NS >= kernel,
            "task {tid}, pinned to cpu {cpu}, waited {kernel} ns there by the kernel's account \
             over a bracket {outside} ns longer than the run, and {measured} ns were counted:\n{text}"
        );
    }
}

/// The bucket the kernel chose and the bucket the emit layer draws must be the
/// same bucket.
///
/// This is what makes building the histogram in the kernel viable at all. The
/// program computes a log2 index with an unrolled binary search over eight
/// registers; `emit::BucketLayout::Log2` computes one from a `f64` in
/// userspace. Nothing but a test crosses those two, and if they disagree by one
/// every wait is reported a factor of two away from where it happened, with
/// nothing anywhere saying so.
///
/// The bridge is the value on the event: each row travels as the low bound of
/// its own bucket, so re-bucketing it has to land it back where it started.
/// Checked through the real renderer rather than against a copy of the formula.
#[test]
fn the_kernels_buckets_are_the_emit_layers_buckets() {
    let _one_at_a_time = CONTENDED_CPU.lock().expect("the cpu lock");
    let name = workload_name("omnbkt");
    let Some(_load) = PinnedSpinners::start(3, &name) else {
        eprintln!("skipped: this machine has no `taskset`, so no cpu can be contended");
        return;
    };
    let output = run(&[
        "sched.runqlat",
        "--system",
        "--duration",
        "1s",
        "--emit",
        "hist",
        "--emit",
        "raw",
        "--filter",
        &format!("comm == '{name}'"),
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    if refused_for_capabilities(&text) {
        return;
    }
    assert!(output.status.success(), "{text}");

    // What the kernel said: the low bound of every bucket it put a wait in,
    // and -- the part that ties the bucket to a real duration -- the exact
    // total of the waits it put there.
    //
    // `total_ns` is the one number on the event that owes nothing to the
    // bucketing: it is a sum of differences between two `bpf_ktime_get_ns`
    // readings. So the mean of a row has to fall inside the bucket the row
    // claims, and a search that is off by one puts it outside by a factor of
    // two. Checking only that the emit layer agrees with `wait_ns` would not
    // catch that: both sides would shift together and still agree.
    let mut from_kernel = std::collections::BTreeSet::new();
    for line in text
        .lines()
        .filter(|line| line.contains("sched.runqlat") && line.contains("wait_ns="))
    {
        let field = |name: &str| {
            let needle = format!("{name}=");
            line.split_whitespace()
                .find_map(|field| field.strip_prefix(&needle))
                .and_then(|value| value.parse::<u64>().ok())
        };
        let (Some(low), Some(count), Some(total)) =
            (field("wait_ns"), field("count"), field("total_ns"))
        else {
            continue;
        };
        assert!(count > 0, "a row stands for no waits at all:\n{line}");
        let mean = total as f64 / count as f64;
        if low == 0 {
            assert_eq!(total, 0, "bucket zero holds only a wait of zero:\n{line}");
        } else {
            assert!(
                (low as f64..(low * 2) as f64).contains(&mean),
                "the kernel put {count} waits averaging {mean} ns in the bucket \
                 [{low}, {}), which does not contain them:\n{line}",
                low * 2
            );
        }
        from_kernel.insert(low);
    }
    assert!(
        !from_kernel.is_empty(),
        "three tasks sharing a cpu must have queued:\n{text}"
    );

    // What the emit layer drew: the bounds of every bucket it put anything in.
    let mut from_emit = std::collections::BTreeSet::new();
    for line in text.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some((range, tail)) = rest.split_once(')') else {
            continue;
        };
        let Some((low, high)) = range.split_once(", ") else {
            continue;
        };
        let (Some(low), Some(high)) = (
            low.strip_suffix("ns").and_then(|n| n.parse::<u64>().ok()),
            high.strip_suffix("ns").and_then(|n| n.parse::<u64>().ok()),
        ) else {
            continue;
        };
        // A log2 bucket is exactly one doubling wide, and the lowest one holds
        // only zero. A bucket of any other width is not the layout the program
        // was written against.
        assert_eq!(
            high,
            if low == 0 { 1 } else { low * 2 },
            "`[{low}ns, {high}ns)` is not a log2 bucket:\n{text}"
        );
        if tail
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            > Some(0)
        {
            from_emit.insert(low);
        }
    }

    assert_eq!(
        from_kernel, from_emit,
        "the kernel bucketed waits at {from_kernel:?} and the emit layer drew them at \
         {from_emit:?}; the two log2 definitions have drifted apart:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// sched.migrations
// ---------------------------------------------------------------------------

/// Ground truth: a task moved between two cpus a known number of times must be
/// counted that many times.
///
/// The independent quantity is `sched_setaffinity`, called from outside the
/// tool by `taskset`. A cpu-bound task pinned to one cpu cannot migrate for any
/// other reason -- it never sleeps, so it is never woken onto another cpu, and
/// the load balancer cannot move it off the one cpu its mask allows. So every
/// migration in the window is one this test caused, and the count is exact
/// rather than a floor.
#[test]
fn a_task_moved_between_two_cpus_is_counted_once_per_move() {
    if std::thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
        eprintln!("skipped: one cpu, so nothing can be moved between two");
        return;
    }
    let name = workload_name("omnmigr");
    let Some(shell) = distinctly_named_shell(&name) else {
        eprintln!("skipped: /bin/sh cannot be copied, so there is no load to pin");
        return;
    };
    let Ok(mut load) = Command::new("taskset")
        .args([
            "-c",
            "0",
            shell.to_str().expect("a path"),
            "-c",
            "while :; do :; done",
        ])
        .spawn()
    else {
        eprintln!("skipped: this machine has no `taskset`, so nothing can be pinned");
        return;
    };
    let pid = load.id().to_string();

    let measuring = Command::new(BINARY)
        .args([
            "sched.migrations",
            "--pid",
            &pid,
            "--mode",
            "trace",
            "--duration",
            "3s",
            "--emit",
            "raw",
            "--format",
            "json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let Ok(mut measuring) = measuring else {
        let _ = load.kill();
        panic!("omn should run");
    };
    // The task is moved until the run ends, and each move is stamped: the
    // moves that are counted are the ones inside the window the run reports,
    // rather than the ones a fixed delay was hoped to put there (flaky tally
    // #5). Fifty milliseconds between moves is how long each lasts, not when
    // anything has started.
    let mut moves = Vec::new();
    let mut step = 0;
    while measuring.try_wait().ok().flatten().is_none() {
        step += 1;
        let cpu = (step % 2).to_string();
        let asked = monotonic_ns();
        let moved = Command::new("taskset")
            .args(["-cp", &cpu, &pid])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        let done = monotonic_ns();
        assert!(moved, "every move must have been made");
        moves.push((asked, done));
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let output = measuring.wait_with_output().expect("omn should finish");
    let _ = load.kill();
    let _ = load.wait();
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    if refused_for_capabilities(&text) {
        return;
    }
    assert!(output.status.success(), "{text}");
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one json document");
    let result = &document["results"][0];
    let opened = result["window"]["opened_ns"].as_u64().expect("a window");
    let closed = result["window"]["closed_ns"].as_u64().expect("a window");
    let inside = moves
        .iter()
        .filter(|(asked, done)| *asked >= opened && *done <= closed)
        .count() as u64;
    let counted: u64 = result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .flat_map(|output| output["events"].as_array().cloned().unwrap_or_default())
        .filter_map(|event| {
            event["attrs"]
                .as_array()?
                .iter()
                .find(|attr| attr["name"] == "migrations")?["value"]
                .as_u64()
        })
        .sum();
    eprintln!(
        "{} moves, {inside} inside the run's window; {counted} migrations counted",
        moves.len()
    );
    assert!(
        inside >= 10,
        "only {inside} of the moves fell inside the run's window, too few to judge:\n{text}"
    );
    assert!(
        counted >= inside,
        "{inside} forced moves between two cpus inside the window are {inside} migrations, \
         and {counted} were counted:\n{text}"
    );
    // A pinned cpu-bound task migrates for no other reason, so anything above
    // every move made, the two at the window's edges included, is a
    // migration being counted twice.
    assert!(
        counted <= moves.len() as u64,
        "{counted} migrations were counted for {} moves:\n{text}",
        moves.len()
    );
    assert_eq!(
        result["losses"]["dropped"].as_u64(),
        Some(0),
        "migrations lost to a full map cannot be checked against a known count:\n{text}"
    );
}

/// Symbolising a stack costs a read of a process's mappings and then of its
/// binaries' symbol tables, and a machine-wide run meets every process on the
/// machine. The emit layer keeps that table only when the request asked for
/// `stack`, so a run that asked for anything else was paying for it and
/// throwing it away.
///
/// Measured on this machine before the collector was told what the request
/// wanted: `--system --duration 1s --emit summary` had not finished after
/// ninety seconds, having spent seventy of them in user time. Afterwards it
/// takes 1.2. The bound below is twenty times the measurement and a fraction of
/// what it was, which is the shape a timing assertion has to have to be worth
/// making at all.
#[test]
fn a_machine_wide_run_that_asked_for_no_stacks_does_not_symbolise_the_machine() {
    let started = std::time::Instant::now();
    let output = run(&[
        "sched.offcpu",
        "--system",
        "--duration",
        "1s",
        "--emit",
        "raw",
    ]);
    let elapsed = started.elapsed();
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    if refused_for_capabilities(&text) {
        return;
    }
    assert!(output.status.success(), "{text}");
    assert!(
        elapsed < std::time::Duration::from_secs(25),
        "a one-second machine-wide run took {elapsed:?}, which is the whole machine being \
         symbolised for a table nothing will read"
    );
    assert!(
        text.contains("sched.offcpu"),
        "the run produced no events at all, so it proves nothing about what it skipped:\n{text}"
    );
    // The other half of the same fact, and the one that does not depend on a
    // clock: an id whose table is empty points at nothing, so it is not put on
    // the event either.
    assert!(
        !text.contains("stack_id"),
        "a request that did not ask for stacks carries stack ids nothing can resolve:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// The family's own attach ordering
// ---------------------------------------------------------------------------

/// The classic tracepoint path is never the first thing tried for an event a
/// raw tracepoint reaches just as well.
///
/// `DECISIONS.md` settled the ordering: `bpf:raw_tp` has strictly fewer
/// requirements than `bpf:tp` and goes ahead of it. What makes that more than a
/// preference on this machine is finding 15 -- a `bpf:tp` requirement probes as
/// *available* when only its directory is readable, so a chain that leads with
/// one resolves and then fails at attach time, after a target has been
/// launched.
///
/// The rule is about `bpf:tp`, the backend that declares itself `equivalent`
/// and therefore offers nothing the raw path does not. A backend ahead of
/// `bpf:raw_tp` that offers *more* -- `sched.offcpu`'s `bpf:tp:stacks`, which
/// adds the kernel side of a blocking stack -- is the chain working as
/// `docs/contributing.md` says it should: best first, and the resolver handles
/// availability.
///
/// The second half is checked against the running kernel rather than against
/// the catalog: a raw tracepoint exists exactly when the kernel publishes a
/// `btf_trace_<name>` type for it, which is what the probe reads, so an entry
/// naming one this kernel does not have would be ordering its chain in front
/// of nothing.
#[test]
fn no_sched_backend_needs_tracefs_before_one_that_does_not() {
    let listing = run(&["list", "sched"]);
    assert!(
        listing.status.success(),
        "{}",
        String::from_utf8_lossy(&listing.stderr)
    );
    let sources: Vec<String> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|word| word.starts_with("sched."))
        .map(str::to_owned)
        .collect();
    assert!(
        sources.len() >= 10,
        "the family has ten sources: {sources:?}"
    );

    let mut checked = 0;
    for source in sources {
        let output = run(&["list", &source]);
        let text = String::from_utf8_lossy(&output.stdout);

        // The chain, in the order the resolver walks it. A backend line starts
        // with its position: `1. bpf:raw_tp  cost med ...`.
        let chain: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with(|c: char| c.is_ascii_digit()) && line.contains(". "))
            .filter_map(|line| line.split_whitespace().nth(1))
            .collect();

        for line in text.lines().map(str::trim) {
            if let Some(rest) = line.strip_prefix("needs    raw_tracepoint:") {
                // The probe reads the kernel's own BTF and needs no
                // privileges, so this is an answer on any machine.
                assert!(
                    rest.contains("available"),
                    "{source} orders a raw tracepoint this kernel does not have ahead of the \
                     classic path, which is ordering it ahead of nothing:\n{line}"
                );
            }
        }

        let Some(classic) = chain.iter().position(|id| *id == "bpf:tp") else {
            continue;
        };
        checked += 1;
        let raw = chain.iter().position(|id| *id == "bpf:raw_tp");
        assert!(
            raw.is_some_and(|raw| raw < classic),
            "{source} reaches a tracepoint through tracefs at position {} and by name at {raw:?}; \
             `bpf:tp` declares itself equivalent, so it offers nothing to be ahead for. Chain: \
             {chain:?}",
            classic + 1
        );
    }
    assert!(
        checked >= 7,
        "only {checked} sources in this family declare a classic tracepoint backend, and the \
         rule this test exists for applies to seven of them"
    );
}

// ---------------------------------------------------------------------------
// A process target and the processes it starts
// ---------------------------------------------------------------------------

/// A program that forks children that each block a known number of times, and
/// that each write the kernel's own account of their run queue waits when they
/// are done: `<pid> <run_delay ns> <pcount>` from `/proc/thread-self/schedstat`,
/// one line each, appended to the file named on the command line.
///
/// `children` zero is the program blocking itself. The sleep is 20 us: short
/// enough that a wait's few hundred nanoseconds of queue are a measurable part
/// of it, and long enough to leave the cpu every time.
const FORKING_BLOCKER: &str = r#"
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void block(int times, const char *out) {
    struct timespec pause = {0, 20000};
    for (int i = 0; i < times; i++) nanosleep(&pause, 0);
    char text[256] = {0};
    int stat = open("/proc/thread-self/schedstat", O_RDONLY);
    if (stat < 0 || read(stat, text, sizeof text - 1) <= 0) exit(2);
    close(stat);
    unsigned long long cpu, delay, slices;
    if (sscanf(text, "%llu %llu %llu", &cpu, &delay, &slices) != 3) exit(3);
    char line[128];
    int length = snprintf(line, sizeof line, "%d %llu %llu\n", getpid(), delay, slices);
    int file = open(out, O_WRONLY | O_APPEND | O_CREAT, 0644);
    if (file < 0 || write(file, line, length) != length) exit(4);
    close(file);
}

int main(int argc, char **argv) {
    if (argc != 4) return 1;
    int children = atoi(argv[1]), times = atoi(argv[2]);
    if (children == 0) { block(times, argv[3]); return 0; }
    for (int k = 0; k < children; k++)
        if (fork() == 0) { block(times, argv[3]); _exit(0); }
    while (wait(0) > 0) {}
    return 0;
}
"#;

/// Compiles one of this file's C programs into the test's scratch directory,
/// or says why it could not.
fn compiled(name: &str, source: &str, flags: &[&str]) -> Option<std::path::PathBuf> {
    let directory = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let source_path = directory.join(format!("{name}.c"));
    let binary = directory.join(name);
    std::fs::write(&source_path, source).ok()?;
    let status = Command::new("cc")
        .args(flags)
        .arg("-o")
        .arg(&binary)
        .arg(&source_path)
        .status()
        .ok()?;
    status.success().then_some(binary)
}

/// Per thread group, the sum of one attribute over every event of one result
/// of a JSON document, and the sum of the events' weights.
fn by_process(
    result: &serde_json::Value,
    attribute: &str,
) -> std::collections::BTreeMap<u64, (u64, u64)> {
    let mut sums = std::collections::BTreeMap::new();
    for output in result["outputs"].as_array().into_iter().flatten() {
        for event in output["events"].as_array().into_iter().flatten() {
            let attr = |name: &str| {
                event["attrs"]
                    .as_array()?
                    .iter()
                    .find(|attr| attr["name"] == name)?["value"]
                    .as_u64()
            };
            if let (Some(pid), Some(value), Some(weight)) =
                (attr("pid"), attr(attribute), attr("weight"))
            {
                let entry = sums.entry(pid).or_insert((0, 0));
                entry.0 += value;
                entry.1 += weight;
            }
        }
    }
    sums
}

/// The run queue waits and the blocks of a launched program's children are
/// measured, and a child's waits are what the kernel says they were.
///
/// Two defects, one workload (`docs/open-findings.md`, the seventeenth wave,
/// stream build). `sched.runqlat` and `sched.offcpu` compared one thread group
/// written into their programs, so a compiler driver's children -- where a
/// build's time is -- had no row at all, where `cpu.profile` followed them.
/// And a wait was timed between the `sched_wakeup` and `sched_switch`
/// tracepoints, which on this machine is about 1.5 us a wait longer than the
/// kernel's own `run_delay`: a cpu coming out of idle for the task it was
/// woken for, which `sched_info` leaves in blocked time. A program blocking
/// 50 us at a time was 1 630 ns runnable a wait by the tracepoints and 121 ns
/// by its `schedstat`, ten times over.
///
/// The reference is each child's own `schedstat`, read by the child as its
/// last act: its `run_delay` is every wait it had until then, from its fork.
/// What a run counts for the child can be no less than that and no more than
/// that and the waits of its last instructions. The tracepoints' account is
/// beside it on every row as `wake_to_run_ns`, and that it is larger is the
/// separation: three thousand waits at a microsecond each is outside the
/// bracket by itself.
#[test]
fn a_launched_programs_children_are_followed_and_wait_what_their_schedstat_says() {
    const CHILDREN: usize = 3;
    const TIMES: u64 = 3000;
    /// The waits of a child after it read its `schedstat`: a write and an
    /// exit, a preemption at most.
    const AFTER_THE_READ_NS: u64 = 2_000_000;
    let Some(program) = compiled("omn-forking-blocker", FORKING_BLOCKER, &["-O1"]) else {
        eprintln!("skipped: no C compiler (`cc`) to build the forking workload with");
        return;
    };
    let reference = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("omn-forking-blocker-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&reference);
    let launch = serde_json::to_string(&[
        program.to_str().expect("a utf-8 path"),
        &CHILDREN.to_string(),
        &TIMES.to_string(),
        reference.to_str().expect("a utf-8 path"),
    ])
    .expect("a launch line");
    let session = format!(
        "name: children\nformat: json\nrequests:\n  - label: runnable\n    source: \
         sched.runqlat\n    target: &program\n      launch: {launch}\n    mode: trace\n    \
         emit: [raw]\n  - label: blocked\n    source: sched.offcpu\n    target: *program\n    \
         mode: trace\n    emit: [raw]\n"
    );
    let session_path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("omn-forking-blocker-{}.yml", std::process::id()));
    std::fs::write(&session_path, session).expect("the session is written");
    let output = run(&[
        "-f",
        session_path.to_str().expect("a utf-8 path"),
        "--format",
        "json",
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    if refused_for_capabilities(&text) {
        return;
    }
    assert!(output.status.success(), "{text}");
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one json document");
    let results = document["results"].as_array().expect("results");
    let labelled = |label: &str| {
        results
            .iter()
            .find(|result| result["label"] == label)
            .unwrap_or_else(|| panic!("no `{label}` result:\n{text}"))
    };
    let waits = by_process(labelled("runnable"), "total_ns");
    let woken = by_process(labelled("runnable"), "wake_to_run_ns");
    let queued = by_process(labelled("blocked"), "queued_ns");

    let children: Vec<(u64, u64, u64)> = std::fs::read_to_string(&reference)
        .expect("every child wrote its schedstat")
        .lines()
        .map(|line| {
            let fields: Vec<u64> = line
                .split_whitespace()
                .map(|field| field.parse().expect("a number"))
                .collect();
            (fields[0], fields[1], fields[2])
        })
        .collect();
    assert_eq!(children.len(), CHILDREN, "every child wrote its line");
    for (pid, run_delay, slices) in children {
        let Some(&(measured, counted)) = waits.get(&pid) else {
            panic!(
                "child {pid} of the launched program waited {run_delay} ns on a run queue by its \
                 own schedstat and has no row: the run did not follow it:\n{waits:?}"
            );
        };
        let (blocked_queue, blocks) = queued.get(&pid).copied().unwrap_or((0, 0));
        let (by_tracepoints, _) = woken.get(&pid).copied().unwrap_or((0, 0));
        eprintln!(
            "child {pid}: {slices} waits, {run_delay} ns by its schedstat; counted {counted} \
             waits, {measured} ns ({by_tracepoints} ns by the tracepoints); {blocks} blocks, \
             {blocked_queue} ns of them queued"
        );
        assert!(
            blocks >= TIMES,
            "child {pid} slept {TIMES} times and {blocks} blocks were counted:\n{queued:?}"
        );
        // A wait the kernel stamped on a path neither tracepoint marks, or a
        // run of a program the kernel skipped on a busy cpu, is a handful in
        // three thousand; a child not followed is all of them.
        assert!(
            counted + slices / 100 + 2 >= slices,
            "child {pid} had {slices} waits by its schedstat and {counted} were counted"
        );
        assert!(
            measured + run_delay / 50 >= run_delay,
            "child {pid} waited {run_delay} ns by its schedstat and only {measured} ns were \
             counted"
        );
        assert!(
            measured <= run_delay + run_delay / 50 + AFTER_THE_READ_NS,
            "child {pid} waited {run_delay} ns by its schedstat, and {measured} ns were \
             counted: more than that and its last instructions' waits"
        );
        assert!(
            blocked_queue <= run_delay + run_delay / 50 + AFTER_THE_READ_NS,
            "child {pid}'s blocks were {blocked_queue} ns queued, more than the {run_delay} ns \
             it waited on a run queue at all"
        );
    }
    let _ = std::fs::remove_file(&reference);
    let _ = std::fs::remove_file(&session_path);
}

// ---------------------------------------------------------------------------
// A block's stack, named
// ---------------------------------------------------------------------------

/// A program that blocks once, three static functions below `main`, built with
/// frame pointers so the kernel's walk reaches them. Stripped, those three are
/// the names only its symbol table had: the file's dynamic symbols are what it
/// imports.
const STATIC_BLOCKER: &str = r#"
#include <time.h>

static __attribute__((noinline)) void omn_blocks_here(void) {
    struct timespec pause = {0, 300000000};
    nanosleep(&pause, 0);
    __asm__ volatile("");
}

static __attribute__((noinline)) void omn_calls_the_block(void) {
    omn_blocks_here();
    __asm__ volatile("");
}

int main(void) {
    omn_calls_the_block();
    return 0;
}
"#;

/// A debuginfod server for one file, on loopback: the real protocol, `GET
/// /buildid/<hex>/debuginfo`, answered with `debuginfo` for its build id and a
/// 404 for anything else. Answers with its url and how many times it served
/// the file.
fn serve_debuginfo(
    debuginfo: std::path::PathBuf,
    build_id: String,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&served);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 2048];
            let read = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..read]).into_owned();
            let path = request.split_whitespace().nth(1).unwrap_or("");
            if path == format!("/buildid/{build_id}/debuginfo") {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = std::fs::read(&debuginfo).expect("the unstripped build");
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            } else {
                let _ = write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        }
    });
    (url, served)
}

/// A file's build id, as `readelf -n` prints it.
fn build_id_of(path: &std::path::Path) -> Option<String> {
    let output = Command::new("readelf").arg("-n").arg(path).output().ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("Build ID: ").map(str::to_owned))
}

/// `sched.offcpu`'s stacks are named through debuginfod when `--symbols
/// debuginfod` asks, and a stripped file's names are said to be its dynamic
/// symbols only when it does not.
///
/// The case it was filed from (`docs/open-findings.md`, the seventeenth wave,
/// stream cases): a stripped `sqlite3`'s block was `clock_nanosleep <-
/// sqlite3+0x57e9a <- ...`, where `sqliteDefaultBusyCallback` names the cause,
/// because `--symbols` reached `cpu.profile` alone. Here the program is this
/// test's own, stripped, and the unstripped build is served by build id from a
/// server on loopback: the names that come back are the ones only the symbol
/// table had, and they come from nowhere else.
///
/// Measured beside it against a real server on the development machine
/// (`https://debuginfod.archlinux.org`, `/usr/bin/sleep 0.3`): `sleep+0x2835;
/// ...;clock_nanosleep` without, `main;xnanosleep;rpl_nanosleep;
/// clock_nanosleep@@GLIBC_2.17` with, in 1.25 s cold.
#[test]
fn a_stripped_blocks_stack_is_named_from_debuginfod_only_when_asked() {
    let flags = [
        "-O1",
        "-fno-omit-frame-pointer",
        "-fno-inline",
        "-Wl,--build-id",
    ];
    let Some(unstripped) = compiled("omn-static-blocker", STATIC_BLOCKER, &flags) else {
        eprintln!("skipped: no C compiler (`cc`) to build the blocking workload with");
        return;
    };
    let stripped = unstripped.with_file_name("omn-static-blocker-stripped");
    let copied = std::fs::copy(&unstripped, &stripped).is_ok()
        && Command::new("strip")
            .arg(&stripped)
            .status()
            .is_ok_and(|status| status.success());
    let Some(build_id) = build_id_of(&stripped).filter(|_| copied) else {
        eprintln!("skipped: no `strip` or `readelf` to make and read a stripped build with");
        return;
    };
    let (url, served) = serve_debuginfo(unstripped.clone(), build_id.clone());
    let cache = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("omn-offcpu-debuginfod-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache);
    let stripped_path = stripped.to_str().expect("a utf-8 path");
    let traced = |symbols: Option<&str>, urls: Option<&str>| {
        let mut command = Command::new(BINARY);
        command.args(["sched.offcpu", "--emit", "stack"]);
        if let Some(symbols) = symbols {
            command.args(["--symbols", symbols]);
        }
        command.args(["--", stripped_path]);
        command.env("DEBUGINFOD_CACHE_PATH", &cache);
        match urls {
            Some(urls) => command.env("DEBUGINFOD_URLS", urls),
            None => command.env_remove("DEBUGINFOD_URLS"),
        };
        let output = command.output().expect("omn runs");
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        (output.status.success(), text)
    };

    // Not asked, with a server named: nothing is fetched, the static
    // functions are addresses, and the run says why.
    let (succeeded, local) = traced(None, Some(&url));
    if refused_for_capabilities(&local) {
        return;
    }
    assert!(succeeded, "{local}");
    assert!(
        local.contains("clock_nanosleep"),
        "the workload blocked in `clock_nanosleep` and the stack does not say so:\n{local}"
    );
    assert!(
        !local.contains("omn_calls_the_block"),
        "a stripped build named a static function from nowhere:\n{local}"
    );
    assert!(
        local.contains("names came from") && local.contains("dynamic symbols only"),
        "a stack named from dynamic symbols only must say so:\n{local}"
    );
    assert_eq!(
        served.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a server was asked without --symbols debuginfod"
    );

    // Asked: the static functions are named from the file the server holds.
    let (succeeded, named) = traced(Some("debuginfod"), Some(&url));
    assert!(succeeded, "{named}");
    assert!(
        named.contains("omn_calls_the_block"),
        "the stripped build's block is not named from debuginfod:\n{named}"
    );
    assert!(
        named.contains(&format!("debuginfod, {url}, build id {build_id}")),
        "the run does not say its names came from the server:\n{named}"
    );
    assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Asked with nowhere to ask: refused before the program runs.
    let (succeeded, refused) = traced(Some("debuginfod"), None);
    assert!(!succeeded, "{refused}");
    assert!(refused.contains("DEBUGINFOD_URLS"), "{refused}");
    let _ = std::fs::remove_dir_all(&cache);
}
