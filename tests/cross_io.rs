//! The I/O path, crossed: what one read cost, from the block layer to the
//! reader waking up, and on what evidence each part of it is attributed.
//!
//! Every source this file joins already collects and is already checked
//! against ground truth in `tests/scenario_io.rs` and `tests/scenario_sched.rs`.
//! What is new here is the arithmetic that puts them in one table and the
//! honesty about what that table is missing, so the ground truth is chosen to
//! test exactly that:
//!
//! - **The reader keeps its own clock.** Every read is timestamped on
//!   `CLOCK_MONOTONIC` immediately before and after the `pread`, by the thread
//!   that issues it. The tool's timestamps are on the same clock, so every
//!   request it reports has to lie *inside* the read that issued it, with no
//!   slack at all: a request that began before its read was called, or ended
//!   after its read returned, is a request the tool got wrong. That is the
//!   check that makes the decomposition a measurement rather than a sum.
//! - **A known delay is injected where the tool cannot see.** Half the reads
//!   spin for 300 us of their own work inside the timed window, before calling
//!   `pread`. That time is above the block layer, which nothing in this build
//!   observes, so it must land in the residue -- every such read's residue at
//!   least 300 us -- and in no measured segment.
//! - **One read is one request** when the page cache is out of the way and the
//!   reads are one page each, issued one at a time. The upper bound is exact.
//! - **A read the page cache answers reaches no device**, so a decomposition of
//!   it has no device segment at all, and a positive control in the same window
//!   proves the tool was watching.
//! - **A pointer recycled between two readers** is told apart by the scope and
//!   not by the pointer: the scoped join cannot credit a reader with more
//!   requests than it issued, and the bare one does.
//!
//! What is *not* checked, with the numbers that show why, is in
//! `docs/scenarios/cross-io.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");
const PAGE: usize = 4096;

/// How long a session is given to load and attach its programs before the
/// workload starts. Three eBPF program sets load in sequence, and a read issued
/// before the last of them attached is a read no count can contain.
const LEAD_IN: Duration = Duration::from_millis(1000);
/// How long after the workload the window stays open: the rings are drained on
/// a ten-millisecond tick, and the last completion has to be drained before the
/// run ends.
const TAIL: Duration = Duration::from_millis(400);

// ---------------------------------------------------------------------------
// Running the tool

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

fn json(output: &Output) -> Value {
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|error| panic!("the json output parses ({error}): {}", stdout(output)))
}

fn spawn(args: &[&str]) -> std::process::Child {
    Command::new(BINARY)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run")
}

/// Runs sessions around a workload that happens inside all of their windows.
fn measure_while<T>(sessions: &[&Path], workload: impl FnOnce() -> T) -> (Vec<Output>, T) {
    let children: Vec<_> = sessions
        .iter()
        .map(|session| spawn(&["-f", &session.to_string_lossy(), "--format", "json"]))
        .collect();
    std::thread::sleep(LEAD_IN);
    let value = workload();
    std::thread::sleep(TAIL);
    let outputs = children
        .into_iter()
        .map(|child| child.wait_with_output().expect("omn should finish"))
        .collect();
    (outputs, value)
}

/// Writes a session this test built, beside the build, and returns its path.
///
/// Built rather than read from `examples/` because it has to name this
/// process's own reader -- by `comm`, since `cargo test` runs every test of a
/// file as a thread of one process and only the thread's name tells them
/// apart -- and the examples name a program somebody runs by hand. The examples
/// are run as they are, by `the_crossing_sessions_run_and_say_what_they_found`.
fn session_file(name: &str, yaml: &str) -> PathBuf {
    let path = scratch(&format!("{name}.yml"));
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the scratch directory");
    std::fs::write(&path, yaml).expect("the session written");
    path
}

fn result<'a>(document: &'a Value, label: &str) -> &'a Value {
    document["results"]
        .as_array()
        .expect("a run reports its results")
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("the session has a request labelled `{label}`: {document}"))
}

fn join<'a>(document: &'a Value, rule: &str) -> &'a Value {
    document["joins"]
        .as_array()
        .expect("the session declares joins")
        .iter()
        .find(|join| join["rule"] == rule)
        .unwrap_or_else(|| panic!("the join `{rule}` ran: {}", document["joins"]))
}

fn raw(result: &Value) -> &[Value] {
    result["outputs"]
        .as_array()
        .expect("a result carries its outputs")
        .iter()
        .find(|output| output["emit"] == "raw")
        .and_then(|output| output["events"].as_array())
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("the request emits `raw`: {result}"))
}

fn attr<'a>(event: &'a Value, name: &str) -> Option<&'a Value> {
    event["attrs"]
        .as_array()?
        .iter()
        .find(|attr| attr["name"] == name)
        .map(|attr| &attr["value"])
}

fn uint(event: &Value, name: &str) -> u64 {
    attr(event, name)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("`{name}` on {event}"))
}

/// The request key, `(pointer, open_ts)`.
fn blk(event: &Value) -> (u64, u64) {
    let key = event["keys"]
        .as_array()
        .and_then(|keys| keys.iter().find_map(|key| key.get("blk")))
        .unwrap_or_else(|| panic!("a block event carries a blk key: {event}"));
    (
        key["ptr"].as_u64().expect("a pointer"),
        key["open_ts"].as_u64().expect("a scope"),
    )
}

fn dropped(result: &Value) -> u64 {
    result["losses"]["dropped"].as_u64().unwrap_or(0)
}

fn count(join: &Value, field: &str) -> u64 {
    join[field]
        .as_u64()
        .unwrap_or_else(|| panic!("`{field}` on the join: {join}"))
}

// ---------------------------------------------------------------------------
// The reader

fn scratch(name: &str) -> PathBuf {
    // `CARGO_TARGET_TMPDIR` is inside the build directory, on the repository's
    // own filesystem. `/tmp` is a tmpfs on most machines, and a read of a file
    // there reaches no block device at all.
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn monotonic_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Safety: `clock_gettime` writes one `timespec` this frame owns.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

fn this_thread() -> i32 {
    // Safety: `gettid` takes nothing and returns this thread's id.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

/// Renames this thread, which is what the block half's `comm` filter reads.
///
/// Distinct per test and per process, so two tests of this file, or two
/// checkouts running their suites at once, never narrow onto each other.
fn name_this_thread(prefix: &str) -> String {
    let name = format!("{prefix}{}", std::process::id() % 1_000_000);
    let bytes = std::ffi::CString::new(name.clone()).expect("a name without a nul");
    // Safety: `PR_SET_NAME` reads a nul-terminated string of at most 16 bytes,
    // and this one is at most 11.
    unsafe { libc::prctl(libc::PR_SET_NAME, bytes.as_ptr() as libc::c_ulong, 0, 0, 0) };
    name
}

/// Pins the calling thread to one cpu.
fn pin_this_thread(cpu: usize) -> bool {
    // Safety: the set is this frame's own and `sched_setaffinity` reads it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

/// The first cpu this process may run on.
fn an_allowed_cpu() -> Option<usize> {
    // Safety: as above, and `sched_getaffinity` writes the set.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return None;
        }
        (0..libc::CPU_SETSIZE as usize).find(|cpu| libc::CPU_ISSET(*cpu, &set))
    }
}

struct AlignedPage {
    storage: Vec<u8>,
    offset: usize,
}

impl AlignedPage {
    fn new() -> Self {
        let storage = vec![0u8; 2 * PAGE];
        let offset = (PAGE - (storage.as_ptr() as usize % PAGE)) % PAGE;
        Self { storage, offset }
    }

    fn pointer(&mut self) -> *mut libc::c_void {
        // Safety: `offset` is under `PAGE` and the allocation is `2 * PAGE`.
        unsafe { self.storage.as_mut_ptr().add(self.offset) as *mut libc::c_void }
    }
}

/// A file of `pages` pages that is on the device, not only in memory.
///
/// `sync_all` matters and it cost a session to find: on btrfs a direct read of
/// a file written and not yet written back is answered from the cache, and the
/// first run of the example session gave 78 requests for 2000 reads.
fn file_on_the_device(name: &str, pages: usize) -> PathBuf {
    use std::io::Write;
    let path = scratch(name);
    let bytes = pages * PAGE;
    if std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0) < bytes as u64 {
        std::fs::create_dir_all(path.parent().expect("a parent")).ok();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut blob = Vec::with_capacity(bytes);
        while blob.len() < bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            blob.extend_from_slice(&state.to_ne_bytes());
        }
        blob.truncate(bytes);
        let mut file = std::fs::File::create(&path).expect("a file to read from");
        file.write_all(&blob).expect("the whole file written");
        file.sync_all().expect("the file on the device");
    }
    path
}

