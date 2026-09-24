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
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("kept 0 of"), "{text}");
    assert!(
        text.contains("no event carried `state`"),
        "an empty series must explain itself: {text}"
    );
}

/// A source the catalog says can be system-wide, whose collector can only watch
/// one process, has to say which of the two is missing. Watching one process
/// and labelling the result "system" is the failure this refusal exists for.
#[test]
fn a_collector_that_needs_a_process_refuses_a_system_target_with_the_reason() {
    let output = run(&[
        "mem.faults",
        "--system",
        "--mode",
        "sample@997hz",
        "--emit",
        "raw",
        "--duration",
        "200ms",
    ]);
    assert!(!output.status.success());
    let complaint = stderr(&output);
    assert!(complaint.contains("mem.faults"), "{complaint}");
    assert!(
        complaint.contains("per-cpu events") && complaint.contains("names no process"),
        "the refusal should say what a system-wide sampler would need: {complaint}"
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
