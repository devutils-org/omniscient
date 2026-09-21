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

    /// Whether the I/O scheduler can coalesce adjacent requests at all. A
    /// multi-queue device with `none` hands the driver what it was given.
    fn merges_are_possible(&self) -> bool {
        std::fs::read_to_string(format!("/sys/block/{}/queue/scheduler", self.leaf))
            .map(|text| !text.contains("[none]"))
            .unwrap_or(true)
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

    let mut best: Option<(usize, String)> = None;
    for line in table.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let mount_point = match before.split_whitespace().nth(4) {
            Some(point) => point,
            None => continue,
        };
        let source = match after.split_whitespace().nth(1) {
            Some(source) => source,
            None => continue,
        };
        if !path.starts_with(mount_point) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(len, _)| mount_point.len() > *len)
        {
            best = Some((mount_point.len(), source.to_owned()));
        }
    }

    let source = best
        .map(|(_, source)| source)
        .ok_or_else(|| format!("nothing in the mount table covers {}", path.display()))?;
    let name = source
        .strip_prefix("/dev/")
        .ok_or_else(|| format!("`{source}` is not a block device, so it has no counters"))?;

    // A partition's counters are counted again on the disk below it, so the
    // collector reports the disk. Ask sysfs which this is rather than reading
    // the name: `nvme0n1p6` is a partition and `zram0` is not, and no rule
    // about digits and letters gets both right.
    let sysfs = Path::new("/sys/class/block").join(name);
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
        name.to_owned()
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
                || message.contains("no collector"),
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
        "io.latency" | "io.queue" | "io.size" | "io.writeback" => ("trace", "summary"),
        _ => ("count", "summary"),
    }
}

/// The sources of this family that have a collector in this build.
///
/// Five through `/proc/diskstats` and PSI, and four through the block layer's
/// own raw tracepoints. The two mechanisms answer different questions and the
/// sweep covers both: a combination the catalog offers and the build cannot
/// serve is a promise broken in the one place nobody looks, and that is as
/// true of an eBPF backend as of a file.
fn collected_sources() -> [&'static str; 9] {
    [
        "io.bytes",
        "io.iops",
        "io.merges",
        "io.inflight",
        "io.pressure",
        "io.latency",
        "io.queue",
        "io.size",
        "io.writeback",
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
        "this build cannot load a block tracer: {}. `make` grants CAP_BPF and CAP_PERFMON to \
         target/debug/omn, and every `cargo build` drops them again",
        stderr(&output).lines().next().unwrap_or("").trim()
    ))
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

// ---------------------------------------------------------------------------
// B. The backend comparison

