//! Scenarios for the `io` family: does the tool answer a real question about
//! the block layer, and is the answer right?
//!
//! Ground truth here is a quantity of I/O that this file put through a device
//! on purpose, and the number the tool has to find is known before the run:
//!
//! - **64 MiB written and `fsync`ed** must reach the platter, because `fsync`
//!   does not return until it has. The device moves at least 67108864 bytes.
//! - **64 MiB read after the page cache was told to forget the file** must
//!   come off it. The device moves exactly 67108864 bytes when nothing else is
//!   reading, in `67108864 / max_sectors_kb` requests -- 512 of them on a
//!   device that takes 128 KiB at a time.
//!
//! Neither number comes from the tool, and neither is a plausible constant: a
//! collector that reported sectors as bytes would be out by 512, one that
//! summed every line of `/proc/diskstats` would be out by two or three, and
//! one that reported the counter rather than its increase would be out by the
//! machine's whole uptime. Each of those is a real mistake and each fails here.
//!
//! Three things this file does not measure with a real device, because a
//! running machine cannot be made to produce them on demand: a counter that
//! goes backwards, a device that appears mid-run, and a line for a device that
//! is not there. Those are driven by mounting a `/proc/diskstats` written by
//! hand over the real one, inside an unprivileged user namespace. It needs no
//! root and no hardware, and it is the only way to check the arithmetic
//! exactly.
//!
//! What is *not* checked, and why, is in `docs/scenarios/io.md`, together with
//! the defects these scenarios found.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use omn::Catalog;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// A request was refused because it cannot mean what it asks for.
const EXIT_REJECTED: i32 = 2;
/// The request is valid; this build does not implement the mechanism.
const EXIT_NOT_IMPLEMENTED: i32 = 3;
/// The request is valid and this machine cannot serve it.
///
/// It used to return the same code as a request that cannot mean anything --
/// `src/main.rs` answered every resolver error with `EXIT_REJECTED` -- which is
/// defect 3 of `docs/scenarios/io.md`: a caller scripting around these three
/// answers could not tell "fix your request" from "run this somewhere else".
const EXIT_NO_BACKEND: i32 = 1;

/// The quantity every ground-truth scenario here puts through a device. Large
/// enough that the machine's own background I/O cannot account for it, small
/// enough that a scenario stays under a second of actual transfer.
const KNOWN_BYTES: u64 = 64 << 20;

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

/// Runs several requests at once.
///
/// A sweep over every mode and emit a source declares is a dozen runs, and a
/// counted run cannot be shorter than one tick however little it has to
/// measure. Sequentially that is ten seconds in a suite that finishes in
/// three, so the sweep is run concurrently: nothing in it checks a number, so
/// the runs cannot disturb each other.
fn run_all(batches: &[Vec<String>]) -> Vec<Output> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = batches
            .iter()
            .map(|args| {
                scope.spawn(move || {
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    run(&args)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a run finishes"))
            .collect()
    })
}

fn session(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|error| panic!("the json output parses ({error}): {}", stdout(output)))
}

/// What this machine lacks for `backend` of `source`, as `omn list` probes it
/// -- the requirements it calls missing -- or `None` when nothing it needs is
/// missing here. A backend this build has not written is refused for the
/// machine before the build (`DECISIONS.md`, "one order for unbuilt and
/// missing"), so which refusal a pin or a key hint earns is this answer's.
fn lacks_here(source: &str, backend: &str) -> Option<Vec<String>> {
    let listing = run(&["list", source, "--format", "json"]);
    assert!(listing.status.success(), "{}", stderr(&listing));
    let entries = json(&listing);
    let probed = entries[0]["probes"]
        .as_array()
        .expect("the listing carries each backend's probes")
        .iter()
        .find(|probed| probed["backend"] == backend)
        .unwrap_or_else(|| panic!("`{source}` lists no backend `{backend}`"));
    let missing: Vec<String> = probed["requirements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|requirement| requirement["probe"]["state"] == "missing")
        .filter_map(|requirement| requirement["needs"].as_str().map(str::to_owned))
        .collect();
    (!missing.is_empty()).then_some(missing)
}

/// The result of one labelled request in a session's output.
fn result<'a>(document: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
    document["results"]
        .as_array()
        .expect("a run reports its results")
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("the session has a request labelled `{label}`: {document}"))
}

/// One emit of one result.
fn output_of<'a>(result: &'a serde_json::Value, emit: &str) -> &'a serde_json::Value {
    result["outputs"]
        .as_array()
        .expect("a result carries its outputs")
        .iter()
        .find(|output| output["emit"] == emit)
        .unwrap_or_else(|| panic!("the request emits `{emit}`: {result}"))
}

/// One series of one emit, by the key the events were grouped under.
fn series<'a>(output: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    output["series"]
        .as_array()
        .expect("an emit carries its series")
        .iter()
        .find(|series| series["key"] == key)
}

fn number(value: &serde_json::Value) -> f64 {
    value
        .as_f64()
        .unwrap_or_else(|| panic!("a number: {value}"))
}

// ---------------------------------------------------------------------------
// The device under the file, and what the kernel says about it

/// A block device as `/proc/diskstats` and the tool both name it.
struct Device {
    /// The device whose counters are its own -- the disk, not the partition:
    /// a partition's I/O is counted again on the disk holding it, so the
    /// collector reports only the ones nothing is stacked on.
    leaf: String,
    /// The key the tool groups this device's series under, `dev:<major>+<minor>`.
    key: String,
}

impl Device {
    /// The largest request this device accepts, which is what turns a known
    /// number of bytes into a floor under the operation count.
    fn max_request_bytes(&self) -> u64 {
        read_number(&format!("/sys/block/{}/queue/max_sectors_kb", self.leaf))
            .map(|kb| kb * 1024)
            .unwrap_or(128 * 1024)
    }
}

fn read_number(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The block device backing a path, through the mount table.
///
/// `stat` is not enough: btrfs gives every subvolume an anonymous device
/// number that appears in no other file, so a `st_dev` of `0:41` cannot be
/// matched against `/proc/diskstats`. The mount source can: `/dev/nvme0n1p6`
/// is a name `/sys/class/block` knows.
fn backing_device(path: &Path) -> Result<Device, String> {
    let path = path
        .canonicalize()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let table = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("/proc/self/mountinfo: {error}"))?;

    let mut best: Option<(usize, String, String)> = None;
    for line in table.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let mount_point = match before.split_whitespace().nth(4) {
            Some(point) => point,
            None => continue,
        };
        let numbers = before.split_whitespace().nth(2).unwrap_or_default();
        let source = match after.split_whitespace().nth(1) {
            Some(source) => source,
            None => continue,
        };
        if !path.starts_with(mount_point) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(len, _, _)| mount_point.len() > *len)
        {
            best = Some((mount_point.len(), source.to_owned(), numbers.to_owned()));
        }
    }

    let (source, numbers) = best
        .map(|(_, source, numbers)| (source, numbers))
        .ok_or_else(|| format!("nothing in the mount table covers {}", path.display()))?;
    let named = source
        .strip_prefix("/dev/")
        .ok_or_else(|| format!("`{source}` is not a block device, so it has no counters"))?;

    // A partition's counters are counted again on the disk below it, so the
    // collector reports the disk. Ask sysfs which this is rather than reading
    // the name: `nvme0n1p6` is a partition and `zram0` is not, and no rule
    // about digits and letters gets both right.
    //
    // The mount source is not always a name sysfs knows: the CI's runner
    // mounts its root from `/dev/root`, an alias the kernel gives the boot
    // device. The mount table's own `major:minor` is then the device, and
    // `/sys/dev/block/<major>:<minor>` names it. (On btrfs that number is the
    // subvolume's anonymous one and names nothing, which is why the source is
    // asked first.)
    let by_number = || {
        Path::new("/sys/dev/block")
            .join(&numbers)
            .canonicalize()
            .ok()
            .and_then(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
    };
    let name = if Path::new("/sys/class/block").join(named).exists() {
        named.to_owned()
    } else {
        by_number().ok_or_else(|| {
            format!(
                "/sys/class/block/{named} does not exist, and neither does /sys/dev/block/{numbers}"
            )
        })?
    };
    let sysfs = Path::new("/sys/class/block").join(&name);
    if !sysfs.exists() {
        return Err(format!("/sys/class/block/{name} does not exist"));
    }
    let leaf = if sysfs.join("partition").exists() {
        sysfs
            .canonicalize()
            .ok()
            .and_then(|path| Some(path.parent()?.file_name()?.to_string_lossy().into_owned()))
            .ok_or_else(|| format!("the disk holding {name} could not be found"))?
    } else {
        name
    };

    let numbers = std::fs::read_to_string(format!("/sys/class/block/{leaf}/dev"))
        .map_err(|error| format!("/sys/class/block/{leaf}/dev: {error}"))?;
    let (major, minor) = numbers
        .trim()
        .split_once(':')
        .ok_or_else(|| format!("`{}` is not a major:minor", numbers.trim()))?;
    Ok(Device {
        key: format!("dev:{major}+{minor}"),
        leaf,
    })
}

/// One device's line of `/proc/diskstats`, read by this test rather than by
/// the tool: the second, independent account every scenario is bracketed with.
#[derive(Clone, Copy, Default)]
struct Counters {
    reads: u64,
    read_sectors: u64,
    writes: u64,
    write_sectors: u64,
    /// Requests the block layer merged into another, reads and writes.
    merges: u64,
}

impl Counters {
    fn bytes(&self) -> u64 {
        (self.read_sectors + self.write_sectors) * 512
    }

    fn since(&self, before: &Counters) -> Counters {
        Counters {
            reads: self.reads.saturating_sub(before.reads),
            read_sectors: self.read_sectors.saturating_sub(before.read_sectors),
            writes: self.writes.saturating_sub(before.writes),
            write_sectors: self.write_sectors.saturating_sub(before.write_sectors),
            merges: self.merges.saturating_sub(before.merges),
        }
    }
}

fn counters_of(device: &Device) -> Counters {
    let text = std::fs::read_to_string("/proc/diskstats").expect("/proc/diskstats is readable");
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 11 || fields[2] != device.leaf {
            continue;
        }
        let at = |index: usize| fields[index].parse().unwrap_or(0);
        return Counters {
            reads: at(3),
            read_sectors: at(5),
            writes: at(7),
            write_sectors: at(9),
            merges: at(4) + at(8),
        };
    }
    Counters::default()
}

// ---------------------------------------------------------------------------
// The workloads
//
// Both are named in `docs/scenarios/io.md` under `## Workloads`, with what
// each one guarantees, because phase two reuses them.

fn scratch(name: &str) -> PathBuf {
    // `CARGO_TARGET_TMPDIR` is inside the build directory, which is on the
    // repository's own filesystem. `std::env::temp_dir()` is not: `/tmp` is a
    // tmpfs on most machines, and a scenario about block devices written to
    // one measures nothing at all.
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Bytes no filesystem can compress away, so that a file of `bytes` costs
/// `bytes` on the device.
fn incompressible(bytes: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut blob = Vec::with_capacity(bytes);
    while blob.len() < bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        blob.extend_from_slice(&state.to_ne_bytes());
    }
    blob.truncate(bytes);
    blob
}

/// Writes `KNOWN_BYTES` and does not return until they are on the device.
///
/// The guarantee is `fsync`'s: it does not return until the filesystem has
/// written the data out, so at least this many bytes must have reached the
/// platter while this call was running. The device moves a little more --
/// btrfs writes its own metadata alongside -- which is why every check against
/// this workload is a floor.
fn write_and_sync(path: &Path) {
    use std::io::Write;
    let blob = incompressible(KNOWN_BYTES as usize);
    let mut file = std::fs::File::create(path).expect("a file to write");
    file.write_all(&blob).expect("the whole file written");
    file.sync_all().expect("the file on the device");
}

/// Reads a file of `KNOWN_BYTES` that the page cache has been told to forget.
///
/// `POSIX_FADV_DONTNEED` drops the clean pages of a file the caller owns, so
/// the read that follows has to come off the device. Exactly the file's own
/// length arrives: readahead cannot read past the end of it, and the
/// filesystem's metadata for a file this size was read when it was created.
fn cold_read(path: &Path) -> u64 {
    let file = std::fs::File::open(path).expect("the file to read back");
    // Safety: `posix_fadvise` takes a descriptor this scope owns and no
    // pointers. A kernel that refuses it leaves the pages where they are,
    // which the scenario detects as a read that never reached the device.
    let advised = unsafe {
        libc::posix_fadvise(
            std::os::unix::io::AsRawFd::as_raw_fd(&file),
            0,
            0,
            libc::POSIX_FADV_DONTNEED,
        )
    };
    assert_eq!(
        advised, 0,
        "the page cache could not be told to forget the file"
    );
    drop(file);

    let mut file = std::fs::File::open(path).expect("the file to read back");
    let mut buffer = vec![0u8; 1 << 20];
    let mut read = 0;
    loop {
        match file.read(&mut buffer).expect("reading the file") {
            0 => break,
            count => read += count as u64,
        }
    }
    read
}

/// Runs a session while a workload runs inside its window, and reports what
/// the kernel's own counters did over a window that encloses both.
///
/// The lead-in exists because the tool takes its baseline when it starts, and
/// a workload that begins first would be partly outside the measurement. The
/// enclosing counters are the upper end of every check: the tool cannot have
/// seen more than the kernel counted over a strictly longer window.
fn measure_while(args: &[&str], device: &Device, workload: impl FnOnce()) -> (Output, Counters) {
    let before = counters_of(device);
    let child = Command::new(BINARY)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    std::thread::sleep(Duration::from_millis(300));
    workload();
    let output = child.wait_with_output().expect("omn should finish");
    let after = counters_of(device);
    (output, after.since(&before))
}

/// What the kernel counted on `device` over a span the session's window must
/// enclose, from just before `workload` to just after it, with the monotonic
/// instants that span began and ended at.
struct Enclosed {
    counters: Counters,
    from_ns: u64,
    to_ns: u64,
}

/// [`measure_while`], and also the kernel's counters over the workload alone:
/// the window that encloses the session's is an upper bound on what it can
/// report, and this one -- inside the session's, which the result's own
/// `window` says -- a lower bound.
fn measure_around(
    args: &[&str],
    device: &Device,
    wait: Duration,
    workload: impl FnOnce(),
) -> (Output, Counters, Enclosed) {
    let before = counters_of(device);
    let child = Command::new(BINARY)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    std::thread::sleep(wait);
    let from_ns = monotonic_ns();
    let inner_before = counters_of(device);
    workload();
    let inner_after = counters_of(device);
    let to_ns = monotonic_ns();
    let output = child.wait_with_output().expect("omn should finish");
    let after = counters_of(device);
    (
        output,
        after.since(&before),
        Enclosed {
            counters: inner_after.since(&inner_before),
            from_ns,
            to_ns,
        },
    )
}

/// What the kernel's own `inflight` file said while a workload ran.
#[derive(Default)]
struct QueueSeen {
    /// How many times it was read.
    reads: u64,
    /// The deepest queue one read found.
    deepest: u64,
    /// The time between reads that found the queue non-empty, summed: the
    /// span the queue held something, to the resolution of the reads.
    busy: Duration,
    /// The longest run of consecutive reads that all found it non-empty,
    /// from the first of them to the last: a span in which no read, a
    /// millisecond apart, saw the queue empty.
    longest: Duration,
    /// From the first read to the last.
    span: Duration,
}

/// Runs `workload` while reading `/sys/block/<dev>/inflight` as fast as one
/// thread can, sleeping a millisecond between reads.
fn while_watching_the_queue(device: &Device, workload: impl FnOnce()) -> QueueSeen {
    use std::sync::atomic::{AtomicBool, Ordering};
    let path = format!("/sys/block/{}/inflight", device.leaf);
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let watcher = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut seen = QueueSeen::default();
            let began = Instant::now();
            let mut last = began;
            let mut run_began: Option<Instant> = None;
            while !stop.load(Ordering::Relaxed) {
                let depth: u64 = std::fs::read_to_string(&path)
                    .unwrap_or_default()
                    .split_whitespace()
                    .filter_map(|field| field.parse::<u64>().ok())
                    .sum();
                let now = Instant::now();
                seen.reads += 1;
                seen.deepest = seen.deepest.max(depth);
                if depth > 0 {
                    seen.busy += now - last;
                    let began = *run_began.get_or_insert(now);
                    seen.longest = seen.longest.max(now - began);
                } else {
                    run_began = None;
                }
                last = now;
                std::thread::sleep(Duration::from_millis(1));
            }
            seen.span = last - began;
            seen
        })
    };
    workload();
    stop.store(true, Ordering::Relaxed);
    watcher.join().expect("the queue watcher")
}

// ---------------------------------------------------------------------------
// What this machine cannot serve, said out loud

/// Why this machine cannot run a scenario that needs a real block device, or
/// `None` when it can.
fn no_device_here(path: &Path) -> Option<String> {
    let device = match backing_device(path) {
        Ok(device) => device,
        Err(reason) => {
            return Some(format!(
                "the build directory is not on a block device ({reason}), so no write of it \
                 reaches one"
            ))
        }
    };
    let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    if table
        .lines()
        .any(|line| line.contains(&device.leaf) && line.contains("compress"))
    {
        return Some(format!(
            "the filesystem on {} is mounted with compression, so a known number of bytes \
             written is an unknown number of bytes stored",
            device.leaf
        ));
    }
    None
}

