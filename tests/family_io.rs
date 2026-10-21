//! The `io` family, through the surfaces a user meets.
//!
//! The collectors are checked against the kernel's own counters in their own
//! unit tests, where a fake `/proc/diskstats` can be written and a known number
//! of bytes can be flushed to a real device. What is checked here is the rest
//! of the promise: that the catalog entries are truthful, that the sources this
//! build can collect say so, and that the combinations these backends cannot
//! mean are refused with the reason rather than answered.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use omn::capabilities::Capabilities;
use omn::Catalog;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// A request was refused because it cannot mean what it asks for.
const EXIT_REJECTED: i32 = 2;

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

/// The sources this family collects today, all of them through procfs.
const PROCFS_SOURCES: [&str; 5] = [
    "io.bytes",
    "io.iops",
    "io.merges",
    "io.inflight",
    "io.pressure",
];

#[test]
fn every_procfs_source_has_a_collector_in_this_build() {
    let implemented = omn::collect::implemented().join(", ");
    for source in PROCFS_SOURCES {
        assert!(
            implemented.contains(&format!("{source} (fs:proc")),
            "`{source}` has no collector: {implemented}"
        );
    }
}

/// A fallback that loses something has to say what, or the reader takes the
/// second-best number for the best one.
#[test]
fn every_io_fallback_states_what_it_degrades() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    for entry in catalog.family("io") {
        for backend in entry.backends.iter().skip(1) {
            assert!(
                backend.degraded.is_some() || backend.equivalent,
                "`{}` falls back to `{}` without saying what that costs",
                entry.id,
                backend.id
            );
        }
    }
}

/// Backend ids are how `--backend force:` names one and how a collector is
/// looked up. Two with the same id inside one chain cannot be told apart.
#[test]
fn backend_ids_are_unique_within_each_io_chain() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    for entry in catalog.family("io") {
        let mut seen = Vec::new();
        for backend in &entry.backends {
            assert!(
                !seen.contains(&backend.id),
                "`{}` lists `{}` twice",
                entry.id,
                backend.id
            );
            seen.push(backend.id.clone());
        }
    }
}

/// Every source this family collects carries a caveat saying what its number
/// does *not* mean. For the block layer that is the whole difficulty: these
/// are the device's bytes, not the application's.
#[test]
fn every_collected_source_says_what_its_number_is_not() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    for source in PROCFS_SOURCES {
        let entry = catalog
            .get(&source.parse().expect("a source id"))
            .unwrap_or_else(|| panic!("`{source}` is in the catalog"));
        assert!(
            !entry.caveats.is_empty(),
            "`{source}` states no caveat, so nothing warns the reader what it is not"
        );
    }
}

/// The listing is the catalog seen through this machine. Where the file is
/// readable the source is usable, and the two must not disagree.
#[test]
fn the_listing_agrees_with_what_this_machine_holds() {
    let listing = stdout(&run(&["list", "io"]));
    let capabilities = Capabilities::detect();
    let readable = capabilities.probe("/proc/diskstats", None).is_available();
    let expected = if readable { "yes" } else { "no" };
    let row = listing
        .lines()
        .find(|line| line.starts_with("io.inflight"))
        .expect("io.inflight is listed");
    assert!(
        row.contains(expected),
        "/proc/diskstats is {}, but the listing says otherwise: {row}",
        if readable { "readable" } else { "not readable" }
    );
}

/// These counters are kept per device for the whole machine. Narrowing them to
/// a process would mean filtering inside the kernel, which only the eBPF
/// backend can do -- so the request is refused rather than answered with the
/// machine's total.
#[test]
fn a_per_process_request_is_refused_rather_than_answered_system_wide() {
    let refused = run(&["io.bytes", "--pid", "1", "-m", "poll", "-e", "timeline"]);
    let message = stderr(&refused);
    assert_ne!(refused.status.code(), Some(0), "{message}");
    assert!(
        stdout(&refused).is_empty(),
        "a machine-wide total must not be produced for a process target:\n{}",
        stdout(&refused)
    );

    // Which refusal arrives depends on what this machine grants. Without
    // readable tracefs no backend can serve a pid at all; with it, the request
    // resolves onto the eBPF backend, which is the one that could answer and
    // has no collector yet. Both are the tool declining to invent a
    // per-process number -- the assertion is on that, not on which of the two
    // privileges the machine happens to have.
    assert!(
        message.contains("in-program filtering")
            || message.contains("no viable backend")
            || message.contains("no collector is written for `io.bytes` on backend `bpf"),
        "the refusal must name why a procfs counter cannot serve a pid, or which backend \
         could: {message}"
    );
}

/// A queue depth is read at an instant, so it counts over no window and cannot
/// produce a rate. The validator says so before anything is opened.
#[test]
fn a_mode_a_source_does_not_support_is_refused_with_what_it_does() {
    let refused = run(&["io.inflight", "--system", "-m", "count", "-e", "rate"]);
    assert_eq!(refused.status.code(), Some(EXIT_REJECTED));
    let message = stderr(&refused);
    assert!(
        message.contains("cannot be observed in `count` mode"),
        "{message}"
    );
    assert!(message.contains("poll, snapshot"), "{message}");
}

