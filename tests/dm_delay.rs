//! `dm-delay`: a device whose latency is set from outside, and the tool held
//! to it.
//!
//! Phase 4's last injection. Every other `io` scenario measures a latency the
//! machine chose; this one chooses it. A loop device over a file in the build
//! directory, a device-mapper `delay` target over the loop device holding
//! every read and every write for `DELAY_MS`, and a program that issues a
//! known number of direct I/Os against the mapped device one at a time. The
//! ground truth is the table the test wrote -- `DELAY_MS` per I/O, so at least
//! `count * DELAY_MS` in all -- and the program's own clock around each call.
//!
//! Where the delay shows is the point. `dm-delay` holds the bio *above* the
//! request layer: it is a bio-based target, so no block request is ever made
//! on the mapped device, and what reaches the loop device below arrives late
//! and is served at the loop device's own speed. So `io.syscall` (what the
//! program waited for) has the delay in every call, `io.latency` (what the
//! device under it did) has none of it, and the difference between the two
//! layers -- the vertical attribution -- is the configured delay times the
//! number of I/Os.
//!
//! It needs root for two commands, `losetup` and `dmsetup`, through `sudo -n`
//! (a sudoers line, `docs/scenarios/io.md`), and a udev rule that gives
//! `omn-*` mapped devices to the user. Without either it skips, saying which
//! piece is missing; with them it always removes the mapped device and the
//! loop device, on a panic too.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// The delay the table sets on every read and every write.
const DELAY_MS: u64 = 20;
const DELAY_NS: u64 = DELAY_MS * 1_000_000;

/// One tick of the kernel's clock, which is how finely `dm-delay` keeps time:
/// it stamps each bio `jiffies + msecs_to_jiffies(delay)` and releases it on
/// the first tick at or past that, so a bio queued late in a tick waits up to
/// one tick less than the table says (19.91 ms of 20 here, at HZ 1000). Read
/// from the kernel's own configuration, and the coarsest the kernel allows
/// (HZ 100) when the configuration is not readable.
fn tick_ns() -> u64 {
    let hz = Command::new("zcat")
        .arg("/proc/config.gz")
        .output()
        .ok()
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.strip_prefix("CONFIG_HZ=")?.trim().parse::<u64>().ok())
        })
        .unwrap_or(100);
    1_000_000_000 / hz.max(1)
}

/// How many reads and how many writes the program issues.
const READS: u64 = 12;
const WRITES: u64 = 12;

/// The size of the file behind the loop device.
const IMAGE_BYTES: u64 = 16 << 20;

/// What the test set up, torn down when it is dropped -- at the end of the
/// test, and on the unwind of a panic inside it.
struct Delayed {
    name: String,
    loop_device: Option<String>,
    image: PathBuf,
    mapped: bool,
}

impl Drop for Delayed {
    fn drop(&mut self) {
        if self.mapped {
            // A mapped device still open by a dying child refuses removal for
            // a moment; `--retry` is dmsetup's own answer to that.
            let removed = sudo(&["dmsetup", "remove", "--retry", &self.name]);
            if !removed.status.success() {
                eprintln!(
                    "warning: `sudo -n dmsetup remove {}` failed: {}",
                    self.name,
                    String::from_utf8_lossy(&removed.stderr).trim()
                );
            }
        }
        if let Some(loop_device) = &self.loop_device {
            let detached = sudo(&["losetup", "-d", loop_device]);
            if !detached.status.success() {
                eprintln!(
                    "warning: `sudo -n losetup -d {loop_device}` failed: {}",
                    String::from_utf8_lossy(&detached.stderr).trim()
                );
            }
        }
        let _ = std::fs::remove_file(&self.image);
    }
}

fn sudo(args: &[&str]) -> Output {
    Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("sudo could not be run: {error}"))
}

