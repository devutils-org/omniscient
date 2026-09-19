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
/// Every source of this family is a counter or a gauge, and the two do not
/// overlap: a volume has no depth to take a snapshot of, and a queue depth
/// counts over no window.
fn something_this_source_does(source: &str) -> (&'static str, &'static str) {
    match source {
        "io.inflight" | "io.pressure" => ("snapshot", "raw"),
        _ => ("count", "summary"),
    }
}

/// The five sources of this family that have a collector in this build.
fn collected_sources() -> [&'static str; 5] {
    [
        "io.bytes",
        "io.iops",
        "io.merges",
        "io.inflight",
        "io.pressure",
    ]
}

/// A mode and an emit are compatible in the abstract or not at all, and the
/// hint that says which does not consult the source it is talking about.
///
/// This is the sweep's own finding, kept as a test because the two messages
/// contradicting each other is the part that matters: one sends a reader to
/// `raw`, the other refuses `raw`. Both are asserted here so that the day the
/// hint is narrowed to what the source declares, this test says so.
#[test]
fn the_mode_hint_offers_an_emit_the_source_refuses() {
    let by_mode = run(&["io.bytes", "--system", "-m", "count", "-e", "timeline"]);
    assert_eq!(by_mode.status.code(), Some(EXIT_REJECTED));
    let hint = stderr(&by_mode);
    assert!(
        hint.contains("in `count` mode the valid emits are"),
        "{hint}"
    );

    let by_source = run(&["io.bytes", "--system", "-m", "count", "-e", "raw"]);
    assert_eq!(by_source.status.code(), Some(EXIT_REJECTED));
    let refusal = stderr(&by_source);
    assert!(refusal.contains("cannot emit `raw`"), "{refusal}");

    // See `docs/scenarios/io.md`, "Defects found": the first message offers
    // `raw` and the second refuses it, so following the hint leads to a second
    // refusal.
    assert!(
        hint.contains("raw"),
        "the mode hint no longer offers `raw`; if it now intersects with what the source \
         declares, this scenario has been fixed and should be removed:\n{hint}"
    );
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
    assert!(
        message.contains("bpf:tp"),
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
/// This family has one implemented backend per source, so there is no second
/// road to compare a number against -- the comparison is with the refusal
/// instead, which is what `BRIEF.md` asks for where a chain has one
/// implementation and others declared. `docs/scenarios/io.md` says what a
/// comparison would look like once the eBPF backends land.
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
            if backend.id == "fs:proc" {
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
        let full = attr(event, "full_stall_ratio");
        let stalled = attr(event, "stalled_ns");
        let interval = attr(event, "interval_ns");
        assert!(
            (0.0..=1.0).contains(&some),
            "a share of a window is between nothing and all of it: {event}"
        );
        assert!(
            full <= some + f64::EPSILON,
            "`full` is a subset of `some` and cannot exceed it: {event}"
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
            "650ms",
            "-e",
            "timeline",
            "--format",
            "json",
        ],
    );

    let step = Duration::from_millis(150);
    std::thread::sleep(Duration::from_millis(250));
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

    // 4. A device that appears mid-run. It is named -- see
    // `docs/scenarios/io.md`, "Defects found": it is *not* counted, so a run
    // where this is the only gap still reports itself complete. The count is
    // deliberately not asserted here, so that the day it is counted this
    // scenario keeps passing and the document is what changes.
    assert!(
        said.contains("appeared during the run"),
        "a device with no earlier reading loses its first interval, and has to say so:\n{said}"
    );
}

/// What a reader at a terminal is told when a line of the file was left out of
/// the total.
///
/// The counted categories are five and the rendered headline names three, so a
/// run whose only loss is `undecoded` -- this family's ordinary case, and how
/// `/proc/diskstats` says a device could not be classified -- announces `LOSS`
/// followed by three zeros. What is asserted here is the part that holds: the
/// run does not claim to be complete, and the reason reaches the text. The
/// headline itself is `docs/scenarios/io.md`, "Defects found" 9.
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
    let unbuilt = run(&["-f", &session("io-per-process.yml")]);
    assert_eq!(
        unbuilt.status.code(),
        Some(EXIT_NOT_IMPLEMENTED),
        "{}",
        stderr(&unbuilt)
    );
    let message = stderr(&unbuilt);
    assert!(
        message.contains("no collector is written for `io.bytes` on backend `bpf:tp`"),
        "{message}"
    );
    assert!(
        message.contains("`fs:proc` was passed over"),
        "the reader needs to know why the readable, free backend was not used:\n{message}"
    );
    assert!(
        stdout(&unbuilt).is_empty(),
        "a machine-wide total must never be produced under one process's name:\n{}",
        stdout(&unbuilt)
    );

    // A mode the catalog declares, resolved onto a backend that cannot serve
    // it. The message is right and names the mechanism that would; the exit
    // code is 1, which is the code for a run that failed -- see
    // `docs/scenarios/io.md`, "Defects found". Only the part that is true is
    // asserted, so that fixing the code does not have to fight this test.
    let at_attach = run(&[
        "io.bytes", "--system", "-m", "trace", "-e", "topk", "-d", "1s",
    ]);
    assert_ne!(at_attach.status.code(), Some(0));
    let message = stderr(&at_attach);
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
    // The exit code here is 2, the code for a request that cannot mean
    // anything, and this is a machine that cannot serve one that can --
    // `docs/scenarios/io.md`, "Defects found". Asserting only that it failed
    // keeps this scenario honest either way.
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
