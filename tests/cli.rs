//! End-to-end checks on the binary.
//!
//! These exist to pin the contract the rest of the tool is written against: the
//! exit codes, the implicit `trace` subcommand, and the promise that a dumped
//! request is a runnable session file. The unit tests cover the reasoning; this
//! covers the surface.

use std::io::Write;
use std::process::{Command, Output, Stdio};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// A request was refused because it cannot mean what it asks for.
const EXIT_REJECTED: i32 = 2;
/// The request is valid and this machine cannot serve it. Distinct from a
/// rejection since the fifth wave: `omn::collect::Whose` says whether a refusal
/// belongs to the request, the machine or the build, and the exit code follows.
const EXIT_UNAVAILABLE: i32 = 1;
/// The request is valid, but nothing collects it yet.
const EXIT_NOT_IMPLEMENTED: i32 = 3;

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn run_with_stdin(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(BINARY)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(stdin.as_bytes())
        .expect("writing to stdin");
    child.wait_with_output().expect("omn should finish")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_source_needs_no_subcommand() {
    let implicit = run(&[
        "io.latency",
        "--system",
        "-m",
        "trace",
        "-e",
        "hist",
        "--dump-request",
    ]);
    let explicit = run(&[
        "trace",
        "io.latency",
        "--system",
        "-m",
        "trace",
        "-e",
        "hist",
        "--dump-request",
    ]);
    assert!(implicit.status.success(), "{}", stderr(&implicit));
    assert_eq!(stdout(&implicit), stdout(&explicit));
}

#[test]
fn an_impossible_request_is_refused_before_anything_is_attached() {
    let output = run(&[
        "mem.bandwidth",
        "--pid",
        "1",
        "-m",
        "count",
        "-e",
        "rate",
        "--dump-request",
    ]);
    assert_eq!(output.status.code(), Some(EXIT_REJECTED));
    assert!(
        stderr(&output).contains("scoped to the package"),
        "{}",
        stderr(&output)
    );
    assert!(
        stdout(&output).is_empty(),
        "a refused request must print no request"
    );
}

/// A source the catalog declares and this build does not collect is refused
/// with exit 3, and the refusal names what *is* collected.
///
/// The source is chosen from the build rather than written down here. This test
/// used to name `io.latency`, which is exactly the kind of thing a wave of
/// collectors turns into a false failure: the assertion was about the shape of
/// the refusal and it broke because the refusal stopped happening.
#[test]
fn a_valid_request_with_no_collector_names_the_ones_that_exist() {
    // Every source with no collector, asked every way its entry declares, on
    // the whole machine where it takes one and on this test's own process
    // otherwise. The first that is neither rejected as a request nor out of
    // this machine's reach is the one the refusal is about. Picking one shape
    // in advance ran out of candidates twice as the breadth waves collected
    // them, and then picked a source this machine cannot serve and asserted
    // nothing.
    let catalog = omn::Catalog::embedded().expect("the embedded catalog loads");
    let pid = std::process::id().to_string();
    let mut candidates: Vec<Vec<String>> = Vec::new();
    for entry in catalog.entries() {
        let id = entry.id.to_string();
        if entry
            .backends
            .iter()
            .any(|backend| omn::collect::implements(&id, &backend.id))
        {
            continue;
        }
        let target = if entry.target_support(omn::request::TargetKind::System)
            != omn::catalog::TargetSupport::No
        {
            vec!["--system".to_owned()]
        } else if entry.target_support(omn::request::TargetKind::Pid)
            != omn::catalog::TargetSupport::No
        {
            vec!["--pid".to_owned(), pid.clone()]
        } else {
            continue;
        };
        for mode in &entry.modes {
            for emit in &entry.emits {
                let mut args = vec![id.clone()];
                args.extend(target.iter().cloned());
                args.extend(["-m".to_owned(), mode.as_str().to_owned()]);
                args.extend(["-e".to_owned(), emit.as_str().to_owned()]);
                if mode.as_str() != "snapshot" {
                    args.extend(["-d".to_owned(), "1s".to_owned()]);
                }
                candidates.push(args);
            }
        }
    }
    if candidates.is_empty() {
        // The day every source is collected there is nothing left to refuse
        // this way, and that is the goal rather than a failure.
        eprintln!("skipped: every source in the catalog has a collector");
        return;
    }

    let mut unavailable = Vec::new();
    for args in &candidates {
        let output = run(&args.iter().map(String::as_str).collect::<Vec<_>>());
        match output.status.code() {
            Some(EXIT_REJECTED) => continue,
            // This machine cannot serve it either, which is a different answer
            // and one this test is not about.
            Some(EXIT_UNAVAILABLE) => unavailable.push(args[0].clone()),
            code => {
                assert_eq!(
                    code,
                    Some(EXIT_NOT_IMPLEMENTED),
                    "`{}` has no collector, so the refusal is the build's: {}",
                    args.join(" "),
                    stderr(&output)
                );
                assert!(stderr(&output).contains("mem.rss"), "{}", stderr(&output));
                return;
            }
        }
    }
    unavailable.dedup();
    eprintln!(
        "skipped: every source with no collector is also out of this machine's reach ({})",
        unavailable.join(", ")
    );
}

#[test]
fn a_request_this_machine_cannot_serve_is_refused_with_the_reason() {
    // `io.latency --pid` needs in-program filtering, so it needs the block
    // tracepoints; without readable tracefs there is no backend that can.
    let output = run(&[
        "io.latency",
        "--pid",
        "1",
        "-m",
        "trace",
        "-e",
        "hist",
        "--dump-request",
    ]);
    if output.status.success() {
        return; // this machine can serve it, which is also a valid outcome
    }
    assert_eq!(
        output.status.code(),
        Some(EXIT_UNAVAILABLE),
        "a machine that cannot serve a valid request is not a request that cannot mean \
         anything: {}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("no viable backend"),
        "{}",
        stderr(&output)
    );

    // The same request still writes down, for a machine that can serve it.
    let written = run(&[
        "io.latency",
        "--pid",
        "1",
        "-m",
        "trace",
        "-e",
        "hist",
        "--no-resolve",
        "--dump-request",
    ]);
    assert!(written.status.success(), "{}", stderr(&written));
}

#[test]
fn a_dumped_request_is_a_session_file_the_tool_reads_back() {
    let dumped = run(&[
        "io.latency",
        "--pid",
        "4242",
        "-m",
        "trace",
        "-e",
        "hist,topk:5",
        "-k",
        "blk,task",
        "--filter",
        "bytes > 4KiB",
        "-d",
        "30s",
        "--format",
        "yaml",
        "--no-resolve",
        "--dump-request",
    ]);
    assert!(dumped.status.success(), "{}", stderr(&dumped));

    let replayed = run_with_stdin(
        &[
            "-f",
            "-",
            "--input-format",
            "yaml",
            "--format",
            "yaml",
            "--no-resolve",
            "--dump-request",
        ],
        &stdout(&dumped),
    );
    assert!(replayed.status.success(), "{}", stderr(&replayed));
    assert_eq!(
        stdout(&replayed),
        stdout(&dumped),
        "a session must survive a trip through the file surface unchanged"
    );
}

#[test]
fn the_same_session_reads_from_yaml_json_and_toml() {
    let yaml = run_with_stdin(
        &[
            "-f",
            "-",
            "--input-format",
            "yaml",
            "--format",
            "json",
            "--dump-request",
        ],
        "requests: [{source: io.latency, target: system, mode: trace, emit: [hist]}]",
    );
    let json = run_with_stdin(
        &[
            "-f",
            "-",
            "--input-format",
            "json",
            "--format",
            "json",
            "--dump-request",
        ],
        r#"{"requests":[{"source":"io.latency","target":"system","mode":"trace","emit":["hist"]}]}"#,
    );
    let toml = run_with_stdin(
        &["-f", "-", "--input-format", "toml", "--format", "json", "--no-resolve", "--dump-request"],
        "[[requests]]\nsource = \"io.latency\"\ntarget = \"system\"\nmode = \"trace\"\nemit = [\"hist\"]\n",
    );
    assert!(yaml.status.success(), "{}", stderr(&yaml));
    assert_eq!(stdout(&yaml), stdout(&json));
    assert_eq!(stdout(&json), stdout(&toml));
}

#[test]
fn the_shipped_examples_validate() {
    // The directory is walked rather than listed here: an example that ships
    // broken because nobody added it to a list is exactly the kind of rot this
    // test exists to prevent, and every family adds its own.
    let mut checked = 0;
    for entry in std::fs::read_dir("examples").expect("the examples directory ships") {
        let path = entry.expect("readable directory entry").path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("yml" | "yaml" | "json" | "toml") => {}
            _ => continue,
        }
        let example = path.to_str().expect("a printable path");
        let output = run(&["validate", "-f", example]);
        assert!(output.status.success(), "{example}: {}", stderr(&output));
        checked += 1;
    }
    assert!(checked >= 3, "only {checked} examples were validated");
}