/// The loop device and the delay target over it, or why this machine cannot
/// have them -- the exact command and what it answered.
fn set_up() -> Result<Delayed, String> {
    let name = format!("omn-delay-{}", std::process::id());
    let image = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.img"));
    std::fs::File::create(&image)
        .and_then(|file| file.set_len(IMAGE_BYTES))
        .map_err(|error| format!("{}: {error}", image.display()))?;
    let mut delayed = Delayed {
        name,
        loop_device: None,
        image,
        mapped: false,
    };

    let attached = sudo(&[
        "losetup",
        "--show",
        "-f",
        delayed.image.to_str().expect("a UTF-8 path"),
    ]);
    if !attached.status.success() {
        return Err(format!(
            "`sudo -n losetup --show -f {}` failed ({}): {}; the scenario needs \
             `<user> ALL=(root) NOPASSWD: /usr/bin/losetup, /usr/bin/dmsetup` in sudoers",
            delayed.image.display(),
            attached.status,
            String::from_utf8_lossy(&attached.stderr).trim()
        ));
    }
    let loop_device = String::from_utf8_lossy(&attached.stdout).trim().to_owned();
    delayed.loop_device = Some(loop_device.clone());

    // `delay <device> <offset> <read delay>`: with no write half the write
    // delay is the read one, so both are held for DELAY_MS.
    let table = format!("0 {} delay {loop_device} 0 {DELAY_MS}", IMAGE_BYTES / 512);
    let created = Command::new("sudo")
        .args(["-n", "dmsetup", "create", &delayed.name, "--table", &table])
        .output()
        .map_err(|error| format!("sudo could not be run: {error}"))?;
    if !created.status.success() {
        let said = String::from_utf8_lossy(&created.stderr).trim().to_owned();
        let missing = if said.contains("password") || said.contains("not allowed") {
            "`sudo -n dmsetup` is not granted (`/usr/bin/dmsetup` in sudoers)"
        } else {
            "the `delay` target could not be created: dm-delay is not loadable here \
             (`modinfo dm_delay`)"
        };
        return Err(format!(
            "{missing}: `sudo -n dmsetup create {} --table '{table}'` failed ({}): {said}",
            delayed.name, created.status
        ));
    }
    delayed.mapped = true;

    // udev gives the node to the user a moment after it exists.
    let node = delayed.node();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
        {
            Ok(_) => break,
            Err(error) if Instant::now() > deadline => {
                return Err(format!(
                    "{node} cannot be opened read-write by this user ({error}): the scenario \
                     needs a udev rule giving `omn-*` mapped devices to the user, \
                     `SUBSYSTEM==\"block\", ENV{{DM_NAME}}==\"omn-*\", OWNER=\"<user>\", \
                     MODE=\"0660\"`"
                ));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    Ok(delayed)
}

impl Delayed {
    fn node(&self) -> String {
        format!("/dev/mapper/{}", self.name)
    }

    /// `dev:<major>+<minor>`, the tool's key for the loop device.
    fn loop_key(&self) -> String {
        let name = Path::new(self.loop_device.as_deref().expect("a loop device"))
            .file_name()
            .expect("a device name")
            .to_string_lossy()
            .into_owned();
        let numbers = std::fs::read_to_string(format!("/sys/class/block/{name}/dev"))
            .expect("the loop device's numbers");
        let (major, minor) = numbers.trim().split_once(':').expect("major:minor");
        format!("dev:{major}+{minor}")
    }
}

/// Reads then writes one 4 KiB page at a time, straight to the device, each
/// timed by the program's own monotonic clock, and writes its account.
const PROGRAM: &str = r#"
import ctypes, json, mmap, os, sys, time
libc = ctypes.CDLL(None, use_errno=True)
libc.pread.argtypes = libc.pwrite.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_long]
libc.pread.restype = libc.pwrite.restype = ctypes.c_ssize_t
path, reads, writes, out = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
fd = os.open(path, os.O_RDWR | os.O_DIRECT)
page = mmap.mmap(-1, 4096)
address = ctypes.addressof(ctypes.c_char.from_buffer(page))
calls = []
for op, count, call in (("read", reads, libc.pread), ("write", writes, libc.pwrite)):
    for index in range(count):
        began = time.monotonic_ns()
        done = call(fd, address, 4096, index * 65536)
        ended = time.monotonic_ns()
        if done != 4096:
            raise OSError(ctypes.get_errno(), f"{op} {index} returned {done}")
        calls.append({"op": op, "ns": ended - began})
json.dump({"fd": fd, "calls": calls}, open(out, "w"))
"#;

fn attr<'a>(event: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    event["attrs"]
        .as_array()?
        .iter()
        .find(|attr| attr["name"] == name)
        .map(|attr| &attr["value"])
}