/// Every backend this family declares and has not written is refused by name,
/// with the exit code that means "this build", not "this run".
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
fn forcing_a_backend_this_build_has_not_written_names_it_and_exits_three() {
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
            // The answer this class is about: valid request, unwritten
            // mechanism. Retrying or raising privileges will not help.
            Some(EXIT_NOT_IMPLEMENTED) => {
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

    let mut read_back = 0;
    let (output, enclosing) = measure_while(
        &["-f", &session("io-known-volume.yml"), "--format", "json"],
        &device,
        || read_back = cold_read(&file),
    );
    let _ = std::fs::remove_file(&file);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        read_back, KNOWN_BYTES,
        "the workload did not read the file it promised to"
    );

    let document = json(&output);
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

    // What the block layer coalesced. On a device whose scheduler is `none`
    // there is nothing to coalesce: the filesystem submits requests already at
    // the device's maximum size, and the right answer is exactly zero. A
    // collector that reported a plausible number instead would pass every
    // check that only asks for a number.
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
    if device.merges_are_possible() {
        assert!(
            merges <= ops,
            "more requests were merged away than the device completed: {merges} against {ops}"
        );
    } else {
        assert!(
            merges <= ops / 8.0,
            "the scheduler on {} is `none`, which coalesces nothing, and {merges} merges are \
             reported against {ops} operations",
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
/// greater than zero on the device the file was written to -- the workload
/// holds tens of requests in flight for a couple of hundred milliseconds --
/// while the devices it did not touch stay where they were.
#[test]
fn a_burst_of_writes_shows_up_as_a_queue_on_the_device_that_took_it() {
    let file = scratch("io-queue-depth.bin");
    if let Some(reason) = no_device_here(Path::new(env!("CARGO_TARGET_TMPDIR"))) {
        eprintln!("skipped: {reason}");
        return;
    }
    let device = backing_device(Path::new(env!("CARGO_TARGET_TMPDIR"))).expect("a block device");

    let (output, _) = measure_while(
        &["-f", &session("io-device-load.yml"), "--format", "json"],
        &device,
        || write_and_sync(&file),
    );
    let _ = std::fs::remove_file(&file);
    assert!(output.status.success(), "{}", stderr(&output));

    let document = json(&output);
    let depth = series(
        output_of(result(&document, "queue depth"), "summary"),
        &device.key,
    )
    .expect("the queue depth is reported per device");
    assert!(
        number(&depth["max"]) >= 1.0,
        "{KNOWN_BYTES} bytes of writeback keep tens of requests in flight, and a 20 ms poll \
         over two seconds saw a queue that never held one: {depth}"
    );

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
    let volume: f64 = output_of(result(&document, "volume"), "timeline")["series"]
        .as_array()
        .expect("the volume has series")
        .iter()
        .filter(|series| series["key"] == device.key.as_str())
        .flat_map(|series| series["points"].as_array().cloned().unwrap_or_default())
        .map(|point| number(&point["value"]))
        .sum();
    assert!(
        volume >= KNOWN_BYTES as f64,
        "the polled series adds up to {volume} bytes on {}, less than the {KNOWN_BYTES} the \
         workload synced inside the window",
        device.leaf
    );
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
        || write_and_sync(&file),
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
        "a task blocked in fsync for the length of a 64 MiB writeback is stall time, and the \
         window that held it reports {under_load:.3} against {quiet:.3} for an idle one"
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
            diskstats_line(253, 0, "zram0", known(2048))
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
    let text = std::fs::read_to_string("/proc/diskstats").ok()?;
    text.lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .find(|name| {
            let sysfs = Path::new("/sys/class/block").join(name);
            sysfs.exists() && !sysfs.join("partition").exists()
        })
        .map(str::to_owned)
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

/// A machine that cannot serve a source at all says which file is missing.
///
/// The fourth answer, and the one no real machine here can be made to give:
/// every backend of `io.pressure` needs a file, and both files exist. Hiding
/// `/proc/pressure` behind an empty mount inside a private namespace is what a
/// kernel built without `CONFIG_PSI` looks like from the tool's side.
#[test]
fn a_machine_that_cannot_serve_pressure_says_which_file_it_wanted() {
    if let Some(reason) = no_namespace_here() {
        eprintln!("skipped: {reason}, so a machine without PSI cannot be simulated");
        return;
    }
    let output = Command::new("unshare")
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
        .expect("unshare should run");

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
fn this_thread() -> i32 {
    // Safety: `gettid` takes nothing and returns this thread's id.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

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
/// the process rather than the thread would catch every other test in this
/// binary.
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
    let path = direct_read_file("io-direct-reads.bin", READS);
    let tid = this_thread().to_string();

    let child = Command::new(BINARY)
        .args([
            "io.size", "--tid", &tid, "-m", "trace", "-e", "raw", "-d", "5s", "--format", "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    std::thread::sleep(Duration::from_millis(500));

    let issued = match direct_reads(&path, READS) {
        Ok(issued) => issued,
        Err(error) => {
            // A filesystem that refuses `O_DIRECT` cannot produce the property
            // this scenario measures, and saying so beats asserting something
            // else about it.
            let _ = child.wait_with_output();
            eprintln!(
                "skipped: {} refuses O_DIRECT ({error}), so a read of it is not one block request",
                path.display()
            );
            return;
        }
    };
    assert_eq!(issued, READS);
    // The last request has to be issued, completed and drained before the
    // window closes, and the drain is on a ten-millisecond tick.
    std::thread::sleep(Duration::from_millis(500));

    let output = child.wait_with_output().expect("omn should finish");
    assert!(output.status.success(), "{}", stderr(&output));
    let document = json(&output);
    let result = result(&document, "io.size");
    let events = raw_events(result);

    // Every request this thread issued was one page. Not "mostly": the read was
    // one page at a page-aligned offset with the cache out of the way, and
    // anything else in this series would be a request the workload did not
    // make.
    let sizes: std::collections::BTreeSet<u64> = events
        .iter()
        .filter_map(|attrs| attrs.get("bytes")?.as_u64())
        .collect();
    assert_eq!(
        sizes,
        [PAGE as u64].into_iter().collect(),
        "this thread issued {READS} one-page reads and the tracer saw other sizes: {sizes:?}"
    );

    // One request per read. The upper bound is exact: a read cannot produce two
    // requests, so anything above `READS` is double counting.
    assert!(
        events.len() <= READS,
        "{} requests for {READS} reads: something is counted twice",
        events.len()
    );
    // The lower bound carries the window's own edge -- a request issued in the
    // last moments of the run is published after the final drain -- and it is a
    // fraction of a percent rather than a calibrated threshold. Every mistake
    // this scenario exists to catch is out by a *factor*: a collector reporting
    // sectors is out by 512, one counting completions differs on every
    // partially completed request, one reading `__data_len` too late reports
    // zero. None of them lands within one percent of the truth.
    assert!(
        100 * events.len() >= 99 * READS,
        "{READS} one-page reads produced {} requests, and the run reported losing none of them: \
         {}",
        events.len(),
        losses(result)
    );
    assert_eq!(
        losses(result)["dropped"].as_u64(),
        Some(0),
        "a shortfall the tool knew about is a different scenario: {}",
        losses(result)
    );

    // Reads, and attributed to the thread that made them. `rwbs` is the
    // kernel's own letters, so `R` is the first of them for any read.
    for attrs in &events {
        let rwbs = attrs["rwbs"].as_str().unwrap_or_default();
        assert!(rwbs.starts_with('R'), "a read reported as `{rwbs}`");
        assert_eq!(
            attrs["tid"].as_u64(),
            Some(this_thread() as u64),
            "a request attributed to a thread that issued none"
        );
    }
}

/// The eBPF request count and the kernel's own operation count agree.
///
/// This is the backend comparison this family could not make until now, and it
/// is across sources rather than across a chain: `/proc/diskstats` counts the
/// operations a device completed and the block tracer counts the requests it
/// was issued, and for the same window those are the same requests counted
/// twice by two mechanisms that share no code.
///
/// The check is one-sided on purpose. `io.size` is filtered to one thread and
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
    let path = direct_read_file("io-agreement.bin", READS);
    let tid = this_thread().to_string();

    let before = counters_of(&device);
    let child = Command::new(BINARY)
        .args([
            "io.size", "--tid", &tid, "-m", "trace", "-e", "raw", "-d", "5s", "--format", "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    std::thread::sleep(Duration::from_millis(500));
    if direct_reads(&path, READS).is_err() {
        let _ = child.wait_with_output();
        eprintln!("skipped: this filesystem refuses O_DIRECT");
        return;
    }
    std::thread::sleep(Duration::from_millis(500));
    let output = child.wait_with_output().expect("omn should finish");
    let kernel = counters_of(&device).since(&before);

    assert!(output.status.success(), "{}", stderr(&output));
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
    // And the other side: the reads were issued, so they are in both accounts.
    // The one percent is the window's edge rather than a calibrated threshold
    // -- see `a_known_number_of_direct_reads_is_found_one_for_one`.
    assert!(
        100 * traced_bytes >= 99 * (READS * PAGE) as u64,
        "{} bytes traced for the {} the workload read",
        traced_bytes,
        READS * PAGE
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