/// Whether this machine lets an unprivileged process build the namespace the
/// substituted-counter scenarios need, and why not when it does not.
fn no_namespace_here() -> Option<String> {
    if read_number("/proc/sys/user/max_user_namespaces") == Some(0) {
        return Some(
            "unprivileged user namespaces are disabled \
             (/proc/sys/user/max_user_namespaces is 0)"
                .to_owned(),
        );
    }
    match Command::new("unshare").args(["-Urm", "true"]).output() {
        Ok(output) if output.status.success() => None,
        Ok(output) => Some(format!(
            "`unshare -Urm` is refused here: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(error) => Some(format!("this machine has no `unshare`: {error}")),
    }
}

/// Runs the tool against a `/proc/diskstats` of this test's own writing.
///
/// The file is bind-mounted over the real one inside a private mount namespace
/// owned by an unprivileged user namespace, so nothing outside this process
/// tree sees it -- and because the mount follows the inode, rewriting the file
/// from here changes what the tool reads on its next tick.
fn with_substituted_diskstats(file: &Path, args: &[&str]) -> std::process::Child {
    let command = format!(
        "mount --bind {} /proc/diskstats && exec {BINARY} {}",
        file.display(),
        args.join(" ")
    );
    Command::new("unshare")
        .args(["-Urm", "sh", "-c", &command])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("unshare should run")
}

/// One device's line, with the counters a scenario chose.
fn diskstats_line(major: u32, minor: u32, name: &str, counters: Counters) -> String {
    format!(
        " {major}       {minor} {name} {} 0 {} 0 {} 0 {} 0 0 0 0\n",
        counters.reads, counters.read_sectors, counters.writes, counters.write_sectors
    )
}

fn write_now(path: &Path, text: &str) {
    std::fs::write(path, text).expect("the substituted counters are written");
}

// ---------------------------------------------------------------------------
// A. The capability sweep

/// Every mode and emit the catalog offers for a source this build collects,
/// against this build.
///
/// The catalog is data and the CLI is a projection of it, so a combination the
/// catalog offers and the build cannot serve is a promise broken in the one
/// place nobody looks. Each pair either produces output or is refused with a
/// message that says which of the three things is missing -- and the sweep is
/// driven from the catalog rather than from a list written here, so a backend
/// or an emit added tomorrow is swept tomorrow.
#[test]
fn every_mode_and_emit_the_catalog_offers_is_served_or_refused_by_name() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    let mut batches = Vec::new();
    let mut asked = Vec::new();
    for source in collected_sources() {
        let entry = catalog
            .get(&source.parse().expect("a source id"))
            .expect("the source is in the catalog");
        for mode in &entry.modes {
            for emit in &entry.emits {
                let mut args = vec![
                    source.to_owned(),
                    "--system".to_owned(),
                    "-m".to_owned(),
                    mode.as_str().to_owned(),
                    "-e".to_owned(),
                    emit.as_str().to_owned(),
                ];
                if mode.as_str() != "snapshot" {
                    args.push("-d".to_owned());
                    args.push("1s".to_owned());
                }
                asked.push((source, mode.as_str(), emit.as_str()));
                batches.push(args);
            }
        }
    }

    let mut served = 0;
    for ((source, mode, emit), output) in asked.iter().zip(run_all(&batches)) {
        let code = output.status.code().unwrap_or(-1);
        let message = stderr(&output);
        if code == 0 {
            served += 1;
            assert!(
                stdout(&output).contains(emit),
                "`{source} -m {mode} -e {emit}` produced no `{emit}`:\n{}",
                stdout(&output)
            );
            continue;
        }
        assert!(
            stdout(&output).is_empty(),
            "`{source} -m {mode} -e {emit}` was refused and printed numbers anyway:\n{}",
            stdout(&output)
        );
        // Three refusals are possible and they are different answers. What is
        // asserted is that the message names the axis it is refusing on --
        // the mode, the emit, or the mechanism that would serve it -- because
        // a refusal that sends a reader to the wrong place costs more than
        // none at all.
        assert!(
            message.contains(mode)
                || message.contains(emit)
                || message.contains("bpf")
                || message.contains("no collector")
                // The machine refusing names what each backend lacked.
                || (code == 1
                    && message.contains("no viable backend")
                    && message.contains("missing:")),
            "`{source} -m {mode} -e {emit}` was refused without naming what is missing:\n{message}"
        );
    }
    assert!(
        served >= 10,
        "a sweep that serves almost nothing is measuring the sweep, not the build: {served} of {}",
        asked.len()
    );
}

/// A mode and an emit each source genuinely offers.
///
/// Every source of this family is a counter, a gauge or a traced event, and
/// the three do not overlap: a volume has no depth to take a snapshot of, a
/// queue depth counts over no window, and a latency is the gap between two
/// events that a sample cannot both catch.
fn something_this_source_does(source: &str) -> (&'static str, &'static str) {
    match source {
        "io.inflight" | "io.pressure" => ("snapshot", "raw"),
        "io.latency" | "io.queue" | "io.size" | "io.writeback" | "io.syscall" | "io.fs"
        | "io.fsync" => ("trace", "summary"),
        _ => ("count", "summary"),
    }
}

/// The sources of this family that have a collector in this build.
///
/// Six through `/proc/diskstats` and PSI, four through the block layer's own
/// raw tracepoints, three through fentry, and two through the page cache's
/// own raw tracepoints. The two mechanisms answer different questions and the
/// sweep covers both: a combination the catalog offers and the build cannot
/// serve is a promise broken in the one place nobody looks, and that is as
/// true of an eBPF backend as of a file.
fn collected_sources() -> [&'static str; 16] {
    [
        "io.bytes",
        "io.iops",
        "io.merges",
        "io.inflight",
        "io.pressure",
        // The twenty-fourth wave's `%util`, from the io-ticks column.
        "io.util",
        "io.latency",
        "io.queue",
        "io.size",
        "io.writeback",
        // The eighth wave's three, over fentry.
        "io.syscall",
        "io.fs",
        "io.fsync",
        // The twenty-fourth wave's VFS call counter.
        "io.vfs",
        // The ninth wave's two, over the page cache's raw tracepoints.
        "io.pagecache",
        "io.readahead",
    ]
}

/// Whether this build can load an eBPF program at all, and why not when it
/// cannot.
///
/// The capabilities are file capabilities on `target/debug/omn` and every
/// relink drops them, so a suite run after a plain `cargo build` finds every
/// eBPF scenario unable to attach. That is a fact about the binary rather than
/// about the machine, and a scenario that quietly passed through it would be
/// reporting a green eBPF layer that was never exercised.
fn no_bpf_here() -> Option<String> {
    let output = run(&[
        "io.size", "--system", "-m", "trace", "-e", "summary", "-d", "100ms",
    ]);
    if output.status.success() {
        return None;
    }
    Some(format!(
        "{}: this build cannot load a block tracer. `make` grants CAP_BPF and CAP_PERFMON to \
         target/debug/omn, and every `cargo build` drops them again",
        machine_reason(&output)
    ))
}

/// What a refusal says the machine lacks, the backends' own missing pieces
/// first: the headline of a "no viable backend" refusal ends in a colon and
/// its reasons are the indented lines under it, which a skip that quoted only
/// the first line left out (`... has no viable backend on this machine:.`, on
/// every eBPF skip of the CI's run at 37f17c8).
fn machine_reason(output: &Output) -> String {
    let said = stderr(output);
    let headline = said.lines().next().unwrap_or("").trim();
    let reasons: Vec<&str> = said
        .lines()
        .skip(1)
        .filter(|line| line.starts_with("  ") && !line.trim_start().starts_with("hint:"))
        .map(str::trim)
        .collect();
    if reasons.is_empty() {
        headline.to_owned()
    } else {
        format!("{} ({headline})", reasons.join("; "))
    }
}

/// A mode and an emit are compatible in the abstract or not at all, and the
/// hint that says which now consults the source it is talking about.
///
/// This was the sweep's own finding: one message sent a reader to `raw` and the
/// next refused `raw`, so following the advice cost a second refusal. Two
/// things changed. The hint is the intersection of the matrix and the entry, so
/// it can only ever name emits this source declares; and `io.bytes` declares
/// `raw`, because the read/write split it measures is on the event and no
/// aggregating emit carries an attribute.
///
/// Both halves are asserted, and the second is the one that matters: the hint
/// is followed, and the run it leads to succeeds.
#[test]
fn the_mode_hint_offers_only_what_the_source_declares() {
    let by_mode = run(&["io.bytes", "--system", "-m", "count", "-e", "timeline"]);
    assert_eq!(by_mode.status.code(), Some(EXIT_REJECTED));
    let hint = stderr(&by_mode);
    assert!(
        hint.contains("in `count` mode `io.bytes` emits"),
        "the hint should name the source rather than the matrix: {hint}"
    );
    assert!(
        !hint.contains("the valid emits are"),
        "the abstract form of the hint is what sent a reader in a circle: {hint}"
    );

    // Every emit it offers is one the source declares. Against the old hint
    // this failed on `raw`, which `io.bytes` refused by name.
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    let entry = catalog
        .get(&"io.bytes".parse().expect("a source id"))
        .expect("io.bytes is in the catalog");
    let offered: Vec<&str> = hint
        .rsplit("emits ")
        .next()
        .expect("the hint lists something")
        .trim()
        .split(", ")
        .collect();
    assert!(!offered.is_empty(), "{hint}");
    for emit in &offered {
        assert!(
            entry.emits.iter().any(|known| known.as_str() == *emit),
            "the hint offered `{emit}`, which `io.bytes` refuses by name:\n{hint}"
        );
    }

    // And following it works, which is the whole point of a hint.
    for emit in &offered {
        let followed = run(&[
            "io.bytes", "--system", "-m", "count", "-e", emit, "-d", "1s",
        ]);
        assert_eq!(
            followed.status.code(),
            Some(0),
            "the hint offered `{emit}` and it was refused:\n{}",
            stderr(&followed)
        );
        assert!(
            stdout(&followed).contains(emit),
            "`io.bytes -m count -e {emit}` produced no `{emit}`:\n{}",
            stdout(&followed)
        );
    }
}

/// Every output format either carries what it was given or says it cannot.
///
/// A format that claims an emit it cannot express is the quietest of the three
/// lies in this class: `folded` is a stack format and a timeline has no
/// stacks, so a file is produced that a flamegraph tool reads as an empty
/// profile rather than as an error.
#[test]
fn every_output_format_either_carries_the_series_or_refuses_it() {
    const CARRIES: [&str; 6] = ["json", "yaml", "toml", "ndjson", "speedscope", "chrome"];
    const CANNOT: [&str; 2] = ["folded", "pprof"];

    let batches: Vec<Vec<String>> = CARRIES
        .iter()
        .chain(CANNOT.iter())
        .map(|format| {
            [
                "io.inflight",
                "--system",
                "-m",
                "poll@50ms",
                "-d",
                "200ms",
                "-e",
                "timeline",
                "--format",
                format,
            ]
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect()
        })
        .collect();

    let outputs = run_all(&batches);
    for (format, output) in CARRIES.iter().zip(&outputs) {
        assert!(
            output.status.success(),
            "`{format}` refused a timeline it can carry: {}",
            stderr(output)
        );
        assert!(
            !stdout(output).trim().is_empty(),
            "`{format}` produced nothing at all"
        );
    }
    for (format, output) in CANNOT.iter().zip(&outputs[CARRIES.len()..]) {
        assert_eq!(
            output.status.code(),
            Some(EXIT_REJECTED),
            "`{format}` cannot carry a timeline and has to say so: {}",
            stderr(output)
        );
        let message = stderr(output);
        assert!(
            message.contains(format) && message.contains("timeline"),
            "the refusal names neither the format nor the emit: {message}"
        );
    }
}

/// A filter field the selected backend does not carry is refused, and the
/// refusal names the backend that would carry it.
///
/// The catalog declares `dev`, `rwbs`, `bytes` and `comm` for `io.bytes`
/// because the chain as a whole serves them; `/proc/diskstats` carries the
/// first and nothing else, since the block layer's counters record no
/// submitter. Answering `comm == 'dd'` from them would return the whole
/// machine's I/O under one program's name.
#[test]
fn a_filter_the_backend_cannot_carry_names_the_backend_that_can() {
    let refused = run(&[
        "io.bytes",
        "--system",
        "-m",
        "count",
        "-d",
        "1s",
        "-e",
        "summary",
        "--filter",
        "comm == 'dd'",
    ]);
    assert_ne!(refused.status.code(), Some(0));
    let message = stderr(&refused);
    assert!(message.contains("comm"), "{message}");
    // The eBPF backend, by whichever of its two ids the resolver reached for:
    // the chain is `bpf:raw_tp` ahead of an equivalent `bpf:tp`, and naming one
    // of them here would make this scenario an assertion about the chain's
    // order rather than about the refusal.
    assert!(
        message.contains("bpf:raw_tp") || message.contains("bpf:tp"),
        "the refusal has to name the backend that carries the field: {message}"
    );
    assert!(stdout(&refused).is_empty(), "{}", stdout(&refused));

    // The field it does carry narrows the series, and what the predicate threw
    // away is counted: an empty result and a quiet machine must not look the
    // same.
    let filtered = run(&[
        "io.bytes",
        "--system",
        "-m",
        "poll@50ms",
        "-d",
        "200ms",
        "-e",
        "timeline",
        "--filter",
        "dev == 'a-device-no-machine-has'",
    ]);
    assert!(filtered.status.success(), "{}", stderr(&filtered));
    let text = stdout(&filtered);
    assert!(
        text.contains("kept 0 of"),
        "a filter that kept nothing has to say how much it dropped:\n{text}"
    );
}

/// A request keyed on what `/proc/diskstats` and `io.stat` cannot say is
/// refused where it resolves, naming the backend that does attach the key.
///
/// The two files are the kernel's, and what they carry is known without the
/// tool: a line of `/proc/diskstats` is a `major minor name` and its counters,
/// a line of `io.stat` a `major:minor` and its counters. Neither names a task
/// or a request, so `io.bytes` and `io.iops` keyed on `task` or `blk` and
/// resolved onto them would be events no join over that key could ever match.
/// The catalog says so in each backend's `keys`, and the resolver refuses with
/// the eBPF backend that carries them -- which this build has not written. The
/// hint names them as the walk would answer (`DECISIONS.md`, "one order for
/// unbuilt and missing"): the build's (exit 3) where this machine could run a
/// carrier, the machine's (exit 1) where it can run none -- without the
/// capabilities, or with tracefs unreadable and no raw tracepoint carrier.
/// Ground truth for which: what `omn list` probes for each carrier. Keyed on
/// the device, the one thing both files do name, the
/// same request runs, and its series are the devices the kernel's file lists.
#[test]
fn a_request_keyed_on_what_the_counter_files_do_not_name_is_refused_with_the_backend_that_does() {
    // The keys each entry offers and the counter file does not name: `io.iops`
    // offers no `task` at all, which is the validator's refusal and another
    // test's.
    let mut cases: Vec<(&str, &str, &str, &str, &[&str])> = vec![
        ("io.bytes", "--system", "", "fs:proc", &["task", "blk"]),
        ("io.iops", "--system", "", "fs:proc", &["blk"]),
    ];
    if Path::new("/sys/fs/cgroup/io.stat").exists() {
        cases.push(("io.bytes", "--cgroup", "/", "fs:cgroup", &["task", "blk"]));
    } else {
        eprintln!(
            "skipped the `fs:cgroup` half: /sys/fs/cgroup/io.stat does not exist here, so \
             `io.bytes --cgroup` resolves onto no counter file"
        );
    }
    for (source, target, value, backend, keys) in cases {
        for &key in keys {
            let mut args = vec![source, target];
            if !value.is_empty() {
                args.push(value);
            }
            args.extend(["-k", key, "-m", "count", "-e", "summary", "-d", "200ms"]);
            let refused = run(&args);
            let message = stderr(&refused);
            if backend == "fs:cgroup" && message.contains("has no viable backend") {
                eprintln!("skipped the `fs:cgroup` half: {}", machine_reason(&refused));
                break;
            }
            // `fs:cgroup` has no collector in this build, so with the
            // capabilities the resolver stops at the unwritten `bpf:raw_tp`
            // ahead of it, and the refusal is that one -- the build's as well.
            if backend == "fs:cgroup" && !message.contains("does not attach") {
                assert_eq!(
                    refused.status.code(),
                    Some(EXIT_NOT_IMPLEMENTED),
                    "{args:?}: {message}"
                );
                assert!(
                    message.contains("no collector is written"),
                    "{args:?}: {message}"
                );
                continue;
            }
            assert!(
                message.contains(&format!(
                    "`{source}` is keyed on `{key}`, which the selected backend `{backend}` \
                     does not attach"
                )),
                "{args:?}: {message}"
            );
            let carriers: Vec<&str> = ["bpf:raw_tp", "bpf:tp"]
                .into_iter()
                .filter(|carrier| message.contains(&format!("`{carrier}` does")))
                .collect();
            assert_eq!(
                carriers.len(),
                1,
                "the refusal has to name the backend that attaches `{key}`: {message}"
            );
            let named = carriers[0];
            // A carrier this machine can run outranks one it cannot, as on the
            // walk, so the hint is the machine's only where no carrier runs.
            let lacking: Vec<&str> = ["bpf:raw_tp", "bpf:tp"]
                .into_iter()
                .filter(|carrier| lacks_here(source, carrier).is_some())
                .collect();
            if lacking.len() == 2 {
                assert!(
                    message.contains(&format!("`{named}` does, and this machine cannot run it")),
                    "no carrier of `{key}` runs here, so the hint is the machine's: {message}"
                );
                assert_eq!(
                    refused.status.code(),
                    Some(EXIT_NO_BACKEND),
                    "no backend that attaches `{key}` runs on this machine: {message}"
                );
            } else {
                assert!(
                    !lacking.contains(&named)
                        && message
                            .contains(&format!("`{named}` does, and this build has no collector")),
                    "a carrier this machine runs is named, as this build's gap: {message}"
                );
                assert_eq!(
                    refused.status.code(),
                    Some(EXIT_NOT_IMPLEMENTED),
                    "the backend that attaches `{key}` is one this build has not written: \
                     {message}"
                );
            }
            assert!(stdout(&refused).is_empty(), "{}", stdout(&refused));
        }
    }

    // Keyed on the device, it runs, and every series is a device the kernel's
    // own file lists.
    let listed: Vec<String> = std::fs::read_to_string("/proc/diskstats")
        .expect("/proc/diskstats is readable")
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            Some(format!("dev:{}+{}", fields.first()?, fields.get(1)?))
        })
        .collect();
    let served = run(&[
        "io.bytes", "--system", "-k", "dev", "-m", "count", "-e", "summary", "-d", "200ms",
        "--format", "json",
    ]);
    assert!(served.status.success(), "{}", stderr(&served));
    let document = json(&served);
    let summary = output_of(&document["results"][0], "summary");
    let keys: Vec<String> = summary["series"]
        .as_array()
        .expect("the summary has series")
        .iter()
        .filter_map(|series| series["key"].as_str().map(str::to_owned))
        .collect();
    assert!(!keys.is_empty(), "no device was reported: {summary}");
    for key in &keys {
        assert!(
            listed.contains(key),
            "`{key}` is not a device /proc/diskstats lists: {listed:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// B. The backend comparison

/// Every backend this family declares and has not written is refused by name,
/// with the exit code that means "this build" -- or "this machine" where it
/// lacks something the backend needs, as the walk answers -- never "this run".
///
/// This family still has one *implemented* backend per source -- procfs for the
/// counters, the raw tracepoint for the block layer -- so there is no second
/// road within a chain to compare a number against, and the comparison is with
/// the refusal instead, which is what `BRIEF.md` asks for where a chain has one
/// implementation and others declared. The comparison across two mechanisms
/// that *is* now possible is
/// `the_tracer_and_the_kernels_own_counter_see_the_same_requests`, further
/// down: `/proc/diskstats` and the block tracer counting the same requests.
///
/// The backend that is skipped is whichever one this build implements, asked of
/// the build rather than written down here: the eBPF collectors landed after
/// this scenario did, and a list in a test is a thing that goes stale quietly.
#[test]
fn forcing_a_backend_this_build_has_not_written_names_it_in_the_chains_order() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    let mut asked = Vec::new();
    let mut batches = Vec::new();
    for source in collected_sources() {
        let entry = catalog
            .get(&source.parse().expect("a source id"))
            .expect("the source is in the catalog");
        // The mode and emit have to be ones this source declares, or the
        // validator refuses the request before the backend is ever looked at
        // and the scenario measures the validator instead.
        let (mode, emit) = something_this_source_does(source);
        assert!(
            entry.modes.iter().any(|kind| kind.as_str() == mode)
                && entry.emits.iter().any(|kind| kind.as_str() == emit),
            "`{source}` no longer declares `{mode}` with `{emit}`, so this sweep is asking for \
             something else than it thinks"
        );
        for backend in &entry.backends {
            if omn::collect::implements(source, &backend.id) {
                continue;
            }
            asked.push((source, backend.id.clone()));
            let mut args: Vec<String> = [source, "--system", "-m", mode, "-e", emit]
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect();
            if mode != "snapshot" {
                args.push("-d".to_owned());
                args.push("1s".to_owned());
            }
            args.push("--backend".to_owned());
            args.push(format!("force:{}", backend.id));
            batches.push(args);
        }
    }
    assert!(
        !batches.is_empty(),
        "this family declares no backend beyond the one it implements"
    );

    for ((source, backend), output) in asked.iter().zip(run_all(&batches)) {
        let message = stderr(&output);
        assert!(
            stdout(&output).is_empty(),
            "`{source}` produced numbers through `{backend}`, which this build has not written:\n{}",
            stdout(&output)
        );
        assert!(
            message.contains(backend),
            "forcing `{backend}` was refused without naming it:\n{message}"
        );
        match output.status.code() {
            // A backend this machine lacks something for is the machine's
            // first, as on the walk (`DECISIONS.md`, "one order for unbuilt
            // and missing"), and only then.
            Some(EXIT_NO_BACKEND) => {
                let lacks = lacks_here(source, backend).unwrap_or_else(|| {
                    panic!(
                        "forcing `{backend}` on `{source}` was refused for the machine, which \
                         `omn list` says lacks nothing for it:\n{message}"
                    )
                });
                assert!(
                    lacks
                        .iter()
                        .all(|needs| message.contains(&format!("{needs}: missing")))
                        && !message.contains("no collector is written"),
                    "the machine's refusal names what it lacks, not the collector:\n{message}"
                );
            }
            // The answer this class is about: valid request, unwritten
            // mechanism, on a machine that lacks nothing it needs. Retrying or
            // raising privileges will not help.
            Some(EXIT_NOT_IMPLEMENTED) => {
                assert!(
                    lacks_here(source, backend).is_none(),
                    "`{backend}` lacks something here and was refused as the build's:\n{message}"
                );
                assert!(
                    message.contains("no collector is written"),
                    "exit 3 has to say that this build is what is missing:\n{message}"
                );
            }
            // A backend that cannot attribute per system is a different answer
            // and a different code: the request, as written, cannot mean
            // anything through that backend.
            Some(EXIT_REJECTED) => {
                assert!(
                    message.contains("cannot attribute per system"),
                    "exit 2 has to say what about the request cannot mean anything:\n{message}"
                );
            }
            other => panic!("forcing `{backend}` on `{source}` exited {other:?}:\n{message}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The ground truth

/// 64 MiB that `fsync` forced onto the platter are found by the session that
/// was watching.
///
/// The floor is the workload's: `fsync` does not return until the data is
/// written out, so at least that many bytes crossed the block layer while the
/// session ran. The ceiling is the kernel's own counters over a window that
/// encloses the session's, read by this test -- so the number is checked from
/// both sides without either side being the tool's.
#[test]
fn a_known_volume_of_writes_reaches_the_device_and_the_session_finds_it() {
    let file = scratch("io-known-volume.bin");
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    let device = backing_device(Path::new(env!("CARGO_TARGET_TMPDIR"))).expect("a block device");

    let (output, enclosing) = measure_while(
        &["-f", &session("io-known-volume.yml"), "--format", "json"],
        &device,
        || write_and_sync(&file),
    );
    let _ = std::fs::remove_file(&file);
    assert!(output.status.success(), "{}", stderr(&output));

    let document = json(&output);
    let volume = output_of(result(&document, "volume"), "summary");
    let measured = series(volume, &device.key)
        .map(|series| number(&series["last"]))
        .unwrap_or_else(|| panic!("the session reported nothing for {}: {volume}", device.leaf));

    assert!(
        measured >= KNOWN_BYTES as f64,
        "{KNOWN_BYTES} bytes were written and synced inside the window and the session found \
         {measured} on {}; the kernel's own counters over a longer window say {}",
        device.leaf,
        enclosing.bytes()
    );
    assert!(
        measured <= enclosing.bytes() as f64,
        "the session reported {measured} bytes on {}, more than the {} the kernel counted over \
         a window enclosing it",
        device.leaf,
        enclosing.bytes()
    );

    // The same window, the same device, the other counter: the operations it
    // took. A device declares the largest request it will accept, so a known
    // number of bytes puts a floor under the count that needs nothing from the
    // tool to work out.
    let operations = output_of(result(&document, "operations"), "summary");
    let ops = series(operations, &device.key)
        .map(|series| number(&series["last"]))
        .expect("the operation count is reported per device");
    let floor = KNOWN_BYTES.div_ceil(device.max_request_bytes());
    assert!(
        ops >= floor as f64,
        "{KNOWN_BYTES} bytes through a device that takes {} at a time is at least {floor} \
         requests, and the session reports {ops}",
        device.max_request_bytes()
    );
}

/// 64 MiB of writes are reported as writes, and the reader can see that they
/// were.
///
/// Defect 7 of `docs/scenarios/io.md`: the collector measures the read/write
/// split, puts it on every event, and the entry declared only emits that
/// aggregate -- so the split was measured and unreachable, and the scenarios
/// above could only ever check a total. A collector that credited every write
/// as a read passed both of them.
///
/// `raw` is declared now, and this is the check the total could not make. The
/// ground truth is the same `fsync`ed 64 MiB and the kernel's own two columns:
/// the workload puts a floor under the write side, and each side is capped by
/// what `/proc/diskstats` counted for that same column over a window that
/// strictly encloses the tool's. Neither cap is the tool's own number.
///
/// Both columns are bracketed rather than the read side being required to be
/// zero, and the reason is the trap `STATE.md` names: nine hundred tests share
/// this machine's disks, and the read scenario below pulls its own 64 MiB off
/// the same device while this one runs. Asserting "a write workload reads
/// nothing" measured the suite and not the tool -- it failed with exactly
/// 67108864 bytes in the read column, which was the neighbouring scenario's.
/// What the brackets say instead is that the tool did not invent traffic in
/// either column, which is true whatever else the machine is doing, and on a
/// quiet machine the read cap is near zero and the check is as sharp as the
/// first form was.
#[test]
fn the_split_a_write_workload_made_is_reported_as_writes() {
    let file = scratch("io-write-split.bin");
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    let device = backing_device(Path::new(env!("CARGO_TARGET_TMPDIR"))).expect("a block device");

    let (output, enclosing) = measure_while(
        &[
            "io.bytes", "--system", "-m", "count", "-e", "raw", "-k", "dev", "-d", "2s",
            "--format", "json",
        ],
        &device,
        || write_and_sync(&file),
    );
    let _ = std::fs::remove_file(&file);
    assert!(
        output.status.success(),
        "`io.bytes -e raw` is what defect 7 was about; it has to be served: {}",
        stderr(&output)
    );

    let document = json(&output);
    let raw = output_of(&document["results"][0], "raw");
    let event = raw["events"]
        .as_array()
        .expect("a raw emit carries its events")
        .iter()
        .find(|event| {
            event["attrs"].as_array().is_some_and(|attrs| {
                attrs
                    .iter()
                    .any(|a| a["name"] == "dev" && a["value"] == device.leaf.as_str())
            })
        })
        .unwrap_or_else(|| panic!("no event for {}: {raw}", device.leaf));

    let written = attr(event, "write_bytes");
    let read = attr(event, "read_bytes");
    let total = attr(event, "bytes");

    let kernel_written = (enclosing.write_sectors * 512) as f64;
    let kernel_read = (enclosing.read_sectors * 512) as f64;

    assert!(
        written >= KNOWN_BYTES as f64,
        "{KNOWN_BYTES} bytes were written and synced inside the window and the event credits \
         {written} to writes on {}; the kernel counted {kernel_written} written over a longer \
         window",
        device.leaf
    );
    assert!(
        written <= kernel_written,
        "the event credits {written} bytes to writes on {}, more than the {kernel_written} the \
         kernel counted in its own write column over a window enclosing this one",
        device.leaf
    );
    assert!(
        read <= kernel_read,
        "the event credits {read} bytes to reads on {}, more than the {kernel_read} the kernel \
         counted in its own read column over a window enclosing this one -- which is what a \
         collector crediting writes as reads looks like, and is what defect 7 could not catch",
        device.leaf
    );
    assert!(
        (written + read - total).abs() <= total * 0.001,
        "the halves do not add up to the total the aggregating emits answer with: \
         {written} written + {read} read against {total}"
    );
}

/// A file the page cache was told to forget is read back off the device, and
/// the session measures it to the byte.
///
/// This is the tighter of the two workloads. A read cannot bring more than the
/// file's own length -- readahead stops at the end of it -- so where the write
/// scenario has a floor and a long ceiling, this one is bracketed by the
/// kernel's counters over the same span and comes back equal to the file size
/// on a machine doing nothing else.
#[test]
fn a_cold_read_of_a_known_file_is_measured_against_the_bytes_it_had_to_move() {
    let file = scratch("io-cold-read.bin");
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    let device = backing_device(Path::new(env!("CARGO_TARGET_TMPDIR"))).expect("a block device");
    write_and_sync(&file);

    // The read has to fall inside the session's window for the kernel's
    // count over the read to be a floor under the session's, and the session
    // takes a while to open it -- longer than 300 ms when five test binaries
    // share the machine (seen here once, in a parallel run). So the read
    // starts later on each attempt until the window, by its own stamps,
    // encloses it; the session lasts 2 s and the read takes well under one.
    let mut attempts = Vec::new();
    let (enclosing, inner, document) = loop {
        let mut read_back = 0;
        let wait = Duration::from_millis(300 * (1 + attempts.len() as u64));
        let (output, enclosing, inner) = measure_around(
            &["-f", &session("io-known-volume.yml"), "--format", "json"],
            &device,
            wait,
            || read_back = cold_read(&file),
        );
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(
            read_back, KNOWN_BYTES,
            "the workload did not read the file it promised to"
        );
        let document = json(&output);
        let window = &result(&document, "merges")["window"];
        let (opened, closed) = (
            window["opened_ns"].as_u64().expect("the window's opening"),
            window["closed_ns"].as_u64().expect("the window's close"),
        );
        let enclosed = opened <= inner.from_ns && inner.to_ns <= closed;
        attempts.push(format!(
            "window [{opened}, {closed}], read [{}, {}] after {wait:?}",
            inner.from_ns, inner.to_ns
        ));
        if enclosed || attempts.len() == 3 {
            assert!(
                enclosed,
                "the session's window never enclosed the read, so the kernel's count over the \
                 read is no floor under it: {attempts:?}"
            );
            break (enclosing, inner, document);
        }
    };
    let _ = std::fs::remove_file(&file);

    let measured = series(
        output_of(result(&document, "volume"), "summary"),
        &device.key,
    )
    .map(|series| number(&series["last"]))
    .expect("the volume is reported per device");
    assert!(
        measured >= KNOWN_BYTES as f64,
        "{KNOWN_BYTES} bytes had to come off {} and the session found {measured}; the kernel \
         counted {} over a longer window. A read that costs nothing means the page cache kept \
         the file, which is the one thing this workload rules out",
        device.leaf,
        enclosing.bytes()
    );
    assert!(
        measured <= enclosing.bytes() as f64,
        "the session reported more than the kernel counted over a window enclosing it: \
         {measured} against {}",
        enclosing.bytes()
    );

    // What the block layer coalesced, held to the kernel's own merge count
    // from both sides: no more than over a window that encloses the
    // session's, no fewer than over the read alone, which the session's
    // window encloses (its own `window` says so, on the clock the test read).
    // Not to zero on a device whose scheduler is `none`, which an earlier
    // version of this check assumed and the CI's `sda` disproved (11 merges in
    // 86 reads, both cells): with no elevator the block layer still merges a
    // bio into a request waiting in the submitter's plug list or the software
    // queue, and counts it in the same column. A collector that reported 0
    // fails the floor wherever the read merged anything; where the kernel
    // itself merged nothing, the floor is 0 and the read says so.
    let merges = series(
        output_of(result(&document, "merges"), "summary"),
        &device.key,
    )
    .map(|series| number(&series["last"]))
    .expect("the merge count is reported per device");
    let ops = series(
        output_of(result(&document, "operations"), "summary"),
        &device.key,
    )
    .map(|series| number(&series["last"]))
    .expect("the operation count is reported per device");
    assert!(
        merges <= enclosing.merges as f64,
        "the session found {merges} merges on {} and the kernel counted {} over a window \
         enclosing it",
        device.leaf,
        enclosing.merges
    );
    assert!(
        merges >= inner.counters.merges as f64,
        "the session found {merges} merges on {} and the kernel counted {} over the read \
         alone, inside the session's window",
        device.leaf,
        inner.counters.merges
    );
    assert!(
        ops >= inner.counters.reads as f64 && ops <= (enclosing.reads + enclosing.writes) as f64,
        "the session found {ops} requests on {}; the kernel completed {} reads over the read \
         alone and {} requests over a window enclosing the session's",
        device.leaf,
        inner.counters.reads,
        enclosing.reads + enclosing.writes
    );
    assert!(
        merges <= ops,
        "more requests were merged away than the device completed: {merges} against {ops}"
    );
    if inner.counters.merges == 0 {
        eprintln!(
            "the kernel merged nothing on {} over the read, so the merge count's floor is 0",
            device.leaf
        );
    }
}

/// The queue depth a burst of writes reaches is seen, and only by a poll fast
/// enough to catch it.
///
/// A gauge is the one thing in this family that cannot be recovered after the
/// fact: a queue that fills and drains between two reads leaves no trace at
/// all. So the session polls at 20 ms, and what it must find is a depth
/// greater than zero on the device the file was written to.
///
/// The workload holds the queue rather than filling it once: eight threads
/// write straight to the device from before the session starts until it has
/// ended, so a request is in flight at most instants of the window. A buffered 64 MiB and one `fsync`, which
/// this used to be, drains in about 20 ms on this machine's NVMe (the kernel's
/// own reads saw the queue non-empty for 16 ms at a stretch at best) and was
/// seen at all only because one 20 ms poll happened to land inside it; on the
/// CI's cloud disk no poll did, 101 times over, on both cells.
///
/// How long the queue held anything is still the device's business, so the
/// test asks the kernel rather than assuming it: while the file is written it
/// reads `/sys/block/<dev>/inflight` itself, about once a millisecond. That is
/// the ground truth the session's maximum is held to, and a device whose
/// queue the kernel itself never saw held is one no gauge can be held to. So the depth is
/// required only when the kernel's own reads say a 20 ms poll could not have
/// missed it -- non-empty for two intervals without a break, or for a share
/// of the write that every poll missing is one chance in a thousand -- and
/// otherwise the test says what the kernel saw -- and still holds the
/// series' spacing and volume below.
#[test]
fn a_burst_of_writes_shows_up_as_a_queue_on_the_device_that_took_it() {
    use std::os::unix::fs::OpenOptionsExt;
    let file = scratch("io-queue-depth.bin");
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    let device = backing_device(Path::new(env!("CARGO_TARGET_TMPDIR"))).expect("a block device");

    if let Err(error) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_DIRECT)
        .open(&file)
    {
        eprintln!(
            "skipped: {} refuses O_DIRECT ({error}), so no write of it is held to a queue",
            device.leaf
        );
        return;
    }

    // The writers run from before the session starts until it has ended, so
    // its window is inside the workload whatever the tool's start-up costs on
    // a loaded machine (more than the 300 ms lead-in at a load of 20, once).
    let child = Command::new(BINARY)
        .args(["-f", &session("io-device-load.yml"), "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    let mut written = Ok(Vec::new());
    let mut output = None;
    let kernel = while_watching_the_queue(&device, || {
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writers = scope.spawn(|| direct_writes_until(&file, WRITERS, &stop));
            output = Some(child.wait_with_output().expect("omn should finish"));
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            written = writers.join().expect("the writers");
        });
    });
    let output = output.expect("the session's output");
    let _ = std::fs::remove_file(&file);
    assert!(output.status.success(), "{}", stderr(&output));
    let written = written.expect("the direct writes");

    let document = json(&output);
    let depth = series(
        output_of(result(&document, "queue depth"), "summary"),
        &device.key,
    )
    .expect("the queue depth is reported per device");
    // When a 20 ms poll cannot miss the queue. Not if the queue stayed
    // non-empty for two of its intervals without a break: one read lands
    // inside that whatever the phase, and the second interval is the slack a
    // loaded machine puts in a tick. Nor, to one chance in a thousand, if the
    // kernel found it non-empty for a share `f` of the writes and they
    // spanned `n` polls: `(1 - f)^n`, the polls missing it every time. On
    // this machine alone `f` is 0.8 and more; with five other streams' tests
    // writing to the same disk it has been 0.1, which leaves one chance in
    // twenty-five of a right gauge that saw nothing, and the depth is then
    // reported, not required.
    const POLL: Duration = Duration::from_millis(20);
    let share = kernel.busy.as_secs_f64() / kernel.span.as_secs_f64().max(1e-9);
    let polls = kernel.span.as_secs_f64() / POLL.as_secs_f64();
    let all_missed = (1.0 - share.min(1.0)).powf(polls);
    if kernel.longest >= POLL * 2 || all_missed <= 1e-3 {
        assert!(
            number(&depth["max"]) >= 1.0,
            "the kernel's own reads of /sys/block/{}/inflight found the queue non-empty for \
             {:?} of {:?} ({:?} at a stretch, deepest {}, {} reads), and a 20 ms poll saw a \
             queue that never held one: {depth}",
            device.leaf,
            kernel.busy,
            kernel.span,
            kernel.longest,
            kernel.deepest,
            kernel.reads
        );
    } else {
        eprintln!(
            "skipped the depth: the kernel's own reads of /sys/block/{}/inflight, {} of them a \
             millisecond apart across the write, found its queue non-empty for {:?} of {:?} \
             ({:?} at a stretch at most, deepest {}), which {polls:.0} polls at 20 ms all miss \
             with probability {all_missed:.3}; the session's maximum was {}",
            device.leaf,
            kernel.reads,
            kernel.busy,
            kernel.span,
            kernel.longest,
            kernel.deepest,
            depth["max"]
        );
    }

    // A floor that owes nothing to /sys/block/<dev>/inflight. The gauge now
    // reads that file too (`docs/scenarios/io.md`, defect 20), so the kernel's
    // reads above only show that the tool reads what the test reads. The
    // workload's own records do not: at each poll's instant, by the writers'
    // clock against the window's stamps (both `CLOCK_MONOTONIC`), how many of
    // the direct writes had begun and not returned. A direct write is a
    // request at the device for nearly all of its syscall, so at an instant
    // when half the writers are inside theirs, the device holding none of
    // them is the gap between a syscall's entry and its bio for every one of
    // four threads at once. Over ten such polls or more, a maximum of 0 is a
    // gauge that does not see the queue.
    let depth_result = result(&document, "queue depth");
    let depth_opened = depth_result["window"]["opened_ns"]
        .as_u64()
        .expect("the window's opening");
    let instants: Vec<u64> = output_of(depth_result, "timeline")["series"]
        .as_array()
        .expect("the queue depth has series")
        .iter()
        .filter(|series| series["key"] == device.key.as_str())
        .flat_map(|series| series["points"].as_array().cloned().unwrap_or_default())
        .filter_map(|point| point["offset_ns"].as_u64())
        .map(|offset| depth_opened + offset)
        .collect();
    let outstanding = |at: u64| {
        written
            .iter()
            .filter(|(began, returned)| *began <= at && at < *returned)
            .count()
    };
    let held = instants
        .iter()
        .filter(|at| outstanding(**at) >= WRITERS / 2)
        .count();
    if held >= 10 {
        eprintln!(
            "the workload's floor: {held} of the {} polls on {} found at least {} of its direct \
             writes outstanding by its own clock; the session's maximum was {}",
            instants.len(),
            device.leaf,
            WRITERS / 2,
            depth["max"]
        );
        assert!(
            number(&depth["max"]) >= 1.0,
            "at {held} of the {} polls on {}, the workload's own clock had at least {} of its \
             {WRITERS} direct writes in their syscalls, and the gauge never read a queue: \
             {depth}",
            instants.len(),
            device.leaf,
            WRITERS / 2
        );
    } else {
        eprintln!(
            "the workload's floor not held: only {held} of the {} polls on {} found at least {} \
             of its direct writes outstanding by its own clock",
            instants.len(),
            device.leaf,
            WRITERS / 2
        );
    }

    // The series has to have the spacing the request asked for. Two seconds at
    // 20 ms is a hundred reads, and a session whose points are counted in tens
    // is one whose interval was decided by something other than the request.
    let points = output_of(result(&document, "queue depth"), "timeline")["series"]
        .as_array()
        .and_then(|series| series.first())
        .and_then(|series| series["points"].as_array())
        .map(Vec::len)
        .unwrap_or(0);
    assert!(
        points >= 60,
        "a 2 s window polled every 20 ms is about a hundred points, and this run has {points}"
    );

    // The volume over the same window, from the polled series rather than a
    // counted total: the two emits are two roads to one number and they have
    // to agree about the floor the workload set.
    //
    // The floor is the writes that began after the series' first read and
    // returned before its last, by the workload's own clock against the
    // window's stamps -- both `CLOCK_MONOTONIC`. Not every write: a tool slow
    // to start under load opens its window after the workload has begun, and
    // the bytes before it are no series' to report.
    let volume_result = result(&document, "volume");
    let points: Vec<serde_json::Value> = output_of(volume_result, "timeline")["series"]
        .as_array()
        .expect("the volume has series")
        .iter()
        .filter(|series| series["key"] == device.key.as_str())
        .flat_map(|series| series["points"].as_array().cloned().unwrap_or_default())
        .collect();
    let volume: f64 = points.iter().map(|point| number(&point["value"])).sum();
    let opened = volume_result["window"]["opened_ns"]
        .as_u64()
        .expect("the window's opening");
    let offset = |point: Option<&serde_json::Value>| {
        point
            .and_then(|point| point["offset_ns"].as_u64())
            .unwrap_or(0)
    };
    let (first, last) = (
        opened + offset(points.first()),
        opened + offset(points.last()),
    );
    let inside = written
        .iter()
        .filter(|(began, returned)| *began >= first && *returned <= last)
        .count() as u64
        * DIRECT_CHUNK as u64;
    assert!(
        inside > 0,
        "none of the {} writes fell between the series' first and last reads",
        written.len()
    );
    assert!(
        volume >= inside as f64,
        "the polled series adds up to {volume} bytes on {}, less than the {inside} the \
         workload wrote straight to it between the series' first and last reads",
        device.leaf
    );
}

/// How many threads write at once in the queue scenario: each holds one
/// request in flight while it waits, so the queue is empty only in the instant
/// between one thread's write returning and the next one's arriving, and every
/// other thread is in the middle of its own.
const WRITERS: usize = 8;

/// The size of each direct write: 128 KiB, a request most devices take whole.
/// Not larger: a megabyte at a time, eight at once, is gigabytes in 600 ms on
/// this machine's NVMe, and the scenarios beside it in this binary bracket the
/// same device's counters -- two of them failed on the extra volume.
const DIRECT_CHUNK: usize = 128 << 10;

/// `threads` threads writing `DIRECT_CHUNK`s straight to the device, each over
/// its own 2 MiB of the file, until `span` has passed. Returns when each
/// write began and returned, in `CLOCK_MONOTONIC`; every one of its
/// `DIRECT_CHUNK` bytes reached the device in between: `O_DIRECT` bypasses the page cache, so there is no
/// writeback to wait for and no burst that drains in a few milliseconds.
fn direct_writes_for(
    path: &Path,
    threads: usize,
    span: Duration,
) -> std::io::Result<Vec<(u64, u64)>> {
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let writers = scope.spawn(|| direct_writes_until(path, threads, &stop));
        std::thread::sleep(span);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writers.join().expect("the writers")
    })
}

/// `direct_writes_for`, until `stop` is set.
fn direct_writes_until(
    path: &Path,
    threads: usize,
    stop: &std::sync::atomic::AtomicBool,
) -> std::io::Result<Vec<(u64, u64)>> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    const REGION: usize = 2 << 20;
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|index| {
                let path = path.to_owned();
                scope.spawn(move || -> std::io::Result<Vec<(u64, u64)>> {
                    let file = std::fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .custom_flags(libc::O_DIRECT)
                        .open(&path)?;
                    let fd = file.as_raw_fd();
                    let mut storage = incompressible(DIRECT_CHUNK + PAGE);
                    let offset = (PAGE - (storage.as_ptr() as usize % PAGE)) % PAGE;
                    // Safety: `offset` is under `PAGE` and the allocation is
                    // `DIRECT_CHUNK + PAGE`, so `DIRECT_CHUNK` bytes from here are
                    // inside it, aligned as `O_DIRECT` requires.
                    let buffer = unsafe { storage.as_mut_ptr().add(offset) } as *const libc::c_void;
                    let mut written = Vec::new();
                    let mut at = 0usize;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let position = (index * REGION + at) as libc::off_t;
                        let began = monotonic_ns();
                        // Safety: the descriptor is open for writing and the
                        // buffer is this thread's own, alive for the call.
                        let done = unsafe { libc::pwrite(fd, buffer, DIRECT_CHUNK, position) };
                        if done != DIRECT_CHUNK as isize {
                            return Err(std::io::Error::last_os_error());
                        }
                        written.push((began, monotonic_ns()));
                        at = (at + DIRECT_CHUNK) % REGION;
                    }
                    Ok(written)
                })
            })
            .collect();
        let mut all = Vec::new();
        for worker in workers {
            all.extend(worker.join().expect("a writer thread")?);
        }
        Ok(all)
    })
}