fn result<'a>(document: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
    document["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("a result labelled `{label}`: {document}"))
}

fn raw(result: &serde_json::Value) -> &Vec<serde_json::Value> {
    result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .find(|output| output["emit"] == "raw")
        .and_then(|output| output["events"].as_array())
        .unwrap_or_else(|| panic!("a raw emit: {result}"))
}

/// A device that holds every I/O for 20 ms is measured holding it: the
/// program's calls each take at least the delay, `io.syscall` counts exactly
/// the calls the program made with at least `count * delay` inside them, the
/// loop device under the delay serves the same I/Os with none of it, and the
/// difference between the two layers is the configured delay times the count.
#[test]
fn a_device_delayed_from_outside_is_measured_with_the_delay_above_the_device() {
    // The tracers first: a machine that cannot attach them is told so before
    // anything is asked of root.
    if let Some(reason) = no_tracers_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let delayed = match set_up() {
        Ok(delayed) => delayed,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let program = directory.join(format!("{}.py", delayed.name));
    let account_path = directory.join(format!("{}.json", delayed.name));
    std::fs::write(&program, PROGRAM).expect("the program");
    let session = format!(
        "name: a device delayed from outside\n\
         requests:\n\
         \x20 - label: syscalls\n\
         \x20   source: io.syscall\n\
         \x20   target: {{launch: [python3, '{}', '{}', '{READS}', '{WRITES}', '{}']}}\n\
         \x20   mode: trace\n\
         \x20   emit: [raw]\n\
         \x20 - label: device\n\
         \x20   source: io.latency\n\
         \x20   target: system\n\
         \x20   mode: trace\n\
         \x20   emit: [raw]\n",
        program.display(),
        delayed.node(),
        account_path.display()
    );
    let session_path = directory.join(format!("{}.yml", delayed.name));
    std::fs::write(&session_path, session).expect("the session");
    let output = Command::new(BINARY)
        .args([
            "-f",
            session_path.to_str().expect("a path"),
            "--format",
            "json",
        ])
        .output()
        .expect("omn runs");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    let account: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&account_path).expect("the program wrote its account"),
    )
    .expect("the account parses");
    for path in [&program, &account_path, &session_path] {
        let _ = std::fs::remove_file(path);
    }

    // The program's own clock: every call waited for the delay the table set.
    // If not, the target is not delaying and nothing below means anything.
    let calls = account["calls"].as_array().expect("the calls");
    assert_eq!(calls.len() as u64, READS + WRITES, "{account}");
    let own_total: u64 = calls.iter().filter_map(|call| call["ns"].as_u64()).sum();
    let held = DELAY_NS - tick_ns();
    for call in calls {
        assert!(
            call["ns"].as_u64().unwrap_or(0) >= held,
            "a {} through the delay target took {} ns by the program's own clock, less than \
             the {DELAY_MS} ms its table sets, less one tick ({held} ns)",
            call["op"],
            call["ns"]
        );
    }

    // The syscall layer: exactly the program's calls on that descriptor, with
    // at least the delay in each and no more time than the program's clock
    // put around them.
    let fd = account["fd"].as_u64().expect("the descriptor");
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("the json output");
    // By the call as well as the descriptor: the interpreter reads its own
    // modules through `read` on the same descriptor numbers before the device
    // is opened, and the program's I/O is `pread64` and `pwrite64` alone.
    let rows: Vec<&serde_json::Value> = raw(result(&document, "syscalls"))
        .iter()
        .filter(|event| attr(event, "fd").and_then(|fd| fd.as_u64()) == Some(fd))
        .filter(|event| {
            attr(event, "syscall")
                .and_then(|name| name.as_str())
                .is_some_and(|name| name == "pread64" || name == "pwrite64")
        })
        .collect();
    let count_of = |call: &str| -> u64 {
        rows.iter()
            .filter(|event| attr(event, "syscall").and_then(|name| name.as_str()) == Some(call))
            .filter_map(|event| attr(event, "count")?.as_u64())
            .sum()
    };
    assert_eq!(
        (count_of("pread64"), count_of("pwrite64")),
        (READS, WRITES),
        "the program made {READS} preads and {WRITES} pwrites on descriptor {fd}: {rows:?}"
    );
    let syscall_total: u64 = rows
        .iter()
        .filter_map(|event| attr(event, "total_ns")?.as_u64())
        .sum();
    let floor = (READS + WRITES) * held;
    assert!(
        syscall_total >= floor,
        "{} calls through a {DELAY_MS} ms delay hold at least {floor} ns, and the syscall layer \
         found {syscall_total}",
        READS + WRITES
    );
    assert!(
        syscall_total <= own_total,
        "the syscall layer found {syscall_total} ns inside calls the program's own clock put \
         {own_total} ns around"
    );

    // The device under the delay: the loop device is this test's alone, so
    // every request on it is the program's, one per I/O, and none of them
    // holds the delay -- it was spent above the request layer.
    let key = delayed.loop_key();
    let on_loop: Vec<&serde_json::Value> = raw(result(&document, "device"))
        .iter()
        .filter(|event| {
            event["keys"].as_array().is_some_and(|keys| {
                keys.iter()
                    .any(|k| k.get("dev").is_some() && dev_key(k) == key)
            })
        })
        .collect();
    assert!(
        on_loop.len() as u64 >= READS + WRITES,
        "the program's {} direct I/Os reached {key} as {} requests",
        READS + WRITES,
        on_loop.len()
    );
    let device_total: u64 = on_loop
        .iter()
        .filter_map(|event| attr(event, "latency_ns")?.as_u64())
        .sum();

    // The vertical attribution: what the program waited for, less what the
    // device did, is the delay the table configured, once per I/O.
    let above = syscall_total.saturating_sub(device_total);
    assert!(
        above >= floor,
        "the syscall layer held {syscall_total} ns and {key} {device_total} ns, so {above} ns \
         were spent above the device -- less than the {floor} ns that {} I/Os through a \
         {DELAY_MS} ms delay must spend there",
        READS + WRITES
    );
    eprintln!(
        "{} I/Os through {DELAY_MS} ms: {syscall_total} ns at the syscall layer, {device_total} \
         ns on {key} in {} requests, {above} ns above the device (configured: {floor})",
        READS + WRITES,
        on_loop.len()
    );
}