/// Why this kernel serves no pressure source, if it serves none: one built
/// without `CONFIG_PSI`, or booted with `psi=0`, has no `/proc/pressure`.
fn no_psi_here() -> Option<&'static str> {
    (!std::path::Path::new("/proc/pressure").exists())
        .then_some("this kernel publishes no PSI: /proc/pressure does not exist")
}

/// The dumped request is a session file, not a pretty-printed summary: what
/// the command line prototypes, a file replays.
#[test]
fn a_dumped_io_request_is_a_session_file_that_validates() {
    let mut args = vec![
        "io.pressure",
        "--system",
        "-m",
        "poll@500ms",
        "-e",
        "timeline",
        "-d",
        "5s",
        "--dump-request",
        "--format",
        "yaml",
    ];
    // Written down for a machine that can serve it: the dump is the request,
    // and this one cannot resolve it.
    if no_psi_here().is_some() {
        args.push("--no-resolve");
    }
    let dumped = run(&args);
    let session = stdout(&dumped);
    assert!(session.contains("io.pressure"), "{session}");

    let mut child = Command::new(BINARY)
        .args(["validate", "-f", "-", "--input-format", "yaml"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(session.as_bytes())
        .expect("writing the session");
    let validated = child.wait_with_output().expect("omn should finish");
    assert!(
        validated.status.success(),
        "a dumped request must validate: {}",
        stderr(&validated)
    );
}

fn validate_stdin(session: &str) -> Output {
    let mut child = Command::new(BINARY)
        .args(["validate", "-f", "-", "--input-format", "yaml"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(session.as_bytes())
        .expect("writing the session");
    child.wait_with_output().expect("omn should finish")
}

/// A join on `blk` is refused (exit 2) only when its `io.latency` side asked
/// for nothing but a log2 histogram, which the kernel builds and which carries
/// no `blk`; a hist-only `io.queue` streams every request and joins. The
/// second session ran on wave 15's binary and matched every request it saw.
#[test]
fn a_join_on_blk_is_refused_only_over_a_histogram_the_kernel_builds() {
    let session = |queue: &str, latency: &str| {
        format!(
            "requests:\n\
             \x20 - {{label: q, source: io.queue, target: system, mode: trace, emit: {queue}}}\n\
             \x20 - {{label: device, source: io.latency, target: system, mode: trace, emit: {latency}}}\n\
             joins:\n\
             \x20 - {{id: halves, from: io.queue, to: io.latency, via: blk, match: scoped_eq, window: 30s}}\n"
        )
    };

    let refused = validate_stdin(&session("[hist, summary]", "[hist]"));
    assert_eq!(
        refused.status.code(),
        Some(EXIT_REJECTED),
        "{}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("`device` asks `io.latency` only for a log2 histogram"),
        "{}",
        stderr(&refused)
    );

    let accepted = validate_stdin(&session("[hist]", "[hist, summary]"));
    assert!(
        accepted.status.success(),
        "a hist-only io.queue carries `blk`: {}",
        stderr(&accepted)
    );
}

/// The examples are part of the documentation, and documentation that does not
/// parse is worse than none.
#[test]
fn the_examples_this_family_ships_validate() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut checked = 0;
    for entry in std::fs::read_dir(examples).expect("examples/ exists") {
        let path = entry.expect("a directory entry").path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with("io-") {
            continue;
        }
        let validated = run(&["validate", "-f", &path.to_string_lossy()]);
        assert!(
            validated.status.success(),
            "{name} does not validate: {}",
            stderr(&validated)
        );
        checked += 1;
    }
    assert!(checked > 0, "this family ships no example");
}

/// The family end to end: resolve, attach, poll, aggregate, render. The unit
/// tests hold the arithmetic; this holds the wiring, which nothing else does.
#[test]
fn a_system_wide_run_produces_events_and_reports_no_loss() {
    let run = run(&[
        "io.inflight",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "400ms",
    ]);
    let out = stdout(&run);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(out.contains("via fs:proc"), "{out}");
    assert!(
        out.contains("no loss"),
        "a poll of a procfs counter on an idle machine loses nothing: {out}"
    );
    let events: usize = out
        .lines()
        .find_map(|line| line.split_whitespace().next()?.parse().ok())
        .expect("the run says how many events it saw");
    assert!(
        events >= 2,
        "a 400 ms run at 100 ms should see several reads, not {events}"
    );
}

/// The value of an event is whatever the collector named, and it has to reach
/// the output under that meaning rather than being guessed from the unit.
#[test]
fn the_summary_carries_the_number_the_collector_measured() {
    let run = run(&[
        "io.bytes",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "300ms",
        "--backend",
        "force:fs:proc",
        "--format",
        "json",
    ]);
    assert!(run.status.success(), "{}", stderr(&run));
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&run)).expect("the json output parses");
    let result = &document["results"][0];
    assert_eq!(result["backend"], "fs:proc");
    assert_eq!(result["unit"], "bytes");
    let series = &result["outputs"][0]["series"][0];
    assert!(
        series["count"].as_u64().unwrap_or(0) >= 1,
        "the summary should cover the reads that happened: {series}"
    );
    assert!(
        series["mean"].as_f64().is_some(),
        "a byte count has to arrive as a number: {series}"
    );
    // Bytes moved by a device are never negative, whatever the machine did.
    assert!(series["min"].as_f64().unwrap_or(-1.0) >= 0.0, "{series}");
}

/// A snapshot of pressure observes no window, so it must say which number it
/// gave rather than passing the kernel's decayed average off as this run's.
#[test]
fn a_pressure_snapshot_says_it_measured_no_window() {
    if let Some(reason) = no_psi_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let run = run(&["io.pressure", "--system", "-m", "snapshot", "-e", "raw"]);
    assert!(run.status.success(), "{}", stderr(&run));
    let out = stdout(&run);
    assert!(out.contains("some_avg10"), "{out}");
    assert!(
        out.contains("observes no window"),
        "the basis has to travel with the number: {out}"
    );
    assert!(
        !out.contains("stall_ratio"),
        "a snapshot has no window to compute a ratio over: {out}"
    );
}

/// Numbers without the record of how they were obtained are numbers whose
/// provenance is gone. Both are written, and the record names the backend and
/// the machine.
#[test]
fn the_numbers_and_the_record_of_how_they_were_taken_are_written_together() {
    if let Some(reason) = no_psi_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let directory = std::env::temp_dir().join(format!("omn-io-artifacts-{}", std::process::id()));
    std::fs::remove_dir_all(&directory).ok();
    let run = run(&[
        "io.pressure",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "300ms",
        "--out",
        &directory.to_string_lossy(),
    ]);
    assert!(run.status.success(), "{}", stderr(&run));

    let resolved = std::fs::read_to_string(directory.join("resolved.json"))
        .expect("the resolved request is written beside the results");
    let document: serde_json::Value =
        serde_json::from_str(&resolved).expect("the resolved record parses");
    assert_eq!(document[0]["backend"]["id"], "fs:proc");
    assert!(
        document[0]["environment"]["kernel_release"].is_string(),
        "the record has to carry the machine, or two runs cannot be compared: {resolved}"
    );
    assert!(
        directory.join("results.txt").exists(),
        "the numbers themselves are written too"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// `io.latency` through `/proc/diskstats` is the block layer's own account of
/// how long the requests it completed were outstanding: under direct random
/// reads, the per-device mean sits below what `fio` measures per call, which
/// adds the system call and the wakeup, and not far below it. Measured: 40.3
/// us against fio's 51.4 us, and the eBPF backend's issue-to-completion mean
/// on the same load 40.4 us.
#[test]
fn the_diskstats_latency_is_the_block_layer_s_mean_under_a_known_load() {
    if Command::new("fio").arg("--version").output().is_err() {
        eprintln!("skipped: no fio to make a known load");
        return;
    }
    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("diskstats-await");
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("load.dat");
    // Incompressible, so a filesystem that compresses (btrfs) cannot turn the
    // direct reads into cached ones.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let bytes: Vec<u8> = (0..(64usize << 20) / 8)
        .flat_map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_ne_bytes()
        })
        .collect();
    std::fs::write(&file, bytes).unwrap();
    let _ = Command::new("sync").status();
    let fio_out = directory.join("fio.json");
    let output = Command::new(BINARY)
        .args([
            "trace",
            "io.latency",
            "--system",
            "-m",
            "count",
            "-e",
            "summary",
            "--backend",
            "force:fs:proc",
            "--format",
            "json",
            "--",
            "fio",
            "--name=r",
            &format!("--filename={}", file.display()),
            "--rw=randread",
            "--bs=4k",
            "--direct=1",
            "--ioengine=psync",
            "--runtime=2",
            "--time_based",
            "--output-format=json",
            &format!("--output={}", fio_out.display()),
        ])
        .output()
        .expect("omn runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("a results document");
    let fio: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&fio_out).unwrap()).unwrap();
    let per_call_ns = fio["jobs"][0]["read"]["clat_ns"]["mean"].as_f64().unwrap();
    let series = document["results"][0]["outputs"][0]["series"]
        .as_array()
        .expect("series")
        .clone();
    // The busiest device is the file's.
    let busiest = series
        .iter()
        .max_by_key(|series| series["count"].as_u64().unwrap_or(0))
        .expect("a device served the reads");
    let mean_ns = busiest["mean"].as_f64().unwrap();
    assert!(
        mean_ns <= per_call_ns * 1.05 && mean_ns >= per_call_ns * 0.3,
        "diskstats says {mean_ns:.0} ns a request, fio {per_call_ns:.0} ns a call: {busiest}"
    );
}
