//! The backends that serve an eBPF source where no program can be loaded,
//! each held to the eBPF backend of the same source over the same window.
//!
//! Both requests run in one session, so they see one launched program and
//! one window; what is compared is the quantity both say they measure, which
//! is the fallback's whole claim. Where the eBPF backend cannot run here, the
//! comparison has nothing to hold the fallback to and is skipped, loudly.

use std::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// The total of every series of each request of a session, by label. A
/// `system` target's window is a launched program's life, as `perf stat -a --
/// prog` measures: `bound`.
fn totals(
    requests: &[(&str, &str, &str)],
    target: serde_json::Value,
    bound: Option<&str>,
) -> Option<Vec<f64>> {
    let mut session = serde_json::json!({
        "requests": requests
            .iter()
            .map(|(source, label, backend)| serde_json::json!({
                "source": source,
                "label": label,
                "mode": "count",
                "emit": ["summary"],
                "backend": backend,
                "target": target,
            }))
            .collect::<Vec<_>>(),
    });
    if let Some(program) = bound {
        session["launch"] = serde_json::json!(["python3", "-c", program]);
    }
    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("fallbacks");
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join(format!("{}.json", requests[0].0));
    std::fs::write(&file, session.to_string()).unwrap();
    let output = Command::new(BINARY)
        .args([
            "-f",
            file.to_str().unwrap(),
            "--input-format",
            "json",
            "--format",
            "json",
        ])
        .output()
        .expect("omn runs");
    if !output.status.success() {
        // Only the eBPF half may be out of reach here -- a binary without its
        // capabilities, a kernel that loads no program; anything else is the
        // pair failing.
        let said = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{said}");
        eprintln!("skipped: the eBPF half cannot run here: {said}");
        return None;
    }
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    Some(
        document["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|result| {
                result["outputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|output| output["series"].as_array().cloned().unwrap_or_default())
                    .map(|series| series["total"].as_f64().unwrap_or(0.0))
                    .sum()
            })
            .collect(),
    )
}

fn launch(program: &str) -> serde_json::Value {
    serde_json::json!({"launch": ["python3", "-c", program]})
}

/// `/proc/vmstat`'s pages allocated over a window are the allocator's
/// tracepoint's, page for page: measured 104 796 against 104 917.
#[test]
fn vmstat_s_pages_allocated_are_the_tracepoint_s() {
    let Some(totals) = totals(
        &[
            ("mem.page_alloc", "vmstat", "force:fs:proc"),
            ("mem.page_alloc", "tracepoint", "force:bpf:raw_tp"),
        ],
        serde_json::json!("system"),
        Some(
            "import time; x = [bytearray(4 << 20) for _ in range(100)]\n\
             for b in x: b[::4096] = b'\\x01' * 1024\n\
             time.sleep(0.2)",
        ),
    ) else {
        return;
    };
    let (counted, traced) = (totals[0], totals[1]);
    assert!(counted > 0.0 && traced > 0.0, "{totals:?}");
    assert!(
        (counted - traced).abs() / traced < 0.1,
        "vmstat {counted} pages, the tracepoint {traced}"
    );
}

/// The perf software counter's migrations of a launched program are the
/// tracepoint's: measured 336/338, 210/217, 155/157.
#[test]
fn the_software_counter_s_migrations_are_the_tracepoint_s() {
    let program = "import threading\n\
                   def spin():\n    s = 0\n    for i in range(3_000_000): s += i\n\
                   ts = [threading.Thread(target=spin) for _ in range(8)]\n\
                   [t.start() for t in ts]; [t.join() for t in ts]";
    let Some(totals) = totals(
        &[
            ("sched.migrations", "counter", "force:pmu:sw"),
            ("sched.migrations", "tracepoint", "force:bpf:raw_tp"),
        ],
        launch(program),
        None,
    ) else {
        return;
    };
    let (counted, traced) = (totals[0], totals[1]);
    assert!(traced > 0.0, "{totals:?}");
    assert!(
        (counted - traced).abs() <= (traced * 0.15).max(5.0),
        "the counter {counted} migrations, the tracepoint {traced}"
    );
}

/// `/proc/stat`'s interrupt time is the handlers' traced time, to the clock
/// tick it counts in: measured 140 ms against 130 ms over two seconds.
#[test]
fn proc_stat_s_handler_time_is_the_traced_handlers_to_the_tick() {
    let Some(totals) = totals(
        &[
            ("sched.irq", "ticks", "force:fs:proc"),
            ("sched.irq", "handlers", "force:bpf:raw_tp"),
        ],
        serde_json::json!("system"),
        Some("import time; time.sleep(2)"),
    ) else {
        return;
    };
    let (counted, traced) = (totals[0], totals[1]);
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    // Each cpu's count is off by up to a tick at each end.
    let slack = 2.0 * 10e6 * cpus;
    assert!(
        (counted - traced).abs() <= slack.max(traced * 0.25),
        "/proc/stat {counted} ns, the handlers {traced} ns"
    );
}

/// TCP's own retransmission counter serves `net.tcp.retransmit` over a
/// window, one event for the machine, as its row says.
#[test]
fn snmp_s_retransmissions_serve_a_counted_window() {
    let output = Command::new(BINARY)
        .args([
            "net.tcp.retransmit",
            "--system",
            "-m",
            "count",
            "-e",
            "summary",
            "--backend",
            "force:fs:proc",
            "-d",
            "300ms",
            "--format",
            "json",
        ])
        .output()
        .expect("omn runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    let result = &document["results"][0];
    assert_eq!(result["backend"], "fs:proc");
    assert_eq!(result["events_seen"], 1, "{result}");
}
