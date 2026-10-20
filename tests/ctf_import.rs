//! `omn import`: a CTF trace merged into a stored run, and joined with the
//! run's own events.
//!
//! LTTng is not installed here, so the trace is written the way LTTng-UST
//! writes one -- a tracepoint stamps `CLOCK_MONOTONIC` and its own thread into
//! a buffer, in-line, before the call it marks -- by the traced program itself.
//! What is under test is everything after: that the reader places those
//! stamps on the run's clock and those threads on the run's tasks, so the
//! kernel's account of the same thread meets them. The reader against
//! LTTng's own traces is `src/ctf.rs`'s tests, against babeltrace's readings.

use std::path::{Path, PathBuf};
use std::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// Ten markers, each stamped just before a 30 ms sleep, and one stamped ten
/// seconds before the program started: a trace of what the program did,
/// written as a CTF 1.8 trace of one packet, with three events the "tracer"
/// says it discarded.
const PROGRAM: &str = r#"
import os, struct, sys, threading, time

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
tid, pid = threading.get_native_id(), os.getpid()
stamps = [time.monotonic_ns() - 10_000_000_000]
for n in range(10):
    stamps.append(time.monotonic_ns())
    time.sleep(0.03)

metadata = """/* CTF 1.8 */
typealias integer { size = 8; align = 8; signed = false; } := uint8_t;
typealias integer { size = 32; align = 8; signed = false; } := uint32_t;
typealias integer { size = 64; align = 8; signed = false; } := uint64_t;
typealias integer { size = 32; align = 8; signed = true; } := int32_t;
trace {
    major = 1; minor = 8; byte_order = le;
    packet.header := struct { uint32_t magic; uint32_t stream_id; };
};
env { domain = "ust"; tracer_name = "lttng-ust"; procname = "marker"; };
clock { name = monotonic; freq = 1000000000; offset = 0; };
typealias integer { size = 64; align = 8; signed = false; map = clock.monotonic.value; } := uint64_clock_monotonic_t;
stream {
    id = 0;
    packet.context := struct {
        uint64_clock_monotonic_t timestamp_begin;
        uint64_clock_monotonic_t timestamp_end;
        uint64_t content_size;
        uint64_t packet_size;
        uint32_t events_discarded;
        uint32_t cpu_id;
    };
    event.header := struct { uint8_t id; uint64_clock_monotonic_t timestamp; };
    event.context := struct { int32_t _vtid; int32_t _vpid; };
};
event {
    name = "app:before_sleep";
    id = 0;
    stream_id = 0;
    fields := struct { uint32_t _n; string _what; };
};
"""
with open(os.path.join(out, "metadata"), "w") as f:
    f.write(metadata)

events = b"".join(
    struct.pack("<BQiiI", 0, stamp, tid, pid, n) + b"sleep\0"
    for n, stamp in enumerate(stamps)
)
header_bytes = 8 + 8 + 8 + 8 + 8 + 4 + 4
content = (header_bytes + len(events)) * 8
packet = ((header_bytes + len(events) + 63) // 64) * 64 * 8
data = struct.pack("<IIQQQQII", 0xC1FC1FC1, 0, stamps[0], stamps[-1], content, packet, 3, 0)
data += events
data += b"\0" * (packet // 8 - len(data))
with open(os.path.join(out, "channel0_0"), "wb") as f:
    f.write(data)
print(tid)
"#;

struct Gone(PathBuf);

impl Drop for Gone {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn machine_cannot_serve(message: &str) -> bool {
    message.contains("no viable backend")
        || message.contains("cannot start collecting")
        || message.contains("CAP_BPF")
        || message.contains("cap_bpf")
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).expect("the file")).expect("JSON")
}

#[test]
fn a_traced_program_s_markers_meet_the_kernel_s_switches_of_the_same_thread() {
    let dir = std::env::temp_dir().join(format!("omn-ctf-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _gone = Gone(dir.clone());
    let trace = dir.join("trace");
    let run = dir.join("run");

    let output = Command::new(BINARY)
        .args([
            "trace",
            "sched.switches",
            "-m",
            "trace",
            "-k",
            "task",
            "-e",
            "raw",
            "--format",
            "json",
            "-o",
        ])
        .arg(&run)
        .args(["--", "python3", "-c", PROGRAM])
        .arg(&trace)
        .output()
        .expect("omn runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        assert!(
            machine_cannot_serve(&stderr),
            "the run failed for a reason that is not the machine's:\n{stderr}"
        );
        eprintln!("SKIPPED: {stderr}. It would have joined an imported trace with the run");
        return;
    }

    let merged = dir.join("merged.json");
    let imported = Command::new(BINARY)
        .arg("import")
        .arg(&trace)
        .arg(&run)
        .arg("-o")
        .arg(&merged)
        .output()
        .expect("omn import runs");
    let said = String::from_utf8_lossy(&imported.stderr);
    assert!(imported.status.success(), "{said}");
    assert!(
        said.contains("10 events imported")
            && said.contains("1 before")
            && said.contains("3 discarded by the tracer"),
        "{said}"
    );
    let document = json(&merged);
    let result = document["results"]
        .as_array()
        .and_then(|results| {
            results
                .iter()
                .find(|result| result["source"] == "ctf.event")
        })
        .expect("the imported result");
    assert_eq!(result["losses"]["dropped"], 3, "{result:#}");
    assert_eq!(result["fields"]["before_window"], 1, "{result:#}");
    let events = result["outputs"][0]["events"]
        .as_array()
        .expect("raw events");
    assert_eq!(events.len(), 10);
    assert_eq!(
        events[0]["attrs"][0]["value"], "app:before_sleep",
        "{:#}",
        events[0]
    );

    // Every marker is followed by its thread's switch out to sleep: the
    // trace's thread and the run's task are one key, on one clock. A switch
    // per event rather than `sched.offcpu`, whose rows sum a drain's blocks.
    let rules = dir.join("rules.yml");
    std::fs::write(
        &rules,
        "{id: marked, from: ctf.event, to: sched.switches, via: task, match: eq, window: 100ms}",
    )
    .unwrap();
    let joined = Command::new(BINARY)
        .arg("join")
        .arg("-f")
        .arg(&rules)
        .arg(&merged)
        .args(["--format", "json"])
        .output()
        .expect("omn join runs");
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stderr)
    );
    let answer: serde_json::Value = serde_json::from_slice(&joined.stdout).expect("JSON");
    let correlation = &answer["joins"][0];
    let matched = correlation["matched"].as_u64().unwrap_or(0);
    assert!(
        matched >= 10,
        "each of the ten sleeps after a marker is a switch of the marking thread: {correlation:#}"
    );
}