/// Why this machine cannot attach `io.syscall` and `io.latency`, the
/// resolver's own reasons first, or `None` when it can.
fn no_tracers_here() -> Option<String> {
    for source in ["io.syscall", "io.latency"] {
        let probe = Command::new(BINARY)
            .args([source, "--system", "-m", "trace", "-e", "raw", "-d", "10ms"])
            .output()
            .expect("omn runs");
        if probe.status.success() {
            continue;
        }
        let said = String::from_utf8_lossy(&probe.stderr);
        // The indented lines under a "no viable backend" refusal are each
        // backend's missing piece: the machine's reason, which goes first.
        let reasons: Vec<&str> = said
            .lines()
            .filter(|line| line.starts_with("  ") && !line.trim_start().starts_with("hint:"))
            .map(str::trim)
            .collect();
        let first = said.lines().next().unwrap_or_default();
        return Some(format!(
            "{} -- `{source}` cannot be attached here ({first}); `make` grants CAP_BPF and \
             CAP_PERFMON to target/debug/omn, and every `cargo build` drops them",
            if reasons.is_empty() {
                first.to_owned()
            } else {
                reasons.join("; ")
            }
        ));
    }
    None
}

fn dev_key(key: &serde_json::Value) -> String {
    let dev = &key["dev"];
    format!(
        "dev:{}+{}",
        dev["major"].as_u64().unwrap_or(u64::MAX),
        dev["minor"].as_u64().unwrap_or(u64::MAX)
    )
}