/// `CLOCK_MONOTONIC`, the clock the tool stamps its windows in.
fn monotonic_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Safety: `now` is this frame's own, and the clock id is a constant.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// Pressure is reported as a share of the window it was measured over, and
/// `full` is never more than `some`.
///
/// What this cannot check is whose stall it is. PSI is kept for the whole
/// machine, so on a machine where anything else is reading or writing, a
/// workload's own contribution cannot be told apart from the rest -- which is
/// why the comparison against an idle baseline is made only when the baseline
/// is low enough to mean something, and says so when it is not.
#[test]
fn pressure_is_a_share_of_its_own_window_and_full_never_exceeds_some() {
    if let Some(reason) = no_psi_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let file = scratch("io-pressure.bin");
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    let device = backing_device(Path::new(env!("CARGO_TARGET_TMPDIR"))).expect("a block device");

    let idle = run(&[
        "io.pressure",
        "--system",
        "-m",
        "poll@100ms",
        "-d",
        "400ms",
        "-e",
        "raw",
        "--format",
        "json",
    ]);
    assert!(idle.status.success(), "{}", stderr(&idle));
    let quiet = peak_stall(&json(&idle));

    let (output, _) = measure_while(
        &[
            "io.pressure",
            "--system",
            "-m",
            "poll@100ms",
            "-d",
            "1s",
            "-e",
            "raw",
            "--format",
            "json",
        ],
        &device,
        || {
            // Writes straight to the device for 600 ms, eight at a time, so
            // a task is blocked on I/O for most of the window rather than for
            // the 20 ms a buffered 64 MiB and one fsync take to drain here --
            // a stall too short to stand out against a shared machine's idle
            // one. A filesystem that refuses O_DIRECT gets the buffered write.
            if direct_writes_for(&file, WRITERS, Duration::from_millis(600)).is_err() {
                write_and_sync(&file);
            }
        },
    );
    let _ = std::fs::remove_file(&file);
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);

    // Every tick, whatever the machine was doing: the arithmetic the source
    // promises. A ratio is the stall over the interval that produced it, and
    // the share during which every task was stalled cannot exceed the share
    // during which one was.
    let events = output_of(result(&document, "io.pressure"), "raw")["events"]
        .as_array()
        .expect("raw carries its events")
        .clone();
    assert!(!events.is_empty(), "a 1 s poll at 100 ms produced no reads");
    for event in &events {
        let some = attr(event, "stall_ratio");
        let _full_ratio = attr(event, "full_stall_ratio");
        let stalled = attr(event, "stalled_ns");
        let interval = attr(event, "interval_ns");
        assert!(
            (0.0..=1.0).contains(&some),
            "a share of a window is between nothing and all of it: {event}"
        );
        // `full` is a subset of `some` by definition -- every task stalled is
        // at least one task stalled -- but the kernel keeps the two in separate
        // accumulators and writes `/proc/pressure/io` in microseconds. Measured
        // here under a saturated machine: some 39670 us, full 39671 us, one
        // microsecond the wrong way, from one read of one file. So the
        // invariant is asserted with the resolution of the thing that reports
        // it, and a violation larger than that is the tool's and not the
        // kernel's.
        let psi_quantum_ns = 1_000.0;
        let full_stalled = attr(event, "full_stalled_ns");
        assert!(
            full_stalled <= stalled + psi_quantum_ns,
            "`full` is a subset of `some` and cannot exceed it by more than the microsecond \
             /proc/pressure is written in: {event}"
        );
        assert!(
            (some - stalled / interval).abs() < 1e-6,
            "the ratio is not the stall over the interval it was measured against: {event}"
        );
    }

    let under_load = peak_stall(&document);
    if quiet > 0.05 {
        eprintln!(
            "skipped the comparison: this machine was already stalled on I/O {:.0} % of an \
             idle window, so a workload's own contribution cannot be told apart from the rest",
            quiet * 100.0
        );
        return;
    }
    assert!(
        under_load > quiet,
        "eight tasks blocked in direct writes for 600 ms are stall time, and the window that \
         held them reports {under_load:.3} against {quiet:.3} for an idle one"
    );
}

/// The largest `some` share any tick of a run reported.
fn peak_stall(document: &serde_json::Value) -> f64 {
    document["results"][0]["outputs"][0]["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .map(|event| attr(event, "stall_ratio"))
                .fold(0.0, f64::max)
        })
        .unwrap_or(0.0)
}

/// One numeric attribute of a raw event.
fn attr(event: &serde_json::Value, name: &str) -> f64 {
    event["attrs"]
        .as_array()
        .expect("an event carries its attributes")
        .iter()
        .find(|attr| attr["name"] == name)
        .map(|attr| number(&attr["value"]))
        .unwrap_or_else(|| panic!("the event carries `{name}`: {event}"))
}

// ---------------------------------------------------------------------------
// C. The loss path

/// A poll asked for faster than the file can be read counts every tick that
/// ran late, and says so where a reader will see it.
///
/// The ground truth is the machine's own: this test times a hundred reads of
/// `/proc/diskstats` itself, and only asks for an interval the file has
/// already been shown not to serve. What must then come back is a late tick
/// for nearly every tick of the run -- and, above all, not the words `no loss`.
#[test]
fn a_poll_faster_than_the_file_can_be_read_counts_every_late_tick() {
    const INTERVAL: Duration = Duration::from_micros(10);
    // The session and this test have to be asking for the same thing, or the
    // ground truth below is about an interval nobody requested.
    let written = std::fs::read_to_string(session("io-late-ticks.yml")).expect("the session file");
    assert!(
        written.contains("poll@10us"),
        "the session no longer polls at the interval this scenario measured against"
    );

    let mut costs = Vec::new();
    for _ in 0..100 {
        let started = Instant::now();
        let _ = std::fs::read_to_string("/proc/diskstats").expect("/proc/diskstats is readable");
        costs.push(started.elapsed());
    }
    costs.sort();
    let typical = costs[costs.len() / 2];
    if typical < INTERVAL {
        eprintln!(
            "skipped: one read of /proc/diskstats costs {typical:?} here, less than the \
             {INTERVAL:?} tick this scenario asks for, so nothing would run late"
        );
        return;
    }

    let output = run(&["-f", &session("io-late-ticks.yml"), "--format", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);
    let losses = &result(&document, "too fast to read")["losses"];
    let late = losses["late_ticks"].as_u64().unwrap_or(0);
    assert!(
        late > 0,
        "one read of the file costs {typical:?} and the tick is {INTERVAL:?}, so every tick ran \
         late and the run reports none: {losses}"
    );

    // Not merely some: all of them. Each round of reads produces one event per
    // device, so the number of rounds is known from the output, and an
    // interval shorter than one read cannot be met even once.
    let seen = result(&document, "too fast to read")["events_seen"]
        .as_u64()
        .expect("the run says how many events it saw");
    let devices = output_of(result(&document, "too fast to read"), "summary")["series"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(1)
        .max(1) as u64;
    let rounds = seen / devices;
    assert!(
        late + 2 >= rounds,
        "every one of the {rounds} rounds of reads took longer than the {INTERVAL:?} it was \
         given, and {late} were counted as late"
    );

    let text = run(&["-f", &session("io-late-ticks.yml")]);
    let rendered = stdout(&text);
    assert!(
        !rendered.contains("no loss"),
        "a run whose every tick was late must not report itself complete:\n{rendered}"
    );
    assert!(
        rendered.contains("late tick"),
        "the count has to reach the text output, which is what a reader sees:\n{rendered}"
    );

    // And it reaches it as one line rather than as a transcript. The reason
    // used to carry the measured duration, so every late tick was a reason of
    // its own with a count of one: a 200 ms run printed some thirteen hundred
    // lines, the count they explained was on line three, and the map grew
    // without bound for the length of the run and into every structured
    // output. That is defect 5 of `docs/scenarios/io.md`.
    let reasons = losses["reasons"].as_object().expect("the reasons");
    let about_the_tick: Vec<_> = reasons
        .iter()
        .filter(|(reason, _)| reason.contains("outran"))
        .collect();
    assert_eq!(
        about_the_tick.len(),
        1,
        "{late} late ticks have to share one reason: {reasons:?}"
    );
    assert_eq!(
        about_the_tick[0].1.as_u64(),
        Some(late),
        "and it has to carry the count it explains: {reasons:?}"
    );
    assert_eq!(
        rendered.matches("outran the").count(),
        1,
        "one line in the text a reader sees, not one per tick:\n{rendered}"
    );
    // The durations are still there, beside the count instead of in the key.
    let overrun = &losses["overrun"];
    assert_eq!(overrun["tick_ns"].as_u64(), Some(10_000));
    assert!(
        overrun["worst_ns"].as_u64().unwrap_or_default() > 10_000,
        "a tick counted as late has to have overrun it: {overrun}"
    );
}

/// Counters written by this test, mounted over the real ones, are converted
/// exactly and the gaps between them are named.
///
/// Four cases in one run, because they are four things the same reader has to
/// get right and only one of them can be produced with a real device:
///
/// 1. 2048 sectors of increase is 1048576 bytes, exactly.
/// 2. A line for a device with no `/sys/class/block` entry is left out of the
///    total and counted as a loss.
/// 3. A counter that goes backwards makes an interval unmeasurable, and is
///    counted rather than wrapped around.
/// 4. A device that appears mid-run has nothing to be measured against. It is
///    named -- and, as `docs/scenarios/io.md` records, not counted.
#[test]
fn substituted_counters_are_converted_exactly_and_their_gaps_are_named() {
    if let Some(reason) = no_namespace_here() {
        eprintln!("skipped: {reason}, so /proc/diskstats cannot be substituted");
        return;
    }
    // The name has to be one this machine has under /sys/class/block, or the
    // collector cannot tell whether its counters are its own -- which is case
    // 2, not case 1.
    let Some(real) = a_real_leaf_device() else {
        eprintln!("skipped: this machine has no block device whose counters are its own");
        return;
    };
    // Case 4's device has to be real too, or it is case 2 again: the CI's
    // runner has no `zram0`, which this scenario used to name, and read the
    // appearing device as one with no `/sys/class/block` entry.
    let Some(appearing) = real_leaf_devices().into_iter().find(|name| *name != real) else {
        eprintln!(
            "skipped: this machine has one block device whose counters are its own ({real}), \
             and a device that appears mid-run has to be a second one"
        );
        return;
    };

    let file = scratch("io-substituted.diskstats");
    let known = |sectors: u64| Counters {
        read_sectors: sectors,
        ..Counters::default()
    };
    write_now(&file, &diskstats_line(259, 0, &real, known(0)));

    let child = with_substituted_diskstats(
        &file,
        &[
            "io.bytes",
            "--system",
            "-m",
            "poll@100ms",
            "-d",
            "1500ms",
            "-e",
            "timeline",
            "--format",
            "json",
        ],
    );

    // Two clocks that are not the same one. This test's starts at `spawn`; the
    // run's starts when `unshare` has built a user and a mount namespace, bound
    // the substituted file over `/proc/diskstats` and exec'd `omn`, which under
    // a suite of nine hundred tests takes appreciably longer than it does
    // alone. Every write below has to land after the run's *first* read, or the
    // increase this scenario chose becomes the baseline it is measured against
    // and every interval reads zero -- which is what happened, reproducibly,
    // once the catalog pass gave the rest of the suite more work to do.
    //
    // So the lead-in is long enough for the namespace, the window is long
    // enough for the three writes to fall inside it wherever it starts, and the
    // assertion below detects the race it cannot rule out rather than blaming
    // the arithmetic for it.
    let step = Duration::from_millis(250);
    std::thread::sleep(Duration::from_millis(700));
    // 1. One mebibyte of sectors.
    write_now(&file, &diskstats_line(259, 0, &real, known(2048)));
    std::thread::sleep(step);
    // 2. A device this machine does not have, and 3. a counter that rewinds.
    write_now(
        &file,
        &format!(
            "{}{}",
            diskstats_line(259, 0, &real, known(1024)),
            diskstats_line(8, 0, "a-device-no-machine-has", known(4096))
        ),
    );
    std::thread::sleep(step);
    // 4. A device that was not there a moment ago.
    write_now(
        &file,
        &format!(
            "{}{}",
            diskstats_line(259, 0, &real, known(1024)),
            diskstats_line(253, 0, &appearing, known(2048))
        ),
    );

    let output = child.wait_with_output().expect("the run finishes");
    let _ = std::fs::remove_file(&file);
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);
    let request = result(&document, "io.bytes");

    // 1. The arithmetic. A sector is 512 bytes whatever the device's own block
    // size is, and the point is the increase, never the counter.
    let points: Vec<f64> = output_of(request, "timeline")["series"]
        .as_array()
        .expect("the timeline has series")
        .iter()
        .filter(|series| series["key"] == "dev:259+0")
        .flat_map(|series| series["points"].as_array().cloned().unwrap_or_default())
        .map(|point| number(&point["value"]))
        .collect();
    // A series with no movement at all is the race above, not a conversion: a
    // wrong conversion produces a wrong number, never an absent one. Skipping
    // loudly here is what keeps a scheduling delay from reading as a defect in
    // the one piece of arithmetic this scenario exists to check.
    if points.iter().all(|value| *value == 0.0) {
        eprintln!(
            "skipped the arithmetic: the run saw no movement at all ({points:?}), which means \
             its first read landed after the counters were written -- the namespace took longer \
             to build than the lead-in allowed"
        );
        return;
    }
    assert!(
        points.contains(&1_048_576.0),
        "2048 sectors of increase is 1048576 bytes exactly, and the run reports {points:?}"
    );
    assert!(
        points.iter().all(|value| *value < 2_097_152.0),
        "no interval moved more than a mebibyte, so nothing here may report the counter \
         itself: {points:?}"
    );

    let losses = &request["losses"];
    let reasons = losses["reasons"]
        .as_object()
        .map(|reasons| reasons.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let said = reasons.join("\n");

    // 2. A device that cannot be classified is left out and counted.
    assert!(
        said.contains("a-device-no-machine-has"),
        "a line for a device with no /sys entry has to be named:\n{said}"
    );
    assert!(
        losses["undecoded"].as_u64().unwrap_or(0) > 0,
        "what was left out of the total has to be counted, or the total looks complete: {losses}"
    );

    // 3. A counter that goes backwards.
    assert!(
        said.contains("went backwards"),
        "a counter that rewound has to say so rather than wrap: \n{said}"
    );

    // 4. A device that appears mid-run. It is named, and it is counted: an
    // interval of one device's I/O is missing from the run, and a reason with
    // nothing counted behind it leaves `Losses::is_clean` true and the run
    // printing `no loss`.
    assert!(
        said.contains("appeared during the run"),
        "a device with no earlier reading loses its first interval, and has to say so:\n{said}"
    );

    // The count is checked against the reasons rather than against a number,
    // because how many ticks saw each gap depends on where the rewrites of the
    // file land. Every reason recorded above is one of the three shapes that
    // leave a reading out of the total, and `undecoded` is what counts all
    // three -- so it cannot be smaller than the occurrences of them. Before
    // this was fixed the device that appeared was noted and not counted, and
    // this assertion failed by exactly those occurrences.
    let occurrences = |needle: &str| -> u64 {
        losses["reasons"]
            .as_object()
            .map(|reasons| {
                reasons
                    .iter()
                    .filter(|(reason, _)| reason.contains(needle))
                    .filter_map(|(_, count)| count.as_u64())
                    .sum()
            })
            .unwrap_or(0)
    };
    let left_out = occurrences("appeared during the run")
        + occurrences("no entry under /sys/class/block")
        + occurrences("went backwards");
    assert!(
        occurrences("appeared during the run") > 0,
        "this scenario has to provoke a device that appears mid-run: {losses}"
    );
    assert!(
        losses["undecoded"].as_u64().unwrap_or(0) >= left_out,
        "{left_out} readings were left out of the total and only {} are counted, so a run \
         whose only gap is one of them would still call itself complete: {losses}",
        losses["undecoded"]
    );
}

/// What a reader at a terminal is told when a line of the file was left out of
/// the total.
///
/// `Losses` counts five kinds and the rendered headline used to name three, so
/// a run whose only loss is `undecoded` -- this family's ordinary case, and how
/// `/proc/diskstats` says a device could not be classified -- announced `LOSS`
/// followed by three zeros, leaving the real count to be inferred from how many
/// reason lines followed. That was defect 9 of `docs/scenarios/io.md`, and the
/// headline asserted below is the fix: all five kinds named, every time.
#[test]
fn a_line_left_out_of_the_total_reaches_the_reader_at_a_terminal() {
    if let Some(reason) = no_namespace_here() {
        eprintln!("skipped: {reason}, so /proc/diskstats cannot be substituted");
        return;
    }
    let file = scratch("io-unclassifiable.diskstats");
    write_now(
        &file,
        &diskstats_line(8, 0, "a-device-no-machine-has", Counters::default()),
    );

    let child = with_substituted_diskstats(
        &file,
        &[
            "io.bytes",
            "--system",
            "-m",
            "poll@100ms",
            "-d",
            "300ms",
            "-e",
            "summary",
        ],
    );
    let output = child.wait_with_output().expect("the run finishes");
    let _ = std::fs::remove_file(&file);
    assert!(output.status.success(), "{}", stderr(&output));

    let text = stdout(&output);
    assert!(
        !text.contains("no loss"),
        "a run that left a device out of its total is not a complete one:\n{text}"
    );
    assert!(
        text.contains("a-device-no-machine-has"),
        "what was left out has to be named where a reader will see it:\n{text}"
    );

    // The headline, which is the line a reader trusts or does not. All five
    // counted kinds are named, so the one that is not zero is in it: against
    // the three-name headline this run announced `LOSS` and three zeros.
    let headline = text
        .lines()
        .find(|line| line.contains("LOSS:"))
        .unwrap_or_else(|| panic!("the run announces what it lost:\n{text}"));
    let counted = |kind: &str| -> u64 {
        headline
            .split(", ")
            .find_map(|part| part.strip_suffix(kind))
            .and_then(|count| count.split_whitespace().next_back()?.parse().ok())
            .unwrap_or_else(|| panic!("`{kind}` is named with a count in `{headline}`"))
    };
    for kind in [
        "dropped",
        "failed reads",
        "late ticks",
        "undecoded",
        "throttled",
    ] {
        let _ = counted(kind);
    }
    assert!(
        counted("undecoded") > 0,
        "the only thing this run lost is the line it could not decode, and the headline \
         has to carry that count rather than leave it to the reason lines:\n{headline}"
    );
}

/// A device on this machine whose counters are its own.
fn a_real_leaf_device() -> Option<String> {
    real_leaf_devices().into_iter().next()
}

/// Every device in `/proc/diskstats` whose counters are its own: a name
/// `/sys/class/block` knows, and not a partition.
fn real_leaf_devices() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string("/proc/diskstats") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .filter(|name| {
            let sysfs = Path::new("/sys/class/block").join(name);
            sysfs.exists() && !sysfs.join("partition").exists()
        })
        .map(str::to_owned)
        .collect()
}

/// A counting request written beside a polled one does not decide its rate.
///
/// `run_all` used to take one interval for the whole run: the smallest tick any
/// collector declared, and where none did -- which is every collector in this
/// family -- the mode of `resolved[0]`, whichever request happened to be
/// written first. A `count` request has no cadence of its own, so its fallback
/// was the default poll interval of one second, and a `poll@100ms` written
/// after it was served at one second: two points instead of eleven, with no
/// warning and no loss recorded. Written the other way round, the same file
/// measured correctly. That is defect 6 of `docs/scenarios/io.md`.
///
/// The reproduction is the pair of files it was found with. What is asserted is
/// that they now measure the same thing, because they describe the same
/// measurement.
#[test]
fn request_order_does_not_decide_another_requests_rate() {
    let scratch = scratch("io-request-order");
    std::fs::create_dir_all(&scratch).expect("a directory to write sessions into");

    const COUNTING: &str = "\x20 - label: counted\n\
                            \x20   source: io.bytes\n\
                            \x20   target: system\n\
                            \x20   mode: count\n\
                            \x20   emit: [summary]\n";
    const POLLING: &str = "\x20 - label: polled\n\
                           \x20   source: io.inflight\n\
                           \x20   target: system\n\
                           \x20   mode: poll@100ms\n\
                           \x20   emit: [raw]\n";

    // The same two requests, written in the two orders.
    let readings = |name: &str, requests: String| {
        let path = scratch.join(name);
        write_now(
            &path,
            &format!("format: json\nduration: 1s\nrequests:\n{requests}"),
        );
        let output = run(&["-f", &path.to_string_lossy(), "--format", "json"]);
        assert!(output.status.success(), "{name}: {}", stderr(&output));
        result(&json(&output), "polled")["events_seen"]
            .as_u64()
            .expect("the run says how many events it saw")
    };

    let after = readings("counting-first.yml", format!("{COUNTING}{POLLING}"));
    let before = readings("polling-first.yml", format!("{POLLING}{COUNTING}"));

    // The polled request asks for 100 ms over a second in both files: about
    // eleven readings, and never the two that a one-second fallback produced.
    // The assertion is a floor rather than a count, because a loaded machine
    // reads late and the tool says so when it does -- what cannot happen is
    // one request being served at another request's cadence.
    assert!(
        after >= 5,
        "`poll@100ms` over a second took {after} readings when written after a `count`, \
         which is the counting request's one-second fallback rather than its own rate"
    );
    assert!(
        before >= 5,
        "`poll@100ms` over a second took {before} readings when written first"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

// ---------------------------------------------------------------------------
// D. The refusal

/// The three things this tool says no to are three different answers, and the
/// session that provokes each one gets the message that belongs to it.
#[test]
fn each_refusal_names_the_thing_that_is_actually_missing() {
    // A request that cannot mean anything: a queue depth is read at an
    // instant, so it counts over no window and can produce no rate.
    let rejected = run(&["io.inflight", "--system", "-m", "count", "-e", "rate"]);
    assert_eq!(rejected.status.code(), Some(EXIT_REJECTED));
    let message = stderr(&rejected);
    assert!(
        message.contains("cannot be observed in `count` mode")
            && message.contains("poll, snapshot"),
        "a rejection has to say what the source does instead:\n{message}"
    );

    // A request this build does not implement: valid, serveable by this
    // machine, and written for nobody.
    //
    // Which of the two answers this gets depends on a privilege, and the two
    // are not interchangeable. `io.bytes --pid` resolves onto `bpf:tp`, whose
    // requirements include loading a program; with CAP_BPF the backend is
    // available and unimplemented, which is exit 3, and without it the backend
    // is unavailable and every other one in the chain refuses the target, which
    // is exit 1. Both are correct about the machine they were run on -- so the
    // scenario asserts the one that matches, rather than assuming the binary
    // carries capabilities that every relink drops.
    let unbuilt = run(&["-f", &session("io-per-process.yml")]);
    let message = stderr(&unbuilt);
    assert!(
        stdout(&unbuilt).is_empty(),
        "a machine-wide total must never be produced under one process's name:\n{}",
        stdout(&unbuilt)
    );
    if message.contains("no viable backend") {
        eprintln!(
            "skipped the unimplemented-backend half: this binary carries no CAP_BPF, so \
             the eBPF backends are unavailable rather than unwritten"
        );
        assert_eq!(unbuilt.status.code(), Some(EXIT_NO_BACKEND), "{message}");
        assert!(
            message.contains("cannot attribute per pid"),
            "the backends that could be read must say why they cannot answer this \
             target:\n{message}"
        );
    } else {
        assert_eq!(
            unbuilt.status.code(),
            Some(EXIT_NOT_IMPLEMENTED),
            "{message}"
        );
        assert!(
            message.contains("no collector is written for `io.bytes` on backend `bpf:raw_tp`")
                || message.contains("no collector is written for `io.bytes` on backend `bpf:tp`"),
            "{message}"
        );
        assert!(
            message.contains("`fs:proc` was passed over"),
            "the reader needs to know why the readable, free backend was not used:\n{message}"
        );
    }

    // A mode the catalog declares, resolved onto a backend that cannot serve
    // it. The message names the mechanism that would, and the code says the
    // same thing: this build has not written it. It used to be 1, the code for
    // a run that went wrong -- out of memory, a target that disappeared, a
    // counter that would not open -- which is defect 1 of
    // `docs/scenarios/io.md`, and it covered every `trace` request of the only
    // source here that declares the mode.
    let at_attach = run(&[
        "io.bytes", "--system", "-m", "trace", "-e", "topk", "-d", "1s",
    ]);
    let message = stderr(&at_attach);
    assert_eq!(
        at_attach.status.code(),
        Some(EXIT_NOT_IMPLEMENTED),
        "a valid request this machine can serve and this build has not written is exit \
         {EXIT_NOT_IMPLEMENTED}:\n{message}"
    );
    assert!(
        message.contains("needs the tracepoint backend"),
        "a mode `/proc/diskstats` cannot serve has to name the one that can:\n{message}"
    );
    assert!(stdout(&at_attach).is_empty(), "{}", stdout(&at_attach));
}

/// Why this kernel serves no pressure source, if it serves none: one built
/// without `CONFIG_PSI`, or booted with `psi=0`, has no `/proc/pressure`.
fn no_psi_here() -> Option<&'static str> {
    (!Path::new("/proc/pressure").exists())
        .then_some("this kernel publishes no PSI: /proc/pressure does not exist")
}

/// A machine that cannot serve a source at all says which file is missing.
///
/// The fourth answer, and the one no real machine here can be made to give:
/// every backend of `io.pressure` needs a file, and both files exist. Hiding
/// `/proc/pressure` behind an empty mount inside a private namespace is what a
/// kernel built without `CONFIG_PSI` looks like from the tool's side.
#[test]
fn a_machine_that_cannot_serve_pressure_says_which_file_it_wanted() {
    // A kernel without PSI is the machine this simulates: asked as it is.
    let output = if no_psi_here().is_some() {
        Command::new(BINARY)
            .args([
                "io.pressure",
                "--system",
                "-m",
                "poll@100ms",
                "-d",
                "200ms",
                "-e",
                "timeline",
            ])
            .output()
            .expect("omn should run")
    } else {
        if let Some(reason) = no_namespace_here() {
            eprintln!("skipped: {reason}, so a machine without PSI cannot be simulated");
            return;
        }
        Command::new("unshare")
            .args([
                "-Urm",
                "sh",
                "-c",
                &format!(
                    "mount -t tmpfs none /proc/pressure && exec {BINARY} io.pressure --system \
                 -m poll@100ms -d 200ms -e timeline"
                ),
            ])
            .output()
            .expect("unshare should run")
    };

    let message = stderr(&output);
    if message.contains("mount") && message.contains("denied") {
        eprintln!("skipped: this machine refuses a tmpfs mount inside a user namespace");
        return;
    }
    assert_ne!(output.status.code(), Some(0), "{message}");
    assert!(
        message.contains("no viable backend"),
        "a machine that cannot serve a source has to say so, not fail:\n{message}"
    );
    assert!(
        message.contains("/proc/pressure/io"),
        "the refusal has to name the file it wanted:\n{message}"
    );
    assert!(
        stdout(&output).is_empty(),
        "nothing may be reported for a source no backend can serve:\n{}",
        stdout(&output)
    );
    // And the code says what the message says. It used to be 2 -- the code for
    // a request that cannot mean anything -- for a machine that cannot serve
    // one that can, which is defect 3 of `docs/scenarios/io.md`.
    assert_eq!(
        output.status.code(),
        Some(EXIT_NO_BACKEND),
        "a kernel without PSI is a machine to change, not a request to rewrite:\n{message}"
    );
}

// ---------------------------------------------------------------------------
// The sessions themselves

/// Every session this stream ships validates, and the two that measure nothing
/// on a bare machine still run.
///
/// `tests/family_io.rs` checks that they validate; what is checked here is
/// that the ones a reader is told to run without any setup actually produce
/// series, because a session that validates and then reports nothing is a
/// worse document than no session at all.
#[test]
fn the_sessions_a_reader_can_run_unaided_produce_series() {
    for name in ["io-device-load.yml", "io-pressure.yml"] {
        if name == "io-pressure.yml" {
            if let Some(reason) = no_psi_here() {
                eprintln!("{name} skipped: {reason}");
                continue;
            }
        }
        let output = run(&["-f", &session(name), "--format", "json"]);
        assert!(output.status.success(), "{name}: {}", stderr(&output));
        let document = json(&output);
        let results = document["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} reports results"));
        assert!(!results.is_empty(), "{name} reported nothing at all");
        for result in results {
            let series = result["outputs"][0]["series"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0);
            assert!(
                series > 0,
                "{name}: `{}` produced no series, so the session documents a measurement that \
                 does not happen",
                result["label"]
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The block layer, through its own tracepoints
//
// Everything below this line is about `io.latency`, `io.queue`, `io.size` and
// `io.writeback`, which see one request at a time rather than a device's
// counters. What they can be checked against is different in kind: not a
// quantity of bytes but a *number of requests*, chosen by the workload and
// found one for one.

/// A buffer the kernel will accept for an `O_DIRECT` read.
///
/// The requirement is alignment, not size: a direct read's buffer, offset and
/// length all have to be multiples of the device's logical block size, and a
/// `Vec` is aligned to one byte. So one page is over-allocated and the aligned
/// window inside it is what is read into.
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
        // Safety: `offset` is under `PAGE` and the allocation is `2 * PAGE`, so
        // the window of `PAGE` bytes starting here is inside it.
        unsafe { self.storage.as_mut_ptr().add(self.offset) as *mut libc::c_void }
    }
}

const PAGE: usize = 4096;

/// This thread's id, which is what makes a machine-wide tracer into a
/// measurement of one workload.
///
/// `--pid` would not do: `cargo test` runs its tests in parallel threads of one
/// process, so a process filter catches every other scenario's I/O as well. A
/// thread filter catches exactly the reads the next few lines issue, which is
/// what turns "at least this much" into "exactly this many".
/// Issues `count` reads of one page each, straight to the device.
///
/// `O_DIRECT` is the whole guarantee. The page cache is bypassed, so every read
/// reaches the block layer; the reads are issued one at a time, so the block
/// layer has nothing to merge them with; and each is one page at a page-aligned
/// offset, so none of them spans two extents. One read, one request, one page.
fn direct_reads(path: &Path, count: usize) -> std::io::Result<usize> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)?;
    let fd = file.as_raw_fd();
    let mut page = AlignedPage::new();
    let pointer = page.pointer();
    let mut issued = 0;
    for index in 0..count {
        // Safety: the descriptor is open for reading and the buffer is a page
        // of this scope's own memory, aligned as `O_DIRECT` requires.
        let read = unsafe { libc::pread(fd, pointer, PAGE, (index * PAGE) as libc::off_t) };
        if read != PAGE as isize {
            return Err(std::io::Error::last_os_error());
        }
        issued += 1;
    }
    Ok(issued)
}

/// `direct_reads`, in a program `omn` launches, and on a thread of its own.
///
/// Launched rather than run in this process, because the window is then the
/// program's lifetime: `omn` attaches before it execs the program, and the
/// last read has completed before the program exits. A tracer spawned beside
/// the test and given a head start has neither edge. Under the rest of this
/// binary it attached as late as 220 ms into a workload it had been given
/// 500 ms to be ready for, and the scenario read that as 3 % to 90 % of the
/// requests missing with no loss reported -- which is what a silent loss looks
/// like, and was not one.
///
/// The reads are made on a worker thread, and the account names it, so that a
/// request attributed to the process rather than the thread still shows: it
/// would carry the main thread's id, which is the process's.
///
/// And the file is evicted from the page cache first, and the program says how
/// much of it was still resident when it started reading. `O_DIRECT` is not a
/// promise that every read reaches the device: on btrfs, fifty direct reads of
/// one page that was resident produced 49 requests, and the partition's own
/// completed-read counter moved by 49 as well. The tracer was right and "one
/// read, one request" was not, for the one read btrfs answered from memory.
const DIRECT_READS: &str = r#"
import ctypes, json, mmap, os, sys, threading
path, count, account = sys.argv[1], int(sys.argv[2]), sys.argv[3]
done = {}
def resident(fd, size):
    libc = ctypes.CDLL(None, use_errno=True)
    libc.mmap.restype = ctypes.c_void_p
    libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int,
                          ctypes.c_int, ctypes.c_long]
    address = libc.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
    pages = (size + mmap.PAGESIZE - 1) // mmap.PAGESIZE
    vector = (ctypes.c_ubyte * pages)()
    if libc.mincore(ctypes.c_void_p(address), ctypes.c_size_t(size), vector) != 0:
        return None
    libc.munmap(ctypes.c_void_p(address), ctypes.c_size_t(size))
    return sum(vector[page] & 1 for page in range(pages))