#[test]
fn the_catalog_listing_is_machine_readable() {
    let output = run(&["list", "io", "--format", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let entries: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid JSON");
    let entries = entries.as_array().expect("a list of entries");
    assert!(entries.len() >= 10);
    assert!(entries.iter().all(|entry| entry["family"] == "io"));
}

/// A format is declared before its encoder is written, and the listing has to
/// say which of the two a given format is at.
///
/// The assertion is about the distinction rather than about any one format:
/// naming the format that happens to be unwritten today turns this into a test
/// that fails the day it is written, which is the day it should say nothing.
/// Every declared format has an encoder as of the profile and trace encoders,
/// so what is left to check is that each one still answers the question --
/// `planned_in` is present exactly when the encoder is not.
#[test]
fn every_declared_format_is_listed_with_whether_its_encoder_exists() {
    let output = run(&["formats", "--format", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let info: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid JSON");
    let outputs = info["output"].as_array().expect("output formats");

    for format in outputs {
        let implemented = format["implemented"].as_bool().unwrap_or_else(|| {
            panic!(
                "`{}` says nothing about whether it can be written: {format}",
                format["name"]
            )
        });
        assert_eq!(
            implemented,
            format["planned_in"].is_null(),
            "`{}` is listed as written and as planned at once: {format}",
            format["name"]
        );
    }

    // `folded` describes a profile and nothing else, which is the whole reason
    // the emit matrix exists.
    let folded = outputs
        .iter()
        .find(|format| format["name"] == "folded")
        .expect("folded is declared");
    assert_eq!(folded["emits"].as_array().unwrap().len(), 1);
    assert_eq!(folded["emits"][0], "stack");
}

#[test]
fn a_result_format_is_refused_for_a_request_dump() {
    let output = run(&[
        "cpu.profile",
        "--pid",
        "1",
        "-m",
        "sample",
        "-e",
        "stack",
        "--format",
        "folded",
        "--dump-request",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("describes results"),
        "{}",
        stderr(&output)
    );
}

/// Ground truth.
///
/// Every other test here checks that the tool is coherent with itself, which
/// proves nothing about whether the number is right. This one makes a known
/// amount of memory exist and asks whether the measurement saw it: without a
/// test of this shape, a collector that returns a plausible constant passes
/// everything else.
#[test]
fn a_known_allocation_shows_up_in_the_measured_series() {
    const TOUCHED: usize = 64 * 1024 * 1024;

    let pid = std::process::id().to_string();
    let measuring = Command::new(BINARY)
        .args([
            "mem.rss",
            "--pid",
            &pid,
            "--mode",
            "poll@50ms",
            "--duration",
            "1200ms",
            "--emit",
            "summary",
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // Let it take a baseline before the memory exists.
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Touching one byte per page is what makes the pages resident: allocating
    // alone would move nothing, which is the source's first caveat.
    let mut block = vec![0u8; TOUCHED];
    for page in block.chunks_mut(4096) {
        page[0] = 1;
    }
    std::hint::black_box(&block);

    let output = measuring.wait_with_output().expect("omn should finish");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(block);

    let run: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json results");
    let series = run["results"][0]["outputs"][0]["series"]
        .as_array()
        .expect("a summary per key");
    let ours = series
        .iter()
        .find(|entry| entry["key"] == format!("task:{pid}"))
        .expect("the series for the measured process");

    let rise = ours["max"].as_f64().unwrap() - ours["min"].as_f64().unwrap();
    assert!(
        rise >= 0.75 * TOUCHED as f64,
        "touched {TOUCHED} bytes but the measured rise was only {rise}"
    );
    assert!(
        rise <= 4.0 * TOUCHED as f64,
        "measured rise of {rise} is far more than the {TOUCHED} bytes touched"
    );
}

#[test]
fn a_run_writes_its_numbers_and_the_record_of_how_they_were_obtained() {
    let directory = std::env::temp_dir().join(format!("omn-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);

    let output = run(&[
        "mem.rss",
        "--pid",
        &std::process::id().to_string(),
        "--mode",
        "poll@50ms",
        "--duration",
        "150ms",
        "--emit",
        "summary",
        "--out",
        directory.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    let resolved = std::fs::read_to_string(directory.join("resolved.json")).expect("resolved.json");
    // The file holds every request of the run, so it is a list even when the
    // run had one.
    let resolved: serde_json::Value = serde_json::from_str(&resolved).unwrap();
    let first = &resolved[0];
    assert_eq!(first["request"]["source"], "mem.rss");
    assert!(first["backend"]["id"]
        .as_str()
        .unwrap()
        .starts_with("fs:proc:"));
    assert!(!first["environment"]["kernel_release"]
        .as_str()
        .unwrap()
        .is_empty());
    assert!(std::fs::read_to_string(directory.join("results.txt")).is_ok());

    let _ = std::fs::remove_dir_all(&directory);
}

/// Ground truth for the join.
///
/// One allocation is written to and one is not, and only the written one can
/// have caused faults. That is checkable rather than merely plausible: a join
/// that matched addresses to whichever range happened to be open would credit
/// both.
///
/// `bytearray(n)` cannot play the untouched role -- CPython zero-fills it, so it
/// is written to in full. A raw `libc.malloc` gives memory that is genuinely
/// never touched, and it still goes through the shim.
#[test]
fn faults_are_attributed_to_the_allocation_that_was_written_to() {
    const TOUCHED: usize = 16 * 1024 * 1024;
    const UNTOUCHED: usize = 16 * 1024 * 1024;

    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let program = format!(
        "import ctypes, time\n\
         libc = ctypes.CDLL(None)\n\
         libc.malloc.restype = ctypes.c_void_p\n\
         untouched = libc.malloc(ctypes.c_size_t({UNTOUCHED}))\n\
         touched = bytearray({TOUCHED})\n\
         time.sleep(0.05)\n"
    );
    let session = session_for(&program);

    let output = run_with_stdin(&["-f", "-", "--input-format", "yaml"], &session);
    if let Some(reason) = skipped(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let join = &run["joins"][0];
    assert_eq!(
        join["evidence"], "KEY",
        "an interval match is a shared typed key"
    );

    let ranges = join["top"].as_array().expect("attributions");
    let large: Vec<&serde_json::Value> = ranges
        .iter()
        .filter(|range| range["len"].as_u64().unwrap_or(0) as usize >= TOUCHED)
        .collect();

    // Both buffers are the same size, so whichever range carries the faults is
    // the one that was written to. Exactly one of them may.
    assert!(
        !large.is_empty(),
        "neither large allocation was attributed anything: {join}"
    );
    let heaviest = large[0]["matched"].as_u64().unwrap_or(0);
    assert!(
        heaviest > 100,
        "the buffer that was written to should carry the faults, got {}",
        large[0]
    );
    for other in large.iter().skip(1) {
        assert!(
            other["matched"].as_u64().unwrap_or(0) * 10 < heaviest,
            "memory that was never written to was credited with faults: {other}"
        );
    }
}

/// Ground truth for the fault collector itself.
///
/// The kernel keeps its own minor-fault counter for every process. If the
/// collector's count and that counter disagree, the sampling is dropping
/// events, whatever the loss line claims.
#[test]
fn the_fault_count_agrees_with_the_kernels_own_counter() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let program = "import sys\n\
                   b = bytearray(16 * 1024 * 1024)\n\
                   faults = int(open('/proc/self/stat').read().split()[9])\n\
                   sys.stderr.write('MINFLT %d\\n' % faults)\n";
    let output = run(&[
        "mem.faults",
        "--mode",
        "trace",
        "--emit",
        "summary",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        program,
    ]);
    if let Some(reason) = skipped(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let reported: u64 = stderr(&output)
        .lines()
        .find_map(|line| line.strip_prefix("MINFLT "))
        .and_then(|value| value.trim().parse().ok())
        .expect("the program reports its own fault count");

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let counted = run["results"][0]["events_seen"]
        .as_u64()
        .expect("events_seen");

    // The collector attaches before exec and the kernel counts from fork, so a
    // handful of faults differ at the edges; an order of magnitude does not.
    let ratio = counted as f64 / reported as f64;
    assert!(
        (0.8..1.2).contains(&ratio),
        "the collector counted {counted} faults and the kernel counted {reported}"
    );
}

/// Builds a two-source session that launches the same program for both.
fn session_for(program: &str) -> String {
    format!(
        "format: json\n\
         requests:\n\
         \x20 - source: mem.heap.alloc\n\
         \x20   target: {{launch: [python3, -c, {program:?}]}}\n\
         \x20   mode: trace\n\
         \x20   emit: [summary]\n\
         \x20 - source: mem.faults\n\
         \x20   target: {{launch: [python3, -c, {program:?}]}}\n\
         \x20   mode: trace\n\
         \x20   emit: [summary]\n\
         joins:\n\
         \x20 - id: pages-of-allocation\n\
         \x20   from: mem.heap.alloc\n\
         \x20   to: mem.faults\n\
         \x20   via: mem\n\
         \x20   match: interval\n\
         \x20   opens: \"op == 'alloc'\"\n\
         \x20   closes: \"op == 'free'\"\n\
         \x20   window: 60s\n"
    )
}

/// Some machines withhold the permission these tests need. That is a skip, not
/// a pass and not a failure.
fn skipped(output: &Output) -> Option<String> {
    if output.status.success() {
        return None;
    }
    let complaint = stderr(output);
    if complaint.contains("perf_event_open") || complaint.contains("paranoid") {
        Some("this machine refuses unprivileged perf sampling".to_owned())
    } else {
        None
    }
}

/// A filter the resolver could not push down has to actually run, and the
/// events it removed have to be accounted for. A predicate that is parsed,
/// validated, reported in the resolved file and then never applied is the
/// worst shape a bug can take here: the numbers look like an answer to the
/// question that was asked, and are an answer to a different one.
#[test]
fn a_filter_that_cannot_be_pushed_down_runs_after_reading_and_says_what_it_removed() {
    const BIG: usize = 8 << 20;
    // Five big allocations among a few hundred small ones: the filter has a
    // known population to separate.
    let program = format!(
        "b = [bytearray({BIG}) for _ in range(5)]\n\
         s = [bytearray(1024) for _ in range(300)]\n"
    );
    let output = run(&[
        "mem.heap.alloc",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--filter",
        &format!("bytes >= {BIG}"),
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        &program,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json results");
    let result = &run["results"][0];
    let events = result["outputs"][0]["events"]
        .as_array()
        .expect("raw emits events");

    assert!(
        events.len() >= 5,
        "five allocations of {BIG} bytes were made, only {} came through",
        events.len()
    );
    for event in events {
        let bytes = event["attrs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|attr| attr["name"] == "bytes")
            .and_then(|attr| attr["value"].as_f64())
            .expect("an allocation carries its size");
        assert!(bytes >= BIG as f64, "an event of {bytes} bytes got through");
    }

    let filtered = &result["filtered"];
    assert_eq!(filtered["kept"].as_u64().unwrap(), events.len() as u64);
    assert!(
        filtered["dropped"].as_u64().unwrap() > 100,
        "the three hundred small allocations should have been dropped: {filtered}"
    );
}

/// A predicate over a field the collector never produces is false everywhere,
/// which empties the series. That is a legitimate outcome and an easy one to
/// misread, so the run says which field was never there.
///
/// `power.cstate --filter state` is the case, and it is not an oversight: one
/// event covers every idle state of a cpu, with a residency per state in its
/// own attribute, so there is no single `state` value the event could carry.
/// The catalog declares the field for the eBPF backend, which sees transitions
/// one at a time. This test used to use `comm` on `mem.rss`, which no longer
/// works for it -- because that field is now emitted, which was the point.
#[test]
fn a_filter_on_a_field_no_event_carries_says_so_instead_of_returning_an_empty_series() {
    let output = run(&[
        "power.cstate",
        "--system",
        "--mode",
        "poll@50ms",
        "--duration",
        "200ms",
        "--emit",
        "summary",
        "--filter",
        "state == 'nothing-here'",
    ]);
    // A cloud VM has no cpuidle driver to read, and no eBPF without the
    // capabilities: the CI's runners serve neither backend.
    if stderr(&output).contains("has no viable backend on this machine") {
        eprintln!("skipped: {}", stderr(&output));
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("kept 0 of"), "{text}");
    assert!(
        text.contains("no event carried `state`"),
        "an empty series must explain itself: {text}"
    );
}

// ================================================ a container is its cgroup

/// A cgroup this test makes and removes, under the nearest cgroup of this
/// process's own ancestry this user may write -- on a systemd machine, the
/// session's own scope.
struct Scope {
    dir: std::path::PathBuf,
}

impl Scope {
    fn create(name: &str) -> Result<Self, String> {
        let own = std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("0::").map(str::to_owned))
            })
            .ok_or("this process is not in a cgroup2 hierarchy")?;
        let root = std::path::Path::new("/sys/fs/cgroup");
        let mut parent = root.join(own.trim().trim_start_matches('/'));
        loop {
            let dir = parent.join(format!("omn-{name}-{}", std::process::id()));
            if std::fs::create_dir(&dir).is_ok() {
                return Ok(Self { dir });
            }
            if parent == root || !parent.pop() {
                break;
            }
        }
        Err(format!(
            "no cgroup in this process's ancestry ({}) lets this user make one below it",
            own.trim()
        ))
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// A task that blocks every two milliseconds until SIGUSR1, after moving
/// itself into the cgroup whose `cgroup.procs` it is given (or staying where it
/// is, given `-`), and logs each sleep on `CLOCK_MONOTONIC` with the voluntary
/// switches the kernel counted for it.
const SLEEPER: &str = r#"
import json, os, resource, signal, sys, time
if sys.argv[1] != "-":
    with open(sys.argv[1], "w") as f:
        f.write(str(os.getpid()))
mono = lambda: time.clock_gettime_ns(time.CLOCK_MONOTONIC)
blocked = lambda: resource.getrusage(resource.RUSAGE_THREAD).ru_nvcsw
done = []
signal.signal(signal.SIGUSR1, lambda *_: done.append(1))
print("ready", os.getpid(), flush=True)
sleeps = []
end = mono() + 60 * 10**9
while not done and mono() < end:
    v = blocked(); a = mono(); time.sleep(0.002); b = mono(); sleeps.append([a, b, blocked() - v])
print(json.dumps(sleeps), flush=True)
"#;

struct Sleeper {
    child: std::process::Child,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    pid: i32,
}

impl Sleeper {
    fn start(procs: &str) -> Self {
        use std::io::BufRead;
        let mut child = Command::new("python3")
            .args(["-c", SLEEPER, procs])
            .stdout(Stdio::piped())
            .spawn()
            .expect("python3 starts");
        let mut stdout = std::io::BufReader::new(child.stdout.take().expect("a pipe"));
        let mut line = String::new();
        stdout
            .read_line(&mut line)
            .expect("the task says it is ready");
        let pid = line
            .trim()
            .strip_prefix("ready ")
            .and_then(|pid| pid.parse().ok())
            .unwrap_or_else(|| panic!("the task did not start: {line:?}"));
        Self { child, stdout, pid }
    }

    /// Stops it, and returns its log: `[start, end, voluntary switches]` per
    /// sleep.
    fn finish(mut self) -> Vec<[u64; 3]> {
        // SAFETY: a signal to the child this test started and has not reaped.
        unsafe { libc::kill(self.pid, libc::SIGUSR1) };
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut self.stdout, &mut rest).expect("the log");
        let _ = self.child.wait();
        serde_json::from_str(rest.trim())
            .unwrap_or_else(|error| panic!("the log is not json ({error}): {rest}"))
    }
}

/// A container runtime on `PATH` that knows the containers it is given, as
/// `docker inspect --format '{{.State.Pid}}' <name>` answers, and no other.
fn runtime_knowing(dir: &std::path::Path, containers: &[(&str, i32)]) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).expect("a directory for the runtime");
    let mut script = String::from("#!/bin/sh\n[ \"$1\" = inspect ] || exit 2\ncase \"$4\" in\n");
    for (name, pid) in containers {
        script.push_str(&format!("  {name}) echo {pid} ;;\n"));
    }
    script.push_str("  *) echo \"Error: No such object: $4\" >&2; exit 1 ;;\nesac\n");
    let path = dir.join("docker");
    std::fs::write(&path, script).expect("the runtime is written");
    let mut permissions = std::fs::metadata(&path).expect("it is there").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).expect("it is executable");
    path
}

/// `--container` is measured as the cgroup its runtime says it runs in.
///
/// Every `sched` and `proc` program serves a cgroup since the thirteenth wave,
/// and a container was still refused, because nothing turned a container's
/// name into its cgroup. Now the tool asks the runtime the way a user would
/// (`docker inspect --format '{{.State.Pid}}' <name>`, then that pid's
/// `/proc/<pid>/cgroup`), and the runtime here is a script on `PATH` that
/// knows one container: a task this test put in a cgroup it made. That is
/// what a container is to the kernel, a cgroup with processes in it, and it
/// needs no image.
///
/// The truth is the task's. Its sleeps inside the window, by the kernel's own
/// count of its voluntary switches, are blocks `sched.offcpu --container` has
/// to find, and the same task beside the cgroup, sleeping the same way, is in
/// no row: the container is its cgroup and nothing else.
#[test]
fn a_container_is_measured_as_the_cgroup_its_runtime_names() {
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: the workload is python3, which this machine does not have");
        return;
    }
    // Asked first, so that a machine that cannot serve the source says that
    // rather than what its cgroups are: the CI's hosted runner has neither.
    let served = run(&[
        "trace",
        "sched.offcpu",
        "--cgroup",
        "/sys/fs/cgroup",
        "-m",
        "trace",
        "-e",
        "raw",
        "--dump-resolved",
    ]);
    if !served.status.success() {
        eprintln!("skipped: {}", stderr(&served));
        return;
    }
    let scope = match Scope::create("container") {
        Ok(scope) => scope,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let procs = scope.dir.join("cgroup.procs").display().to_string();
    let inside = Sleeper::start(&procs);
    let beside = Sleeper::start("-");
    let name = format!("omn-web-{}", std::process::id());
    let bin = std::env::temp_dir().join(format!("omn-runtime-{}", std::process::id()));
    runtime_knowing(&bin, &[(name.as_str(), inside.pid)]);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(BINARY)
        .args([
            "trace",
            "sched.offcpu",
            "--container",
            &name,
            "-m",
            "trace",
            "-e",
            "raw",
            "-d",
            "800ms",
            "--format",
            "json",
        ])
        .env("PATH", &path)
        .env_remove("OMN_CONTAINER_RUNTIME")
        .output()
        .expect("omn runs");
    let (pid_inside, pid_beside) = (inside.pid, beside.pid);
    let log = inside.finish();
    let _ = beside.finish();
    let _ = std::fs::remove_dir_all(&bin);
    let said = stderr(&output);
    let cgroup = scope.dir.display().to_string();
    assert!(
        said.contains(&format!(
            "container `{name}` runs in cgroup {cgroup} (pid {pid_inside}"
        )),
        "the run names the cgroup the runtime's pid is in:\n{said}"
    );
    if !output.status.success() {
        if said.contains("has no viable backend on this machine") {
            eprintln!("skipped: the container was found, and {said}");
            return;
        }
        panic!("the run failed: {said}");
    }
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("a JSON document");
    let result = &document["results"][0];
    let opened = result["window"]["opened_ns"].as_u64().expect("a window");
    let closed = result["window"]["closed_ns"].as_u64().expect("a window");
    let events: Vec<serde_json::Map<String, serde_json::Value>> = result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .filter_map(|output| output["events"].as_array())
        .flatten()
        .map(|event| {
            event["attrs"]
                .as_array()
                .expect("attributes")
                .iter()
                .map(|attr| {
                    (
                        attr["name"].as_str().expect("a name").to_owned(),
                        attr["value"].clone(),
                    )
                })
                .collect()
        })
        .collect();
    let of = |pid: i32| {
        events
            .iter()
            .filter(|event| {
                event.get("tid").and_then(serde_json::Value::as_i64) == Some(pid as i64)
            })
            .filter(|event| {
                event.get("state").and_then(serde_json::Value::as_str) == Some("sleeping")
            })
            .map(|event| {
                event
                    .get("blocks")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1)
            })
            .sum::<u64>()
    };
    let blocked: u64 = log
        .iter()
        .filter(|sleep| sleep[0] >= opened && sleep[1] <= closed)
        .map(|sleep| sleep[2])
        .sum();
    let lost = result["losses"]["dropped"].as_u64().unwrap_or(0);
    let (kept, strays) = (of(pid_inside), of(pid_beside));
    eprintln!(
        "sched.offcpu --container: {kept} blocks of the task inside, {blocked} by the kernel's \
         count inside the window, {strays} of the task beside, {lost} lost"
    );
    assert!(
        blocked > 50,
        "the task blocked {blocked} times, too few to judge"
    );
    assert!(
        kept + lost >= blocked,
        "the task in the container blocked {blocked} times inside the window and the run kept \
         {kept}, reporting {lost} lost"
    );
    assert_eq!(
        strays, 0,
        "the same task beside the container is in no row of it"
    );
}

/// `--container` on a source that serves a cgroup and whose catalog entry
/// says `container: no`: `cpu.context_switches`. The validator reads the
/// catalog, and refuses the container before it is bound; the entry's
/// `container` key dropped (filed for P) makes the kind fall back to `cgroup`,
/// and this test is what then holds the binding to the kernel's count. Until
/// then it skips, with the refusal.
///
/// The truth is the task's, as above: the kernel's count of its voluntary
/// switches over the window. The run counts the cgroup's switches -- the task
/// inside, which blocks every 2 ms -- and not the same task beside it, which
/// blocks as often: the count is at least half the task's blocks inside the
/// window (a counter multiplexed with the other streams' is scaled, which is
/// what the half is for) and under one and a half times them, where the task
/// beside would double it.
#[test]
fn a_container_is_served_by_a_source_that_serves_its_cgroup() {
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: the workload is python3, which this machine does not have");
        return;
    }
    let served = run(&[
        "trace",
        "cpu.context_switches",
        "--cgroup",
        "/sys/fs/cgroup",
        "-m",
        "count",
        "-e",
        "summary",
        "--dump-resolved",
    ]);
    if !served.status.success() {
        eprintln!("skipped: {}", stderr(&served));
        return;
    }
    let scope = match Scope::create("container-counted") {
        Ok(scope) => scope,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let procs = scope.dir.join("cgroup.procs").display().to_string();
    let inside = Sleeper::start(&procs);
    let beside = Sleeper::start("-");
    let name = format!("omn-counted-{}", std::process::id());
    let bin = std::env::temp_dir().join(format!("omn-runtime-counted-{}", std::process::id()));
    runtime_knowing(&bin, &[(name.as_str(), inside.pid)]);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(BINARY)
        .args([
            "trace",
            "cpu.context_switches",
            "--container",
            &name,
            "-m",
            "count",
            "-e",
            "summary",
            "-d",
            "800ms",
            "--format",
            "json",
        ])
        .env("PATH", &path)
        .env_remove("OMN_CONTAINER_RUNTIME")
        .output()
        .expect("omn runs");
    let log = inside.finish();
    let _ = beside.finish();
    let _ = std::fs::remove_dir_all(&bin);
    let said = stderr(&output);
    if output.status.code() == Some(2) && said.contains("per container") {
        // The catalog's own answer: the entry says `container: no`, and the
        // catalog is what the validator reads. Dropping that key is P's
        // (filed, fourteenth wave, stream R); this test runs once it lands.
        eprintln!(
            "SKIP: `cpu.context_switches` still says `container: no` in the catalog, so the \
             validator refuses the container before it is bound: {said}"
        );
        return;
    }
    assert!(
        output.status.success(),
        "a container whose cgroup `cpu.context_switches` serves is not refused:\n{said}"
    );
    assert!(
        said.contains(&format!(
            "container `{name}` runs in cgroup {}",
            scope.dir.display()
        )),
        "the run names the cgroup it measured:\n{said}"
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("a JSON document");
    let result = &document["results"][0];
    let opened = result["window"]["opened_ns"].as_u64().expect("a window");
    let closed = result["window"]["closed_ns"].as_u64().expect("a window");
    let counted: f64 = result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .filter_map(|output| output["series"].as_array())
        .flatten()
        .filter_map(|series| series["last"].as_f64())
        .sum();
    let blocked: u64 = log
        .iter()
        .filter(|sleep| sleep[0] >= opened && sleep[1] <= closed)
        .map(|sleep| sleep[2])
        .sum();
    eprintln!(
        "cpu.context_switches --container: {counted} switches counted in the cgroup, {blocked} \
         blocks of the task inside by the kernel's count inside the window"
    );
    assert!(
        blocked > 50,
        "the task blocked {blocked} times, too few to judge"
    );
    assert!(
        counted >= blocked as f64 / 2.0 && counted <= blocked as f64 * 1.5 + 20.0,
        "the cgroup's switches are the task inside's, {blocked}, and not the task beside's too: \
         {counted}"
    );
}

/// A container no runtime knows is refused before anything is attached, with
/// what each runtime asked said; and a machine with no runtime at all says so,
/// and what to pass instead.
#[test]
fn a_container_no_runtime_knows_is_refused_with_what_it_said() {
    let bin = std::env::temp_dir().join(format!("omn-runtime-none-{}", std::process::id()));
    let runtime = runtime_knowing(&bin, &[]);
    let asked = |runtime: &str| {
        Command::new(BINARY)
            .args([
                "trace",
                "mem.cgroup",
                "--container",
                "omn-no-such-container",
                "-m",
                "poll",
                "-e",
                "raw",
                "-d",
                "200ms",
            ])
            .env("OMN_CONTAINER_RUNTIME", runtime)
            .output()
            .expect("omn runs")
    };

    let output = asked(runtime.to_str().expect("a path"));
    let complaint = stderr(&output);
    assert_eq!(output.status.code(), Some(EXIT_UNAVAILABLE), "{complaint}");
    assert!(
        complaint.contains("no container named `omn-no-such-container`")
            && complaint.contains("No such object: omn-no-such-container"),
        "the refusal names the container and what the runtime said: {complaint}"
    );

    let output = asked("/nonexistent/omn-runtime");
    let complaint = stderr(&output);
    let _ = std::fs::remove_dir_all(&bin);
    assert_eq!(output.status.code(), Some(EXIT_UNAVAILABLE), "{complaint}");
    assert!(
        complaint.contains("none of `/nonexistent/omn-runtime` is installed")
            && complaint.contains("--cgroup"),
        "a machine with no runtime says so, and what to pass instead: {complaint}"
    );
}

/// Both session indexes hold exactly the sessions on disk — no more and no
/// fewer.
///
/// `examples/README.md` and `GUIDE.md` belong to nobody, which is how they came
/// to be twenty-eight sessions short after the sixth wave: every stream wrote
/// sessions and no stream owned the list. The commit that fixed the count said
/// both were "checked to hold exactly the sessions on disk", and no test
/// performed that check — so the claim was true for one afternoon and then
/// drifted again by ten.
///
/// Both directions matter and they fail differently. A session missing from an
/// index is a demonstration nobody finds; a row naming a file that does not
/// exist is a reader typing a command that cannot run.
#[test]
fn the_session_indexes_hold_exactly_the_sessions_on_disk() {
    let on_disk: std::collections::BTreeSet<String> = std::fs::read_dir("examples")
        .expect("the examples directory ships")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().into_owned();
            (name.ends_with(".yml") || name.ends_with(".yaml") || name.ends_with(".toml"))
                .then_some(name)
        })
        .collect();
    assert!(on_disk.len() > 100, "only {} sessions found", on_disk.len());

    for index in ["examples/README.md", "GUIDE.md"] {
        let text = std::fs::read_to_string(index).expect("the index ships");
        // A row is `| `<file>` | what it answers |`, which is the convention
        // both files already followed.
        let listed: std::collections::BTreeSet<String> = text
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("| `")?;
                let name = rest.split('`').next()?;
                // A row naming `sched-*.yml` is a prerequisite table saying
                // "every session of this family", not an index entry. A glob
                // is not a session name, here as in the catalog.
                (!name.contains('*')
                    && (name.ends_with(".yml")
                        || name.ends_with(".yaml")
                        || name.ends_with(".toml")))
                .then(|| name.to_owned())
            })
            .collect();

        let missing: Vec<_> = on_disk.difference(&listed).collect();
        assert!(
            missing.is_empty(),
            "{index} does not list {} of the sessions on disk: {missing:?}",
            missing.len()
        );
        let invented: Vec<_> = listed.difference(&on_disk).collect();
        assert!(
            invented.is_empty(),
            "{index} names {} sessions that do not exist: {invented:?}",
            invented.len()
        );
    }
}

/// A scratch directory of this test binary's own, removed when the test
/// that made it ends, passing or not.
///
/// The directories used to outlive their test: `no-shim` holds a copy of the
/// binary, 154 MB, and a run's worth of them filled a per-user `/tmp` quota.
/// A run killed before its guards drop still leaves its own, so each new one
/// first removes those of test processes that are gone.
struct Scratch(std::path::PathBuf);

impl std::ops::Deref for Scratch {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for Scratch {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    sweep_dead_scratch();
    let dir = std::env::temp_dir().join(format!("omn-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    Scratch(dir)
}

/// Removes the `omn-cli-<pid>-*` directories whose process no longer exists.
fn sweep_dead_scratch() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix("omn-cli-"))
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(pid, _)| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The ground truth of the guard: a scratch directory is gone once the value
/// that names it is, and one left by a process that no longer exists is
/// removed by the next one made.
#[test]
fn a_scratch_directory_does_not_outlive_its_test() {
    let path = {
        let dir = scratch("guard");
        std::fs::write(dir.join("file"), b"x").unwrap();
        dir.to_path_buf()
    };
    assert!(!path.exists(), "{} outlived its test", path.display());

    // A pid above the kernel's maximum names no process.
    let dead = std::env::temp_dir().join("omn-cli-4194305-leaked");
    std::fs::create_dir_all(&dead).unwrap();
    let _fresh = scratch("sweep");
    assert!(!dead.exists(), "{} was left behind", dead.display());
}

/// What a command prints, trimmed: the machine's answer, asked outside `omn`.
fn command_line(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

/// Phase 2's exit criterion, held to the machine: `--dump-resolved io.latency
/// --pid <a live pid>` prints a complete `ResolvedRequest` -- every field the
/// struct has reads back, and nothing else is there -- and what it says of the
/// machine is what the machine says of itself when asked another way: the
/// release from `uname`, the cpu count from `nproc`, sysfs's own files, and a
/// backend that is the first of its chain whose requirements were answered.
#[test]
fn the_resolved_request_for_a_live_pid_is_complete_and_says_what_the_machine_says() {
    let mut child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("a live process");
    let pid = child.id().to_string();
    let output = run(&[
        "--dump-resolved",
        "io.latency",
        "--pid",
        &pid,
        "-m",
        "trace",
        "-e",
        "hist",
        "--format",
        "json",
    ]);
    let _ = child.kill();
    let _ = child.wait();
    if output.status.code() == Some(EXIT_UNAVAILABLE) {
        eprintln!(
            "skipped: this machine cannot serve io.latency --pid: {}",
            stderr(&output)
        );
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let dumped: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json");
    let dumped = if dumped.is_array() {
        dumped[0].clone()
    } else {
        dumped
    };

    // Complete: the struct reads it back, and writes back the same document.
    let typed: omn::resolve::ResolvedRequest =
        serde_json::from_value(dumped.clone()).expect("a ResolvedRequest");
    assert_eq!(serde_json::to_value(&typed).unwrap(), dumped);
    for field in ["request", "backend", "chain", "environment"] {
        assert!(dumped.get(field).is_some(), "no `{field}` in {dumped}");
    }
    // The optional ones are absent exactly when they say nothing.
    assert!(typed.filter.is_none() && dumped.get("filter").is_none());
    assert!(typed.stack.is_none(), "io.latency takes no stacks");
    assert_eq!(typed.request.target.first_pid(), Some(pid.parse().unwrap()));

    // The machine, asked outside the tool.
    let environment = &typed.environment;
    if let Some(release) = command_line("uname", &["-r"]) {
        assert_eq!(environment.kernel_release, release);
        let mut numbers = release.split(|c: char| !c.is_ascii_digit());
        let major: u32 = numbers.next().unwrap().parse().unwrap();
        let minor: u32 = numbers.next().unwrap().parse().unwrap();
        let version = environment.kernel_version.expect("a parsed release");
        assert_eq!((version.major, version.minor), (major, minor));
    }
    if let Some(arch) = command_line("uname", &["-m"]) {
        assert_eq!(environment.arch, arch);
    }
    if let Some(cpus) = command_line("nproc", &[]) {
        assert_eq!(environment.cpus.to_string(), cpus);
    }
    assert_eq!(
        environment.clocksource,
        read_trimmed("/sys/devices/system/clocksource/clocksource0/current_clocksource")
    );
    assert_eq!(
        environment.cpu_governor,
        read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
    );
    assert_eq!(
        environment.smt,
        read_trimmed("/sys/devices/system/cpu/smt/control")
    );
    assert_eq!(
        environment.perf_event_paranoid,
        read_trimmed("/proc/sys/kernel/perf_event_paranoid").and_then(|value| value.parse().ok())
    );
    if let Some(line) = read_trimmed("/sys/kernel/mm/transparent_hugepage/enabled") {
        let selected = line
            .split('[')
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .map(str::to_owned);
        assert_eq!(environment.transparent_hugepage, selected);
    }
    let vulnerabilities: std::collections::BTreeMap<String, String> =
        std::fs::read_dir("/sys/devices/system/cpu/vulnerabilities")
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let text = std::fs::read_to_string(entry.path()).ok()?;
                Some((
                    entry.file_name().to_string_lossy().into_owned(),
                    text.trim().to_owned(),
                ))
            })
            .collect();
    assert_eq!(environment.mitigations, vulnerabilities);

    // The backend: the first of the chain nothing ruled out, every one above
    // it rejected with a reason, every one below it not reached, and each of
    // its requirements answered available.
    let selected = typed
        .chain
        .iter()
        .position(|considered| considered.verdict == omn::resolve::Verdict::Selected)
        .expect("a selected backend in the chain");
    assert_eq!(typed.chain[selected].id, typed.backend.id);
    assert_eq!(typed.backend.rank, selected + 1);
    for considered in &typed.chain[..selected] {
        assert!(
            matches!(&considered.verdict, omn::resolve::Verdict::Rejected(reasons) if !reasons.is_empty()),
            "{considered:?} is above the selected backend and was not rejected with a reason"
        );
    }
    for considered in &typed.chain[selected + 1..] {
        assert_eq!(
            considered.verdict,
            omn::resolve::Verdict::NotReached,
            "{considered:?}"
        );
    }
    let catalog = omn::Catalog::embedded().unwrap();
    let entry = catalog.get(&"io.latency".parse().unwrap()).unwrap();
    let chain: Vec<&str> = entry.backends.iter().map(|b| b.id.as_str()).collect();
    let dumped_chain: Vec<&str> = typed.chain.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        dumped_chain, chain,
        "the chain is the catalog's, in its order"
    );
    for requirement in &typed.backend.requirements {
        assert!(
            requirement.probe.is_available() || requirement.probe.is_unknown(),
            "{requirement:?}"
        );
        // Two of them the test can ask the machine about itself.
        if requirement.needs == "btf" {
            assert!(
                std::fs::File::open("/sys/kernel/btf/vmlinux").is_ok(),
                "`btf` answered available and /sys/kernel/btf/vmlinux does not open"
            );
        }
        if let Some(floor) = requirement.needs.strip_prefix("kernel>=") {
            let mut floor = floor.split('.').map(|part| part.parse::<u32>().unwrap());
            let floor = (floor.next().unwrap(), floor.next().unwrap_or(0));
            let version = environment.kernel_version.unwrap();
            assert!((version.major, version.minor) >= floor, "{requirement:?}");
        }
    }
}

/// A program whose allocations this file decided: exactly 1000 of 77777
/// bytes, each freed, and it says so itself.
const ALLOCS: &str = r#"
#include <stdio.h>
#include <stdlib.h>
int main(void) {
    int made = 0;
    for (int i = 0; i < 1000; i++) {
        volatile char *p = malloc(77777);
        if (!p) return 1;
        p[0] = 1;
        free((void *)p);
        made++;
    }
    fprintf(stderr, "ALLOCS %d\n", made);
    return 0;
}
"#;

/// The summary of the one result of a run's `results.json`, without the key
/// (a pid, which differs between two runs of one program).
fn allocation_summary(dir: &std::path::Path) -> (serde_json::Value, serde_json::Value, u64) {
    let results: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("results.json")).unwrap()).unwrap();
    let result = &results["results"][0];
    let mut series = result["outputs"][0]["series"][0].clone();
    series.as_object_mut().unwrap().remove("key");
    (
        series,
        result["filtered"].clone(),
        result["events_seen"].as_u64().unwrap(),
    )
}

/// Phase 7's exit criterion, held to a workload of known count: a run from
/// the command line and a run of the session file that command line dumps
/// give the same numbers, and both are the program's own -- 1000
/// allocations of 77777 bytes, and the 1000 frees the filter set aside. The
/// shim sees every call, so the workload's resolution is one allocation, and
/// the two runs agree to it.
#[test]
fn a_session_dumped_from_the_command_line_measures_what_the_command_line_measured() {
    let dir = scratch("replay");
    let source = dir.join("allocs.c");
    std::fs::write(&source, ALLOCS).unwrap();
    let program = dir.join("allocs");
    let built = Command::new("cc")
        .arg("-O0")
        .arg("-o")
        .arg(&program)
        .arg(&source)
        .status();
    if !built.is_ok_and(|status| status.success()) {
        eprintln!("skipped: no C compiler (`cc`) to build the workload with");
        return;
    }
    let program = program.to_string_lossy().into_owned();
    let asked = [
        "mem.heap.alloc",
        "-m",
        "trace",
        "-e",
        "summary",
        "--filter",
        "bytes == 77777",
        "--format",
        "json",
    ];

    let from_cli = dir.join("cli");
    let mut args: Vec<&str> = asked.to_vec();
    let out = from_cli.to_string_lossy().into_owned();
    args.extend(["-o", &out, "--", &program]);
    let cli = run(&args);
    if !cli.status.success() && stderr(&cli).contains("preload") {
        eprintln!(
            "skipped: the preload shim is not here to count allocations: {}",
            stderr(&cli)
        );
        return;
    }
    assert!(cli.status.success(), "{}", stderr(&cli));
    assert!(stderr(&cli).contains("ALLOCS 1000"), "{}", stderr(&cli));

    let mut args: Vec<&str> = asked.to_vec();
    args.extend(["--dump-request", "--", &program]);
    let dumped = run(&args);
    assert!(dumped.status.success(), "{}", stderr(&dumped));
    let session = dir.join("session.json");
    std::fs::write(&session, stdout(&dumped)).unwrap();

    let from_file = dir.join("file");
    let replay = run(&[
        "-f",
        &session.to_string_lossy(),
        "-o",
        &from_file.to_string_lossy(),
    ]);
    assert!(replay.status.success(), "{}", stderr(&replay));
    assert!(
        stderr(&replay).contains("ALLOCS 1000"),
        "{}",
        stderr(&replay)
    );

    let (cli_series, cli_filtered, cli_seen) = allocation_summary(&from_cli);
    let (file_series, file_filtered, file_seen) = allocation_summary(&from_file);
    // The program's own numbers, first.
    assert_eq!(cli_series["count"], 1000, "{cli_series}");
    assert_eq!(cli_series["min"], 77777.0, "{cli_series}");
    assert_eq!(cli_series["max"], 77777.0, "{cli_series}");
    assert_eq!(cli_filtered["kept"], 1000, "{cli_filtered}");
    // Then the replay's, against the command line's.
    assert_eq!(file_series, cli_series);
    assert_eq!(file_filtered, cli_filtered);
    assert_eq!(file_seen, cli_seen);
    // And the two resolved files resolved the same request the same way.
    let resolved = |dir: &std::path::Path| -> serde_json::Value {
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("resolved.json")).unwrap())
                .unwrap();
        let value = if value.is_array() {
            value[0].clone()
        } else {
            value
        };
        serde_json::json!([value["request"], value["backend"]["id"]])
    };
    assert_eq!(resolved(&from_file), resolved(&from_cli));
}

/// A copy of the catalog directory with one line changed, in `OMN_CATALOG_DIR`.
fn patched_catalog(name: &str, file: &str, from: &str, to: &str) -> Scratch {
    let dir = scratch(name);
    let mut patched = false;
    for entry in std::fs::read_dir("catalog").expect("the catalog directory") {
        let path = entry.unwrap().path();
        // `catalog/references/` holds the test-only reference pairs.
        if !path.is_file() {
            continue;
        }
        let mut text = std::fs::read_to_string(&path).unwrap();
        if path.file_name().is_some_and(|n| n == file) {
            assert!(text.contains(from), "`{from}` is not in {file}");
            text = text.replacen(from, to, 1);
            patched = true;
        }
        std::fs::write(dir.join(path.file_name().unwrap()), text).unwrap();
    }
    assert!(patched, "no {file} in the catalog");
    dir
}

/// A request keyed on a key the backend does not attach is refused, with
/// the backend that does. The ground truth is the kernel's file:
/// `/proc/diskstats` has a line per device and no task in it, and a run keyed
/// on `task` through it today produces events carrying `dev` alone -- which a
/// join over `task` would attribute nothing to. Declared (`keys: [dev]` on
/// `fs:proc`), the same request is refused before anything runs.
#[test]
fn a_request_keyed_on_what_its_backend_does_not_attach_is_refused_with_the_backend_that_does() {
    if std::fs::metadata("/proc/diskstats").is_err() {
        eprintln!("skipped: no /proc/diskstats here");
        return;
    }
    let args = [
        "io.bytes",
        "--system",
        "--backend",
        "force:fs:proc",
        "-k",
        "task",
        "-m",
        "poll@50ms",
        "-e",
        "raw",
        "-d",
        "200ms",
        "--format",
        "json",
    ];
    // Undeclared: accepted, and no event carries a task. Once the shipped
    // catalog declares it the run is refused here already, which is the
    // half below.
    let undeclared = run(&args);
    if stderr(&undeclared).contains("does not attach") {
        let message = stderr(&undeclared);
        // The hint and the pin give one answer for the backend they name
        // (`DECISIONS.md`, "one order for unbuilt and missing"): the machine's
        // where that carrier lacks something here -- the capabilities, on a
        // binary that does not carry them -- and the build's where it lacks
        // nothing. Which carrier is named is the probes' to say (an unwritten
        // one this machine can run ranks above one it cannot), so it is read
        // off the message rather than written down.
        let named = |answer: &str| {
            let end = message.find(&format!("` does, and {answer}"))?;
            let start = message[..end].rfind('`')? + 1;
            Some(message[start..end].to_owned())
        };
        if let Some(carrier) = named("this machine cannot run it") {
            assert_eq!(
                undeclared.status.code(),
                Some(EXIT_UNAVAILABLE),
                "{message}"
            );
            let mut pinned = args.to_vec();
            let forced_arg = format!("force:{carrier}");
            pinned[3] = &forced_arg;
            let forced = run(&pinned);
            assert_eq!(
                forced.status.code(),
                Some(EXIT_UNAVAILABLE),
                "the hint called `{carrier}` the machine's, and pinning it said: {}",
                stderr(&forced)
            );
            return;
        }
        let carrier = named("this build has no collector for it").unwrap_or_else(|| {
            panic!("the refusal names neither the machine's nor the build's carrier: {message}")
        });
        assert_eq!(
            undeclared.status.code(),
            Some(EXIT_NOT_IMPLEMENTED),
            "{message}"
        );
        let mut pinned = args.to_vec();
        let forced_arg = format!("force:{carrier}");
        pinned[3] = &forced_arg;
        let forced = run(&pinned);
        assert!(
            forced.status.code() == Some(EXIT_NOT_IMPLEMENTED)
                && stderr(&forced).contains("no collector is written"),
            "the hint called `{carrier}` unwritten, and pinning it said: {}",
            stderr(&forced)
        );
        return;
    }
    assert!(undeclared.status.success(), "{}", stderr(&undeclared));
    let results: serde_json::Value = serde_json::from_str(&stdout(&undeclared)).unwrap();
    let events = results["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("raw events")
        .clone();
    assert!(!events.is_empty(), "no line of /proc/diskstats was read");
    for event in &events {
        let keys = event["keys"].as_array().unwrap();
        assert!(
            keys.iter().all(|key| key.get("task").is_none()),
            "/proc/diskstats named a task: {event}"
        );
    }

    // Declared: refused, naming a backend that attaches it, and the refusal
    // is held to what following it does. A `--backend force:` hint must lead
    // to a run this build does not refuse as unwritten; a backend named as
    // unwritten must be one the runner itself refuses as unwritten.
    let catalog = patched_catalog(
        "keys",
        "io.yaml",
        "targets: {system: native}, filter_fields: [dev],\n       degraded: \"system-wide, per device\"}\n  modes: [count, trace, poll]",
        "targets: {system: native}, filter_fields: [dev], keys: [dev],\n       degraded: \"system-wide, per device\"}\n  modes: [count, trace, poll]",
    );
    let refused = Command::new(BINARY)
        .env("OMN_CATALOG_DIR", &catalog)
        .args(args)
        .output()
        .unwrap();
    let message = stderr(&refused);
    assert!(
        message.contains("keyed on `task`, which the selected backend `fs:proc` does not attach"),
        "{message}"
    );
    assert!(stdout(&refused).is_empty(), "{}", stdout(&refused));
    let named = message
        .lines()
        .find_map(|line| line.trim_start().strip_prefix('`'))
        .and_then(|rest| rest.split('`').next())
        .expect("a backend that attaches `task` is named")
        .to_owned();
    let forced_arg = format!("force:{named}");
    let mut forced_args = args.to_vec();
    forced_args[3] = forced_arg.as_str();
    let forced = Command::new(BINARY)
        .env("OMN_CATALOG_DIR", &catalog)
        .args(&forced_args)
        .output()
        .unwrap();
    let unwritten = |output: &std::process::Output| {
        output.status.code() == Some(EXIT_NOT_IMPLEMENTED)
            && stderr(output).contains("no collector is written")
    };
    if message.contains(&format!("--backend force:{named}")) {
        assert_eq!(refused.status.code(), Some(EXIT_REJECTED), "{message}");
        assert!(
            !unwritten(&forced),
            "the hint `--backend force:{named}` leads to an unwritten backend: {}",
            stderr(&forced)
        );
    } else if message.contains("has no collector for it") {
        assert_eq!(
            refused.status.code(),
            Some(EXIT_NOT_IMPLEMENTED),
            "an unwritten backend is the build's gap: {message}"
        );
        assert!(
            unwritten(&forced),
            "`{named}` was called unwritten, and forcing it said: {}",
            stderr(&forced)
        );
    } else {
        assert!(
            message.contains("this machine cannot run it"),
            "neither a pin, an unwritten backend nor a missing one: {message}"
        );
        assert_eq!(refused.status.code(), Some(EXIT_UNAVAILABLE), "{message}");
    }
}

/// A chain that ends on a backend nobody wrote leads its refusal with the
/// backend passed over on the way, when this build collects that one: the
/// line the reader can act on. The ground truth is the file system: a copy of
/// the binary in a directory with no preload shim beside it, which is what
/// `preload` needs (`preload:shim`).
#[test]
fn a_refusal_for_an_unwritten_backend_leads_with_the_one_passed_over() {
    let dir = scratch("no-shim");
    let copy = dir.join("omn");
    std::fs::copy(BINARY, &copy).expect("a copy of the binary");
    assert!(!dir.join("libomn_preload.so").exists());
    let output = Command::new(&copy)
        .env_remove("OMN_PRELOAD_LIB")
        .args([
            "mem.heap.alloc",
            "-m",
            "trace",
            "-e",
            "summary",
            "--",
            "true",
        ])
        .output()
        .unwrap();
    let message = stderr(&output);
    // Which backend the chain ends on is this machine's: only a chain ending
    // on one with no collector is this refusal.
    if !message.contains("has no collector written yet") {
        eprintln!("skipped: the chain did not end on an unwritten backend here: {message}");
        return;
    }
    let first = message.lines().next().unwrap_or_default();
    assert!(
        first.starts_with(
            "error: `mem.heap.alloc` has no viable backend on this machine: `preload`, which \
             this build collects, was passed over"
        ),
        "{message}"
    );
    assert!(first.contains("libomn_preload.so"), "{message}");
    assert!(
        !message.contains("`preload` was passed over"),
        "the lead is said twice: {message}"
    );
    assert_eq!(output.status.code(), Some(EXIT_UNAVAILABLE), "{message}");
}

/// A catalog directory carries its unwind modes: the table beside the entries
/// is the one the choice is made from, and the one the resolved file names.
#[test]
fn the_unwind_modes_are_read_from_the_catalog_directory_they_sit_in() {
    let catalog = patched_catalog(
        "unwind",
        "unwind.yaml",
        "degraded: \"the caller of a function that sets up no frame pointer is lost",
        "degraded: \"PATCHED IN THIS DIRECTORY: the caller of a function that sets up no frame pointer is lost",
    );
    let dump = |dir: Option<&std::path::Path>| -> Output {
        let mut command = Command::new(BINARY);
        if let Some(dir) = dir {
            command.env("OMN_CATALOG_DIR", dir);
        }
        command
            .args([
                "--dump-resolved",
                "cpu.profile",
                "--stack",
                "fp",
                "--emit",
                "stack",
            ])
            .args(["--mode", "sample@99hz", "--format", "json", "--", "true"])
            .output()
            .unwrap()
    };
    let patched = dump(Some(&catalog));
    if stderr(&patched).contains("no viable backend") {
        eprintln!(
            "skipped: this machine would not profile: {}",
            stderr(&patched)
        );
        return;
    }
    assert!(patched.status.success(), "{}", stderr(&patched));
    assert!(
        stdout(&patched).contains("PATCHED IN THIS DIRECTORY"),
        "{}",
        stdout(&patched)
    );
    let embedded = dump(None);
    assert!(embedded.status.success(), "{}", stderr(&embedded));
    assert!(!stdout(&embedded).contains("PATCHED"));
}

/// A target and a mode the entry declares, which no one backend of its chain
/// declares together, is the request's to change: exit 2, before anything
/// runs, rather than "this machine cannot serve it", which sent a reader after
/// privileges no machine would have made enough. The pairs are read from the
/// catalog's own declarations, backend by backend.
///
/// The validator refuses the pair before resolution (`check_pair`), and the
/// resolver's `NoBackendDeclares` stays behind it for a chain the validator
/// let through. Either is the same refusal, the request's, and both say "no
/// backend of its chain declares": whichever answers is asserted, never
/// skipped on, since no machine could make it go away.
#[test]
fn a_target_and_mode_no_backend_declares_together_is_refused_as_the_request_s() {
    use omn::catalog::TargetSupport;
    use omn::request::TargetKind;
    let catalog = omn::Catalog::embedded().unwrap();
    let mut pairs = Vec::new();
    for entry in catalog.entries() {
        for kind in [TargetKind::Cgroup, TargetKind::System] {
            if entry.target_support(kind) == TargetSupport::No {
                continue;
            }
            for mode in &entry.modes {
                let together = entry.backends.iter().any(|backend| {
                    entry.backend_target_support(backend, kind) != TargetSupport::No
                        && entry.backend_modes(backend).contains(mode)
                });
                if !together {
                    pairs.push((entry.id.to_string(), kind, *mode, entry.emits[0]));
                }
            }
        }
    }
    // The catalog, not the machine, decides whether such a pair exists: an
    // empty list is this test losing its subject, which a skip would hide.
    let Some((source, kind, mode, emit)) = pairs.first() else {
        panic!(
            "every pair an entry declares now has a backend that serves it: \
             this test has no request left to refuse, replace or remove it"
        );
    };
    let mode = match mode.as_str() {
        "poll" => "poll@50ms".to_owned(),
        "sample" => "sample@99hz".to_owned(),
        other => other.to_owned(),
    };
    let mut args = vec![source.as_str()];
    let cgroup;
    match kind {
        TargetKind::Cgroup => {
            cgroup = "/sys/fs/cgroup".to_owned();
            args.extend(["--cgroup", &cgroup]);
        }
        _ => args.push("--system"),
    }
    let emit = emit.as_str();
    args.extend(["-m", &mode, "-e", emit, "-d", "100ms", "--dump-resolved"]);
    let output = run(&args);
    let message = stderr(&output);
    assert_ne!(
        output.status.code(),
        Some(EXIT_UNAVAILABLE),
        "`{}`: a pair no backend declares was answered as the machine's: {message}",
        args.join(" ")
    );
    assert_eq!(
        output.status.code(),
        Some(EXIT_REJECTED),
        "`{}`: {message}",
        args.join(" ")
    );
    // A refusal of the emit or the shape is exit 2 as well, and says
    // something else: only the pair's refusal is under test.
    assert!(
        message.contains("no backend of its chain declares"),
        "`{}` was refused, but not for its pair: {message}",
        args.join(" ")
    );
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
}

/// The sources a family table's Metric cell names: the first in full, and
/// each `.x` after it as the family's `x` or, where the family has no such
/// source, the first id's stem before its last `_` with `_x` (`mem.cache_l1`
/// / `.llc` is `mem.cache_llc`).
fn row_sources(cell: &str, catalog: &omn::Catalog) -> Vec<String> {
    let named: Vec<&str> = cell
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|token| !token.is_empty())
        .collect();
    let Some(first) = named.first() else {
        return Vec::new();
    };
    let exists = |id: &str| {
        id.parse::<omn::request::SourceId>()
            .ok()
            .is_some_and(|id| catalog.get(&id).is_some())
    };
    let family = first.split('.').next().unwrap_or_default();
    let mut ids = vec![(*first).to_owned()];
    for short in &named[1..] {
        let Some(rest) = short.strip_prefix('.') else {
            continue;
        };
        let in_family = format!("{family}.{rest}");
        let beside = first
            .rsplit_once('_')
            .map(|(stem, _)| format!("{stem}_{rest}"));
        match beside.filter(|beside| !exists(&in_family) && exists(beside)) {
            Some(beside) => ids.push(beside),
            None => ids.push(in_family),
        }
    }
    ids
}

/// The tiers a chain declares, in chain order and without repeats, written as
/// `CATALOG.md`'s Cost column writes them.
fn chain_tiers<'a>(entries: impl IntoIterator<Item = &'a omn::catalog::Entry>) -> String {
    let mut tiers: Vec<String> = Vec::new();
    for entry in entries {
        for backend in &entry.backends {
            let tier = backend.cost.to_string();
            if !tiers.contains(&tier) {
                tiers.push(tier);
            }
        }
    }
    tiers.join(" / ")
}

/// Each family table's Cost cell in `CATALOG.md` is the catalog's chain
/// written out: the tiers its backends declare, in order and without repeats.
/// A row naming several sources holds each one's chain to it, or their chains
/// end to end. A row naming a source the catalog does not hold is named too:
/// the table would describe something nothing resolves.
#[test]
fn the_catalog_document_s_cost_column_is_the_catalog_s_chain() {
    let catalog = omn::Catalog::embedded().expect("the embedded catalog loads");
    let document = std::fs::read_to_string("CATALOG.md").expect("CATALOG.md ships");
    // The Cost column of the table being read; the Metric column is first.
    let mut cost_column: Option<usize> = None;
    let mut checked = 0;
    let mut wrong: Vec<String> = Vec::new();
    for (number, line) in document.lines().enumerate() {
        let cells: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        if !line.starts_with('|') {
            cost_column = None;
            continue;
        }
        if cells.first() == Some(&"Metric") {
            cost_column = cells.iter().position(|cell| *cell == "Cost");
            continue;
        }
        let Some(cost) = cost_column else {
            continue;
        };
        let (Some(named), Some(cell)) = (cells.first(), cells.get(cost)) else {
            continue;
        };
        if named.starts_with("---") {
            continue;
        }
        let ids = row_sources(named, &catalog);
        if ids.is_empty() {
            continue;
        }
        let mut entries = Vec::new();
        for id in &ids {
            match id
                .parse::<omn::request::SourceId>()
                .ok()
                .and_then(|parsed| catalog.get(&parsed))
            {
                Some(entry) => entries.push(entry),
                None => wrong.push(format!(
                    "CATALOG.md:{}: `{id}` is in no catalog file",
                    number + 1
                )),
            }
        }
        if entries.len() != ids.len() {
            continue;
        }
        let each_alike = entries.iter().all(|entry| chain_tiers([*entry]) == *cell);
        let end_to_end = chain_tiers(entries.iter().copied()) == *cell;
        checked += 1;
        if !each_alike && !end_to_end {
            let declared: Vec<String> = entries
                .iter()
                .map(|entry| format!("{}: {}", entry.id, chain_tiers([*entry])))
                .collect();
            wrong.push(format!(
                "CATALOG.md:{}: Cost is `{cell}`, and the chain declares {}",
                number + 1,
                declared.join("; ")
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    assert!(checked >= 80, "only {checked} rows read: the tables moved");
}
