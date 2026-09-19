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
fn distinctly_named_shell(name: &str) -> Option<std::path::PathBuf> {
    // Fifteen characters is what the kernel keeps of a name, so the file is
    // named inside that: a workload whose `comm` is truncated is one the
    // filter cannot match.
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
    fn start(count: usize, name: &str) -> Option<Self> {
        let shell = distinctly_named_shell(name)?;
        let cpu = std::thread::available_parallelism().ok()?.get() - 1;
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

/// The waits of the pinned tasks, in nanoseconds, by thread.
fn waits_by_task(text: &str) -> std::collections::BTreeMap<u64, u64> {
    let mut totals = std::collections::BTreeMap::new();
    for line in text.lines().filter(|line| line.contains("sched.runqlat")) {
        let field = |name: &str| {
            let needle = format!("{name}=");
            line.split_whitespace()
                .find_map(|field| field.strip_prefix(&needle))
                .and_then(|value| value.parse::<u64>().ok())
        };
        if let (Some(tid), Some(total)) = (field("tid"), field("total_ns")) {
            *totals.entry(tid).or_insert(0) += total;
        }
    }
    totals
}

/// Ground truth, and the only kind this source can have: the expected number
/// comes from the scheduler's own fair-share arithmetic, not from anything this
/// code computed.
///
/// *N* cpu-bound tasks pinned to one cpu each get `1/N` of it, so each one is
/// runnable and not running for `(N-1)/N` of the window. One task alone on a
/// cpu never queues at all; three of them queue for two thirds of the run,
/// each. Nothing about that prediction comes from the tracer, and it separates
/// a measurement of run queue latency from a measurement of anything else: a
/// program that reported blocked time, or on-cpu time, or the whole window,
/// fails at least one of the two cases.
#[test]
fn tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them() {
    const WINDOW_NS: f64 = 1_000_000_000.0;
    let mut measured = Vec::new();

    let _one_at_a_time = CONTENDED_CPU.lock().expect("the cpu lock");
    for spinners in [1usize, 3] {
        let Some(_load) = PinnedSpinners::start(spinners, "omnrunqshare") else {
            eprintln!("skipped: this machine has no `taskset`, so no cpu can be contended");
            return;
        };
        let output = run(&[
            "sched.runqlat",
            "--system",
            "--duration",
            "1s",
            "--emit",
            "raw",
            "--filter",
            "comm == 'omnrunqshare'",
        ]);
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        if refused_for_capabilities(&text) {
            return;
        }
        assert!(output.status.success(), "{text}");

        let waits = waits_by_task(&text);
        assert_eq!(
            waits.len(),
            spinners,
            "{spinners} pinned tasks must each appear once, saw {waits:?}:\n{text}"
        );
        let share = waits.values().sum::<u64>() as f64 / spinners as f64 / WINDOW_NS;
        measured.push(share);
    }

    let (alone, contended) = (measured[0], measured[1]);
    assert!(
        alone < 0.25,
        "a task alone on a cpu waits for nothing, and this one waited {:.0} % of the window",
        alone * 100.0
    );
    // Two thirds is the prediction; the band is wide enough for a machine that
    // is also doing something else on that cpu, and far too narrow for any of
    // the numbers this source could be confused with.
    assert!(
        (0.45..0.90).contains(&contended),
        "three tasks sharing one cpu each wait two thirds of the window, and these waited \
         {:.0} %",
        contended * 100.0
    );
    assert!(
        contended > alone * 3.0,
        "contending for a cpu must show up as waiting for one: alone {:.1} %, contended {:.1} %",
        alone * 100.0,
        contended * 100.0
    );
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
    let Some(_load) = PinnedSpinners::start(3, "omnrunqbkt") else {
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
        "comm == 'omnrunqbkt'",
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

/// Sums one attribute over the `raw` rendering of a source.
fn sum_over(text: &str, kind: &str, attribute: &str) -> u64 {
    let needle = format!("{attribute}=");
    text.lines()
        .filter(|line| line.contains(kind))
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix(&needle))
                .and_then(|value| value.parse::<u64>().ok())
        })
        .sum()
}

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
    const MOVES: usize = 20;

    if std::thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
        eprintln!("skipped: one cpu, so nothing can be moved between two");
        return;
    }
    let Some(shell) = distinctly_named_shell("omnmigrate") else {
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
    std::thread::sleep(std::time::Duration::from_millis(300));

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
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let Ok(measuring) = measuring else {
        let _ = load.kill();
        panic!("omn should run");
    };
    // Long enough for the program to be loaded and attached before the first
    // move: a migration that happens before the tracepoint is attached is one
    // this test caused and cannot expect to see.
    std::thread::sleep(std::time::Duration::from_millis(800));

    let mut moved = 0;
    for step in 1..=MOVES {
        let cpu = (step % 2).to_string();
        if Command::new("taskset")
            .args(["-cp", &cpu, &pid])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            moved += 1;
        }
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
    assert_eq!(moved, MOVES, "every move must have been asked for:\n{text}");

    let counted = sum_over(&text, "sched.migrations", "migrations");
    assert!(
        counted >= MOVES as u64,
        "{MOVES} forced moves between two cpus are {MOVES} migrations, and {counted} were \
         counted:\n{text}"
    );
    // A pinned cpu-bound task migrates for no other reason, so anything much
    // above the number asked for is a migration being counted twice.
    assert!(
        counted <= MOVES as u64 + 4,
        "{counted} migrations were counted for {MOVES} moves:\n{text}"
    );
    assert!(
        text.contains("no loss"),
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