def reads():
    done["tid"] = threading.get_native_id()
    cached = os.open(path, os.O_RDONLY)
    os.posix_fadvise(cached, 0, 0, os.POSIX_FADV_DONTNEED)
    done["resident"] = resident(cached, count * mmap.PAGESIZE)
    os.close(cached)
    try:
        fd = os.open(path, os.O_RDONLY | os.O_DIRECT)
    except OSError as error:
        done["refused"] = str(error)
        return
    page = mmap.mmap(-1, mmap.PAGESIZE)
    issued = 0
    try:
        for index in range(count):
            if os.preadv(fd, [page], index * mmap.PAGESIZE) != mmap.PAGESIZE:
                break
            issued += 1
    except OSError as error:
        done["refused"] = str(error)
    done["issued"] = issued
worker = threading.Thread(target=reads)
worker.start()
worker.join()
done["pid"] = os.getpid()
json.dump(done, open(account, "w"))
"#;

/// Launches `DIRECT_READS` under `io.size` and returns the run and the
/// program's account, or why the property cannot be created here.
fn launched_direct_reads(name: &str, reads: usize) -> Result<(Output, serde_json::Value), String> {
    let path = direct_read_file(&format!("{name}.bin"), reads);
    let workload = script(&format!("{name}.py"), DIRECT_READS);
    let account_path = scratch(&format!("{name}.json"));
    let _ = std::fs::remove_file(&account_path);
    let output = run(&[
        "io.size",
        "-m",
        "trace",
        "-e",
        "raw",
        "--format",
        "json",
        "--",
        "python3",
        workload.to_str().expect("a path"),
        path.to_str().expect("a path"),
        &reads.to_string(),
        account_path.to_str().expect("a path"),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let did = account(&account_path);
    if let Some(error) = did["refused"].as_str() {
        // A filesystem that refuses `O_DIRECT` cannot produce the property
        // these scenarios measure, and saying so beats asserting something
        // else about it.
        return Err(format!(
            "{} refuses O_DIRECT ({error}), so a read of it is not one block request",
            path.display()
        ));
    }
    match did["resident"].as_u64() {
        Some(0) => {}
        resident => {
            return Err(format!(
                "{resident:?} of {reads} pages of {} were still in the page cache after it was \
                 evicted, and a direct read of a resident page can be answered without a request",
                path.display()
            ))
        }
    }
    Ok((output, did))
}

/// A file of incompressible bytes, on the device, large enough for `pages`
/// pages of direct reads.
fn direct_read_file(name: &str, pages: usize) -> PathBuf {
    use std::io::Write;
    let path = scratch(name);
    let bytes = pages * PAGE;
    if std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0) < bytes as u64 {
        std::fs::create_dir_all(path.parent().expect("a parent")).ok();
        let mut file = std::fs::File::create(&path).expect("a file to read from");
        file.write_all(&incompressible(bytes))
            .expect("the whole file written");
        file.sync_all().expect("the file on the device");
    }
    path
}

/// Every `raw` event of a result, as `(attribute, value)` maps.
fn raw_events(
    result: &serde_json::Value,
) -> Vec<std::collections::BTreeMap<String, serde_json::Value>> {
    output_of(result, "raw")["events"]
        .as_array()
        .expect("a raw emit carries its events")
        .iter()
        .map(|event| {
            event["attrs"]
                .as_array()
                .expect("an event carries its attributes")
                .iter()
                .map(|attr| {
                    (
                        attr["name"].as_str().expect("a name").to_owned(),
                        attr["value"].clone(),
                    )
                })
                .collect()
        })
        .collect()
}

/// The losses a result reports.
fn losses(result: &serde_json::Value) -> &serde_json::Value {
    &result["losses"]
}

// ---------------------------------------------------------------------------
// Ground truth: a number of requests chosen in advance

/// A known number of direct reads is found one for one, at the size they were
/// issued at.
///
/// This is the family's sharpest ground truth and the one that could not be
/// taken before the block layer's tracepoints were collected. `/proc/diskstats`
/// can say that 16 MiB moved; only this can say that it moved as 4096 requests
/// of one page each, and the difference between those two statements is the
/// whole reason the eBPF backends exist.
///
/// Nothing here is a plausible constant. A collector reading `__data_len`
/// after the block layer decremented it would report zero; one reporting
/// sectors would be out by 512; one counting completions rather than issues
/// would differ on every partially completed request; one that attributed to
/// the process rather than the thread would put every request on the
/// program's main thread, which made none of them.
#[test]
fn a_known_number_of_direct_reads_is_found_one_for_one() {
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }

    const READS: usize = 4096;
    let (output, did) = match launched_direct_reads("io-direct-reads", READS) {
        Ok(run) => run,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    assert_eq!(
        did["issued"].as_u64(),
        Some(READS as u64),
        "the program's account: {did}"
    );
    let worker = did["tid"].as_u64().expect("the reading thread");
    let document = json(&output);
    let result = result(&document, "io.size");
    // The program is the whole target, and the interpreter loading itself from
    // a cold cache is requests too; they are its main thread's, and not the
    // ones counted here.
    let on_worker: Vec<_> = raw_events(result)
        .into_iter()
        .filter(|attrs| attrs["tid"].as_u64() == Some(worker))
        .collect();
    // Nor is every read a one-page request, even evicted and direct. On
    // btrfs the program's first `O_DIRECT` read fell back to a buffered one,
    // on every run measured: it arrived as a 16 KiB request flagged `RA`, at
    // the sector of the file's first page, issued before any other -- the
    // readahead window around the page that was asked for. The partition's
    // completed-read counter moved by exactly the reads issued, so that request
    // is the read, and a readahead is the only other shape one may take. A
    // collector reporting sectors, or `__data_len` after the block layer
    // consumed it, turns every request into a size that is not a page, which
    // the count below catches.
    let (events, other): (Vec<_>, Vec<_>) = on_worker
        .into_iter()
        .partition(|attrs| attrs["bytes"].as_u64() == Some(PAGE as u64));
    for attrs in &other {
        let rwbs = attrs["rwbs"].as_str().unwrap_or_default();
        assert!(
            rwbs.contains('A'),
            "a request of {} bytes on the reading thread that is not a readahead: `{rwbs}`",
            attrs["bytes"]
        );
    }
    // And one per page: a request counted twice would repeat a sector.
    let sectors: std::collections::BTreeSet<u64> = events
        .iter()
        .chain(&other)
        .filter_map(|attrs| attrs["sector"].as_u64())
        .collect();
    assert_eq!(
        sectors.len(),
        events.len() + other.len(),
        "a sector was requested twice"
    );

    // One request per read. The window is the program's lifetime, so it has no
    // edge for a request to fall off: every read was issued after the attach
    // and completed before the exit. Above `READS` is double counting, always.
    let found = events.len() + other.len();
    assert!(
        found <= READS,
        "{found} requests for {READS} reads on thread {worker}: something is counted twice"
    );
    // Below it is a loss, and it has to be one the run reported. The block
    // programs are skipped by the kernel when an interrupt re-enters one on its
    // own cpu, and under the rest of this binary that is every run: 2 to 23 of
    // these reads, counted in `recursion_misses`. The count is the machine's
    // -- it cannot say whose request a skipped run was -- so it bounds this
    // thread's shortfall rather than equalling it. With nothing reported, the
    // count must be exact.
    let dropped = losses(result)["dropped"].as_u64().expect("a drop count") as usize;
    assert!(
        found + dropped >= READS,
        "{READS} one-page reads on thread {worker} produced {} one-page requests and {} \
         readaheads on it, and the run reported losing only {dropped}: {}",
        events.len(),
        other.len(),
        losses(result)
    );
    if dropped > 0 && found < READS {
        eprintln!(
            "note: {} of {READS} reads missing, within the {dropped} runs the kernel reported \
             skipping; exactness was not creatable in this run",
            READS - found
        );
    }

    // Reads, and attributed to the thread that made them. `rwbs` is the
    // kernel's own letters, so `R` is the first of them for any read.
    for attrs in events.iter().chain(&other) {
        let rwbs = attrs["rwbs"].as_str().unwrap_or_default();
        assert!(rwbs.starts_with('R'), "a read reported as `{rwbs}`");
    }
    // And they are the thread's, not the process's: attributed by the process,
    // every one would carry the main thread's id and the filter above would
    // have kept none of them.
    assert_ne!(
        did["pid"].as_u64(),
        Some(worker),
        "the reads ran on a thread of their own"
    );
}

/// The eBPF request count and the kernel's own operation count agree.
///
/// This is the backend comparison this family could not make until now, and it
/// is across sources rather than across a chain: `/proc/diskstats` counts the
/// operations a device completed and the block tracer counts the requests it
/// was issued, and for the same window those are the same requests counted
/// twice by two mechanisms that share no code.
///
/// The check is one-sided on purpose. `io.size` is filtered to one program and
/// the kernel's counter is not, so the counter is the ceiling: the machine's
/// own background I/O is in it and cannot be in the other. What the comparison
/// catches is the failure that matters -- a tracer reporting more requests than
/// the device completed, which is double counting, or a fraction of them,
/// which is silent loss.
#[test]
fn the_tracer_and_the_kernels_own_counter_see_the_same_requests() {
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let device = match backing_device(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        Ok(device) => device,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };

    const READS: usize = 4096;
    // Written before the counters are read, so that writing it is not in them.
    direct_read_file("io-agreement.bin", READS);
    let before = counters_of(&device);
    let run = launched_direct_reads("io-agreement", READS);
    let kernel = counters_of(&device).since(&before);
    let (output, did) = match run {
        Ok(run) => run,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    assert_eq!(
        did["issued"].as_u64(),
        Some(READS as u64),
        "the program's account: {did}"
    );
    let document = json(&output);
    let result = result(&document, "io.size");
    let events = raw_events(result);
    let traced_bytes: u64 = events
        .iter()
        .filter_map(|attrs| attrs.get("bytes")?.as_u64())
        .sum();

    assert!(
        events.len() as u64 <= kernel.reads + kernel.writes,
        "the tracer counted {} requests and the device completed {} over a strictly longer \
         window, so one of them is counting something twice",
        events.len(),
        kernel.reads + kernel.writes
    );
    assert!(
        traced_bytes <= kernel.bytes(),
        "the tracer saw {traced_bytes} bytes and the kernel counted {} over a window that \
         contains it",
        kernel.bytes()
    );
    // And the other side: the reads were issued, so they are in both accounts,
    // and the window has no edge to lose them at. A read the kernel skipped the
    // program for is counted as dropped, and was one page -- see
    // `a_known_number_of_direct_reads_is_found_one_for_one`.
    let dropped = losses(result)["dropped"].as_u64().expect("a drop count");
    // Who the traced requests were credited to, for the failure that is still
    // open (the flaky tally's #19, class d): a total miss, "0 bytes traced",
    // whose cause the coordinator's measurement ruled one hypothesis out of.
    // The counts say which task the reads went to when they did not go to the
    // workload's thread.
    let mut by_comm: BTreeMap<String, u64> = BTreeMap::new();
    for attrs in &events {
        let comm = attrs
            .get("comm")
            .and_then(|value| value.as_str())
            .unwrap_or("(none)");
        *by_comm.entry(comm.to_owned()).or_default() += 1;
    }
    assert!(
        traced_bytes + dropped * PAGE as u64 >= (READS * PAGE) as u64,
        "{} bytes traced for the {} the workload read, and {dropped} requests reported lost: \
         {}; the traced requests by task: {by_comm:?}",
        traced_bytes,
        READS * PAGE,
        losses(result)
    );
}

/// A recycled request pointer is told apart by its scope, as far as the
/// kernel's own stamp can tell them apart -- and this says how far that is.
///
/// The catalog says the key is `(pointer, open timestamp)` and not the pointer,
/// because a `struct request` is freed the instant it completes and handed
/// straight back out. Under the workload below a few hundred pointers carry a
/// hundred and sixty thousand requests, so the claim is testable.
///
/// **And it is not quite true, which is what this scenario is really for.** The
/// scope is `request.start_time_ns`, which the kernel takes with
/// `blk_time_get_ns()` -- and that function caches its reading for the lifetime
/// of the submitting task's *plug*, so every request created inside one plug
/// section carries the same timestamp. Two lives of one pointer inside one plug
/// section are therefore indistinguishable. Measured under eight concurrent
/// readers driven from outside the suite:
///
/// ```text
/// 166448 events, 527 pointers, 165921 reuses, 5722 keys used twice
/// ```
///
/// The scope separated 96.6 % of the reuses there and a bare pointer would have
/// separated none of them, which is the separation this asserts. How many are
/// left over depends on how the plugs fall, so the run reports its own number
/// rather than asserting one; the residual is a limit of the key rather than of
/// this collector, and `docs/families/io.md` records what would close it.
#[test]
fn a_recycled_request_pointer_is_told_apart_by_its_scope() {
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }

    const READERS: usize = 8;
    const READS_EACH: usize = 12_000;
    const PAGES: usize = 16_384;

    let path = direct_read_file("io-recycling.bin", PAGES);
    if direct_reads(&path, 1).is_err() {
        eprintln!("skipped: this filesystem refuses O_DIRECT");
        return;
    }

    let child = Command::new(BINARY)
        .args([
            "io.latency",
            "--system",
            "-m",
            "trace",
            "-e",
            "raw",
            "-d",
            "3s",
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    std::thread::sleep(Duration::from_millis(500));
    std::thread::scope(|scope| {
        for reader in 0..READERS {
            let path = path.clone();
            scope.spawn(move || {
                use std::os::unix::fs::OpenOptionsExt;
                use std::os::unix::io::AsRawFd;
                let Ok(file) = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(&path)
                else {
                    return;
                };
                let fd = file.as_raw_fd();
                let mut page = AlignedPage::new();
                let pointer = page.pointer();
                for index in 0..READS_EACH {
                    let at = ((reader * 997 + index) % PAGES) * PAGE;
                    // Safety: as in `direct_reads`.
                    unsafe { libc::pread(fd, pointer, PAGE, at as libc::off_t) };
                }
            });
        }
    });
    let output = child.wait_with_output().expect("omn should finish");
    assert!(output.status.success(), "{}", stderr(&output));

    let document = json(&output);
    let result = result(&document, "io.latency");
    let events = output_of(result, "raw")["events"]
        .as_array()
        .expect("a raw emit carries its events");

    let mut pointers: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut keys: std::collections::BTreeSet<(u64, u64)> = std::collections::BTreeSet::new();
    let mut collisions = 0usize;
    for event in events {
        let key = event["keys"]
            .as_array()
            .expect("an event carries its keys")
            .iter()
            .find_map(|key| key.get("blk"))
            .unwrap_or_else(|| panic!("every `io.latency` event carries a blk key: {event}"));
        let pointer = key["ptr"].as_u64().expect("a pointer");
        let scope = key["open_ts"].as_u64().expect("a scope");
        assert_ne!(
            scope, 0,
            "a key with no scope is a bare pointer under a name that promises more"
        );
        pointers.insert(pointer);
        if !keys.insert((pointer, scope)) {
            collisions += 1;
        }
    }

    let reuses = events.len().saturating_sub(pointers.len());
    if reuses == 0 {
        eprintln!(
            "skipped the half that matters: none of the {} request pointers in this run was \
             reused, so the run says nothing about recycling. Every key still carried a scope",
            pointers.len()
        );
        return;
    }

    // The separation, and it is an order of magnitude rather than a threshold:
    // a bare pointer would have given as many keys as there are pointers, and
    // the scope gives hundreds of times more. That is the whole claim
    // `scoped_eq` rests on.
    assert!(
        keys.len() > pointers.len() * 10,
        "{} pointers carried {} requests and produced only {} distinct keys: the scope is not \
         separating the lives of a pointer, and a join over them would attribute one request's \
         latency to another",
        pointers.len(),
        events.len(),
        keys.len()
    );

    // And what the scope does *not* separate, said with its number rather than
    // left out. `blk_time_get_ns` caches its reading for the lifetime of a
    // plug, so requests created inside one plug section share a timestamp, and
    // two lives of one pointer inside one section are one key.
    eprintln!(
        "{} events, {} pointers, {reuses} reuses, {collisions} keys used twice: the scope \
         separated {:.1} % of the reuses, and the rest were created inside one plug section, \
         where the kernel's own timestamp does not change",
        events.len(),
        pointers.len(),
        100.0 * (reuses - collisions.min(reuses)) as f64 / reuses as f64,
    );
}

// ---------------------------------------------------------------------------
// The join the engine was written for

/// The two halves of a request join on the scoped key, across two collectors
/// that never met.
///
/// `scoped_eq` is one of the four match kinds the join engine implements and
/// until these collectors existed it was the only one **nothing produced**.
/// What this checks is not that the engine works -- `tests/join_engine.rs`
/// holds that -- but that two independently loaded program sets agree about
/// what a request *is*, which they only do because the scope is the kernel's
/// own `request.start_time_ns` and not a timestamp either of them took.
#[test]
fn the_two_halves_of_a_request_join_across_two_collectors() {
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }

    let path = scratch("io-join.bin");
    let (output, _) = match backing_device(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        Ok(device) => measure_while(
            &["-f", &session("io-request-halves.yml"), "--format", "json"],
            &device,
            || write_and_sync(&path),
        ),
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);

    let joins = document["joins"]
        .as_array()
        .unwrap_or_else(|| panic!("the session declares a join: {document}"));
    let join = joins
        .iter()
        .find(|entry| entry["rule"] == "queue-then-device")
        .unwrap_or_else(|| panic!("the join ran: {document}"));

    assert_eq!(
        join["evidence"], "KEY",
        "a shared typed key is KEY evidence and nothing stronger"
    );
    let matched = number(&join["matched"]);
    let unmatched = number(&join["unmatched"]);
    if matched + unmatched == 0.0 {
        eprintln!(
            "skipped: no request was seen by both halves in this window, so the join had nothing \
             to do. `io.queue` sees an issue and `io.latency` a completion, and a machine quiet \
             enough for neither to fire produces no correlation to check"
        );
        return;
    }

    // The claim: a completion whose request this run also saw issued is
    // attributed, and nothing else is. An unmatched completion is a request
    // that was already in flight when the collectors attached, which is a
    // property of the window rather than a failure -- so what is asserted is
    // that the matched ones exist and that the engine called them KEY.
    assert!(
        matched > 0.0,
        "no completion was joined to its issue, and {unmatched} were not: the two collectors \
         disagree about what a request is"
    );
    let key = join["top"][0]["key"]
        .as_str()
        .expect("the join names the keys it matched on");
    assert!(
        key.starts_with("blk:0x") && key.contains('@'),
        "the key has to carry the pointer and which life of it this is: {key}"
    );
}

// ---------------------------------------------------------------------------
// C. The loss path

/// Driven hard enough, the ring overflows and the run says how much it lost.
///
/// The BPF ring buffer does not report its own overflows: `bpf_ringbuf_output`
/// returns an error, the record is gone, and nothing anywhere would record that
/// it existed. So every program in this layer carries its own drop counter, and
/// this is the check that the counter reaches the output.
///
/// The arithmetic of the overflow: the ring holds 256 KiB, a record is 96 bytes
/// plus an 8-byte header, and it is drained every ten milliseconds -- so it
/// starts losing above roughly 250 000 requests a second. Thirty-two concurrent
/// readers of one page each get there on this machine's NVMe. A slower device
/// cannot, and this says so with the rate it managed rather than passing
/// quietly.
#[test]
fn driving_the_block_tracer_past_the_ring_says_how_much_it_lost() {
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }

    const READERS: usize = 32;
    const READS_EACH: usize = 40_000;
    /// Above this many requests a second the ring cannot be drained in time.
    const OVERFLOWS_ABOVE: f64 = 250_000.0;

    let path = direct_read_file("io-overflow.bin", 131_072);
    if direct_reads(&path, 1).is_err() {
        eprintln!("skipped: this filesystem refuses O_DIRECT");
        return;
    }

    let child = Command::new(BINARY)
        .args([
            "io.size", "--system", "-m", "trace", "-e", "summary", "-d", "3s", "--format", "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    std::thread::sleep(Duration::from_millis(500));

    let started = Instant::now();
    std::thread::scope(|scope| {
        for reader in 0..READERS {
            let path = path.clone();
            scope.spawn(move || {
                use std::os::unix::fs::OpenOptionsExt;
                use std::os::unix::io::AsRawFd;
                let Ok(file) = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(&path)
                else {
                    return;
                };
                let fd = file.as_raw_fd();
                let mut page = AlignedPage::new();
                let pointer = page.pointer();
                for index in 0..READS_EACH {
                    let at = ((reader * 4_000 + index) % 131_072) * PAGE;
                    // Safety: as in `direct_reads`.
                    unsafe { libc::pread(fd, pointer, PAGE, at as libc::off_t) };
                }
            });
        }
    });
    let elapsed = started.elapsed().as_secs_f64();

    let output = child.wait_with_output().expect("omn should finish");
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);
    let result = result(&document, "io.size");
    let losses = losses(result);
    let dropped = losses["dropped"].as_u64().unwrap_or(0);
    let seen = number(&result["events_seen"]);
    let rate = (READERS * READS_EACH) as f64 / elapsed;

    if dropped == 0 {
        eprintln!(
            "skipped the loss itself: {READERS} readers issued {} requests in {elapsed:.2}s, \
             which is {rate:.0} a second, and the ring overflows above about \
             {OVERFLOWS_ABOVE:.0}. This device cannot be driven hard enough here, and the run \
             correctly reported no loss over {seen} events",
            READERS * READS_EACH
        );
        return;
    }

    // The loss reached the output, and it says which loss it was. A count with
    // no reason is a number a reader cannot act on.
    let reasons = losses["reasons"]
        .as_object()
        .map(|reasons| reasons.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("ring was full")),
        "{dropped} records were dropped and the run does not say the ring overflowed: {reasons:?}"
    );
    // And the count is a real fraction of the run rather than a token. A tool
    // that admitted one dropped record out of six hundred thousand would be
    // telling a reader the distribution is complete.
    assert!(
        dropped > 0 && (dropped as f64) < seen + dropped as f64,
        "{dropped} dropped against {seen} seen is not a fraction of anything"
    );
    eprintln!(
        "the ring lost {dropped} of {} requests at {rate:.0} a second, and said so",
        seen as u64 + dropped
    );
}

// ---------------------------------------------------------------------------
// `io.writeback`: the half of a write nobody waits for

/// A `sync` produces rounds that say they waited for the device.
///
/// `sync(2)` does not return until the data is out, and it asks for
/// `WB_SYNC_ALL` -- the mode in which the flusher waits for the device rather
/// than handing it work and moving on. So a window containing a `sync` of a
/// freshly written file must contain at least one `WB_SYNC_ALL` round, and that
/// is a property this test created rather than one it observed.
///
/// What is **not** checked here is the page count, and the reason is measured
/// rather than assumed: see `docs/families/io.md`.
#[test]
fn a_sync_produces_writeback_rounds_that_waited_for_the_device() {
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }

    let path = scratch("io-writeback.bin");
    let child = Command::new(BINARY)
        .args([
            "io.writeback",
            "--system",
            "-m",
            "trace",
            "-e",
            "raw",
            "-d",
            "4s",
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    std::thread::sleep(Duration::from_millis(500));

    {
        use std::io::Write;
        let mut file = std::fs::File::create(&path).expect("a file to write");
        file.write_all(&incompressible(KNOWN_BYTES as usize))
            .expect("the whole file written");
        // Deliberately not `sync_all`: an `fsync` of one file goes down the
        // filesystem's own path and may never reach the flusher at all. It is
        // `sync(2)` that puts the flusher into WB_SYNC_ALL, which is the
        // property this scenario chose.
        drop(file);
    }
    // Safety: `sync` takes nothing and returns nothing.
    unsafe { libc::sync() };

    let output = child.wait_with_output().expect("omn should finish");
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);
    let result = result(&document, "io.writeback");
    let events = raw_events(result);

    let waited: Vec<_> = events
        .iter()
        .filter(|attrs| attrs.get("sync_mode").and_then(|v| v.as_str()) == Some("all"))
        .collect();
    assert!(
        !waited.is_empty(),
        "a `sync` ran inside the window and no round reported WB_SYNC_ALL; the {} rounds seen \
         were {:?}",
        events.len(),
        events
            .iter()
            .filter_map(|attrs| attrs.get("sync_mode")?.as_str())
            .collect::<Vec<_>>()
    );

    // Every round's duration fits inside the window it was measured in. A
    // duration computed from an uninitialised timestamp would be the whole
    // uptime, which is the shape this catches.
    for attrs in &events {
        let duration = attrs["writeback_ns"].as_u64().expect("a duration");
        assert!(
            duration < 4_000_000_000,
            "a round lasted {duration} ns inside a four-second window"
        );
    }
}

// ---------------------------------------------------------------------------
// D. The refusal

/// Each of the block tracer's refusals names the thing that is actually
/// missing, and returns the code that says whose gap it is.
///
/// Three different answers, and a caller scripting around them reads the exit
/// code: 2 is a request that cannot mean anything, 1 is a machine that cannot
/// serve it, 3 is a build that has not written it. A refusal that sends a
/// reader to the wrong one costs more than no message at all.
#[test]
fn each_block_refusal_names_the_thing_that_is_actually_missing() {
    // A sampled latency is a latency whose beginning or end was not seen, and
    // the request cannot mean anything however it is run.
    let sampled = run(&[
        "io.latency",
        "--system",
        "-m",
        "sample@997hz",
        "-e",
        "hist",
        "-d",
        "200ms",
    ]);
    assert_eq!(
        sampled.status.code(),
        Some(EXIT_REJECTED),
        "a sampled latency is a request to rewrite: {}",
        stderr(&sampled)
    );

    // `io.queue` declares no process target, so the validator refuses before
    // anything is attached: the insert happens in a kworker for a write, and a
    // per-process queueing time would be the reads only without saying so.
    let per_process = run(&[
        "io.queue", "--pid", "1", "-m", "trace", "-e", "hist", "-d", "200ms",
    ]);
    assert_eq!(
        per_process.status.code(),
        Some(EXIT_REJECTED),
        "the catalog gives `io.queue` no pid target: {}",
        stderr(&per_process)
    );
    assert!(
        stderr(&per_process).contains("pid"),
        "the refusal has to name the target it is refusing: {}",
        stderr(&per_process)
    );

    // A cgroup target the catalog offers and this build does not serve: a gap
    // in the build, so exit 3, and the message says where the request for it
    // is written down.
    let cgrouped = run(&[
        "io.latency",
        "--cgroup",
        "/sys/fs/cgroup",
        "-m",
        "trace",
        "-e",
        "hist",
        "-d",
        "200ms",
    ]);
    if cgrouped.status.code() == Some(EXIT_NOT_IMPLEMENTED) {
        let message = stderr(&cgrouped);
        assert!(
            message.contains("cgroup") && message.contains("open-findings"),
            "a target this build has not written has to say so and say where: {message}"
        );
    }

    // A task target for writeback, which has no task at all.
    let attributed = run(&[
        "io.writeback",
        "--pid",
        "1",
        "-m",
        "trace",
        "-e",
        "hist",
        "-d",
        "200ms",
    ]);
    assert_ne!(attributed.status.code(), Some(0));
    let message = stderr(&attributed);
    assert!(
        message.contains("pid") || message.contains("kworker"),
        "the refusal has to say why writeback has no process: {message}"
    );
}

/// The sessions this stream ships, run unaided.
///
/// A session that validates and then measures nothing is a worse document than
/// no session at all. These four need no setup: they watch the whole machine,
/// and a machine that is doing no I/O at all for three seconds is itself worth
/// saying rather than asserting past.
#[test]
fn the_block_sessions_run_and_say_what_they_found() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let path = scratch("io-sessions.bin");
    for name in [
        "io-device-latency.yml",
        "io-known-requests.yml",
        "io-writeback-rounds.yml",
        "io-ring-overflow.yml",
    ] {
        let child = Command::new(BINARY)
            .args(["-f", &session(name), "--format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("omn should run");
        std::thread::sleep(Duration::from_millis(300));
        write_and_sync(&path);
        let output = child.wait_with_output().expect("omn should finish");
        assert!(output.status.success(), "{name}: {}", stderr(&output));
        let document = json(&output);
        let results = document["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} reports results"));
        assert!(!results.is_empty(), "{name} reported nothing at all");
        for result in results {
            // Not "produced series": a machine can genuinely be idle. What must
            // hold is that the run accounted for itself either way.
            assert!(
                result["events_seen"].is_number(),
                "{name}: `{}` did not report how many events it saw",
                result["label"]
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The layers above the block one: `io.syscall`, `io.fs` and `io.fsync`
//
// Three sources over fentry, the eighth wave's attach kind, and the first in
// this family that see a call rather than a request. Ground truth here is a
// number of calls the workload made, on descriptors and inodes it reports
// itself, and every scenario is a launched Python program for that reason: it
// states what it did in a file the test reads afterwards, so the reference is
// the program's own account and never the tool's.
//
// The four classes, for all three sources:
//
// - **ground truth**: a known number of reads, writes and syncs, found call for
//   call on the descriptor and the inode the program names; a pipe wait the
//   program timed itself, found in one layer and absent from the other;
// - **the capability sweep**: the three are in `collected_sources` above;
// - **the loss path**: more distinct rows in one drain than the histogram
//   holds, and more syncs in one drain than the ring holds;
// - **the refusal**: a cgroup target, a mode the source cannot mean, and a
//   binary without the capabilities.
//
// `docs/families/io.md` has what each one measured and what it could not.

/// Whether this build can attach an fentry program at all, and why not.
///
/// The same argument as `no_bpf_here`: the capabilities are file capabilities
/// every relink drops, and a suite that quietly passed without them would be
/// reporting a green attach kind that never ran.
fn no_fentry_here() -> Option<String> {
    let output = run(&[
        "io.fs", "--system", "-m", "trace", "-e", "summary", "-d", "100ms",
    ]);
    if output.status.success() {
        return None;
    }
    Some(format!(
        "{}: this build cannot attach an fentry program. `make` grants CAP_BPF and CAP_PERFMON \
         to target/debug/omn, and every `cargo build` drops them again",
        machine_reason(&output)
    ))
}

/// Writes a session of several requests on one launched program, and runs it.
///
/// The CLI takes one source; a session takes any number on one target, which is
/// what lets two layers be measured over the same calls.
fn run_on_launch(name: &str, requests: &[(&str, &str, &str, &str)], argv: &[&str]) -> Output {
    run_on_launch_pinned(name, requests, None, argv)
}

/// The same, every request pinned to `backend` when one is given.
fn run_on_launch_pinned(
    name: &str,
    requests: &[(&str, &str, &str, &str)],
    backend: Option<&str>,
    argv: &[&str],
) -> Output {
    let pinned = backend
        .map(|backend| format!("    backend: force:{backend}\n"))
        .unwrap_or_default();
    let launch = argv
        .iter()
        .map(|arg| {
            format!(
                "        - {}",
                serde_json::to_string(arg).expect("a string")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut text = format!("name: {name}\nformat: json\nrequests:\n");
    for (label, source, mode, emits) in requests {
        text.push_str(&format!(
            "  - label: {label}\n    source: {source}\n    target:\n      launch:\n{launch}\n    \
             mode: {mode}\n    emit: [{emits}]\n{pinned}"
        ));
    }
    let path = scratch(&format!("{name}.yml"));
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the scratch directory");
    std::fs::write(&path, text).expect("the session");
    run(&["-f", path.to_str().expect("a path"), "--format", "json"])
}

/// A workload script, written where the scratch files are.
fn script(name: &str, body: &str) -> PathBuf {
    let path = scratch(name);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the scratch directory");
    std::fs::write(&path, body).expect("the workload");
    path
}

/// What the workload says it did.
fn account(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("the workload wrote no account to {path:?}: {error}"));
    serde_json::from_str(&text).expect("the account parses")
}

fn attr_u64(event: &BTreeMap<String, serde_json::Value>, name: &str) -> u64 {
    event
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("an event carries `{name}`: {event:?}"))
}

fn attr_text<'a>(event: &'a BTreeMap<String, serde_json::Value>, name: &str) -> &'a str {
    event
        .get(name)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("an event carries `{name}`: {event:?}"))
}

use std::collections::BTreeMap;

/// The superblock device a path's filesystem is mounted from, as
/// `/proc/self/mountinfo` states it: the longest mount point that contains the
/// path. This is the reference for `io.fs`'s file key, and it is deliberately
/// not `stat(2)`: on btrfs `stat` reports a per-subvolume device the VFS never
/// sees, which is `docs/families/io.md`'s finding.
fn mounted_device(path: &Path) -> (u64, String) {
    let canonical = std::fs::canonicalize(path).expect("a path that exists");
    let text = std::fs::read_to_string("/proc/self/mountinfo").expect("mountinfo");
    let mut best: Option<(usize, u64, String)> = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        let mount = Path::new(fields[4]);
        if !canonical.starts_with(mount) {
            continue;
        }
        let depth = mount.components().count();
        if best.as_ref().is_some_and(|(seen, _, _)| *seen >= depth) {
            continue;
        }
        let (major, minor) = fields[2].split_once(':').expect("major:minor");
        let dev = libc::makedev(
            major.parse().expect("a major"),
            minor.parse().expect("a minor"),
        );
        best = Some((depth, dev, fields[separator + 1].to_owned()));
    }
    let (_, dev, fs) = best.expect("a mount holds every path");
    (dev, fs)
}

/// The `file` key of an event, as `(dev, ino)`.
fn file_key(event: &serde_json::Value) -> Option<(u64, u64)> {
    event["keys"].as_array()?.iter().find_map(|key| {
        let file = key.get("file")?;
        Some((file["dev"].as_u64()?, file["ino"].as_u64()?))
    })
}

/// A result's raw events, each with its keys as well as its attributes.
fn raw_with_keys(
    result: &serde_json::Value,
) -> Vec<(serde_json::Value, BTreeMap<String, serde_json::Value>)> {
    output_of(result, "raw")["events"]
        .as_array()
        .expect("a raw emit carries its events")
        .iter()
        .map(|event| {
            let attrs = event["attrs"]
                .as_array()
                .expect("attributes")
                .iter()
                .map(|attr| {
                    (
                        attr["name"].as_str().expect("a name").to_owned(),
                        attr["value"].clone(),
                    )
                })
                .collect();
            (event.clone(), attrs)
        })
        .collect()
}

const KNOWN_CALLS: &str = r#"
import json, os, sys
source, target, account = sys.argv[1], sys.argv[2], sys.argv[3]
reads, read_bytes = 500, 4096
writes, write_bytes = 300, 1000
# Descriptors high enough that nothing the interpreter opened before this
# line can have had the same number: a descriptor key names a number, and
# numbers are reused the moment they are closed.
r = os.dup2(os.open(source, os.O_RDONLY), 200)
for _ in range(reads):
    assert len(os.read(r, read_bytes)) == read_bytes
w = os.dup2(os.open(target, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644), 201)
block = b"w" * write_bytes
for i in range(writes):
    assert os.pwrite(w, block, i * write_bytes) == write_bytes
json.dump({
    "read_fd": r, "read_ino": os.fstat(r).st_ino, "reads": reads, "read_bytes": read_bytes,
    "write_fd": w, "write_ino": os.fstat(w).st_ino, "writes": writes,
    "write_bytes": write_bytes,
}, open(account, "w"))
"#;

/// Five hundred reads of 4096 bytes and three hundred `pwrite`s of 1000, on two
/// descriptors the program names, found call for call in both layers.
///
/// The reference is the program's loop bounds and the descriptors and inodes
/// `fstat` gave it -- nothing the tool computed. Two layers are asked over the
/// same calls, and each has its own identity for them: `io.syscall` keys the
/// row by descriptor, `io.fs` by `file:<superblock dev>+<ino>`. Both must reach
/// the same counts, exactly, and the same bytes, exactly.
#[test]
fn a_known_number_of_reads_and_writes_is_counted_call_for_call_in_both_layers() {
    if let Some(reason) = no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let source = scratch("fentry-known-calls.src");
    std::fs::write(&source, incompressible(4 << 20)).expect("a file to read");
    let target = scratch("fentry-known-calls.dst");
    let account_path = scratch("fentry-known-calls.json");
    let workload = script("fentry-known-calls.py", KNOWN_CALLS);
    let output = run_on_launch(
        "fentry-known-calls",
        &[
            ("syscall", "io.syscall", "trace", "raw, hist"),
            ("fs", "io.fs", "trace", "raw, hist"),
        ],
        &[
            "python3",
            workload.to_str().expect("a path"),
            source.to_str().expect("a path"),
            target.to_str().expect("a path"),
            account_path.to_str().expect("a path"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let did = account(&account_path);
    let document = json(&output);

    // The descriptor layer.
    let mut by_fd: BTreeMap<(u64, String), (u64, u64)> = BTreeMap::new();
    for (_, event) in raw_with_keys(result(&document, "syscall")) {
        let entry = by_fd
            .entry((
                attr_u64(&event, "fd"),
                attr_text(&event, "syscall").to_owned(),
            ))
            .or_default();
        entry.0 += attr_u64(&event, "count");
        entry.1 += attr_u64(&event, "bytes");
    }
    let reads = did["reads"].as_u64().expect("reads");
    let writes = did["writes"].as_u64().expect("writes");
    let read_fd = did["read_fd"].as_u64().expect("a descriptor");
    let write_fd = did["write_fd"].as_u64().expect("a descriptor");
    assert_eq!(
        by_fd.get(&(read_fd, "read".to_owned())),
        Some(&(reads, reads * did["read_bytes"].as_u64().expect("bytes"))),
        "io.syscall on descriptor {read_fd}: {by_fd:?}"
    );
    assert_eq!(
        by_fd.get(&(write_fd, "pwrite64".to_owned())),
        Some(&(writes, writes * did["write_bytes"].as_u64().expect("bytes"))),
        "io.syscall on descriptor {write_fd}: {by_fd:?}"
    );

    // The file layer, keyed by what mountinfo and fstat say independently.
    let (dev, fs) = mounted_device(&source);
    let mut by_file: BTreeMap<((u64, u64), String), (u64, u64)> = BTreeMap::new();
    let mut named_fs = Vec::new();
    for (raw, event) in raw_with_keys(result(&document, "fs")) {
        let key = file_key(&raw).expect("every io.fs event names its file");
        if key.1 == did["read_ino"].as_u64().expect("an inode") {
            named_fs.push(attr_text(&event, "fs").to_owned());
        }
        let entry = by_file
            .entry((key, attr_text(&event, "op").to_owned()))
            .or_default();
        entry.0 += attr_u64(&event, "count");
        entry.1 += attr_u64(&event, "bytes");
    }
    let read_key = (dev, did["read_ino"].as_u64().expect("an inode"));
    let write_key = (dev, did["write_ino"].as_u64().expect("an inode"));
    assert_eq!(
        by_file.get(&(read_key, "read".to_owned())),
        Some(&(reads, reads * 4096)),
        "io.fs on file {read_key:?}, which mountinfo and fstat name: {by_file:?}"
    );
    assert_eq!(
        by_file.get(&(write_key, "write".to_owned())),
        Some(&(writes, writes * 1000)),
        "io.fs on file {write_key:?}: {by_file:?}"
    );
    assert!(
        named_fs.iter().all(|name| *name == fs),
        "the filesystem is named from mountinfo, which says `{fs}`: {named_fs:?}"
    );
}

const PIPE_WAIT: &str = r#"
import json, os, sys, threading, time
account = sys.argv[1]
r, w = os.pipe()
r = os.dup2(r, 202)
def later():
    time.sleep(0.3)
    os.write(w, b"x")
threading.Thread(target=later).start()
started = time.monotonic_ns()
os.read(r, 1)
waited = time.monotonic_ns() - started
json.dump({"pipe_fd": r, "waited_ns": waited}, open(account, "w"))
"#;

/// A read that waits three hundred milliseconds on an empty pipe is three
/// hundred milliseconds in `io.syscall` and none at all in `io.fs`.
///
/// This is the separation the two layers exist for, created on purpose. The
/// program times its own `os.read` around the call, and the syscall layer must
/// land inside that interval: at most what the program saw, and within a
/// millisecond of it, because the Python around the call is microseconds. The
/// file layer must not have it at all, because a pipe is not a regular file --
/// which is a decision, stated in `src/bpf/io_vfs.rs`, rather than an accident.
#[test]
fn a_pipe_wait_is_in_the_syscall_layer_and_not_in_the_file_layer() {
    if let Some(reason) = no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let account_path = scratch("fentry-pipe-wait.json");
    let workload = script("fentry-pipe-wait.py", PIPE_WAIT);
    let output = run_on_launch(
        "fentry-pipe-wait",
        &[
            ("syscall", "io.syscall", "trace", "raw"),
            ("fs", "io.fs", "trace", "raw"),
        ],
        &[
            "python3",
            workload.to_str().expect("a path"),
            account_path.to_str().expect("a path"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let did = account(&account_path);
    let waited = did["waited_ns"].as_u64().expect("the program's own clock");
    let pipe_fd = did["pipe_fd"].as_u64().expect("the pipe");
    assert!(waited >= 300_000_000, "the program slept {waited} ns");

    let document = json(&output);
    let on_pipe: Vec<u64> = raw_with_keys(result(&document, "syscall"))
        .into_iter()
        .filter(|(_, event)| attr_u64(event, "fd") == pipe_fd && attr_text(event, "op") == "read")
        .map(|(_, event)| attr_u64(&event, "total_ns"))
        .collect();
    let dropped = losses(result(&document, "syscall"))["dropped"]
        .as_u64()
        .expect("a drop count");
    if on_pipe.is_empty() && dropped > 0 {
        // Another tracer's workload on the same machine can make the kernel
        // skip a run of these programs -- they run on every read anywhere, to
        // check whose it is -- and a skipped run is a lost call. It is counted,
        // which is the part that must hold; the property itself could not be
        // created in this run.
        eprintln!(
            "skipped: the pipe read is in no row and the run counted {dropped} lost call(s): \
             {}",
            losses(result(&document, "syscall"))["reasons"]
        );
        return;
    }
    assert_eq!(
        on_pipe.len(),
        1,
        "one read of the pipe, one row, and nothing counted as lost: {on_pipe:?}"
    );
    let timed = on_pipe[0];
    assert!(
        timed <= waited && waited - timed < 1_000_000,
        "the syscall layer timed {timed} ns inside a call the program timed at {waited} ns"
    );

    let fs_total: u64 = raw_with_keys(result(&document, "fs"))
        .iter()
        .map(|(_, event)| attr_u64(event, "total_ns"))
        .sum();
    assert!(
        fs_total < waited / 10,
        "the file layer holds {fs_total} ns for a program whose only long call was a {waited} ns \
         pipe wait: a pipe is not a filesystem"
    );
}

const KPROBE_CALLS: &str = r#"
import json, os, sys
path, account = sys.argv[1], sys.argv[2]
with open(path, "wb") as f:
    f.write(b"x" * 4096)
fd = os.open(path, os.O_RDWR)
reads, writes = 5000, 3000
for _ in range(reads):
    os.pread(fd, 1, 0)
for _ in range(writes):
    os.pwrite(fd, b"y", 0)
r, w = os.pipe()
os.write(w, b"z")
os.read(r, 1)
json.dump({"ino": os.fstat(fd).st_ino, "reads": reads, "writes": writes}, open(account, "w"))
"#;

/// `io.fs` through a kprobe pair (`bpf:kprobe`, for a kernel built without
/// the function tracer) counts what the trampoline pair counts: every read
/// and write of the program's file, the file named from probe reads of the
/// same fields, and nothing lost. Each pinned in a run of its own, so the two
/// do not attach to the same functions at once. Five thousand reads and three
/// thousand writes, and the one write that made the file.
#[test]
fn a_kprobe_pair_counts_the_calls_the_trampoline_pair_counts() {
    if let Some(reason) = no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let path = scratch("kprobe-fs.bin");
    let account_path = scratch("kprobe-fs.json");
    let workload = script("kprobe-fs.py", KPROBE_CALLS);
    for backend in ["bpf:fentry", "bpf:kprobe"] {
        let output = run_on_launch_pinned(
            "kprobe-fs",
            &[("fs", "io.fs", "trace", "raw")],
            Some(backend),
            &[
                "python3",
                workload.to_str().expect("a path"),
                path.to_str().expect("a path"),
                account_path.to_str().expect("a path"),
            ],
        );
        assert!(output.status.success(), "{backend}: {}", stderr(&output));
        let did = account(&account_path);
        let (dev, _) = mounted_device(&path);
        let file = (dev, did["ino"].as_u64().expect("an inode"));
        let document = json(&output);
        let fs = result(&document, "fs");
        let count = |op: &str| -> u64 {
            raw_with_keys(fs)
                .iter()
                .filter(|(raw, event)| file_key(raw) == Some(file) && attr_text(event, "op") == op)
                .map(|(_, event)| attr_u64(event, "count"))
                .sum()
        };
        assert_eq!(
            (count("read"), count("write")),
            (5000, 3001),
            "{backend}: every read and write of {file:?}"
        );
        assert_eq!(
            losses(fs)["dropped"].as_u64(),
            Some(0),
            "{backend}: {}",
            losses(fs)
        );
    }
}

const SYNCS: &str = r#"
import json, mmap, os, sys
path, account = sys.argv[1], sys.argv[2]
fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
dirty, clean, flushes = 20, 5, 3
for _ in range(dirty):
    os.write(fd, os.urandom(65536))
    os.fsync(fd)
for _ in range(clean):
    os.fdatasync(fd)
mapping = mmap.mmap(fd, 65536)
for i in range(flushes):
    mapping[i] = 1
    mapping.flush()
mapping.close()
json.dump({"fd": fd, "ino": os.fstat(fd).st_ino, "dirty": dirty, "clean": clean,
           "flushes": flushes}, open(account, "w"))
"#;

/// Twenty `fsync`s of freshly written data, five `fdatasync`s of nothing, and
/// three `msync`s through a mapping: twenty-eight events, each on the file the
/// program names, each named for the call it was, and each timed at the
/// function the kernel actually routed it through.
///
/// The first two reach `do_fsync` on this machine -- the function the catalog
/// named is inlined into it, `src/bpf/io_fsync.rs` has the measurement -- and
/// the third reaches `vfs_fsync_range` by its argument. So this is also the
/// check that both identities, the descriptor walk and the typed argument,
/// produce the same file key.
///
/// The separation is the second property: a sync with 64 KiB to write waits
/// for the device and one with nothing to write does not. The two medians are
/// asserted apart by a factor of ten, which is three orders of magnitude inside
/// what this machine measures.
#[test]
fn every_sync_is_one_event_on_the_file_it_named() {
    if let Some(reason) = no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let path = scratch("fentry-syncs.bin");
    let account_path = scratch("fentry-syncs.json");
    let workload = script("fentry-syncs.py", SYNCS);
    let output = run_on_launch(
        "fentry-syncs",
        &[("fsync", "io.fsync", "trace", "raw")],
        &[
            "python3",
            workload.to_str().expect("a path"),
            path.to_str().expect("a path"),
            account_path.to_str().expect("a path"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let did = account(&account_path);
    let (dev, _) = mounted_device(&path);
    let file = (dev, did["ino"].as_u64().expect("an inode"));
    let document = json(&output);
    let events = raw_with_keys(result(&document, "fsync"));
    let mine: Vec<_> = events
        .iter()
        .filter(|(raw, _)| file_key(raw) == Some(file))
        .collect();
    let by = |call: &str, via: &str| -> Vec<u64> {
        let mut found: Vec<u64> = mine
            .iter()
            .filter(|(_, event)| attr_text(event, "call") == call && attr_text(event, "via") == via)
            .map(|(_, event)| attr_u64(event, "lat_ns"))
            .collect();
        found.sort_unstable();
        found
    };
    let whole: Vec<u64> = by("fsync", "do_fsync")
        .into_iter()
        .chain(by("fsync", "vfs_fsync"))
        .collect();
    let clean: Vec<u64> = by("fdatasync", "do_fsync")
        .into_iter()
        .chain(by("fdatasync", "vfs_fsync"))
        .collect();
    let ranged = by("fdatasync", "vfs_fsync_range");
    assert_eq!(
        (whole.len(), clean.len(), ranged.len()),
        (20, 5, 3),
        "twenty fsyncs, five fdatasyncs and three msyncs on {file:?}, and nothing else: {mine:?}"
    );
    assert_eq!(
        mine.len(),
        events.len(),
        "the program synced one file, and every event must name it: {events:?}"
    );
    let fd = did["fd"].as_i64().expect("a descriptor");
    assert!(
        mine.iter().all(|(_, event)| {
            attr_text(event, "via") == "vfs_fsync_range"
                || event.get("fd").and_then(serde_json::Value::as_i64) == Some(fd)
        }),
        "a sync that arrived by descriptor carries descriptor {fd}"
    );
    let median = |values: &[u64]| values[values.len() / 2];
    assert!(
        median(&whole) > 10 * median(&clean),
        "a sync with 64 KiB to write ({} ns) and one with nothing to write ({} ns) must be \
         apart by more than an order of magnitude",
        median(&whole),
        median(&clean)
    );
}

/// The same twenty-eight syncs through `bpf:fentry:syscall`, the backend for
/// a kernel that inlined `do_fsync` and `vfs_fsync` into the syscalls: timed
/// at the two syscalls' wrappers, the file found through the descriptor in the
/// saved registers, and the msyncs at `vfs_fsync_range` as before. A sync the
/// wrapper opened and `vfs_fsync_range` reached inside it is one event.
#[test]
fn every_sync_at_the_syscalls_is_one_event_on_the_file_it_named() {
    if let Some(reason) = no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let path = scratch("fentry-syscall-syncs.bin");
    let account_path = scratch("fentry-syscall-syncs.json");
    let workload = script("fentry-syscall-syncs.py", SYNCS);
    let output = run_on_launch_pinned(
        "fentry-syscall-syncs",
        &[("fsync", "io.fsync", "trace", "raw")],
        Some("bpf:fentry:syscall"),
        &[
            "python3",
            workload.to_str().expect("a path"),
            path.to_str().expect("a path"),
            account_path.to_str().expect("a path"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let did = account(&account_path);
    let (dev, _) = mounted_device(&path);
    let file = (dev, did["ino"].as_u64().expect("an inode"));
    let document = json(&output);
    let events = raw_with_keys(result(&document, "fsync"));
    let mine: Vec<_> = events
        .iter()
        .filter(|(raw, _)| file_key(raw) == Some(file))
        .collect();
    let count = |call: &str, via: &str| {
        mine.iter()
            .filter(|(_, event)| attr_text(event, "call") == call && attr_text(event, "via") == via)
            .count()
    };
    assert_eq!(
        (
            count("fsync", "fsync"),
            count("fdatasync", "fdatasync"),
            count("fdatasync", "vfs_fsync_range")
        ),
        (20, 5, 3),
        "twenty fsyncs, five fdatasyncs and three msyncs on {file:?}, and nothing else: {mine:?}"
    );
    assert_eq!(
        mine.len(),
        events.len(),
        "the program synced one file, and every event must name it: {events:?}"
    );
    let fd = did["fd"].as_i64().expect("a descriptor");
    assert!(
        mine.iter().all(|(_, event)| {
            attr_text(event, "via") == "vfs_fsync_range"
                || event.get("fd").and_then(serde_json::Value::as_i64) == Some(fd)
        }),
        "a sync timed at its syscall carries descriptor {fd}"
    );
}

const MANY_ROWS: &str = r#"
import json, os, resource, sys, threading
directory, account = sys.argv[1], sys.argv[2]
files, threads = 1000, 100
soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
resource.setrlimit(resource.RLIMIT_NOFILE, (min(hard, 4096), hard))
os.makedirs(directory, exist_ok=True)
fds = []
for i in range(files):
    path = os.path.join(directory, f"{i}")
    if not os.path.exists(path):
        with open(path, "wb") as f:
            f.write(b"x")
    fds.append(os.open(path, os.O_RDONLY))
start = threading.Barrier(threads)
def reader():
    start.wait()
    for fd in fds:
        os.pread(fd, 1, 0)
workers = [threading.Thread(target=reader) for _ in range(threads)]
for worker in workers:
    worker.start()
for worker in workers:
    worker.join()
json.dump({"fds": fds, "inos": [os.fstat(fd).st_ino for fd in fds],
           "reads": files * threads}, open(account, "w"))
"#;

/// A hundred threads each reading a thousand files once is a hundred thousand
/// distinct rows, three times what the histogram holds in one drain, and every
/// call that found no row is counted as lost.
///
/// The property that closes against a number: every read the program made is
/// either in a row or in the drop count, so the two together are at least the
/// program's own total, and the rows alone are at most it. A run that dropped
/// calls without counting them fails the first; a run that counted a call twice
/// fails the second. Both layers are asked over the same workload, because both
/// key a row on something the workload has a thousand of.
///
/// Where the machine cannot make the workload fast enough to fill one drain's
/// worth of rows, nothing is dropped and the property cannot be created: that
/// is a skip, with the numbers, and never a pass.
#[test]
fn more_rows_than_the_histogram_holds_are_counted_as_lost_and_the_rest_are_exact() {
    if let Some(reason) = no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let directory = scratch("fentry-many-rows");
    let account_path = scratch("fentry-many-rows.json");
    let workload = script("fentry-many-rows.py", MANY_ROWS);
    let output = run_on_launch(
        "fentry-many-rows",
        &[
            ("syscall", "io.syscall", "trace", "raw"),
            ("fs", "io.fs", "trace", "raw"),
        ],
        &[
            "python3",
            workload.to_str().expect("a path"),
            directory.to_str().expect("a path"),
            account_path.to_str().expect("a path"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let did = account(&account_path);
    let reads = did["reads"].as_u64().expect("reads");
    let fds: std::collections::BTreeSet<u64> = did["fds"]
        .as_array()
        .expect("fds")
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .collect();
    let inos: std::collections::BTreeSet<u64> = did["inos"]
        .as_array()
        .expect("inos")
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .collect();
    let document = json(&output);
    type Mine<'a> = &'a dyn Fn(&BTreeMap<String, serde_json::Value>) -> bool;
    let by_fd = |event: &BTreeMap<String, serde_json::Value>| {
        attr_text(event, "syscall") == "pread64" && fds.contains(&attr_u64(event, "fd"))
    };
    let by_ino = |event: &BTreeMap<String, serde_json::Value>| {
        attr_text(event, "op") == "read" && inos.contains(&attr_u64(event, "ino"))
    };
    let layers: [(&str, Mine); 2] = [("syscall", &by_fd), ("fs", &by_ino)];
    for (label, mine) in layers {
        let run = result(&document, label);
        let counted: u64 = raw_with_keys(run)
            .iter()
            .filter(|(_, event)| mine(event))
            .map(|(_, event)| attr_u64(event, "count"))
            .sum();
        let dropped = losses(run)["dropped"].as_u64().expect("a drop count");
        if dropped == 0 {
            eprintln!(
                "skipped ({label}): {counted} of {reads} reads were counted and none dropped, so \
                 this machine did not fill a drain's worth of rows -- the histogram holds 32768 \
                 per 100 ms, and the loss path was not created"
            );
            assert_eq!(
                counted, reads,
                "with nothing dropped, every read is in a row"
            );
            continue;
        }
        assert!(
            counted + dropped >= reads,
            "{label}: {counted} counted and {dropped} dropped, for {reads} reads the program \
             made: calls were lost without being counted"
        );
        assert!(
            counted <= reads,
            "{label}: {counted} counted for {reads} reads: a call was counted twice"
        );
        let reasons = losses(run)["reasons"].to_string();
        assert!(
            reasons.contains("lower bound"),
            "{label}: a lossy histogram must say its counts are a floor: {reasons}"
        );
    }
}

/// Each of the three refuses what it cannot serve, with the reason and the
/// exit code that belongs to it.
///
/// - a cgroup target is the build's gap (3), and names the helper it needs;
/// - a `poll` is a request that cannot mean anything (2): a latency is the gap
///   between two halves of one call;
/// - `io.fsync` in `count` is the same (2), because the catalog declares it in
///   `trace` alone -- one event per sync is the point of the source;
/// - and a binary without the capabilities is this machine's gap (1), naming
///   both capabilities and how to grant them. That binary is a copy of this
///   one, which is exactly what a relink produces: file capabilities do not
///   survive it.
#[test]
fn the_fentry_io_sources_refuse_with_the_reason_that_is_actually_missing() {
    for source in ["io.syscall", "io.fs", "io.fsync"] {
        let cgroup = run(&[
            source,
            "--cgroup",
            "/sys/fs/cgroup",
            "-m",
            "trace",
            "-e",
            "summary",
            "-d",
            "100ms",
        ]);
        let complaint = stderr(&cgroup);
        if cgroup.status.code() == Some(EXIT_NO_BACKEND) {
            eprintln!("skipped the cgroup refusal of {source}: {complaint}");
        } else {
            assert_eq!(
                cgroup.status.code(),
                Some(EXIT_NOT_IMPLEMENTED),
                "{source}: {complaint}"
            );
            assert!(
                complaint.contains("bpf_get_current_cgroup_id"),
                "{source}: the refusal should name what would serve it: {complaint}"
            );
        }

        let polled = run(&[
            source,
            "--system",
            "-m",
            "poll@100ms",
            "-e",
            "summary",
            "-d",
            "200ms",
        ]);
        assert_eq!(
            polled.status.code(),
            Some(EXIT_REJECTED),
            "{source}: {}",
            stderr(&polled)
        );
        assert!(
            stderr(&polled).contains("poll"),
            "{source}: {}",
            stderr(&polled)
        );
    }
    let counted = run(&[
        "io.fsync", "--system", "-m", "count", "-e", "summary", "-d", "200ms",
    ]);
    assert_eq!(
        counted.status.code(),
        Some(EXIT_REJECTED),
        "{}",
        stderr(&counted)
    );

    let bare = scratch("omn-without-capabilities");
    std::fs::create_dir_all(bare.parent().expect("a parent")).expect("the scratch directory");
    std::fs::copy(BINARY, &bare).expect("a copy of the binary");
    for source in ["io.syscall", "io.fs", "io.fsync"] {
        let output = Command::new(&bare)
            .args([
                source, "--system", "-m", "trace", "-e", "summary", "-d", "100ms",
            ])
            .output()
            .expect("the copy runs");
        let complaint = stderr(&output);
        if output.status.success() {
            // Root, or a kernel that lets anyone load a tracing program: the
            // copy is as privileged as the original and there is no refusal to
            // see.
            eprintln!("skipped the capability refusal of {source}: the copy collected");
            continue;
        }
        assert_eq!(
            output.status.code(),
            Some(EXIT_NO_BACKEND),
            "{source}: {complaint}"
        );
        assert!(
            complaint.contains("CAP_BPF") && complaint.contains("setcap"),
            "{source}: a binary without the capabilities must be told which and how: {complaint}"
        );
    }
}

/// A second tracer attaching to the same function in the middle of a call
/// removes that call from this one's histogram, and the run must say so.
///
/// This is not a defect of this collector and it cannot be fixed here: when any
/// fentry or fexit program is attached to or detached from a function, the
/// kernel rebuilds that function's trampoline, and it patches the old one first
/// so that calls still in flight return around their exit programs. So the
/// three-hundred-millisecond pipe read below loses its exit half the moment a
/// second `omn` attaches for thirty milliseconds -- deterministically, and on
/// any machine where two tracers share a function.
///
/// What this scenario holds the collector to is the invariant: the stamp the
/// read left is found, the loss is counted, and the reason names the
/// mechanism. The control run beside it, with no second tracer, must find the
/// read and count nothing.
#[test]
fn a_call_whose_exit_the_kernel_skipped_is_counted_as_lost() {
    if let Some(reason) = no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let workload = script("fentry-skipped-exit.py", PIPE_WAIT);
    for disturbed in [false, true] {
        let account_path = scratch(&format!("fentry-skipped-exit-{disturbed}.json"));
        let second = disturbed.then(|| {
            std::thread::spawn(|| {
                // Inside the read's three hundred milliseconds: the program
                // starts, its thread starts sleeping, and then this attaches.
                std::thread::sleep(Duration::from_millis(150));
                run(&[
                    "io.syscall",
                    "--system",
                    "-m",
                    "trace",
                    "-e",
                    "summary",
                    "-d",
                    "30ms",
                ])
            })
        });
        let output = run(&[
            "io.syscall",
            "-m",
            "trace",
            "-e",
            "raw",
            "--format",
            "json",
            "--",
            "python3",
            workload.to_str().expect("a path"),
            account_path.to_str().expect("a path"),
        ]);
        if let Some(second) = second {
            let second = second.join().expect("the second tracer ran");
            assert!(second.status.success(), "{}", stderr(&second));
        }
        assert!(output.status.success(), "{}", stderr(&output));
        let did = account(&account_path);
        let pipe_fd = did["pipe_fd"].as_u64().expect("the pipe");
        let document = json(&output);
        let run = &document["results"][0];
        let found = raw_with_keys(run)
            .iter()
            .any(|(_, event)| attr_u64(event, "fd") == pipe_fd && attr_text(event, "op") == "read");
        let dropped = losses(run)["dropped"].as_u64().expect("a drop count");
        let reasons = losses(run)["reasons"].to_string();
        // The invariant, for both runs: the read is in a row, or it is counted
        // as lost and the loss names why. The control cannot be promised
        // undisturbed -- the rest of this suite attaches to the same function
        // at the same moment -- so it is held to the same line.
        assert!(
            found || (dropped >= 1 && reasons.contains("exit half of a call never ran")),
            "the read is in no row, and the run did not count it as a skipped exit: \
             dropped {dropped}, {reasons}"
        );
        match (disturbed, found) {
            (true, true) => eprintln!(
                "skipped: the second tracer missed the read's window, so the lost exit was not \
                 created in this run"
            ),
            (false, false) => eprintln!(
                "note: the control was disturbed by another tracer in the suite, and counted it"
            ),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The page cache: `io.pagecache` and `io.readahead`
//
// One program pair on the two tracepoints either side of a read --
// `mm_filemap_get_pages`, what a read looked up, and
// `mm_filemap_add_to_page_cache`, what the cache had to fill -- read two ways.
// `src/bpf/io_pagecache.rs` has why those two.
//
// Ground truth here is the page cache's own state, reported by the workload
// through `mincore(2)`: which pages of the file are resident before a read and
// after it. That is the kernel's answer through a different door -- a syscall
// on a mapping, not a tracepoint -- and nothing the tool computed. A cold read
// must fill exactly the pages that were not resident, a warm one exactly none,
// and once readahead has settled, what became resident is exactly what was
// filled.
//
// The four classes:
//
// - **ground truth**: a file read cold and then warm; the first 4 MiB of a
//   cold 64 MiB file read sequentially against 256 random pages of another;
//   pages a write appends, which are created rather than missed;
// - **the capability sweep**: both are in `collected_sources` above;
// - **the loss path**: more distinct `(task, file)` rows in one drain than the
//   map holds;
// - **the refusal**: a mode neither can mean, a cgroup target, and a binary
//   without the capabilities.
//
// Every file is on the build directory's filesystem. A tmpfs has no page cache
// lookups of this kind -- shmem reads do not pass `filemap_get_pages` -- and a
// scenario about the page cache run on one measures nothing.

/// `mincore(2)` from Python, for the workloads below: how many pages of a file
/// the page cache holds right now. Mapping a file and asking touches no page,
/// so the question does not change the answer.
const RESIDENT: &str = r#"
import ctypes, mmap, os
_libc = ctypes.CDLL(None, use_errno=True)
_libc.mmap.restype = ctypes.c_void_p
_libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int,
                       ctypes.c_int, ctypes.c_long]
_libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_char_p]
_libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
def resident(fd):
    size = os.fstat(fd).st_size
    pages = (size + 4095) // 4096
    addr = _libc.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
    assert addr is not None and addr != ctypes.c_void_p(-1).value, ctypes.get_errno()
    vec = ctypes.create_string_buffer(pages)
    assert _libc.mincore(addr, size, vec) == 0, ctypes.get_errno()
    _libc.munmap(addr, size)
    return sum(byte & 1 for byte in vec.raw)
"#;

/// A file read whole in 1 MiB reads, cold or warm, with the page cache's own
/// account of it before the read.
const READ_WHOLE: &str = r#"
import json, os, sys
path, phase, account = sys.argv[1], sys.argv[2], sys.argv[3]
fd = os.open(path, os.O_RDONLY)
if phase == "cold":
    os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
before = resident(fd)
read = 0
while True:
    chunk = os.read(fd, 1 << 20)
    if not chunk:
        break
    read += len(chunk)
st = os.fstat(fd)
json.dump({"ino": st.st_ino, "pages": (st.st_size + 4095) // 4096, "read": read,
           "resident_before": before, "resident_after": resident(fd)}, open(account, "w"))
"#;

/// A file written and synced, so that `POSIX_FADV_DONTNEED` can drop every
/// page of it: a dirty page is not dropped, and a read of it is not cold.
fn synced_file(name: &str, bytes: usize) -> PathBuf {
    use std::io::Write;
    let path = scratch(name);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the scratch directory");
    let mut file = std::fs::File::create(&path).expect("a file to write");
    file.write_all(&incompressible(bytes))
        .expect("the whole file");
    file.sync_all().expect("the file on the device");
    path
}

fn attr_i64(event: &BTreeMap<String, serde_json::Value>, name: &str) -> i64 {
    event
        .get(name)
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_else(|| panic!("an event carries `{name}`: {event:?}"))
}

/// The lookups of every file a result names, as `(pages, calls)` per
/// `(dev, ino)`: what a failing count prints, so that a lookup credited to
/// the wrong file and one never counted can be told apart.
fn lookups_by_file(result: &serde_json::Value) -> BTreeMap<(u64, u64), (u64, u64)> {
    let mut by_file = BTreeMap::new();
    for (raw, event) in raw_with_keys(result) {
        let Some(file) = file_key(&raw) else { continue };
        let entry = by_file.entry(file).or_insert((0, 0));
        entry.0 += attr_u64(&event, "lookups");
        entry.1 += attr_u64(&event, "lookup_calls");
    }
    by_file
}

/// The page counts of every event on one file, summed:
/// `(lookups, lookup calls, filled, created, value)`, the value being the
/// source's own -- `misses` or `ahead`.
fn page_counts(
    result: &serde_json::Value,
    file: (u64, u64),
    value: &str,
) -> (u64, u64, u64, u64, i64) {
    let mut sums = (0, 0, 0, 0, 0);
    for (raw, event) in raw_with_keys(result) {
        if file_key(&raw) != Some(file) {
            continue;
        }
        sums.0 += attr_u64(&event, "lookups");
        sums.1 += attr_u64(&event, "lookup_calls");
        sums.2 += attr_u64(&event, "filled");
        sums.3 += attr_u64(&event, "created");
        sums.4 += attr_i64(&event, value);
    }
    sums
}

/// How far short a cold read's lookups may be: the first folio batch of each
/// of its four reads, at most. `PAGEVEC_SIZE` is 31 folios, and the shortfalls
/// measured here were 1, 48, 88 and 96 pages on reads of 256.
const FIRST_BATCHES: u64 = 4 * 256 / 2;

/// A 4 MiB file read cold and then warm, in 1 MiB reads.
///
/// The reference is `mincore(2)` before each read: a read of the whole file
/// must fill exactly the pages that were not resident -- all 1024 cold, none
/// warm -- and look up every page of the file once. Warm, exactly; cold, a
/// read's first batch may go unseen, because `filemap_get_pages` returns
/// without its tracepoint when the batch's last folio is still locked under
/// readahead, so the cold count is a floor and never over. The reads are
/// 1 MiB on purpose: `filemap_read` asks for a read's remaining range once per
/// batch of folios, so a 1 MiB read reports its tail five or six times, and a
/// collector that summed those ranges counted 3168 pages for this file. The
/// EOF read at the end asks for nothing the file has and must add nothing.
#[test]
fn a_file_read_cold_fills_every_page_and_read_warm_fills_none() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let path = synced_file("pagecache-cold-warm.bin", 4 << 20);
    let workload = script("pagecache-cold-warm.py", &format!("{RESIDENT}{READ_WHOLE}"));
    let (dev, _) = mounted_device(&path);
    let mut filled_by_phase = Vec::new();
    for phase in ["cold", "warm"] {
        // Warmed by a read of its own rather than left so by the cold one: a
        // kernel may drop at close the pages a descriptor read after
        // `POSIX_FADV_DONTNEED` on it, as 7.2 on the aarch64 machine does.
        if phase == "warm" {
            std::fs::read(&path).expect("the file reads");
        }
        let account_path = scratch(&format!("pagecache-{phase}.json"));
        let output = run_on_launch(
            &format!("pagecache-{phase}"),
            &[("cache", "io.pagecache", "count", "raw")],
            &[
                "python3",
                workload.to_str().expect("a path"),
                path.to_str().expect("a path"),
                phase,
                account_path.to_str().expect("a path"),
            ],
        );
        assert!(output.status.success(), "{phase}: {}", stderr(&output));
        let did = account(&account_path);
        let pages = did["pages"].as_u64().expect("pages");
        let before = did["resident_before"].as_u64().expect("resident pages");
        assert_eq!(
            did["read"].as_u64(),
            Some(4 << 20),
            "the workload read the file"
        );
        assert_eq!(pages, 1024);
        let file = (dev, did["ino"].as_u64().expect("an inode"));
        let document = json(&output);
        let (lookups, calls, filled, created, misses) =
            page_counts(result(&document, "cache"), file, "misses");
        let lost = losses(result(&document, "cache"));
        if phase == "warm" {
            assert_eq!(
                lookups, pages,
                "warm: a whole read of the file looks up each of its {pages} pages once, and \
                 {lookups} were counted in {calls} lookups; the run lost {lost}"
            );
            assert_eq!(
                calls, 4,
                "warm: four 1 MiB reads, and the one at EOF asks for nothing"
            );
        } else {
            // Cold, a read's first batch can go unseen: `filemap_get_pages`
            // returns early, without its tracepoint, when the batch's last
            // folio is still locked under readahead. What must never happen is
            // a page counted twice.
            assert!(
                lookups <= pages && lookups + FIRST_BATCHES >= pages,
                "cold: {lookups} of the file's {pages} pages were counted as looked up, in \
                 {calls} lookups; a cold count may be short by the first batch of each read, \
                 and is never over; the run lost {lost}"
            );
            if lookups < pages {
                eprintln!(
                    "note: the cold read's lookups were {} pages short, the first batch of a read \
                     that met a folio still under readahead",
                    pages - lookups
                );
            }
        }
        assert_eq!(
            filled,
            pages - before,
            "{phase}: mincore said {before} of {pages} pages were resident before the read, so \
             the read had to fill {}; the tool counted {filled}",
            pages - before
        );
        assert_eq!(misses as u64, filled, "{phase}: the value is the fill");
        assert_eq!(created, 0, "{phase}: a read creates no page");
        filled_by_phase.push((phase, before, filled));
    }
    let (_, cold_before, cold) = filled_by_phase[0];
    let (_, warm_before, warm) = filled_by_phase[1];
    if cold_before > 0 || warm_before < 1024 {
        eprintln!(
            "note: the cold read began with {cold_before} pages resident and the warm one with \
             {warm_before}; the counts matched mincore either way, but the separation below is \
             between a read that was not fully cold and one that was not fully warm"
        );
    }
    assert!(
        cold > warm,
        "a cold read must fill more than a warm one: {cold} against {warm}"
    );
}

/// The parent forks and waits; the child goes on to `READ_WHOLE`.
const FORK_THEN: &str = r#"
import os, sys
if os.fork() != 0:
    _, status = os.wait()
    sys.exit(os.waitstatus_to_exitcode(status))
"#;

/// A launched program's forked child is followed by `io.pagecache` and
/// `io.readahead`: a parent that reads nothing and a child that reads a 4 MiB
/// file cold is that file's pages filled, all of them.
///
/// Before the programs followed the target's descendants they compared the
/// reader with the launched process alone, so this run counted no page of the
/// file. The reference is `mincore(2)` in the child, as for the read above.
#[test]
fn the_page_cache_fills_of_a_forked_child_are_counted() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let path = synced_file("pagecache-forked.bin", 4 << 20);
    let workload = script(
        "pagecache-forked.py",
        &format!("{RESIDENT}{FORK_THEN}{READ_WHOLE}"),
    );
    let (dev, _) = mounted_device(&path);
    for source in ["io.pagecache", "io.readahead"] {
        let account_path = scratch("pagecache-forked.json");
        let _ = std::fs::remove_file(&account_path);
        let output = run_on_launch(
            "pagecache-forked",
            &[("cache", source, "count", "raw")],
            &[
                "python3",
                workload.to_str().expect("a path"),
                path.to_str().expect("a path"),
                "cold",
                account_path.to_str().expect("a path"),
            ],
        );
        assert!(output.status.success(), "{source}: {}", stderr(&output));
        let did = account(&account_path);
        let pages = did["pages"].as_u64().expect("pages");
        let before = did["resident_before"].as_u64().expect("resident pages");
        let file = (dev, did["ino"].as_u64().expect("an inode"));
        let document = json(&output);
        let value = if source == "io.pagecache" {
            "misses"
        } else {
            "ahead"
        };
        let (_, _, filled, _, _) = page_counts(result(&document, "cache"), file, value);
        let lost = losses(result(&document, "cache"));
        assert_eq!(
            filled,
            pages - before,
            "{source}: the forked child read the file with {before} of its {pages} pages \
             resident, so it filled {}; the run counted {filled} and lost {lost}: the \
             launched program's child is not followed",
            pages - before
        );
    }
}

/// The first 4 MiB of a cold 64 MiB file, read sequentially, and 256 random
/// pages of another, both in one run.
///
/// Readahead reads ahead of a sequential reader and not of a random one, so the
/// first must fill more than it looked up and the second what it looked up. The
/// reference is again `mincore(2)`, half a second after the reads so that the
/// last readahead has landed: what became resident is what was filled, and
/// what was looked up is what the workload read. The separation is asserted
/// rather than a size: how far the kernel reads ahead depends on its window,
/// which grows, and on the device.
///
/// Trace mode on purpose, and the workload's own `CLOCK_MONOTONIC` around each
/// phase: every row is stamped where its first event happened (finding 44), so
/// each file's events must fall inside the phase that read it -- one clock
/// domain, checked against the program's own reading of it.
///
/// The counts are held exact only in a run that lost nothing, and the run is
/// repeated until one does: the run's loss is the kernel's skipped runs over
/// every task of the machine, which no count of the workload's can be tied to.
#[test]
fn a_sequential_read_is_read_ahead_of_and_a_random_one_is_not() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    const SEQUENTIAL_THEN_RANDOM: &str = r#"
import json, os, random, sys, time
sequential, scattered, account = sys.argv[1], sys.argv[2], sys.argv[3]
now = lambda: time.clock_gettime_ns(time.CLOCK_MONOTONIC)
phases = {}
for path, how in ((sequential, "sequential"), (scattered, "random")):
    fd = os.open(path, os.O_RDONLY)
    os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    before = resident(fd)
    pages = os.fstat(fd).st_size // 4096
    begin = now()
    if how == "sequential":
        asked = 1024
        for i in range(asked // 16):
            assert len(os.pread(fd, 65536, i * 65536)) == 65536
    else:
        asked = 256
        for page in random.Random(7).sample(range(pages), asked):
            assert len(os.pread(fd, 4096, page * 4096)) == 4096
    end = now()
    time.sleep(0.5)
    phases[how] = {"ino": os.fstat(fd).st_ino, "asked": asked, "begin": begin, "end": end,
                   "resident_before": before, "resident_after": resident(fd)}
    os.close(fd)
json.dump(phases, open(account, "w"))
"#;
    let sequential = synced_file("readahead-sequential.bin", 64 << 20);
    let scattered = synced_file("readahead-random.bin", 64 << 20);
    let account_path = scratch("readahead.json");
    let workload = script(
        "readahead.py",
        &format!("{RESIDENT}{SEQUENTIAL_THEN_RANDOM}"),
    );
    // The run's `losses.dropped` is not the workload's: most of it is the
    // kernel's skipped runs of these programs, counted over every task of the
    // machine, and six streams share it -- 395 000 to 477 000 in a loaded run
    // against a shortfall of 3 to 15 lookups of 256. As slack it is wide
    // enough to hide anything, a silent loss in the drain included. So the
    // counts are held exact in a run that lost nothing, and the run is
    // repeated until one does; a lossy attempt is checked only for what a
    // loss cannot excuse -- a page counted twice -- and printed.
    const ATTEMPTS: usize = 4;
    let mut lossy = Vec::new();
    for attempt in 1..=ATTEMPTS {
        let output = run_on_launch(
            "readahead",
            &[("ahead", "io.readahead", "trace", "raw, rate")],
            &[
                "python3",
                workload.to_str().expect("a path"),
                sequential.to_str().expect("a path"),
                scattered.to_str().expect("a path"),
                account_path.to_str().expect("a path"),
            ],
        );
        assert!(output.status.success(), "{}", stderr(&output));
        let did = account(&account_path);
        let document = json(&output);
        let run = result(&document, "ahead");
        let (dev, _) = mounted_device(&sequential);
        let lost = losses(run)["dropped"].as_u64().unwrap_or(0);
        let evidence = || {
            format!(
                "\n  the run's losses: {}\n  lookups per file (dev, ino) -> (pages, calls): {:?}",
                losses(run),
                lookups_by_file(run)
            )
        };
        let mut ahead_of = BTreeMap::new();
        let mut shortfall = Vec::new();
        for how in ["sequential", "random"] {
            let phase = &did[how];
            let file = (dev, phase["ino"].as_u64().expect("an inode"));
            let asked = phase["asked"].as_u64().expect("pages asked");
            let became = phase["resident_after"].as_u64().expect("resident")
                - phase["resident_before"].as_u64().expect("resident");
            let (lookups, _, filled, created, ahead) = page_counts(run, file, "ahead");
            // Never over, lossy or not: a loss can only make a count short.
            assert!(
                lookups <= asked && filled <= became,
                "{how}: the workload read {asked} pages and {became} became resident, and the \
                 tool counted {lookups} looked up and {filled} filled: more than happened{}",
                evidence()
            );
            assert_eq!(created, 0, "{how}: a read creates no page");
            assert_eq!(
                ahead,
                filled as i64 - lookups as i64,
                "{how}: ahead is the fill minus the lookup"
            );
            // Each file's rows were stamped inside the phase that read it.
            let (begin, end) = (
                phase["begin"].as_u64().expect("a start"),
                phase["end"].as_u64().expect("an end"),
            );
            for (raw, _) in raw_with_keys(run) {
                if file_key(&raw) != Some(file) {
                    continue;
                }
                let ts = raw["ts_ns"].as_u64().expect("a timestamp");
                assert!(
                    (begin..=end).contains(&ts),
                    "{how}: a row stamped at {ts} is outside the phase that read the file, \
                     [{begin}, {end}] by the workload's own CLOCK_MONOTONIC"
                );
            }
            if lost > 0 {
                shortfall.push(format!(
                    "{how} {lookups}/{asked} looked up, {filled}/{became} filled"
                ));
                continue;
            }
            if how == "random" {
                // A single-page read is a batch of one folio, which the
                // kernel's early return cannot shorten: exact.
                assert_eq!(
                    lookups,
                    asked,
                    "random: the workload read {asked} pages and {lookups} lookups were \
                     counted, in a run that lost nothing{}",
                    evidence()
                );
            } else {
                assert!(
                    lookups * 2 >= asked,
                    "sequential: the workload read {asked} pages and {lookups} lookups were \
                     counted, in a run that lost nothing; cold, a read's first batch may go \
                     unseen, but not half of them{}",
                    evidence()
                );
            }
            assert_eq!(
                filled,
                became,
                "{how}: mincore says {became} pages became resident, and the tool counted \
                 {filled} filled, in a run that lost nothing{}",
                evidence()
            );
            ahead_of.insert(how, ahead);
        }
        if lost > 0 {
            eprintln!(
                "note: attempt {attempt} of {ATTEMPTS} lost {lost} runs of the programs, over \
                 every task of the machine, so its counts are floors and not held exact: {}",
                shortfall.join("; ")
            );
            lossy.push(lost);
            continue;
        }
        let (sequential, random) = (ahead_of["sequential"], ahead_of["random"]);
        assert!(
            sequential > 0,
            "a sequential read of a cold file must be read ahead of: {sequential} pages ahead"
        );
        assert!(
            random * 8 <= sequential,
            "random reads are read ahead of by next to nothing -- two random pages may sit side \
             by side -- and a sequential reader by a window: {random} against {sequential}"
        );
        return;
    }
    // Every attempt lost runs: the machine is busy enough that a preempted run
    // of these programs keeps a cpu's recursion guard raised. A difference of
    // two floors is no separation, and a floor is no exact count.
    eprintln!(
        "skipped: the exact counts and the sequential/random separation were not measured: \
         each of {ATTEMPTS} attempts lost runs of the programs ({lossy:?}), counted over every \
         task of the machine and not attributable to the workload's reads"
    );
}

/// Pages a write appends enter the cache and are created, never missed.
///
/// A megabyte in sixteen writes to a new file: 256 pages must be created and
/// none filled or looked up. Before the program told the two apart, every
/// write of a new file was a miss, and a workload that only writes read as a
/// cache that never hits.
#[test]
fn a_page_a_write_appends_is_created_and_never_a_miss() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    const APPEND: &str = r#"
import json, os, sys
path, account = sys.argv[1], sys.argv[2]
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
block = os.urandom(65536)
for _ in range(16):
    assert os.write(fd, block) == len(block)
json.dump({"ino": os.fstat(fd).st_ino, "pages": 256}, open(account, "w"))
"#;
    let path = scratch("pagecache-append.bin");
    let _ = std::fs::remove_file(&path);
    let account_path = scratch("pagecache-append.json");
    let workload = script("pagecache-append.py", APPEND);
    let output = run_on_launch(
        "pagecache-append",
        &[("cache", "io.pagecache", "count", "raw")],
        &[
            "python3",
            workload.to_str().expect("a path"),
            path.to_str().expect("a path"),
            account_path.to_str().expect("a path"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let did = account(&account_path);
    let (dev, _) = mounted_device(&path);
    let file = (dev, did["ino"].as_u64().expect("an inode"));
    let document = json(&output);
    let (lookups, _, filled, created, misses) =
        page_counts(result(&document, "cache"), file, "misses");
    assert_eq!(created, 256, "a megabyte appended is 256 pages created");
    assert_eq!(
        (filled, misses, lookups),
        (0, 0, 0),
        "and nothing missed or looked up"
    );
}

/// A hundred threads each reading a thousand one-byte files once is a hundred
/// thousand distinct `(task, file)` rows, several times what the map holds in
/// one drain, and every lookup that found no row is counted as lost.
///
/// The property that closes against a number, as for the fentry histograms:
/// every read is either in a row or in the drop count, and no read is in a row
/// twice. Where this machine cannot make the rows fast enough to fill one
/// drain's worth, nothing is dropped and the loss path was not created: that
/// is a skip with the numbers, and never a pass.
#[test]
fn more_page_cache_rows_than_the_map_holds_are_counted_as_lost() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let directory = scratch("pagecache-many-rows");
    let account_path = scratch("pagecache-many-rows.json");
    let workload = script("pagecache-many-rows.py", MANY_ROWS);
    let output = run_on_launch(
        "pagecache-many-rows",
        &[("cache", "io.pagecache", "trace", "raw")],
        &[
            "python3",
            workload.to_str().expect("a path"),
            directory.to_str().expect("a path"),
            account_path.to_str().expect("a path"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let did = account(&account_path);
    let reads = did["reads"].as_u64().expect("reads");
    let inos: std::collections::BTreeSet<u64> = did["inos"]
        .as_array()
        .expect("inos")
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .collect();
    let document = json(&output);
    let run = result(&document, "cache");
    let counted: u64 = raw_with_keys(run)
        .iter()
        .filter(|(_, event)| inos.contains(&attr_u64(event, "ino")))
        .map(|(_, event)| attr_u64(event, "lookup_calls"))
        .sum();
    let dropped = losses(run)["dropped"].as_u64().expect("a drop count");
    if dropped == 0 {
        eprintln!(
            "skipped: {counted} of {reads} reads were counted and none dropped, so this machine \
             did not fill a drain's worth of rows -- the map holds 16384 per 100 ms -- and the \
             loss path was not created"
        );
        assert_eq!(
            counted, reads,
            "with nothing dropped, every read is in a row"
        );
        return;
    }
    assert!(
        counted + dropped >= reads,
        "{counted} counted and {dropped} dropped, for {reads} reads the program made: lookups \
         were lost without being counted"
    );
    assert!(
        counted <= reads,
        "{counted} counted for {reads} reads: a lookup was counted twice"
    );
    let reasons = losses(run)["reasons"].to_string();
    assert!(
        reasons.contains("lower bound"),
        "a lossy run must say its totals are a floor: {reasons}"
    );
}

/// Both refuse what they cannot serve, with the reason and the exit code that
/// belongs to it: a `poll` cannot mean anything for a count of instants (2); a
/// cgroup target is the build's gap for `io.pagecache`, which declares it (3),
/// and a request that cannot mean anything for `io.readahead`, which does not
/// (2); and a binary without the capabilities is this machine's gap (1).
#[test]
fn the_page_cache_sources_refuse_with_the_reason_that_is_actually_missing() {
    for source in ["io.pagecache", "io.readahead"] {
        let polled = run(&[
            source,
            "--system",
            "-m",
            "poll@100ms",
            "-e",
            "summary",
            "-d",
            "200ms",
        ]);
        assert_eq!(
            polled.status.code(),
            Some(EXIT_REJECTED),
            "{source}: {}",
            stderr(&polled)
        );
    }
    let cgroup = |source| {
        run(&[
            source,
            "--cgroup",
            "/sys/fs/cgroup",
            "-m",
            "count",
            "-e",
            "summary",
            "-d",
            "100ms",
        ])
    };
    let declared = cgroup("io.pagecache");
    if declared.status.code() == Some(EXIT_NO_BACKEND) {
        eprintln!(
            "skipped the cgroup refusal of io.pagecache: {}",
            stderr(&declared)
        );
    } else {
        assert_eq!(
            declared.status.code(),
            Some(EXIT_NOT_IMPLEMENTED),
            "{}",
            stderr(&declared)
        );
        assert!(
            stderr(&declared).contains("bpf_get_current_cgroup_id"),
            "the refusal should name what would serve it: {}",
            stderr(&declared)
        );
    }
    let undeclared = cgroup("io.readahead");
    assert_eq!(
        undeclared.status.code(),
        Some(EXIT_REJECTED),
        "{}",
        stderr(&undeclared)
    );

    let bare = scratch("omn-without-capabilities-pagecache");
    std::fs::create_dir_all(bare.parent().expect("a parent")).expect("the scratch directory");
    std::fs::copy(BINARY, &bare).expect("a copy of the binary");
    for source in ["io.pagecache", "io.readahead"] {
        let output = Command::new(&bare)
            .args([
                source, "--system", "-m", "count", "-e", "summary", "-d", "100ms",
            ])
            .output()
            .expect("the copy runs");
        let complaint = stderr(&output);
        if output.status.success() {
            eprintln!("skipped the capability refusal of {source}: the copy collected");
            continue;
        }
        assert_eq!(
            output.status.code(),
            Some(EXIT_NO_BACKEND),
            "{source}: {complaint}"
        );
        assert!(
            complaint.contains("CAP_BPF") && complaint.contains("setcap"),
            "{source}: a binary without the capabilities must be told which and how: {complaint}"
        );
    }
}

/// A cold read of sixteen 1 MiB reads may go unseen for the first batch of
/// each, which bounds how far above its exact value the session's total can be.
const FIRST_BATCHES_OF_SIXTEEN: f64 = 16.0 * 128.0;

/// The two page cache sessions say what their own headers predict.
///
/// `io-cold-then-warm.yml` reads a 4096-page file twice, so its heaviest
/// `io.pagecache` series is that file with exactly 4096 misses, and the same
/// file's `io.readahead` warm window is exactly -4096: the whole file found and
/// none of it filled. `io-readahead-patterns.yml`
/// cannot be held to a size -- the readahead window is the machine's -- but the
/// sequential file must come out ahead, which no random reader and no
/// interpreter reading its own modules does. Both write their files into the
/// directory they are run from, which here is on the build directory's
/// filesystem rather than a tmpfs.
#[test]
fn the_page_cache_sessions_say_what_their_headers_predict() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let here = scratch("pagecache-sessions");
    std::fs::create_dir_all(&here).expect("a directory to run the sessions from");
    let in_scratch = |name: &str| {
        Command::new(BINARY)
            .args(["-f", &session(name), "--format", "json"])
            .current_dir(&here)
            .output()
            .expect("omn should run")
    };

    let output = in_scratch("io-cold-then-warm.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);
    let heaviest = &output_of(result(&document, "cache"), "topk")["series"][0];
    // Exact unless the run says it lost something: a skipped run of the
    // program or a full ring is a miss nobody counted, reported in
    // `losses.dropped` (the flaky tally's #18), and then only the ceiling
    // holds.
    let skipped = losses(result(&document, "cache"))["dropped"]
        .as_u64()
        .expect("a drop count") as f64;
    let misses = number(&heaviest["total"]);
    if skipped == 0.0 {
        assert_eq!(
            misses, 4096.0,
            "a 4096-page file read cold and then warm is 4096 misses: {heaviest}"
        );
    } else {
        assert!(
            (0.0..=4096.0).contains(&misses),
            "{skipped} losses reported, so fewer than 4096 misses may be counted, never more: \
             {heaviest}"
        );
    }
    let file = heaviest["key"].as_str().expect("a series key");
    let ahead = series(output_of(result(&document, "ahead"), "rate"), file)
        .unwrap_or_else(|| panic!("io.readahead has a series for {file}"));
    // The warm read's window is exact: every page was there, and the reader was
    // served 4096 pages it did not fill. The cold window may be a little
    // positive rather than zero, because a cold read's first batch can go
    // unseen by the lookup tracepoint, and never negative.
    let windows = series(output_of(result(&document, "ahead"), "summary"), file)
        .unwrap_or_else(|| panic!("io.readahead summarises {file}"));
    let skipped_ahead = losses(result(&document, "ahead"))["dropped"]
        .as_u64()
        .expect("a drop count") as f64;
    // A dropped record is a whole window of up to a read's worth of pages, not
    // one page, so when the run reports any the exact figure is out of reach
    // and only its sign and its floor are asserted -- measured once under the
    // other streams' suites: 197 of 214 records lost to a full ring, and a
    // warm window of -3584.
    let warm = number(&windows["min"]);
    if skipped_ahead == 0.0 {
        assert_eq!(
            warm, -4096.0,
            "the warm read looked up 4096 pages and filled none: {windows}"
        );
    } else {
        eprintln!(
            "note: {skipped_ahead} readahead records were lost, so the warm window ({warm}) is \
             bounded rather than exact"
        );
        assert!(
            (-4096.0..0.0).contains(&warm),
            "a warm read fills nothing, so its window is negative and no lower than the file: \
             {windows}"
        );
    }
    let total = number(&ahead["total"]);
    let exact = skipped_ahead == 0.0;
    assert!(
        (!exact && total <= FIRST_BATCHES_OF_SIXTEEN)
            || (-4096.0..=-4096.0 + FIRST_BATCHES_OF_SIXTEEN).contains(&total),
        "two whole reads of 4096 pages, one filling them, is -4096 and at most a cold read's \
         first batches above it: {ahead}, per window {windows}"
    );

    let output = in_scratch("io-readahead-patterns.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);
    let rates = output_of(result(&document, "ahead"), "rate")["series"]
        .as_array()
        .expect("rate series")
        .iter()
        .map(|series| number(&series["total"]))
        .fold(f64::MIN, f64::max);
    assert!(
        rates > 0.0,
        "the sequential file must come out ahead of its reads: the largest total was {rates}"
    );
}

// ============================================ the drain race, on a short tick

/// Eight threads reading one cached page at a time as fast as they can for a
/// given time, each counting its own reads; the sum is printed at the end.
/// One page-aligned 4 KiB `pread` of a cached file is one
/// `mm_filemap_get_pages` call over one page, so the program's own count is
/// the number of lookups `io.pagecache` must find.
const PAGE_READER_SOURCE: &str = r#"
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

#define THREADS 8
#define PAGES 64

static double run_for;
static int fd;
static unsigned long long reads[THREADS];

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void *reader(void *slot) {
    unsigned long long *mine = slot;
    char page[4096];
    double until = now() + run_for;
    unsigned long long at = (unsigned long long)(mine - reads);
    while (now() < until) {
        for (int i = 0; i < 256; i++) {
            if (pread(fd, page, sizeof page, (off_t)((at++ % PAGES) * 4096)) != 4096) abort();
        }
        *mine += 256;
    }
    return NULL;
}

int main(int argc, char **argv) {
    run_for = atof(argv[2]);
    fd = open(argv[1], O_RDONLY);
    if (fd < 0) return 2;
    pthread_t threads[THREADS];
    for (int i = 0; i < THREADS; i++) pthread_create(&threads[i], NULL, reader, &reads[i]);
    unsigned long long total = 0;
    for (int i = 0; i < THREADS; i++) {
        pthread_join(threads[i], NULL);
        total += reads[i];
    }
    fprintf(stderr, "omn-reads calls=%llu\n", total);
    return 0;
}
"#;

/// Builds a C fixture beside the scratch files, renamed into place so a
/// half-written binary is never at the path a test launches.
fn build_c(name: &str, source: &str) -> Result<PathBuf, String> {
    let path = scratch(name);
    let dir = path.parent().expect("a directory").to_owned();
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let source_path = scratch(&format!("{name}.c"));
    std::fs::write(&source_path, source).map_err(|error| error.to_string())?;
    let partial = scratch(&format!("{name}.{}", std::process::id()));
    let built = Command::new("cc")
        .args(["-O1", "-pthread", "-o"])
        .arg(&partial)
        .arg(&source_path)
        .output()
        .map_err(|error| format!("no C compiler (`cc`) to build the fixture with: {error}"))?;
    if !built.status.success() {
        return Err(format!(
            "the `{name}` fixture did not build:\n{}",
            String::from_utf8_lossy(&built.stderr)
        ));
    }
    std::fs::rename(&partial, &path).map_err(|error| error.to_string())?;
    Ok(path)
}

/// Every lookup a program made is in a row or in a counted loss, over several
/// runs on the collector's own 100 ms tick -- the drain never takes a row out
/// from under the increment about to land in it.
///
/// The race stream K measured on `proc.syscall` (seventeen to twenty-five of
/// twenty million calls gone per run, `docs/families/drains.md`) was in
/// `io_pagecache.rs` too, until it moved onto `map::Rows`. Eight threads at
/// millions of lookups a second into eight rows, drained fifteen times a run,
/// is its shape. Exact, not close: the program's count is its own, the rows are
/// narrowed to its file, and a lookup that is neither in a row nor in `losses`
/// is the defect.
#[test]
fn every_lookup_a_program_made_is_in_a_row_or_a_counted_loss_on_a_short_tick() {
    const RUNS: usize = 3;
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let fixture = match build_c("page-reader", PAGE_READER_SOURCE) {
        Ok(path) => path,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let file = scratch("page-reader.data");
    std::fs::write(&file, vec![7u8; 64 * 4096]).expect("the file the fixture reads");
    // Read once here so every page is cached before the run starts.
    let _ = std::fs::read(&file).expect("the file reads back");
    let ino = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&file).expect("the file").ino()
    };
    let mut seen = Vec::new();
    for run in 0..RUNS {
        let output = run_on_launch(
            "page-reader",
            &[("cache", "io.pagecache", "trace", "raw")],
            &[
                fixture.to_str().expect("a path"),
                file.to_str().expect("a path"),
                "1.5",
            ],
        );
        assert!(output.status.success(), "{}", stderr(&output));
        let made: u64 = stderr(&output)
            .lines()
            .find_map(|line| line.strip_prefix("omn-reads calls="))
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or_else(|| panic!("the fixture says its reads:\n{}", stderr(&output)));
        let document = json(&output);
        let result = result(&document, "cache");
        let counted: u64 = raw_with_keys(result)
            .iter()
            .filter(|(_, event)| attr_u64(event, "ino") == ino)
            .map(|(_, event)| attr_u64(event, "lookup_calls"))
            .sum();
        let losses = losses(result);
        let lost: u64 = ["dropped", "undecoded", "throttled"]
            .iter()
            .filter_map(|field| losses[*field].as_u64())
            .sum();
        seen.push(format!(
            "run {run}: made {made}, counted {counted}, lost {lost}"
        ));
        assert!(
            counted <= made,
            "{counted} lookups counted for {made} made -- an increment landed in another row's \
             element: {seen:?} {losses}"
        );
        assert!(
            counted + lost >= made,
            "the program made {made} lookups, {counted} are in rows and {lost} are counted as \
             lost: {} vanished without a trace: {seen:?} {losses}",
            made - counted - lost
        );
    }
    eprintln!("{}", seen.join("\n"));
}

/// The same count through fentry: every `pread64` the page reader made is in
/// an `io.syscall` row or a counted loss, over several runs on the
/// collector's own tick -- including the calls closed between the last drain's
/// flip and the end of the run, which the old last drain took after sleeping a
/// millisecond as its grace period. It drains through `membarrier` now.
#[test]
fn every_call_through_fentry_is_in_a_row_or_a_counted_loss_to_the_last_drain() {
    const RUNS: usize = 3;
    if let Some(reason) = no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let fixture = match build_c("page-reader", PAGE_READER_SOURCE) {
        Ok(path) => path,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let file = scratch("page-reader-fentry.data");
    std::fs::write(&file, vec![7u8; 64 * 4096]).expect("the file the fixture reads");
    let mut seen = Vec::new();
    for run in 0..RUNS {
        let output = run_on_launch(
            "page-reader-fentry",
            &[("calls", "io.syscall", "trace", "raw")],
            &[
                fixture.to_str().expect("a path"),
                file.to_str().expect("a path"),
                "1.0",
            ],
        );
        assert!(output.status.success(), "{}", stderr(&output));
        let made: u64 = stderr(&output)
            .lines()
            .find_map(|line| line.strip_prefix("omn-reads calls="))
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or_else(|| panic!("the fixture says its reads:\n{}", stderr(&output)));
        let document = json(&output);
        let result = result(&document, "calls");
        let counted: u64 = raw_with_keys(result)
            .iter()
            .filter(|(_, event)| attr_text(event, "syscall") == "pread64")
            .map(|(_, event)| attr_u64(event, "count"))
            .sum();
        let losses = losses(result);
        let lost: u64 = ["dropped", "undecoded", "throttled"]
            .iter()
            .filter_map(|field| losses[*field].as_u64())
            .sum();
        seen.push(format!(
            "run {run}: made {made}, counted {counted}, lost {lost}"
        ));
        assert!(
            counted <= made,
            "{counted} calls counted for {made} made: {seen:?} {losses}"
        );
        assert!(
            counted + lost >= made,
            "the program made {made} calls, {counted} are in rows and {lost} are counted as \
             lost: {} vanished without a trace: {seen:?} {losses}",
            made - counted - lost
        );
    }
    eprintln!("{}", seen.join("\n"));
}

// ---------------------------------------------------------------------------
// The histogram the kernel builds

/// Reads of two known sizes through `O_DIRECT`, each one block request, and
/// the program's own count of how many of each it completed.
const TWO_SIZES: &str = r#"
import json, mmap, os, sys
path, account = sys.argv[1], sys.argv[2]
done = {}
cached = os.open(path, os.O_RDONLY)
os.posix_fadvise(cached, 0, 0, os.POSIX_FADV_DONTNEED)
os.close(cached)
try:
    fd = os.open(path, os.O_RDONLY | os.O_DIRECT)
except OSError as error:
    done["refused"] = str(error)
else:
    offset = 0
    for size, count in ((4096, 256), (65536, 64)):
        buffer = mmap.mmap(-1, size)
        issued = 0
        for _ in range(count):
            if os.preadv(fd, [buffer], offset) != size:
                break
            offset += size
            issued += 1
        done[str(size)] = issued
json.dump(done, open(account, "w"))
"#;

/// The requests of a log2 histogram series, by bucket low bound.
fn buckets_of(series: &serde_json::Value) -> BTreeMap<u64, u64> {
    series["buckets"]
        .as_array()
        .expect("a histogram series has buckets")
        .iter()
        .filter_map(|bucket| Some((bucket["low"].as_u64()?, bucket["count"].as_u64()?)))
        .filter(|(_, count)| *count > 0)
        .collect()
}

/// `io.latency --emit hist` builds its histogram in the kernel, and it is the
/// histogram the streamed requests make.
///
/// One launch, two requests over it: `hist` alone, which the kernel answers
/// with one `(device, log2 bucket)` row per bucket and no record per request,
/// and `raw` with `hist`, which streams every completion through the ring and
/// buckets it up here. The program reads 256 pages and 64 blocks of 64 KiB
/// through `O_DIRECT` and counts what completed, so the ground truth is its
/// own, and each path is held to it separately before the two are compared:
/// each counts no more requests than the program issued and, with what it
/// dropped added, no fewer than that less the two a filesystem may turn into
/// readaheads; the stream's are of those two sizes or readaheads. Then the
/// kernel's histogram holds exactly as many requests as the stream carried, in
/// the same buckets. A run that dropped anything still gets the first half.
///
/// Not bit for bit per bucket, and why: the two program sets are two programs
/// on each tracepoint, each reading the clock when it runs, so the same request
/// has two intervals apart by whatever ran between the two reads -- the first
/// set's own work, and an interrupt when one lands there. Measured with a third
/// set, the witness, paired with the stream request by request: -350 to
/// +570 ns, median +120, on 999 direct 4 KiB reads of about 5.7 us. That is
/// what put 4 of 318 across an edge in one run of five (the fifteenth wave's
/// finding): the old tolerance was 2 % of the value, 115 ns at 5.7 us, and the
/// skew is absolute. The comparison now allows a request to cross only where
/// the stream's value is within twice this run's median skew of an edge, plus
/// as many as the witness itself was seen to put in another bucket from
/// beyond that reach, and the totals must agree exactly. The arithmetic is not
/// the cause: `insn::the_unrolled_log2_is_the_same_arithmetic_as_the_layouts`
/// holds the kernel's bucketing to the emit layer's at every edge.
#[test]
fn a_histogram_built_in_the_kernel_is_the_one_the_streamed_requests_make() {
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    const PAGES: usize = 256;
    const BLOCKS: usize = 64;
    let path = direct_read_file("io-two-sizes.bin", PAGES + BLOCKS * 16);
    let workload = script("io-two-sizes.py", TWO_SIZES);
    let account_path = scratch("io-two-sizes.json");
    let _ = std::fs::remove_file(&account_path);
    let launch = format!(
        "[python3, {}, {}, {}]",
        workload.display(),
        path.display(),
        account_path.display()
    );
    let session = format!(
        "name: one launch, two histograms\n\
         requests:\n\
         \x20 - label: in-kernel\n\
         \x20   source: io.latency\n\
         \x20   target: {{launch: {launch}}}\n\
         \x20   mode: trace\n\
         \x20   emit: [hist]\n\
         \x20 - label: streamed\n\
         \x20   source: io.latency\n\
         \x20   target: {{launch: {launch}}}\n\
         \x20   mode: trace\n\
         \x20   emit: [raw, hist]\n\
         \x20 - label: witness\n\
         \x20   source: io.latency\n\
         \x20   target: {{launch: {launch}}}\n\
         \x20   mode: trace\n\
         \x20   emit: [raw]\n"
    );
    let session_path = scratch("io-two-histograms.yml");
    std::fs::write(&session_path, session).expect("the session");
    let output = run(&[
        "-f",
        session_path.to_str().expect("a path"),
        "--format",
        "json",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let did = account(&account_path);
    if let Some(error) = did["refused"].as_str() {
        eprintln!("skipped: {} refuses O_DIRECT ({error})", path.display());
        return;
    }
    assert_eq!(
        did["4096"].as_u64(),
        Some(PAGES as u64),
        "the program's account: {did}"
    );
    assert_eq!(
        did["65536"].as_u64(),
        Some(BLOCKS as u64),
        "the program's account: {did}"
    );

    let document = json(&output);
    let kernel = result(&document, "in-kernel");
    let streamed = result(&document, "streamed");
    // The kernel's answer says how it was obtained, and the stream's does not.
    let said = |result: &serde_json::Value| result["notes"].to_string();
    assert!(
        said(kernel).contains("the histogram was built in the kernel"),
        "the `hist`-only request did not say it was built in the kernel: {}",
        said(kernel)
    );
    assert!(
        !said(streamed).contains("built in the kernel"),
        "{}",
        said(streamed)
    );

    // What the stream carried: every request one of the two sizes, a
    // fragment of a 64 KiB read, or the readahead a filesystem may turn a
    // first direct read into, and never more requests of a size than reads of
    // it. A fragment is what a direct read spanning two extents of a
    // fragmented file becomes -- one request per extent (btrfs at 99 % full
    // here: 40 960 + 24 576, six times in 64 reads). Fragments are synchronous
    // reads of less than a block, and together they make whole blocks.
    let events = raw_events(streamed);
    let of = |bytes: u64| {
        events
            .iter()
            .filter(|attrs| attrs["bytes"].as_u64() == Some(bytes) && attrs["rwbs"] != "RA")
            .count()
    };
    let fragments: Vec<u64> = events
        .iter()
        .filter(|attrs| !attrs["rwbs"].as_str().unwrap_or_default().contains('A'))
        .filter_map(|attrs| attrs["bytes"].as_u64())
        .filter(|&bytes| bytes != 4096 && bytes != 65536)
        .collect();
    assert!(
        of(4096) <= PAGES && of(65536) <= BLOCKS,
        "more requests than reads: {}",
        of(4096)
    );
    for attrs in &events {
        let bytes = attrs["bytes"].as_u64().unwrap_or_default();
        let rwbs = attrs["rwbs"].as_str().unwrap_or_default();
        assert!(
            bytes == 4096
                || bytes == 65536
                || rwbs.contains('A')
                || (bytes < 65536 && bytes % 512 == 0),
            "a request of {bytes} bytes (`{rwbs}`) that the program did not make"
        );
    }
    let fragment_bytes: u64 = fragments.iter().sum();
    assert_eq!(
        fragment_bytes % 65536,
        0,
        "requests of {fragments:?} bytes do not make whole 64 KiB reads"
    );
    let fragmented_reads = fragment_bytes / 65536;
    // The requests the fragments add over the reads they make up.
    let split = fragments.len() as u64 - fragmented_reads;
    if !fragments.is_empty() {
        eprintln!(
            "{} of the program's 64 KiB reads reached the device as {} requests: the file is \
             fragmented on this filesystem",
            fragmented_reads,
            fragments.len()
        );
    }
    let lost = |result: &serde_json::Value| losses(result)["dropped"].as_u64().unwrap_or(0);
    let series = |result: &serde_json::Value| -> BTreeMap<String, BTreeMap<u64, u64>> {
        output_of(result, "hist")["series"]
            .as_array()
            .expect("series")
            .iter()
            .map(|series| {
                (
                    series["key"].as_str().unwrap_or("").to_owned(),
                    buckets_of(series),
                )
            })
            .collect()
    };
    let (from_kernel, from_stream) = (series(kernel), series(streamed));
    let total = |all: &BTreeMap<String, BTreeMap<u64, u64>>| -> u64 {
        all.values().flat_map(|buckets| buckets.values()).sum()
    };

    // The program's own account, against each path separately and before
    // either is compared with the other. It made PAGES + BLOCKS reads, each
    // one request; a filesystem may turn the first direct read of a size into
    // a readahead (`RA`, of any size), which is the only request allowed that
    // the program did not ask for, and it replaces a read rather than adding
    // to one. So a path that lost nothing holds between PAGES + BLOCKS - 2 of
    // the program's sizes and PAGES + BLOCKS in all -- the two readaheads
    // included -- and a request both paths count and the program never made
    // (the interpreter's own reads, another process's under the same launch)
    // shows here as a total above what the program issued.
    let issued = (PAGES + BLOCKS) as u64;
    let readahead = events.len() as u64 - (of(4096) + of(65536) + fragments.len()) as u64;
    assert!(
        readahead <= 2,
        "{readahead} requests of neither size the program read at: at most the first read \
         of each size may be a readahead in its place"
    );
    let own = |result: &serde_json::Value, counted: u64| {
        // A lost request is one the path did not count; it was still made.
        let seen = counted + lost(result);
        assert!(
            seen + 2 >= issued && counted <= issued + split,
            "the `{}` path counted {counted} requests and dropped {}, and the program made \
             {issued} ({PAGES} of 4 KiB and {BLOCKS} of 64 KiB), {split} more where a read was \
             split along the file's extents",
            result["label"].as_str().unwrap_or("?"),
            lost(result)
        );
    };
    own(kernel, total(&from_kernel));
    own(streamed, events.len() as u64);
    assert_eq!(
        total(&from_stream),
        events.len() as u64,
        "the stream's histogram is its events"
    );
    // Both lost nothing, or the comparison of one with the other is of two
    // different sets of requests. What each path could be held to alone has
    // been checked above, with its losses counted, so this is not a skip.
    if lost(kernel) > 0 || lost(streamed) > 0 {
        eprintln!(
            "not compared bucket by bucket: the kernel skipped runs of the programs ({} and {} \
             dropped), each set its own, so the two histograms are of different requests; \
             each was held to the program's account on its own",
            lost(kernel),
            lost(streamed)
        );
        return;
    }
    let blocks = of(65536) as u64 + fragmented_reads;
    assert!(
        of(4096) + 1 >= PAGES && blocks + 1 >= BLOCKS as u64,
        "a run that lost nothing is short of the program's reads by more than the one a \
         filesystem may turn into a readahead: {} of {PAGES} pages, {blocks} of {BLOCKS} blocks",
        of(4096)
    );
    assert_eq!(
        from_kernel.keys().collect::<Vec<_>>(),
        from_stream.keys().collect::<Vec<_>>(),
        "the two histograms are of different devices"
    );
    assert_eq!(
        total(&from_kernel),
        total(&from_stream),
        "the kernel counted {} requests and the stream carried {}",
        total(&from_kernel),
        total(&from_stream)
    );

    // How far apart two program sets read the clock for one request, measured
    // rather than assumed: the witness is a third set on the same tracepoints,
    // and the kernel's own stamp on each request (`blk:rq@open_ts`) pairs its
    // records with the stream's one for one. Their disagreement is the
    // skew of this run, on this machine, under this load -- a few hundred
    // nanoseconds here, absolute and not relative to the latency, which is why
    // a tolerance of 2 % of the value was too narrow for a request of 5 us.
    let witness = result(&document, "witness");
    let (by_stream, by_witness) = (latency_by_request(streamed), latency_by_request(witness));
    let mut skews: Vec<u64> = by_stream
        .iter()
        .filter_map(|(request, value)| Some(value.abs_diff(*by_witness.get(request)?)))
        .collect();
    skews.sort_unstable();
    let paired = skews.len();
    assert!(
        paired * 10 >= by_stream.len() * 9,
        "the witness paired {paired} of the stream's {} requests by their kernel stamp",
        by_stream.len()
    );
    // The usual skew, and the requests something ran between the reads of.
    // The reach is twice the *median* skew, not a high percentile: a request
    // the kernel's set and the stream's read 120 ns apart crosses an edge only
    // if it lies within that of one. What the median leaves out -- an
    // interrupt or a preemption between two sets' programs, microseconds once
    // in a while (11.5 us once in six runs here) -- is allowed only as the
    // witness saw it actually happen: a witnessed request skewed beyond the
    // reach *and* put in another bucket than the stream's. The kernel's set is
    // not the witness, so this is a margin measured on the run, not a
    // calibration; it is not one counted twice either, since a crosser the
    // witness saw beyond the reach is not one the reach accounts for.
    let bucket = |value: u64| if value == 0 { 0 } else { 1u64 << value.ilog2() };
    let skew = skews[paired / 2];
    let reach = 2 * skew;
    let outliers = by_stream
        .iter()
        .filter_map(|(request, value)| Some((*value, *by_witness.get(request)?)))
        .filter(|(stream, witness)| {
            stream.abs_diff(*witness) > reach && bucket(*stream) != bucket(*witness)
        })
        .count() as u64;
    let near_an_edge = events
        .iter()
        .filter_map(|attrs| attrs["latency_ns"].as_u64())
        .filter(|&value| {
            let edge = value.next_power_of_two();
            let below = edge / 2;
            let near = |edge: u64| value.abs_diff(edge) <= reach;
            near(edge) || (below > 0 && near(below))
        })
        .count() as u64;
    // The fewest edge crossings that turn one histogram into the other: walking
    // up the buckets, whatever one side holds more of below an edge must have
    // crossed that edge, so the running difference after each bucket is the
    // traffic across the edge above it. Every one of those crossings is a
    // request within reach of that edge.
    let mut crossings = 0;
    for (key, kernel_buckets) in &from_kernel {
        let stream_buckets = &from_stream[key];
        let lows: std::collections::BTreeSet<&u64> =
            kernel_buckets.keys().chain(stream_buckets.keys()).collect();
        let mut carried: i64 = 0;
        for low in lows {
            let a = kernel_buckets.get(low).copied().unwrap_or(0);
            let b = stream_buckets.get(low).copied().unwrap_or(0);
            carried += a as i64 - b as i64;
            crossings += carried.unsigned_abs();
        }
    }
    eprintln!(
        "the two histograms are {crossings} edge crossings apart; {near_an_edge} requests were \
         within {reach} ns of an edge (twice the median skew of {paired} witnessed requests) \
         and {outliers} witnessed requests were skewed further and into another bucket"
    );
    assert!(
        crossings <= near_an_edge + outliers,
        "the kernel's buckets and the stream's are {crossings} edge crossings apart, and only \
         {near_an_edge} requests were within twice the measured skew ({skew} ns, the median \
         of {paired}) of an edge, with {outliers} witnessed crossers skewed further:\n kernel \
         {from_kernel:?}\n stream {from_stream:?}"
    );
}

/// Each request's latency as one streamed result reports it, by the kernel's
/// own stamp on the request -- the `blk` key's pointer and `open_ts` -- which
/// two program sets read identically.
fn latency_by_request(result: &serde_json::Value) -> BTreeMap<(u64, u64), u64> {
    output_of(result, "raw")["events"]
        .as_array()
        .expect("a raw emit carries its events")
        .iter()
        .filter_map(|event| {
            let blk = event["keys"]
                .as_array()?
                .iter()
                .find_map(|key| key.get("blk"))?;
            let latency = event["attrs"]
                .as_array()?
                .iter()
                .find(|attr| attr["name"] == "latency_ns")?["value"]
                .as_u64()?;
            Some(((blk["ptr"].as_u64()?, blk["open_ts"].as_u64()?), latency))
        })
        .collect()
}

/// Phase 6's exit criterion: `io.latency --emit hist` at two million requests
/// a second costs under 1 % of a real device's request -- 200 ns of a fast
/// NVMe's 20 us 4K read -- per request, measured on `null_blk`, which takes
/// no time of its own and so shows the programs' cost alone.
///
/// The load needs a device that answers that fast and no disk does: `null_blk`
/// completes every request at once from memory, and loading it is root. With
/// it loaded and readable (`/dev/nullb0`), `fio` drives it from 8 jobs at queue
/// depth 64, three times bare and three times beside a system-wide in-kernel
/// histogram, alternated, and the ratio of the median IOPS figures is the
/// overhead: the target's own rate is the ground truth, measured the same way
/// each time. System-wide, where the queues keep statistics, the histogram is
/// one program reading the kernel's issue stamp; a process target needs a
/// program at the issue too, the only place the submitter is known, and its
/// figure is printed beside. `1 %` is the criterion's
/// number; a loaded machine widens the noise, so this runs alone.
///
/// The percentage is also printed as what it is made of: nanoseconds per
/// request, `jobs * (1/measured - 1/bare)`. With `irqmode=0` the completion
/// runs inline in the submitting thread, so both programs run in a job's own
/// time and that difference is their cost. And when the kernel is keeping
/// program statistics (`kernel.bpf_stats_enabled=1`, machine wide, granted),
/// the tool reports its programs' own run time (`overhead.in_target_cpu_ns`
/// and `in_target_runs`, summed over the programs it loaded) -- the part of
/// the cost that is the programs, apart from the tracepoints' dispatch and the
/// cache. It is the tool that reads it, from its own descriptors: a test
/// process cannot, because `make` gives `omn` file capabilities and the
/// kernel then makes it non-dumpable, which closes its `/proc/<pid>/fdinfo`
/// to anyone unprivileged. When the run time is not known, the tool's own
/// reason (`in_target_from`) is printed instead. Keeping the statistics costs
/// two clock reads per run, so the percentage is read from a run without them
/// and the run time from a second run with them.
///
/// A pass is only evidence if the histogram measured the load: the process
/// filter runs before the clock, so I/O the filter refused would cost one
/// helper call and leave an empty histogram behind a 0 %. The requests the
/// histogram counted are therefore checked against fio's own count of the
/// reads it completed.
///
/// ```console
/// sudo modprobe null_blk queue_mode=2 irqmode=0 completion_nsec=0 nr_devices=1 \
///     gb=4 bs=4096 hw_queue_depth=256 submit_queues=8
/// udevadm settle --timeout=10   # udev applies its rule after modprobe returns
/// sudo chmod a+r /dev/nullb0
/// cargo test --test scenario_io -- --ignored --exact \
///     the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent --nocapture
/// sudo sysctl kernel.bpf_stats_enabled=1   # then again, for the programs' own time
/// cargo test --test scenario_io -- --ignored --exact \
///     the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent --nocapture
/// sudo sysctl kernel.bpf_stats_enabled=0
/// sudo rmmod null_blk
/// ```
///
/// The `udevadm settle` is not ceremony: `modprobe` returns before udev has
/// applied its rule to the new node, and the rule sets its mode, so a `chmod`
/// made at once is undone a moment later and the test finds `/dev/nullb0` at
/// 0600 (twice in one session, the coordinator's after the seventeenth wave).
#[test]
#[ignore = "needs null_blk (root): see the doc comment for the commands"]
fn the_in_kernel_histogram_costs_a_two_million_iops_load_under_one_percent() {
    // What the cost is 1 % of: a fast NVMe's 4K read at queue depth 1. A
    // request `null_blk` answers from memory takes no time of its own, and 1 %
    // of nothing is under one program run; this machine's nvme takes 50 us
    // (29 us at best, fio, 2026-10-20), so 20 us holds a faster disk too
    // (`DECISIONS.md`, settled 2026-10-20).
    const REAL_REQUEST_NS: f64 = 20_000.0;
    const DEVICE: &str = "/dev/nullb0";
    const JOBS: f64 = 8.0;
    if std::fs::File::open(DEVICE).is_err() {
        panic!(
            "{DEVICE} is not there or not readable: load null_blk as the doc comment says, and \
             `udevadm settle` before the `chmod`, or udev resets the node to 0600 after it"
        );
    }
    if let Some(reason) = no_bpf_here() {
        panic!("{reason}");
    }
    let fio = |under: Option<&str>| -> (f64, u64, Option<serde_json::Value>) {
        let job = [
            "fio",
            "--name=omn-iops",
            "--filename",
            DEVICE,
            "--rw=randread",
            "--bs=4k",
            "--direct=1",
            "--ioengine=io_uring",
            "--iodepth=64",
            "--numjobs=8",
            "--thread",
            "--group_reporting",
            "--time_based",
            "--runtime=10",
            "--output-format=json",
        ];
        // System-wide, omn runs beside fio, attached before it starts and
        // outliving it, and its document is read from its own stdout.
        let beside = (under == Some("system")).then(|| {
            let child = Command::new(BINARY)
                .args([
                    "io.latency",
                    "-m",
                    "trace",
                    "-e",
                    "hist",
                    "--system",
                    "-d",
                    "15s",
                    "--format",
                    "json",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("omn starts");
            std::thread::sleep(std::time::Duration::from_secs(2));
            child
        });
        let mut command = match under {
            None | Some("system") => {
                let mut command = Command::new(job[0]);
                command.args(&job[1..]);
                command
            }
            Some(emit) => {
                let mut command = Command::new(BINARY);
                command
                    .args([
                        "io.latency",
                        "-m",
                        "trace",
                        "-e",
                        emit,
                        "--format",
                        "json",
                        "--",
                    ])
                    .args(job);
                command
            }
        };
        let output = command.output().expect("fio runs (is it installed?)");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Under omn, fio's own document comes first on stdout, then omn's.
        let start = text.find('{').expect("fio printed json");
        let mut documents = serde_json::Deserializer::from_str(&text[start..])
            .into_iter::<serde_json::Value>()
            .map(|document| document.expect("the json parses"));
        let report = documents.next().expect("fio's document");
        let read = &report["jobs"][0]["read"];
        let iops = read["iops"].as_f64().expect("fio reports iops");
        let ios = read["total_ios"].as_u64().expect("fio reports its reads");
        let omn = match beside {
            Some(child) => {
                let output = child.wait_with_output().expect("omn ends");
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Some(serde_json::from_slice(&output.stdout).expect("omn's document"))
            }
            None => under.map(|_| documents.next().expect("omn's document after fio's")),
        };
        (iops, ios, omn)
    };
    let (bare, _, _) = fio(None);
    assert!(
        bare >= 2_000_000.0,
        "the device answered {bare:.0} IOPS bare, short of the 2 M the criterion is about"
    );
    // A process target pays a program at the issue, which alone knows the
    // submitter; printed, and not what the criterion holds.
    let (paired, _, _) = fio(Some("hist"));
    eprintln!(
        "under a process target: {paired:.0} IOPS, {:.0} ns per request (two programs: the \
         submitter is known at the issue alone)",
        JOBS * (1.0 / paired - 1.0 / bare) * 1e9
    );
    // Three bare runs and three under it, alternated, compared by their
    // medians: one pair of ten-second runs moved by 70 ns a request between
    // two runs of the same build.
    let mut bares = vec![bare];
    let mut unders = Vec::new();
    let mut last = None;
    for round in 0..3 {
        if round > 0 {
            bares.push(fio(None).0);
        }
        let (iops, reads, omn) = fio(Some("system"));
        unders.push(iops);
        last = Some((reads, omn));
    }
    let median = |values: &mut Vec<f64>| {
        values.sort_by(f64::total_cmp);
        values[values.len() / 2]
    };
    let bare = median(&mut bares);
    let measured = median(&mut unders);
    let (reads, omn) = last.expect("three rounds");
    let omn = omn.expect("a run beside fio");
    let result = &omn["results"][0];
    // The machine's other devices are in the run too: the device's row alone.
    let dev = std::fs::read_to_string("/sys/block/nullb0/dev").expect("nullb0's device number");
    let key = format!("dev:{}", dev.trim().replace(':', "+"));
    let counted = series(output_of(result, "hist"), &key)
        .and_then(|series| series["count"].as_u64())
        .unwrap_or_else(|| panic!("no `{key}` series: {result}"));
    let overhead = (bare - measured) / bare * 100.0;
    let per_request_ns = JOBS * (1.0 / measured - 1.0 / bare) * 1e9;
    let budget_ns = REAL_REQUEST_NS / 100.0;
    // Keeping the programs' statistics costs two clock reads a run, which
    // this run then pays and the criterion is not about.
    let stats_on = std::fs::read_to_string("/proc/sys/kernel/bpf_stats_enabled")
        .is_ok_and(|value| value.trim() == "1");
    eprintln!(
        "medians of three: bare {bare:.0} IOPS, under the in-kernel histogram {measured:.0}: {overhead:.2} % of \
         null_blk, {per_request_ns:.0} ns per request against a budget of {budget_ns:.0} ns \
         (1 % of a {:.0} us request){}; the histogram counted {counted} requests of fio's \
         {reads} reads",
        REAL_REQUEST_NS / 1e3,
        if stats_on {
            ", with the programs' statistics on, which this cost includes"
        } else {
            ""
        }
    );
    let inside = &result["overhead"];
    match (
        inside["in_target_cpu_ns"].as_u64(),
        inside["in_target_runs"].as_u64(),
    ) {
        (Some(ns), runs) => {
            let runs = runs.unwrap_or(0);
            eprintln!(
                "the programs ran {runs} times for {ns} ns: {:.1} ns per run, {:.1} ns per \
                 request counted",
                ns as f64 / runs.max(1) as f64,
                ns as f64 / counted.max(1) as f64
            );
        }
        (None, _) => eprintln!(
            "the programs' own run time is not known: {}",
            inside["in_target_from"]
                .as_str()
                .unwrap_or("omn gave no reason")
        ),
    }
    // Requests still in flight when fio stops are counted by the histogram
    // and not by fio, up to its 8 jobs times queue depth 64; the rest must
    // match within 1 %.
    let in_flight = 8 * 64;
    assert!(
        counted * 100 >= reads * 99 && counted <= reads + reads / 100 + in_flight,
        "the histogram counted {counted} requests of fio's {reads} reads: the cost below is \
         not the cost of measuring this load"
    );
    assert!(
        per_request_ns <= budget_ns,
        "the in-kernel histogram cost {per_request_ns:.0} ns per request at {bare:.0} IOPS, \
         over 1 % of a {:.0} us request{}",
        REAL_REQUEST_NS / 1e3,
        if stats_on {
            " -- with `kernel.bpf_stats_enabled` 1, whose clock reads are in it: set it to 0 \
             for the figure the criterion is about"
        } else {
            ""
        }
    );
}

/// A device that takes a latency chosen in advance, and both histograms put
/// every request in the bucket that latency is in.
///
/// `null_blk` with `irqmode=2` completes each request from a high-resolution
/// timer `completion_nsec` after the driver took it, so every request's
/// interval between `block_rq_issue` and `block_rq_complete` is at least that
/// and only a timer's slack more. The value read from the module's own
/// parameters is the ground truth, and the doc comment's 3 ms is the middle of
/// the log2 bucket `[2^21, 2^22)` ns: 0.9 ms from the edge below and 1.2 ms
/// from the one above, against the few hundred nanoseconds two program sets
/// read the clock apart (`docs/scenarios/io.md`, defect 16). So no request can
/// be an edge-crosser, and the in-kernel histogram and the streamed one must
/// agree bucket for bucket and request for request -- the bit equality
/// `a_histogram_built_in_the_kernel_is_the_one_the_streamed_requests_make`
/// cannot ask of a real disk's spread of latencies.
///
/// ```console
/// sudo modprobe null_blk queue_mode=2 irqmode=2 completion_nsec=3000000 nr_devices=1 \
///     gb=1 bs=4096
/// udevadm settle --timeout=10
/// sudo chmod a+r /dev/nullb0
/// cargo test --test scenario_io -- --ignored --exact \
///     a_device_with_a_known_latency_puts_every_request_in_its_bucket_in_both_histograms \
///     --nocapture
/// sudo rmmod null_blk
/// ```
#[test]
#[ignore = "needs null_blk (root) with a timer completion: see the doc comment"]
fn a_device_with_a_known_latency_puts_every_request_in_its_bucket_in_both_histograms() {
    const DEVICE: &str = "/dev/nullb0";
    const READS: u64 = 64;
    let parameter = |name: &str| -> Option<u64> {
        std::fs::read_to_string(format!("/sys/module/null_blk/parameters/{name}"))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    let (Some(2), Some(latency)) = (parameter("irqmode"), parameter("completion_nsec")) else {
        panic!(
            "null_blk is not loaded with a timer completion (irqmode {:?}, completion_nsec \
             {:?}): load it as the doc comment says",
            parameter("irqmode"),
            parameter("completion_nsec")
        );
    };
    if std::fs::File::open(DEVICE).is_err() {
        panic!(
            "{DEVICE} is not there or not readable: `udevadm settle` before the `chmod`, or \
             udev resets the node to 0600 after it"
        );
    }
    if let Some(reason) = no_bpf_here() {
        panic!("{reason}");
    }
    let low = 1u64 << latency.ilog2();
    let high = low * 2;
    // A quarter of the bucket on either side: far more than any skew between
    // two program sets, or a timer's slack, on a machine that is not starved.
    assert!(
        latency >= low + low / 4 && latency + low / 4 <= high,
        "completion_nsec={latency} is within a quarter bucket of an edge of [{low}, {high}): \
         choose one nearer the middle, as 3000000 is"
    );
    let launch = format!(
        "[dd, if={DEVICE}, of=/dev/null, bs=4096, count={READS}, iflag=direct, status=none]"
    );
    let session = format!(
        "name: a known latency\n\
         requests:\n\
         \x20 - label: in-kernel\n\
         \x20   source: io.latency\n\
         \x20   target: {{launch: {launch}}}\n\
         \x20   mode: trace\n\
         \x20   emit: [hist]\n\
         \x20 - label: streamed\n\
         \x20   source: io.latency\n\
         \x20   target: {{launch: {launch}}}\n\
         \x20   mode: trace\n\
         \x20   emit: [raw, hist]\n"
    );
    let session_path = scratch("io-known-latency.yml");
    std::fs::write(&session_path, session).expect("the session");
    let output = run(&[
        "-f",
        session_path.to_str().expect("a path"),
        "--format",
        "json",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);
    let (kernel, streamed) = (
        result(&document, "in-kernel"),
        result(&document, "streamed"),
    );
    // The `hist`-only request must have counted in the kernel: had it quietly
    // run on the streaming path, the two histograms would agree trivially and
    // the in-kernel one would not be held to the known latency at all.
    let said = |result: &serde_json::Value| result["notes"].to_string();
    assert!(
        said(kernel).contains("the histogram was built in the kernel"),
        "the `hist`-only request did not say it was built in the kernel: {}",
        said(kernel)
    );
    assert!(
        !said(streamed).contains("built in the kernel"),
        "the streamed request says it was built in the kernel: {}",
        said(streamed)
    );
    for result in [kernel, streamed] {
        assert_eq!(
            losses(result)["dropped"].as_u64().unwrap_or(0),
            0,
            "the `{}` path dropped requests: {}",
            result["label"],
            losses(result)
        );
    }

    // Every streamed request: one of dd's, and its interval the device's
    // latency and a timer's slack, inside the bucket.
    let events = raw_events(streamed);
    assert_eq!(
        events.len() as u64,
        READS,
        "dd made {READS} direct reads of 4 KiB, and the stream carried {} requests",
        events.len()
    );
    let mut latencies: Vec<u64> = events
        .iter()
        .map(|attrs| attrs["latency_ns"].as_u64().expect("a latency"))
        .collect();
    latencies.sort_unstable();
    eprintln!(
        "completion_nsec {latency}: the stream measured {} to {} ns (median {}) over {READS} \
         requests, in [{low}, {high})",
        latencies[0],
        latencies[latencies.len() - 1],
        latencies[latencies.len() / 2]
    );
    assert!(
        latencies[0] >= latency,
        "a request completed in {} ns on a device that takes {latency}",
        latencies[0]
    );
    assert!(
        latencies[latencies.len() - 1] < high,
        "a request took {} ns on a device that takes {latency}, past the bucket's edge at \
         {high}",
        latencies[latencies.len() - 1]
    );

    // Both histograms: one bucket, every request in it, nothing elsewhere.
    let only = |result: &serde_json::Value| -> BTreeMap<u64, u64> {
        output_of(result, "hist")["series"]
            .as_array()
            .expect("a hist output carries its series")
            .iter()
            .flat_map(buckets_of)
            .fold(BTreeMap::new(), |mut all, (low, count)| {
                *all.entry(low).or_insert(0) += count;
                all
            })
    };
    let expected = BTreeMap::from([(low, READS)]);
    assert_eq!(only(streamed), expected, "the streamed histogram");
    assert_eq!(only(kernel), expected, "the histogram built in the kernel");
}

/// A file event names the filesystem's superblock device, and `io.bytes`
/// counts the block devices under it: `blk_devs` is where the two meet, each
/// device spelled as `io.bytes` keys its series.
///
/// The reference is the test's own walk from the mount source through `/sys`
/// ([`backing_device`]), which shares no code with the collector's: on btrfs
/// the superblock's `0:28` names nothing, and both have to reach the disk
/// (`dev:259+0`) through the partition the mount names.
#[test]
fn a_file_event_names_the_block_devices_its_filesystem_is_on() {
    if let Some(reason) = no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let target = scratch("blk-devs.bin");
    std::fs::create_dir_all(target.parent().expect("a parent")).expect("the scratch directory");
    std::fs::write(&target, b"").expect("the target exists before the run");
    let device = match backing_device(&target) {
        Ok(device) => device,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let (superblock, _) = mounted_device(&target);
    let output = run(&[
        "io.fs",
        "-m",
        "trace",
        "-e",
        "raw",
        "--format",
        "json",
        "--",
        "dd",
        "if=/dev/zero",
        &format!("of={}", target.display()),
        "bs=64k",
        "count=16",
        "conv=fsync",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);
    let result = &document["results"][0];
    let on_it: Vec<_> = raw_with_keys(result)
        .into_iter()
        .filter(|(event, _)| file_key(event).is_some_and(|(dev, _)| dev == superblock))
        .collect();
    assert!(
        !on_it.is_empty(),
        "dd's writes reached no file on the scratch filesystem"
    );
    for (_, attrs) in &on_it {
        let under = attr_text(attrs, "blk_devs");
        assert!(
            under.split(',').any(|key| key == device.key),
            "a file on {} carries blk_devs `{under}`, which does not hold {}, the disk its \
             mount source is on",
            device.leaf,
            device.key
        );
    }
    let _ = std::fs::remove_file(&target);
}

// ---------------------------------------------------------------------------
// The launched program's descendants

/// Forks, and runs `DIRECT_READS` in the child alone: the parent submits
/// nothing, the way `fio` without `thread` forks its job and waits.
const FORKED_DIRECT_READS: &str = r#"
import os, sys
reads = sys.argv[1]
child = os.fork()
if child == 0:
    sys.argv = [reads] + sys.argv[2:]
    exec(compile(open(reads).read(), reads, "exec"))
    os._exit(0)
_, status = os.waitpid(child, 0)
sys.exit(os.waitstatus_to_exitcode(status))
"#;

/// A launched program's forked child is followed: its reads are the run's.
///
/// `omn trace io.latency -- fio <job>` without `thread` reported 0 requests
/// for 28 807 reads fio completed, and "29678 block_rq_complete events had no
/// opening", because the programs compared the submitter with the launched
/// process alone (`docs/scenarios/io.md`, defect 25). Here the launched
/// process forks and the child makes every read, so a filter on the launched
/// process alone keeps none of them; the child's are counted one for one,
/// under the child's pid.
#[test]
fn a_launched_programs_forked_child_is_followed() {
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = lacks_here("io.latency", "bpf:raw_tp") {
        eprintln!("skipped: {reason:?}");
        return;
    }
    const READS: usize = 1024;
    let path = direct_read_file("io-forked-reads.bin", READS);
    let reads = script("io-forked-reads.py", DIRECT_READS);
    let forks = script("io-forked-parent.py", FORKED_DIRECT_READS);
    let account_path = scratch("io-forked-reads.json");
    let _ = std::fs::remove_file(&account_path);
    let output = run(&[
        "io.latency",
        "-m",
        "trace",
        "-e",
        "raw",
        "--format",
        "json",
        "--",
        "python3",
        forks.to_str().expect("a path"),
        reads.to_str().expect("a path"),
        path.to_str().expect("a path"),
        &READS.to_string(),
        account_path.to_str().expect("a path"),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let did = account(&account_path);
    if let Some(error) = did["refused"].as_str() {
        eprintln!("skipped: {} refuses O_DIRECT ({error})", path.display());
        return;
    }
    if did["resident"].as_u64() != Some(0) {
        eprintln!("skipped: the file stayed in the page cache: {did}");
        return;
    }
    let child = did["pid"].as_u64().expect("the child's pid");
    let document = json(&output);
    let result = result(&document, "io.latency");
    let found = raw_events(result)
        .into_iter()
        .filter(|attrs| attrs["pid"].as_u64() == Some(child))
        .filter(|attrs| {
            attrs["rwbs"]
                .as_str()
                .is_some_and(|rwbs| rwbs.starts_with('R'))
        })
        .count();
    let dropped = losses(result)["dropped"].as_u64().unwrap_or(0) as usize;
    assert!(
        found <= READS,
        "{found} requests from the child for {READS} reads: something is counted twice"
    );
    assert!(
        found + dropped >= READS,
        "the child made {READS} direct reads and the run kept {found} of its requests, \
         reporting {dropped} lost: the launched program's child is not followed. {}",
        stderr(&output)
    );
}

/// Forks, and makes `N` `pread64` calls in the child alone, then reports the
/// child's pid: the parent makes none.
const FORKED_PREADS: &str = r#"
import json, os, sys
path, count, account = sys.argv[1], int(sys.argv[2]), sys.argv[3]
child = os.fork()
if child == 0:
    fd = os.open(path, os.O_RDONLY)
    for _ in range(count):
        os.pread(fd, 512, 0)
    os.close(fd)
    with open(account, "w") as out:
        json.dump({"pid": os.getpid()}, out)
    os._exit(0)
_, status = os.waitpid(child, 0)
sys.exit(os.waitstatus_to_exitcode(status))
"#;

/// A launched program's forked child is followed by the timed-pair sources:
/// its calls are the run's.
///
/// `omn trace io.syscall -- ./script.sh` timed the shell's own calls and none
/// of the programs it ran: the entry programs compared the caller with the
/// launched process alone, where every `sched` source follows the processes
/// it starts (`STATE.md`, item 1, `src/bpf/fentry.rs`). Ground truth is the
/// child's own count of the `pread64` calls it made, which the parent never
/// makes; they are counted one for one, under the child's pid.
#[test]
fn a_timed_pair_follows_a_launched_programs_forked_child() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = lacks_here("io.syscall", "bpf:fentry") {
        eprintln!("skipped: {reason:?}");
        return;
    }
    const PREADS: u64 = 200;
    let forks = script("io-forked-preads.py", FORKED_PREADS);
    let account_path = scratch("io-forked-preads.json");
    let _ = std::fs::remove_file(&account_path);
    let output = run(&[
        "io.syscall",
        "-m",
        "count",
        "-e",
        "raw",
        "--format",
        "json",
        "--",
        "python3",
        forks.to_str().expect("a path"),
        forks.to_str().expect("a path"),
        &PREADS.to_string(),
        account_path.to_str().expect("a path"),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let child = account(&account_path)["pid"]
        .as_u64()
        .expect("the child's pid");
    let document = json(&output);
    let result = result(&document, "io.syscall");
    let found: u64 = raw_events(result)
        .into_iter()
        .filter(|attrs| attrs["pid"].as_u64() == Some(child))
        .filter(|attrs| attrs["syscall"].as_str() == Some("pread64"))
        .map(|attrs| attrs["count"].as_u64().unwrap_or(0))
        .sum();
    let dropped = losses(result)["dropped"].as_u64().unwrap_or(0);
    assert!(
        found <= PREADS,
        "{found} pread64 calls from the child for {PREADS}: something is counted twice"
    );
    assert!(
        found + dropped >= PREADS,
        "the child made {PREADS} pread64 calls and the run kept {found}, reporting {dropped} \
         lost: the launched program's child is not followed. {}",
        stderr(&output)
    );
}

/// `io.vfs` counts every `vfs_write`, of any file type: a program that writes
/// 20 000 times to `/dev/null` from a forked child is found 20 000 times, and
/// the parent, which writes nothing, adds nothing. `/dev/null` is not a
/// regular file, so `io.fs` would count none of them.
#[test]
fn every_vfs_write_of_a_forked_child_is_counted() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = lacks_here("io.vfs", "bpf:fentry") {
        eprintln!("skipped: {reason:?}");
        return;
    }
    const WRITES: u64 = 20_000;
    let workload = script(
        "io-vfs-writes.py",
        r#"
import os, sys
count = int(sys.argv[1])
child = os.fork()
if child == 0:
    fd = os.open(os.devnull, os.O_WRONLY)
    for _ in range(count):
        os.write(fd, b"x")
    os._exit(0)
os.waitpid(child, 0)
"#,
    );
    let output = run(&[
        "io.vfs",
        "-m",
        "count",
        "-e",
        "raw",
        "--filter",
        "op == \"vfs_write\"",
        "--format",
        "json",
        "--",
        "python3",
        workload.to_str().expect("a path"),
        &WRITES.to_string(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);
    let result = result(&document, "io.vfs");
    let events = raw_events(result);
    assert_eq!(events.len(), 1, "one counted event: {events:?}");
    let calls = events[0]["calls"].as_u64().expect("calls");
    let dropped = losses(result)["dropped"].as_u64().unwrap_or(0);
    // Python writes nothing on its way in or out; a few calls of slack for
    // an interpreter that one day does.
    assert!(
        calls + dropped >= WRITES && calls <= WRITES + 8,
        "{WRITES} writes from a forked child, {calls} counted, {dropped} reported lost"
    );
    assert_eq!(events[0]["vfs_write"].as_u64(), Some(calls));
}

/// `io.vfs` counts a sync once, whichever functions the kernel routes it
/// through: twenty `fsync`s and three `msync`s are twenty `do_fsync`s and three
/// `vfs_fsync_range`s. On this machine `fsync(2)` never reaches the range
/// function, which the compiler inlined into `do_fsync`; on a kernel that did
/// not, it reaches both, and the mark `do_fsync` leaves on its thread is what
/// keeps the second count out. Either way an `msync`, which passes no
/// `do_fsync`, is counted where it arrives.
#[test]
fn io_vfs_counts_each_sync_once() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = lacks_here("io.vfs", "bpf:fentry") {
        eprintln!("skipped: {reason:?}");
        return;
    }
    let path = synced_file("io-vfs-syncs.bin", 1 << 16);
    let workload = script(
        "io-vfs-syncs.py",
        r#"
import mmap, os, sys
fd = os.open(sys.argv[1], os.O_RDWR)
for _ in range(20):
    os.pwrite(fd, b"x", 0)
    os.fsync(fd)
mapping = mmap.mmap(fd, 4096)
for index in range(3):
    mapping[index] = 0x79
    mapping.flush()
mapping.close()
os.close(fd)
"#,
    );
    let output = run(&[
        "io.vfs",
        "-m",
        "count",
        "-e",
        "raw",
        "--format",
        "json",
        "--",
        "python3",
        workload.to_str().expect("a path"),
        path.to_str().expect("a path"),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);
    let result = result(&document, "io.vfs");
    let events = raw_events(result);
    assert_eq!(events.len(), 1, "one counted event: {events:?}");
    // The kernel's skipped runs are counted machine-wide, and under load a
    // few are reported: each is a call that may be one of these, so a count
    // short by at most what was reported is not a defect; one over is.
    let dropped = losses(result)["dropped"].as_u64().unwrap_or(0);
    let Some(by_descriptor) = events[0]["do_fsync"].as_u64() else {
        eprintln!("skipped: this kernel has no `do_fsync` to count: {events:?}");
        return;
    };
    let ranged = events[0]["vfs_fsync_range"].as_u64().unwrap_or(0);
    assert!(
        by_descriptor <= 20 && by_descriptor + dropped >= 20,
        "twenty fsyncs, {by_descriptor} counted at `do_fsync`, {dropped} reported lost"
    );
    assert!(
        ranged <= 3 && ranged + dropped >= 3,
        "three msyncs and twenty fsyncs, and `vfs_fsync_range` counted {ranged} with \
         {dropped} reported lost: an fsync counted twice, or an msync not at all"
    );
}

/// `io.fsync` follows the launched program's forked children: a parent that
/// syncs nothing and a child that syncs three files is three syncs, under the
/// child's pid. The filter compared the launched process alone before.
#[test]
fn the_syncs_of_a_forked_child_are_timed() {
    if let Some(reason) = no_bpf_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    if let Some(reason) = lacks_here("io.fsync", "bpf:fentry") {
        eprintln!("skipped: {reason:?}");
        return;
    }
    let directory = scratch("io-forked-syncs");
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let workload = script(
        "io-forked-syncs.py",
        r#"
import json, os, sys
directory, account = sys.argv[1], sys.argv[2]
child = os.fork()
if child == 0:
    for index in range(3):
        fd = os.open(os.path.join(directory, f"sync-{index}"), os.O_WRONLY | os.O_CREAT, 0o644)
        os.write(fd, b"x" * 4096)
        os.fsync(fd)
        os.close(fd)
    json.dump({"pid": os.getpid()}, open(account, "w"))
    os._exit(0)
os.waitpid(child, 0)
"#,
    );
    let account_path = scratch("io-forked-syncs.json");
    let _ = std::fs::remove_file(&account_path);
    let output = run(&[
        "io.fsync",
        "-m",
        "trace",
        "-e",
        "raw",
        "--format",
        "json",
        "--",
        "python3",
        workload.to_str().expect("a path"),
        directory.to_str().expect("a path"),
        account_path.to_str().expect("a path"),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let child = account(&account_path)["pid"]
        .as_u64()
        .expect("the child's pid");
    let document = json(&output);
    let result = result(&document, "io.fsync");
    let syncs = raw_events(result)
        .into_iter()
        .filter(|attrs| attrs["pid"].as_u64() == Some(child))
        .count();
    let dropped = losses(result)["dropped"].as_u64().unwrap_or(0) as usize;
    assert!(
        syncs <= 3 && syncs + dropped >= 3,
        "the child synced 3 files, {syncs} were timed under its pid and {dropped} reported \
         lost: {}",
        stderr(&output)
    );
}