/// Tells the page cache to forget a file.
///
/// Needed even for `O_DIRECT`, which is the second thing this crossing found:
/// on btrfs the first direct read of a file whose pages are resident is
/// answered without a block request, so 2000 reads gave 1999 requests in every
/// run until the cache was emptied first, and then gave 2000.
fn forget(path: &Path) {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::File::open(path).expect("the file");
    // Safety: a descriptor this scope owns, and no pointers.
    let advised = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    assert_eq!(
        advised, 0,
        "the page cache could not be told to forget the file"
    );
}

/// How many of a file's pages the page cache holds, asked of the kernel with
/// `mincore` rather than inferred.
fn resident_pages(path: &Path) -> (usize, usize) {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::File::open(path).expect("the file");
    let length = file.metadata().expect("its size").len() as usize;
    let pages = length.div_ceil(PAGE);
    // Safety: a shared read-only mapping of a file this scope holds open,
    // unmapped before returning; `mincore` writes one byte per page into a
    // vector of exactly that many.
    unsafe {
        let base = libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        );
        assert_ne!(base, libc::MAP_FAILED, "the file could not be mapped");
        let mut vector = vec![0u8; pages];
        let status = libc::mincore(base, length, vector.as_mut_ptr());
        libc::munmap(base, length);
        assert_eq!(status, 0, "mincore refused the mapping");
        (vector.iter().filter(|byte| **byte & 1 == 1).count(), pages)
    }
}

/// One read, as the reader saw it.
#[derive(Clone, Copy)]
struct Read {
    /// `CLOCK_MONOTONIC` just before the read, and before any injected work.
    called: u64,
    /// `CLOCK_MONOTONIC` just after the read returned.
    returned: u64,
    /// Nanoseconds of the reader's own work spent inside the window before the
    /// read was issued.
    injected: u64,
}

/// Reads one page at a time, timing each read on the reader's own clock.
///
/// Every `spin_every`-th read (never, when zero) spins for `spin` nanoseconds
/// inside its window before calling `pread`.
fn timed_reads(
    path: &Path,
    count: usize,
    direct: bool,
    spin_every: usize,
    spin: u64,
) -> std::io::Result<Vec<Read>> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(if direct { libc::O_DIRECT } else { 0 })
        .open(path)?;
    let fd = file.as_raw_fd();
    let mut page = AlignedPage::new();
    let pointer = page.pointer();
    let mut reads = Vec::with_capacity(count);
    for index in 0..count {
        let injected = if spin_every > 0 && index % spin_every == 0 {
            spin
        } else {
            0
        };
        let called = monotonic_ns();
        while monotonic_ns() < called + injected {
            std::hint::spin_loop();
        }
        // Safety: the descriptor is open for reading and the buffer is a page
        // of this scope's own memory, aligned as `O_DIRECT` requires.
        let read = unsafe { libc::pread(fd, pointer, PAGE, (index * PAGE) as libc::off_t) };
        let returned = monotonic_ns();
        if read != PAGE as isize {
            return Err(std::io::Error::last_os_error());
        }
        reads.push(Read {
            called,
            returned,
            injected,
        });
    }
    Ok(reads)
}

// ---------------------------------------------------------------------------
// What this machine cannot serve, said out loud

/// Why reads of the build directory would reach no block device, or `None`.
fn no_device_here() -> Option<String> {
    let path = match Path::new(env!("CARGO_TARGET_TMPDIR")).canonicalize() {
        Ok(path) => path,
        Err(error) => return Some(format!("the build directory cannot be resolved: {error}")),
    };
    let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut best: Option<(usize, String, String)> = None;
    for line in table.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let (Some(point), Some(options)) = (
            before.split_whitespace().nth(4),
            before.split_whitespace().nth(5),
        ) else {
            continue;
        };
        let mut fields = after.split_whitespace();
        let (Some(fstype), Some(source)) = (fields.next(), fields.next()) else {
            continue;
        };
        let super_options = fields.next().unwrap_or("");
        if path.starts_with(point) && best.as_ref().is_none_or(|(len, ..)| point.len() > *len) {
            best = Some((
                point.len(),
                format!("{fstype} on {source}"),
                format!("{options},{super_options}"),
            ));
        }
    }
    let Some((_, what, options)) = best else {
        return Some(format!(
            "nothing in the mount table covers {}",
            path.display()
        ));
    };
    if !what.contains(" on /dev/") {
        return Some(format!(
            "the build directory is {what}, which is not a block device, so no read of it \
             becomes a block request"
        ));
    }
    if options.contains("compress") {
        return Some(format!(
            "the build directory is {what} mounted with compression, and a direct read of a \
             compressed extent is served through the page cache rather than as one request"
        ));
    }
    None
}

/// Why this binary cannot load a block tracer, or `None`.
///
/// Every relink drops `CAP_BPF` and `CAP_PERFMON`, so a suite run after a plain
/// `cargo build` finds no eBPF source able to attach. That is a fact about the
/// binary, and a crossing that passed through it quietly would report an I/O
/// path that was never observed.
fn no_bpf_here() -> Option<String> {
    let output = run(&[
        "io.size", "--system", "-m", "trace", "-e", "summary", "-d", "100ms",
    ]);
    if output.status.success() {
        return None;
    }
    Some(format!(
        "this build cannot load a block tracer: {}. `make` grants CAP_BPF and CAP_PERFMON to \
         target/debug/omn, and every `cargo build` drops them again",
        stderr(&output).lines().next().unwrap_or("").trim()
    ))
}

macro_rules! skip_unless_served {
    () => {
        if let Some(reason) = no_device_here().or_else(no_bpf_here) {
            eprintln!("skipped: {reason}");
            return;
        }
    };
}

// ---------------------------------------------------------------------------
// The decomposition

/// One block request, from the tool's two halves of it.
struct Request {
    issued: u64,
    completed: u64,
    /// The queue half, when the machine-wide session kept it: its duration,
    /// and the issue time `io.queue` recorded -- a second program's reading of
    /// the same moment `issued` is derived from.
    queue: Option<(u64, u64)>,
    device_ns: u64,
}

/// One read, split into what the tool saw and what it did not.
struct Split {
    read: Read,
    request: Option<Request>,
    /// Completion to the first wakeup of the reader after it, when there was
    /// one inside the read. None means the reader was never put to sleep for
    /// this read, or the wakeup was lost; the run's losses say which.
    wake_ns: Option<u64>,
    /// Who issued that wakeup, as `sched.wakeup` reports it.
    waker: Option<u64>,
}

impl Split {
    fn total(&self) -> u64 {
        self.read.returned - self.read.called
    }

    /// Everything the tool did not see: syscall entry, the filesystem, the
    /// submission path, the run queue wait after the wakeup, and the return --
    /// and, on a read that had it, the injected work.
    fn residue(&self) -> u64 {
        let seen = self.request.as_ref().map_or(0, |request| {
            request.queue.map_or(0, |(queue_ns, _)| queue_ns) + request.device_ns
        }) + self.wake_ns.unwrap_or(0);
        self.total() - seen
    }
}

/// The reader's reads, joined to the tool's requests and wakeups.
///
/// Which read a request belongs to is decided by the window: the request names
/// the reader's thread (KEY), and of that thread's reads it is the one whose
/// window contains it (TEMPORAL). Nothing stronger exists in this build. An
/// IDENTITY link would need a request id placed in task storage at the syscall
/// and read back at `block_rq_issue` -- boundaries 3, 5 and 6 of `ROADMAP.md`
/// phase 4 -- and the syscall edge is behind `fentry`.
fn split(
    reads: &[Read],
    requests: Vec<Request>,
    wakeups: &[(u64, u64)],
) -> (Vec<Split>, Vec<Request>) {
    let mut splits: Vec<Split> = reads
        .iter()
        .map(|read| Split {
            read: *read,
            request: None,
            wake_ns: None,
            waker: None,
        })
        .collect();
    let mut strays = Vec::new();
    for request in requests {
        let index = reads.partition_point(|read| read.called <= request.issued);
        let inside = index
            .checked_sub(1)
            .filter(|&at| request.completed <= reads[at].returned);
        match inside {
            Some(at) if splits[at].request.is_none() => splits[at].request = Some(request),
            _ => strays.push(request),
        }
    }
    for split in &mut splits {
        let Some(request) = &split.request else {
            continue;
        };
        let after = wakeups.partition_point(|(ts, _)| *ts < request.completed);
        if let Some((ts, waker)) = wakeups
            .get(after)
            .filter(|(ts, _)| *ts <= split.read.returned)
        {
            split.wake_ns = Some(ts - request.completed);
            split.waker = Some(*waker);
        }
    }
    (splits, strays)
}

fn percentile(values: &mut [u64], share: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let rank = ((share * values.len() as f64).ceil() as usize).clamp(1, values.len());
    values[rank - 1]
}

fn human(ns: u64) -> String {
    match ns {
        0..=9_999 => format!("{ns}ns"),
        10_000..=9_999_999 => format!("{:.1}us", ns as f64 / 1e3),
        _ => format!("{:.2}ms", ns as f64 / 1e6),
    }
}

/// The table this crossing exists to print, in the house style, on stderr so
/// the suite shows it under `--nocapture`.
fn render(
    title: &str,
    splits: &[Split],
    evidence: &BTreeMap<&str, String>,
    losses: &str,
) -> String {
    use std::fmt::Write;
    let whole: u64 = splits.iter().map(Split::total).sum();
    let mut out = String::new();
    let _ = writeln!(out, "{title}");
    let _ = writeln!(
        out,
        "  each segment reaches its read through task:tid (KEY) inside the read's own window \
         (TEMPORAL);\n  IDENTITY would need a request id propagated from the syscall, which needs fentry"
    );
    let _ = writeln!(out, "  {losses}");
    let _ = writeln!(
        out,
        "  {:<24} {:<9} {:>6} {:>10} {:>10} {:>11} {:>7}",
        "SEGMENT", "EVIDENCE", "READS", "P50", "P99", "TOTAL", "SHARE"
    );
    let _ = writeln!(
        out,
        "  {:<24} {:<9} {:>6} {:>10} {:>10} {:>11} {:>7}   needs fentry: io.syscall, io.fs",
        "syscall -> block issue", "NOT SEEN", "-", "-", "-", "-", "-"
    );
    let mut row = |name: &str, level: &str, mut values: Vec<u64>| {
        let total: u64 = values.iter().sum();
        let share = if whole == 0 {
            0.0
        } else {
            100.0 * total as f64 / whole as f64
        };
        let _ = writeln!(
            out,
            "  {:<24} {:<9} {:>6} {:>10} {:>10} {:>11} {:>6.1}%",
            name,
            level,
            values.len(),
            human(percentile(&mut values, 0.5)),
            human(percentile(&mut values, 0.99)),
            human(total),
            share
        );
    };
    let requests = || splits.iter().filter_map(|split| split.request.as_ref());
    row(
        "software queue",
        &evidence["blk"],
        // Only the requests whose queue half the machine-wide session kept: a
        // missing half is not a queueing time of zero.
        requests()
            .filter_map(|request| request.queue.map(|(queue_ns, _)| queue_ns))
            .collect(),
    );
    row(
        "device",
        &evidence["blk"],
        requests().map(|request| request.device_ns).collect(),
    );
    row(
        "completion -> wakeup",
        &evidence["wakeup"],
        splits.iter().filter_map(|split| split.wake_ns).collect(),
    );
    row("residue", "-", splits.iter().map(Split::residue).collect());
    row(
        "whole read",
        "own clock",
        splits.iter().map(Split::total).collect(),
    );
    let _ = writeln!(
        out,
        "  the residue is everything above: syscall entry, the filesystem, submission, the run \
         queue after the wakeup, the return{}",
        if splits.iter().any(|split| split.read.injected > 0) {
            ", and the injected work"
        } else {
            ""
        }
    );
    out
}

fn losses_line(results: &[&Value]) -> String {
    let short: Vec<String> = results
        .iter()
        .filter(|result| dropped(result) > 0)
        .map(|result| format!("{} dropped {}", result["label"], dropped(result)))
        .collect();
    if short.is_empty() {
        "no loss in any source: every count below is a measurement".to_owned()
    } else {
        format!(
            "LOSS: {} -- the counts below are lower bounds",
            short.join(", ")
        )
    }
}

/// The session that sees the block half, machine-wide and narrowed by `comm`.
///
/// Machine-wide because `io.queue` serves no process target (the catalog gives
/// it `system` and `cgroup`), and narrowed by `comm` because the block
/// collectors apply that filter and say how much it removed.
fn block_half(name: &str, comm: &str) -> PathBuf {
    session_file(
        name,
        &format!(
            "name: {name}\nformat: json\nduration: 4s\nrequests:\n\
             \x20 - {{label: queue, source: io.queue, target: system, mode: trace, emit: [raw], \
             filter: \"comm == '{comm}'\"}}\n\
             \x20 - {{label: device, source: io.latency, target: system, mode: trace, emit: [raw], \
             filter: \"comm == '{comm}'\"}}\n\
             joins:\n\
             \x20 - {{id: halves, from: io.queue, to: io.latency, via: blk, match: scoped_eq, \
             window: 30s}}\n"
        ),
    )
}

/// The session that sees the device half and the wakeup half, narrowed in the
/// kernel to this process.
///
/// Separate from the machine-wide one for two reasons, both measured. A block
/// tracer narrowed by `comm` applies the predicate after the drain, so on a
/// machine where something else is driving the device its ring overflows with
/// the neighbours' requests before the reader's are picked out -- one run here
/// lost 382889 of them. A process target is applied in the kernel. And
/// `sched.wakeup` ignores a `comm` filter altogether, so machine-wide it would
/// be every wakeup on a machine running a test suite.
///
/// A process and not the reader's thread, which is defect 2 of
/// `docs/scenarios/cross-io.md`: on the scheduler half a `tid` target is
/// filtered as a thread group id, so for any thread but a process's first it
/// keeps nothing, and says nothing. The reader's own events are picked out
/// afterwards, by `tid` and `wakee_tid`.
fn process_half(name: &str) -> PathBuf {
    let pid = std::process::id();
    session_file(
        name,
        &format!(
            "name: {name}\nformat: json\nduration: 4s\nrequests:\n\
             \x20 - {{label: wakeups, source: sched.wakeup, target: {{pid: [{pid}]}}, mode: trace, \
             emit: [raw]}}\n\
             \x20 - {{label: device, source: io.latency, target: {{pid: [{pid}]}}, mode: trace, \
             emit: [raw]}}\n\
             joins:\n\
             \x20 - {{id: released, from: sched.wakeup, to: io.latency, via: task, match: wakeup, \
             waker: waker_tid, depth: 3, window: 30s}}\n"
        ),
    )
}

/// The reader's requests: the device half from the process session, the queue
/// half from the machine-wide one, matched on the scoped key.
fn requests_of(block: &Value, process: &Value, tid: i32) -> Vec<Request> {
    let queued: BTreeMap<(u64, u64), &Value> = raw(result(block, "queue"))
        .iter()
        .map(|event| (blk(event), event))
        .collect();
    raw(result(process, "device"))
        .iter()
        .filter(|event| uint(event, "tid") == tid as u64)
        .map(|event| {
            let completed = event["ts_ns"].as_u64().expect("a timestamp");
            let device_ns = uint(event, "latency_ns");
            Request {
                issued: completed - device_ns,
                completed,
                queue: queued.get(&blk(event)).map(|queue| {
                    (
                        uint(queue, "queue_ns"),
                        queue["ts_ns"].as_u64().expect("a timestamp"),
                    )
                }),
                device_ns,
            }
        })
        .collect()
}

/// One thread's wakeups, as `(timestamp, waker)`, in time order.
fn wakeups_of(document: &Value, tid: i32) -> Vec<(u64, u64)> {
    let mut wakeups: Vec<(u64, u64)> = raw(result(document, "wakeups"))
        .iter()
        .filter(|event| uint(event, "wakee_tid") == tid as u64)
        .map(|event| {
            (
                event["ts_ns"].as_u64().expect("a timestamp"),
                uint(event, "waker_tid"),
            )
        })
        .collect();
    wakeups.sort_unstable();
    wakeups
}

/// The tasks that delivered the completions, most frequent first, named from
/// `/proc` after the run.
///
/// The name is a hint and not a record. A kworker renames itself after the
/// work item it is running, so what `/proc/<tid>/comm` says now is the work it
/// took up since, not the one that woke the reader -- the same worker has read
/// `btrfs-endio`, `events_unbound` and `btrfs-delayed-meta` in three runs here.
/// That it is a kworker and not the interrupt is the measurement; which queue
/// it served is not. `sched.wakeup` carries the wakee's `comm` and not the
/// waker's, which `docs/open-findings.md` asks for.
fn wakers(splits: &[Split]) -> String {
    let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
    for waker in splits.iter().filter_map(|split| split.waker) {
        *counts.entry(waker).or_default() += 1;
    }
    let mut ranked: Vec<(u64, usize)> = counts.into_iter().collect();
    ranked.sort_by_key(|(_, times)| std::cmp::Reverse(*times));
    let named: Vec<String> = ranked
        .iter()
        .take(3)
        .map(|(tid, times)| {
            let name = if *tid == 0 {
                "idle, an interrupt".to_owned()
            } else {
                std::fs::read_to_string(format!("/proc/{tid}/comm"))
                    .map(|comm| comm.trim().to_owned())
                    .unwrap_or_else(|_| "gone".to_owned())
            };
            format!("task:{tid} ({name}) {times}x")
        })
        .collect();
    if named.is_empty() {
        "nobody: no completion was followed by a wakeup inside its read".to_owned()
    } else {
        named.join(", ")
    }
}

/// Checks both sessions exited and returns their documents.
fn documents(outputs: &[Output]) -> Vec<Value> {
    outputs
        .iter()
        .map(|output| {
            assert!(output.status.success(), "{}", stderr(output));
            json(output)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// A. The decomposition, against the reader's own clock

/// Every read is split into the segments the tool saw and a residue it did
/// not, and every segment lies inside the read it is credited to.
///
/// The containment is the ground truth and it carries no slack, because there
/// is nothing to carry: the reader's timestamps and the tool's are both
/// `CLOCK_MONOTONIC`, read on this machine, and a request is issued after its
/// `pread` was called and completes before it returns. A collector whose
/// latency were an uptime, whose clock were `CLOCK_REALTIME`, or whose join
/// attached a request to the wrong life of its pointer, fails it on the first
/// read.
///
/// The injected work is the check on the residue. It happens inside the window
/// and above the block layer, so on every read that carries it the request
/// cannot start before `called + injected`, and the residue is at least the
/// injection. A decomposition that absorbed the unseen segment into a seen one
/// -- the rounding the brief warns about -- fails there.
#[test]
fn every_read_decomposes_inside_the_readers_own_clock() {
    skip_unless_served!();

    const READS: usize = 2000;
    const SPIN_EVERY: usize = 2;
    const INJECTED: u64 = 300_000;

    let path = file_on_the_device("cross-io-reads.bin", READS);
    let tid = this_thread();
    let comm = name_this_thread("omnxa");
    let block = block_half("cross-io-decompose-block", &comm);
    let wakeup = process_half("cross-io-decompose-process");

    forget(&path);
    let (outputs, reads) = measure_while(&[&block, &wakeup], || {
        timed_reads(&path, READS, true, SPIN_EVERY, INJECTED)
    });
    let reads = match reads {
        Ok(reads) => reads,
        Err(error) => {
            eprintln!(
                "skipped: {} refuses a one-page O_DIRECT read ({error}), so a read of it is not \
                 one block request",
                path.display()
            );
            return;
        }
    };
    let documents = documents(&outputs);
    let (block, wakeup) = (&documents[0], &documents[1]);

    let requests = requests_of(block, wakeup, tid);
    let seen = requests.len();
    let wakeups = wakeups_of(wakeup, tid);
    let halves = join(block, "halves");
    let released = join(wakeup, "released");
    let evidence: BTreeMap<&str, String> = [
        ("blk", halves["evidence"].as_str().unwrap_or("?").to_owned()),
        (
            "wakeup",
            released["evidence"].as_str().unwrap_or("?").to_owned(),
        ),
    ]
    .into_iter()
    .collect();
    let losses = losses_line(&[
        result(block, "queue"),
        result(block, "device"),
        result(wakeup, "device"),
        result(wakeup, "wakeups"),
    ]);

    let (splits, strays) = split(&reads, requests, &wakeups);

    eprint!(
        "{}",
        render(
            &format!(
                "one read, {READS} of them: task:{tid} ({comm}), one page with O_DIRECT, every \
                 {SPIN_EVERY}nd carrying {} of its own work",
                human(INJECTED)
            ),
            &splits,
            &evidence,
            &losses,
        )
    );
    let missing: Vec<String> = splits
        .iter()
        .enumerate()
        .filter(|(_, split)| split.request.is_none())
        .map(|(index, split)| format!("#{index} in {}", human(split.total())))
        .collect();
    eprintln!(
        "  reads that reached no device: {}",
        match missing.len() {
            0 => "none".to_owned(),
            1..=5 => missing.join(", "),
            more => format!("{more}, the first {}", missing[..5].join(", ")),
        }
    );
    eprintln!(
        "  who woke the reader after its completions, named now rather than then: {}",
        wakers(&splits)
    );

    // Every request the reader's thread submitted lies inside one of its reads,
    // and no read holds two. This is the containment, with no slack.
    assert!(
        strays.is_empty(),
        "{} of {seen} requests lie outside every read of the thread that submitted them, or \
         share a read with another -- first at issue {} and completion {}, against reads from {} \
         to {}",
        strays.len(),
        strays[0].issued,
        strays[0].completed,
        reads[0].called,
        reads[READS - 1].returned
    );

    // One read is one request. Exact above; the lower bound is the window's
    // edge, and every mistake this checks for is out by a factor rather than a
    // percent.
    let placed = splits
        .iter()
        .filter(|split| split.request.is_some())
        .count();
    assert!(
        placed <= READS,
        "{placed} requests for {READS} reads: something is counted twice"
    );
    if dropped(result(wakeup, "device")) == 0 {
        assert!(
            100 * placed >= 99 * READS,
            "{READS} one-page direct reads, and only {placed} were found as a request inside \
             their own window, with nothing dropped"
        );
    } else {
        eprintln!(
            "not asserted: the lower bound on requests, because `io.latency` dropped {} and a \
             shortfall the tool admits is not one it hid",
            dropped(result(wakeup, "device"))
        );
    }

    // The residue carries what the tool cannot see, and never less than what
    // was put there on purpose.
    for split in &splits {
        if let Some(request) = &split.request {
            assert!(
                request.issued >= split.read.called + split.read.injected,
                "a request issued {} into its read, before the {} of work the reader did first \
                 had ended: the tool's clock and the reader's disagree",
                human(request.issued - split.read.called),
                human(split.read.injected)
            );
        }
        assert!(
            split.residue() >= split.read.injected,
            "a residue of {} on a read that spent {} of its own work before the request: the \
             unseen segment was absorbed into a seen one",
            human(split.residue()),
            human(split.read.injected)
        );
    }

    // The queue half joined the device half on the scoped key, for every one of
    // the reader's requests, and said at what level.
    assert_eq!(
        evidence["blk"], "KEY",
        "two halves of one request share a typed key, which is KEY and nothing stronger"
    );
    assert_eq!(
        evidence["wakeup"], "WAKEUP",
        "a wakeup edge is WAKEUP evidence, which is weaker than a key and has to say so"
    );
    let matched = count(halves, "matched");
    assert!(
        matched as usize <= READS,
        "the scoped join matched {matched} completions for {READS} reads"
    );
    if dropped(result(block, "queue")) + dropped(result(block, "device")) == 0 {
        // A separation, for the reason given in the scoped-key test below.
        assert!(
            2 * matched as usize >= READS,
            "the scoped join matched {matched} of {READS} requests with nothing dropped"
        );
    } else {
        eprintln!(
            "not asserted: the lower bound on the scoped join, because the machine-wide session \
             dropped {} issues and {} completions -- the neighbours' I/O, not the reader's",
            dropped(result(block, "queue")),
            dropped(result(block, "device"))
        );
    }

    // And the join put each queue half on the right life of its pointer. The
    // issue time is read twice, by two programs loaded by two runs: once by
    // `io.queue` as the moment it closed, once derived from `io.latency` as
    // completion minus latency. A queue half matched to another life of the
    // pointer carries another request's issue, which lies in another read.
    for split in &splits {
        let Some(request) = &split.request else {
            continue;
        };
        let Some((_, queue_issued)) = request.queue else {
            continue;
        };
        assert!(
            split.read.called + split.read.injected <= queue_issued
                && queue_issued <= request.completed,
            "a queue half joined to this read's request says it was issued {} from the read's \
             start, outside the read or after the completion: the scoped key put it on another \
             life of the pointer",
            queue_issued as i64 - split.read.called as i64
        );
    }
}

// ---------------------------------------------------------------------------
// C. The completion and the wakeup that released the reader, by the engine

/// Boundary 8 of the crossing, joined by the engine: each completion paired
/// with the next wakeup of the task it was for, and the distance a part of
/// the read's decomposition.
///
/// The reader's own clock is the truth. Every one-page `O_DIRECT` read blocks
/// until its request completes, so each request's completion is followed,
/// inside the same read, by the wakeup that let the reader return: the pair
/// the `woke` match makes must lie inside the read -- the wakeup no later
/// than `returned` -- and there must be one per request, give or take what a
/// source reported dropping. The engine used to have only `wakeup`, which pairs
/// each request with the wakeup *before* it (the previous read's delivery).
///
/// The decomposition is checked for containment: the syscall time of the
/// reader's `pread64`s, read off `io.syscall`, is inside its brackets, and the
/// device time and the completion-to-wakeup time are inside the syscall, so
/// the residue is never negative.
#[test]
fn every_completion_is_paired_with_the_wakeup_that_released_its_reader() {
    skip_unless_served!();

    const READS: usize = 1000;

    let path = file_on_the_device("cross-io-woke.bin", READS);
    let tid = this_thread();
    name_this_thread("omnxw");
    let pid = std::process::id();
    let text = format!(
        "name: completion-released\nformat: json\nduration: 4s\nrequests:\n\
         \x20 - {{label: syscalls, source: io.syscall, target: {{pid: [{pid}]}}, mode: trace, \
         emit: [raw], filter: \"syscall == 'pread64'\"}}\n\
         \x20 - {{label: device, source: io.latency, target: {{pid: [{pid}]}}, mode: trace, \
         emit: [raw]}}\n\
         \x20 - {{label: wakeups, source: sched.wakeup, target: {{pid: [{pid}]}}, mode: trace, \
         emit: [raw]}}\n\
         joins:\n\
         \x20 - {{id: completion-released, from: io.latency, to: sched.wakeup, via: task, \
         match: woke, woken: wakee_tid, waker: waker_tid, window: 1s, top: 50}}\n"
    );
    let session_path = session_file("cross-io-woke", &text);

    forget(&path);
    let (outputs, reads) =
        measure_while(&[&session_path], || timed_reads(&path, READS, true, 0, 0));
    let reads = match reads {
        Ok(reads) => reads,
        Err(error) => {
            eprintln!(
                "skipped: {} refuses a one-page O_DIRECT read ({error})",
                path.display()
            );
            return;
        }
    };
    let output = &outputs[0];
    assert!(output.status.success(), "{}", stderr(output));
    let run: omn::encode::RunOutput = serde_json::from_str(&stdout(output))
        .unwrap_or_else(|error| panic!("the run parses ({error})"));
    let document = json(output);
    let released = join(&document, "completion-released");
    assert_eq!(released["evidence"], "WAKEUP");

    // The reader's requests, completion times from the tool, and whether a
    // wakeup of the reader lies after each completion and inside its read.
    let requests: Vec<Request> = raw(result(&document, "device"))
        .iter()
        .filter(|event| uint(event, "tid") == tid as u64)
        .map(|event| {
            let completed = event["ts_ns"].as_u64().expect("a timestamp");
            let device_ns = uint(event, "latency_ns");
            Request {
                issued: completed - device_ns,
                completed,
                queue: None,
                device_ns,
            }
        })
        .collect();
    let wakeups = wakeups_of(&document, tid);
    let (splits, strays) = split(&reads, requests, &wakeups);
    assert!(
        strays.is_empty(),
        "{} requests outside every read",
        strays.len()
    );
    let served = splits
        .iter()
        .filter(|split| split.request.is_some())
        .count();
    let woken_inside = splits
        .iter()
        .filter(|split| split.wake_ns.is_some())
        .count();

    let task = format!("task:{tid}");
    let rows: Vec<&Value> = released["top"]
        .as_array()
        .expect("rows")
        .iter()
        .filter(|row| row["key"].as_str().is_some_and(|key| key.ends_with(&task)))
        .collect();
    let paired: u64 = rows
        .iter()
        .map(|row| row["matched"].as_u64().unwrap_or(0))
        .sum();
    let waited: f64 = rows
        .iter()
        .map(|row| row["change"]["delta"].as_f64().unwrap_or(0.0))
        .sum();
    let bracket: u64 = splits
        .iter()
        .filter_map(|split| {
            split
                .request
                .as_ref()
                .map(|request| split.read.returned - request.completed)
        })
        .sum();
    eprintln!(
        "{READS} reads, {served} reached the device; {woken_inside} woken inside their read by \
         the reader's clock; the engine paired {paired}, waiting {} in all against {} between \
         each completion and its read's return; by {:?}",
        human(waited as u64),
        human(bracket),
        rows.iter()
            .map(|row| format!("{} x{}", row["key"].as_str().unwrap_or("?"), row["matched"]))
            .collect::<Vec<_>>()
    );

    // One pair per completion that a wakeup inside its read followed, and no
    // more than there were requests.
    assert!(
        paired as usize <= served,
        "{paired} pairs for {served} requests: a completion was paired twice"
    );
    let lossless =
        dropped(result(&document, "device")) + dropped(result(&document, "wakeups")) == 0;
    if lossless {
        assert_eq!(
            paired as usize, woken_inside,
            "the reader's clock puts a wakeup after the completion and inside the read for \
             {woken_inside} requests, and the engine paired {paired}"
        );
        assert!(
            100 * woken_inside >= 99 * served,
            "a one-page direct read blocks until its request completes, and only {woken_inside} \
             of {served} requests were followed by a wakeup inside their read"
        );
    }
    // Each pair lies inside its read: its wait ends before the read returned.
    assert!(
        waited <= bracket as f64,
        "the engine's completion-to-wakeup time, {}, exceeds what the reader's own clock allows \
         between the completions and the returns, {}",
        human(waited as u64),
        human(bracket)
    );

    // The decomposition, through the library: the part a join defines.
    let spec: omn::request::Session = serde_norway::from_str(&format!(
        "{text}decompose:\n\
         \x20 - {{id: one-read, top: 100, whole: {{from: io.syscall, value: total_ns}}, parts: [\
         {{state: device, from: io.latency, value: latency_ns}}, \
         {{state: completion -> wakeup, from: completion-released, value: paired}}]}}\n"
    ))
    .expect("the session parses");
    let catalog = omn::catalog::Catalog::embedded().expect("the catalog");
    omn::join::check_decomposition_in(&spec.decompose[0], &spec.requests, &spec.joins, &catalog)
        .expect("a decomposition this session can compute");
    let events: Vec<omn::event::Event> = run
        .results
        .iter()
        .flat_map(|results| &results.outputs)
        .filter_map(|output| match output {
            omn::emit::Output::Raw { events } => Some(events.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    let decomposed = omn::join::decompose_with(
        &spec.decompose[0],
        &spec.requests,
        &run.results,
        &events,
        &spec.joins,
    );
    let table = decomposed
        .tables
        .iter()
        .find(|table| table.key == task)
        .expect("a table for the reader");
    let whole = table.whole.amount.expect("the syscalls were read");
    let brackets: u64 = reads.iter().map(|read| read.returned - read.called).sum();
    let row = |state: &str| {
        table
            .rows
            .iter()
            .find(|row| row.state == state)
            .unwrap_or_else(|| panic!("no `{state}` row"))
    };
    eprintln!(
        "  whole {} (brackets {}), device {}, completion -> wakeup {} ({:?}), residue {}",
        human(whole as u64),
        human(brackets),
        human(row("device").amount.unwrap_or(0.0) as u64),
        human(row("completion -> wakeup").amount.unwrap_or(0.0) as u64),
        row("completion -> wakeup").evidence,
        table
            .residue
            .map_or("-".to_owned(), |residue| format!("{residue:.0}ns"))
    );
    assert_eq!(
        row("completion -> wakeup").evidence,
        Some(omn::join::Evidence::Wakeup)
    );
    assert!(
        whole <= brackets as f64,
        "the syscall time {} is longer than the reads by their own clock, {}",
        human(whole as u64),
        human(brackets)
    );
    if lossless && dropped(result(&document, "syscalls")) == 0 {
        assert!(
            (row("completion -> wakeup").amount.unwrap_or(0.0) - waited).abs() < 1.0,
            "the part and the join disagree about the same pairs"
        );
        let residue = table.residue.expect("every row was read");
        assert!(
            residue >= 0.0,
            "a residue of {residue:.0} ns: the device and the wakeup were counted outside the \
             syscalls that contain them"
        );
    }
}

// ---------------------------------------------------------------------------
// B. The scoped key, against a bare pointer

/// A completion is never credited to a request that had already finished.
///
/// Two threads read one-page pages from one file, **pinned to one cpu**, so they
/// share one hardware queue and therefore one pool of request structures: each
/// is handed pointers the other has just released. The session watches the
/// issues of one of them and the completions of everybody, and joins the two
/// twice -- once scoped, once on the bare pointer.
///
/// The ground truth is the reader's own count. It issued `READS` reads, one
/// request each, so the scoped join cannot credit it with more: anything above
/// `READS` is a completion of the other thread's request attributed to one of
/// the reader's, which is exactly the failure the scope exists to prevent. The
/// bare join is then required to do what the scope prevents, so the run proves
/// the recycling happened rather than assuming it.
#[test]
fn no_completion_is_credited_to_a_request_that_had_already_finished() {
    skip_unless_served!();

    const READS: usize = 3000;
    const NOISE: usize = 12000;
    const PAGES: usize = 4096;

    let Some(cpu) = an_allowed_cpu() else {
        eprintln!("skipped: this process's cpu affinity could not be read");
        return;
    };
    // A file each. Two readers of one file are not two sources of one request
    // per read on btrfs: a direct read of a page the other reader's traffic
    // left resident can be answered from the cache, and a shared file gave
    // 2960 requests for 3000 reads with nothing dropped.
    let path = file_on_the_device("cross-io-pair.bin", PAGES);
    let noise_path = file_on_the_device("cross-io-noise.bin", PAGES);
    let comm = name_this_thread("omnxb");
    let session = session_file(
        "cross-io-stale",
        &format!(
            "name: stale\nformat: json\nduration: 4s\nrequests:\n\
             \x20 - {{label: issues, source: io.queue, target: system, mode: trace, \
             emit: [summary], filter: \"comm == '{comm}'\"}}\n\
             \x20 - {{label: completions, source: io.latency, target: system, mode: trace, \
             emit: [summary]}}\n\
             joins:\n\
             \x20 - {{id: scoped, from: io.queue, to: io.latency, via: blk, match: scoped_eq, \
             window: 30s}}\n\
             \x20 - {{id: bare, from: io.queue, to: io.latency, via: blk, match: eq, \
             window: 30s}}\n"
        ),
    );

    forget(&path);
    forget(&noise_path);
    let (outputs, issued) = measure_while(&[&session], || {
        std::thread::scope(|scope| {
            let noise = scope.spawn(|| {
                name_this_thread("omnxn");
                pin_this_thread(cpu) && scattered_reads(&noise_path, NOISE, PAGES, 31).is_ok()
            });
            let pinned = pin_this_thread(cpu);
            let issued = scattered_reads(&path, READS, PAGES, 7919);
            (pinned && noise.join().unwrap_or(false), issued)
        })
    });
    let (pinned, issued) = issued;
    if !pinned {
        eprintln!("skipped: the two readers could not both be pinned to cpu {cpu}");
        return;
    }
    let Ok(issued) = issued else {
        eprintln!(
            "skipped: {} refuses a one-page O_DIRECT read",
            path.display()
        );
        return;
    };
    let document = &documents(&outputs)[0];
    let scoped = join(document, "scoped");
    let bare = join(document, "bare");
    let (credited, bare_credited) = (count(scoped, "matched"), count(bare, "matched"));
    let completions = count(scoped, "matched") + count(scoped, "unmatched");

    eprintln!(
        "the reader issued {issued} requests; the scoped join credited it with {credited}, the \
         bare pointer with {bare_credited}, of {completions} completions on the machine -- {} of \
         them another thread's, landing on a request the reader had already finished",
        bare_credited.saturating_sub(credited)
    );

    assert!(
        credited as usize <= issued,
        "the scoped join credited {credited} completions to a reader that issued {issued} \
         requests: a completion was attributed to a request that had already finished"
    );
    // The lower bound only establishes that the reader was observed at all, so
    // it is a separation and not a threshold: the failures it guards against --
    // no join, the wrong key, a filter that matched nobody -- credit nothing,
    // while a working run on this machine credits 2960 to 2999 of 3000. The
    // shortfall below 3000 is reads btrfs answered without a request (see
    // `docs/scenarios/cross-io.md`), and a 99 % bar on it measured the machine.
    if dropped(result(document, "issues")) + dropped(result(document, "completions")) == 0 {
        assert!(
            2 * credited as usize >= issued,
            "the scoped join found only {credited} of the {issued} requests the reader issued, \
             with nothing dropped"
        );
    }
    assert_eq!(scoped["evidence"], "KEY");
    // Two lives of one pointer inside one plug section share their whole key,
    // because the kernel's start stamp is the plug's cached clock. The engine
    // pairs them by order and says so: a join that saw a collapse says the
    // crediting within the key was the clock's, and one that saw none says
    // nothing.
    let collapsed = scoped["states"]["scope_collapsed"].as_u64().unwrap_or(0);
    eprintln!("the scoped join saw {collapsed} pointers reissued under the scope they already had");
    assert_eq!(
        scoped["within_key"].as_str() == Some("TEMPORAL"),
        collapsed > 0,
        "{collapsed} collapsed scopes, and the join says within_key {}",
        scoped["within_key"]
    );
    // Defect 3 of `docs/scenarios/cross-io.md`, fixed by the eighth wave's
    // stream J: a bare equality on the recycled pointer compares the address
    // alone, which establishes two events at one address around the same
    // time, and is rated for exactly that (`docs/families/join.md`).
    assert_eq!(bare["evidence"], "TEMPORAL");

    if bare_credited <= credited {
        eprintln!(
            "skipped the half that proves the recycling: the bare pointer credited {bare_credited} \
             and the scope {credited}, so no pointer passed from one reader to the other on cpu \
             {cpu} in this run, and the run says nothing about what the scope prevents"
        );
        return;
    }
    // What the scope prevented, and it is a separation rather than a margin: the
    // other thread issued four times as many requests, and a bare pointer hands
    // most of them to the reader.
    assert!(
        bare_credited as usize > issued,
        "the bare pointer credited {bare_credited}, no more than the {issued} the reader issued"
    );
}

/// Reads one page at a time at offsets scattered by `stride` over `pages`.
fn scattered_reads(
    path: &Path,
    count: usize,
    pages: usize,
    stride: usize,
) -> std::io::Result<usize> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)?;
    let fd = file.as_raw_fd();
    let mut page = AlignedPage::new();
    let pointer = page.pointer();
    for index in 0..count {
        let at = (index * stride % pages) * PAGE;
        // Safety: as in `timed_reads`.
        let read = unsafe { libc::pread(fd, pointer, PAGE, at as libc::off_t) };
        if read != PAGE as isize {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// C. The negative: a read that never reaches a device

/// A read the page cache answers has no device segment, and the table says so
/// rather than inventing one.
///
/// The property is the kernel's: `mincore` says every page of the file is
/// resident before the reads and still resident after them, so no buffered
/// read of it had anything to fetch. The positive control is the other half and
/// it is not optional: a window in which nothing happened and a tool that was
/// not watching look identical, so the same thread then issues direct reads in
/// the same window, and those must be found.
#[test]
fn a_read_the_page_cache_answers_reaches_no_device() {
    skip_unless_served!();

    const CACHED: usize = 2000;
    const CONTROL: usize = 200;

    let path = file_on_the_device("cross-io-cached.bin", CACHED);
    let tid = this_thread();
    let comm = name_this_thread("omnxc");
    let block = block_half("cross-io-cached-block", &comm);
    let wakeup = process_half("cross-io-cached-process");

    // Warm the whole file through the cache, then ask the kernel whether it is.
    let _ = timed_reads(&path, CACHED, false, 0, 0);
    let (before, pages) = resident_pages(&path);
    if before < pages {
        eprintln!(
            "skipped: only {before} of the file's {pages} pages stayed resident after it was read \
             whole, so a buffered read of it might reach the device"
        );
        return;
    }

    let control_path = file_on_the_device("cross-io-control.bin", CONTROL);
    forget(&control_path);
    let (outputs, (cached, after, control)) = measure_while(&[&block, &wakeup], || {
        let cached = timed_reads(&path, CACHED, false, 0, 0);
        let after = resident_pages(&path).0;
        let control = timed_reads(&control_path, CONTROL, true, 0, 0);
        (cached, after, control)
    });
    let (Ok(cached), Ok(control)) = (cached, control) else {
        eprintln!("skipped: a read of the scratch files failed");
        return;
    };
    if after < pages {
        eprintln!(
            "skipped: {} of the file's {pages} pages were evicted while it was being read, so \
             some reads may have reached the device and the negative cannot be asserted",
            pages - after
        );
        return;
    }
    let documents = documents(&outputs);
    let (block, wakeup) = (&documents[0], &documents[1]);
    let wakeups = wakeups_of(wakeup, tid);

    let mut reads = cached.clone();
    reads.extend_from_slice(&control);
    let (splits, strays) = split(&reads, requests_of(block, wakeup, tid), &wakeups);
    let (cached_splits, control_splits) = splits.split_at(CACHED);
    let evidence: BTreeMap<&str, String> = [
        (
            "blk",
            join(block, "halves")["evidence"]
                .as_str()
                .unwrap_or("?")
                .to_owned(),
        ),
        (
            "wakeup",
            join(wakeup, "released")["evidence"]
                .as_str()
                .unwrap_or("?")
                .to_owned(),
        ),
    ]
    .into_iter()
    .collect();
    eprint!(
        "{}",
        render(
            &format!("{CACHED} one-page reads the page cache answered, task:{tid} ({comm})"),
            cached_splits,
            &evidence,
            &losses_line(&[result(block, "queue"), result(wakeup, "device")]),
        )
    );

    let found = control_splits
        .iter()
        .filter(|split| split.request.is_some())
        .count();
    assert!(
        100 * found >= 99 * CONTROL,
        "the positive control found {found} of {CONTROL} direct reads: the tool was not watching \
         this thread, and the negative below would be a window in which nothing was looked at"
    );
    let reached = cached_splits
        .iter()
        .filter(|split| split.request.is_some())
        .count();
    assert_eq!(
        reached, 0,
        "{reached} of {CACHED} reads of a file whose every page was resident before and after \
         were credited with a block request"
    );
    assert!(
        strays.is_empty(),
        "{} requests of this thread fell in no read at all",
        strays.len()
    );
    for split in cached_splits {
        assert_eq!(
            split.residue(),
            split.total(),
            "a read that reached no device must be all residue"
        );
    }
}

// ---------------------------------------------------------------------------
// D. A defect the crossing found, held in place

/// `sched.wakeup` and `sched.offcpu` accept a `comm` filter, validate it, and
/// ignore it, and nothing in the run says so.
///
/// Held rather than worked around, the way `docs/scenarios/io.md` holds its
/// defects: this asserts the defect exists, so the day the filter is applied
/// this fails, and whoever fixed it flips the assertion and updates defect 1
/// of `docs/scenarios/cross-io.md`. What it must never become is the silent
/// case in reverse -- a filter applied with no record of how much it removed.
#[test]
fn the_scheduler_half_ignores_a_filter_and_says_nothing() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    const NOBODY: &str = "omn-no-such-task";
    let predicate = format!("comm == '{NOBODY}'");
    for source in ["sched.wakeup", "sched.offcpu"] {
        let output = run(&[
            source, "--system", "-m", "trace", "-e", "raw", "-d", "300ms", "--filter", &predicate,
            "--format", "json",
        ]);
        assert!(output.status.success(), "{source}: {}", stderr(&output));
        let document = json(&output);
        let result = &document["results"][0];
        let events = raw(result);
        let strangers = events
            .iter()
            .filter(|event| attr(event, "comm").and_then(Value::as_str) != Some(NOBODY))
            .count();
        let said = result["degradations"]
            .as_array()
            .is_some_and(|notes| notes.iter().any(|note| note.to_string().contains("filter")));
        eprintln!(
            "{source} --filter \"{predicate}\": {strangers} of {} events belong to other tasks, \
             and the run {} the filter",
            events.len(),
            if said { "mentions" } else { "never mentions" }
        );
        if events.is_empty() {
            eprintln!(
                "{source}: nothing happened in 300 ms, so this run says nothing about the filter"
            );
            continue;
        }
        assert!(
            strangers > 0 && !said,
            "{source} now narrows by `comm` or says it did not: defect 1 of \
             docs/scenarios/cross-io.md is fixed. Flip this assertion, and drop the unfiltered \
             request from examples/cross-io-one-read.yml"
        );
    }
}

/// A `tid` target on the scheduler half keeps nothing for any thread but a
/// process's first, and the run reports no loss.
///
/// The ground truth is a thread of this process -- never its first, since the
/// harness runs tests on threads it spawns -- sleeping sixty times for ten
/// milliseconds, so it is woken at least sixty times and blocked for at least
/// 600 ms. A process target, run over the same window, is the positive control:
/// it must see those wakeups, or the negative below says nothing.
///
/// Held as a defect, like the one above. The collectors hand a thread id to a
/// program that compares it against `tgid`; the two agree only for a thread
/// whose id is its process's, which is the one case every earlier test used.
#[test]
fn a_thread_target_on_the_scheduler_half_keeps_nothing_and_says_nothing() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    const SLEEPS: u64 = 60;
    let tid = this_thread();
    let pid = std::process::id() as i32;
    assert_ne!(tid, pid, "the harness runs a test on a thread of its own");

    for source in ["sched.wakeup", "sched.offcpu"] {
        let by_thread = spawn(&[
            source,
            "--tid",
            &tid.to_string(),
            "-m",
            "trace",
            "-e",
            "raw",
            "-d",
            "2s",
            "--format",
            "json",
        ]);
        let by_process = spawn(&[
            source,
            "--pid",
            &pid.to_string(),
            "-m",
            "trace",
            "-e",
            "raw",
            "-d",
            "2s",
            "--format",
            "json",
        ]);
        std::thread::sleep(LEAD_IN);
        for _ in 0..SLEEPS {
            std::thread::sleep(Duration::from_millis(10));
        }
        let kept = |child: std::process::Child| {
            let output = child.wait_with_output().expect("omn should finish");
            assert!(output.status.success(), "{source}: {}", stderr(&output));
            let document = json(&output);
            let result = &document["results"][0];
            let mine = raw(result)
                .iter()
                .filter(|event| {
                    let field = if source == "sched.wakeup" {
                        "wakee_tid"
                    } else {
                        "tid"
                    };
                    attr(event, field).and_then(Value::as_u64) == Some(tid as u64)
                })
                .count();
            (mine, result["losses"].clone())
        };
        let (through_thread, thread_losses) = kept(by_thread);
        let (through_process, _) = kept(by_process);
        eprintln!(
            "{source}: a thread that slept {SLEEPS} times -- {through_process} of its events \
             through --pid {pid}, {through_thread} through --tid {tid}, whose run reported \
             losses {thread_losses}"
        );
        if through_process == 0 {
            eprintln!(
                "{source}: the process target saw nothing of this thread either, so this run \
                 says nothing about the thread target"
            );
            continue;
        }
        assert!(
            through_thread == 0,
            "{source} --tid now keeps the thread's own events: defect 2 of \
             docs/scenarios/cross-io.md is fixed. Flip this assertion, and point `wakeup_half` \
             in this file at the reader's thread"
        );
    }
}

// ---------------------------------------------------------------------------
// E. The sessions a reader runs by hand

/// Both example sessions run, with the workload their own comments give, and
/// say what they found.
///
/// The workload is read out of the session's comment rather than copied here,
/// so the command a reader pastes is the command this runs.
#[test]
fn the_crossing_sessions_run_and_say_what_they_found() {
    skip_unless_served!();
    if Command::new("python3")
        .arg("-c")
        .arg("")
        .status()
        .map(|status| !status.success())
        .unwrap_or(true)
    {
        eprintln!(
            "skipped: the sessions' workloads are python3 one-liners and there is no python3"
        );
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::create_dir_all(root.join("target")).expect("the target directory");

    // One session after the other: both are machine-wide over the block layer,
    // and run side by side each is the other's neighbour.
    let [(one_read, one_read_started), (stale, _)] =
        ["cross-io-one-read.yml", "cross-io-stale-pointer.yml"].map(|name| {
            let session = root.join("examples").join(name);
            let workload = workload_of(&session);
            let child = spawn(&["-f", &session.to_string_lossy(), "--format", "json"]);
            std::thread::sleep(LEAD_IN);
            let started = monotonic_ns();
            let ran = Command::new("sh")
                .arg("-c")
                .arg(&workload)
                .current_dir(root)
                .output()
                .expect("sh runs");
            assert!(
                ran.status.success(),
                "the workload in {name} failed: {}",
                String::from_utf8_lossy(&ran.stderr)
            );
            let output = child.wait_with_output().expect("omn should finish");
            assert!(output.status.success(), "{name}: {}", stderr(&output));
            (json(&output), started)
        });

    let halves = join(&one_read, "queue-then-device");
    let released = join(&one_read, "what-released-the-reader");
    assert_eq!(halves["evidence"], "KEY");
    assert_eq!(released["evidence"], "WAKEUP");
    let matched = count(halves, "matched") as usize;
    assert!(
        matched <= 2000,
        "the one-liner issues 2000 direct reads and the scoped join matched {matched}"
    );
    // The session is machine-wide and narrowed after the drain, so a neighbour
    // driving the disk fills its rings with requests that are not the reader's:
    // one run beside another suite matched 142. The lower bound is the
    // reader's only when nothing was dropped.
    let lost = dropped(result(&one_read, "queue")) + dropped(result(&one_read, "device"));
    // And only when every program set was attached before the first read:
    // `LEAD_IN` is a guess at how long three sets take to load, and under load
    // it can be short, in which case the first reads were issued while nothing
    // watched them -- neither matched nor lost. The run says when each one
    // opened, on the clock the test read before it started the workload.
    let last_attached = one_read["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter_map(|result| result["window"]["opened_ns"].as_u64())
        .max()
        .unwrap_or(0);
    let late_ms = last_attached.saturating_sub(one_read_started) as f64 / 1e6;
    if lost == 0 && last_attached > one_read_started {
        eprintln!(
            "not asserted: the lower bound on cross-io-one-read's join, which matched {matched} \
             of 2000 -- its last program set opened {late_ms:.1} ms after the workload began, \
             so the first reads were issued while nothing watched them"
        );
    } else if lost == 0 {
        assert!(
            matched >= 1980,
            "the one-liner issues 2000 direct reads and the scoped join matched {matched}, with \
             nothing dropped"
        );
    } else {
        eprintln!(
            "not asserted: the lower bound on cross-io-one-read's join, which matched {matched} \
             of 2000 while its machine-wide block half dropped {lost} records -- the \
             neighbours' I/O, admitted as a loss"
        );
    }
    assert!(
        count(released, "matched") > 0,
        "no request of the reader followed a wakeup of it: {released}"
    );
    // The same boundary paired the right way round: each completion with the
    // next wakeup of the reader. Two thousand direct reads each block until
    // their request completes, so each completion the reader's requests made
    // is followed by a wakeup of it; the pairs are bounded by the completions.
    let paired = join(&one_read, "completion-released");
    assert_eq!(paired["evidence"], "WAKEUP");
    let completions = count(paired, "matched") + count(paired, "unmatched");
    assert!(
        count(paired, "matched") as usize <= 2000 && completions > 0,
        "the reader's completions were paired {} times of {completions}: {paired}",
        count(paired, "matched")
    );
    eprintln!(
        "cross-io-one-read: {matched} requests joined at KEY; chain {}",
        released["top"][0]["key"]
    );

    let (scoped, bare) = (
        count(join(&stale, "scoped"), "matched"),
        count(join(&stale, "bare"), "matched"),
    );
    assert!(
        scoped <= 3000,
        "the issuer made 3000 requests and the scoped join credited it with {scoped}"
    );
    eprintln!("cross-io-stale-pointer: scoped {scoped}, bare {bare}");
}

/// The shell lines under "while it watches" in a session's header comment.
fn workload_of(session: &Path) -> String {
    let text = std::fs::read_to_string(session).expect("the session");
    let lines: Vec<&str> = text
        .lines()
        .skip_while(|line| !line.contains("while it watches"))
        .skip(1)
        .skip_while(|line| line.trim() == "#")
        .take_while(|line| line.trim() != "#")
        .map(|line| line.trim_start_matches('#').trim())
        .collect();
    assert!(
        !lines.is_empty(),
        "{} gives no workload under `while it watches`",
        session.display()
    );
    lines.join("\n")
}
