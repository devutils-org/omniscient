//! The flight recorder: a session whose duration is *until a predicate
//! fires*, with a retention bound and one document per trigger.
//!
//! Ground truth is a workload that reports on its own `CLOCK_MONOTONIC`: it
//! idles, opens a known number of descriptors at a moment it stamps, holds
//! them, closes them, idles again and does it a second time. `proc.fd` reads
//! the count exactly -- it is a directory walk, not an estimate -- so each
//! window's burst is checked to the descriptor, and each window's position
//! against the workload's own stamps rather than against anything the tool
//! computed. The workload is this test binary, relaunched with a variable
//! that makes it one, so nothing has to be installed.
//!
//! `docs/recorder.md` is what these measured.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// How the workload is told what to do: `idle_ms:fds:hold_ms:gap_ms:tail_ms`.
const BURSTS_VAR: &str = "OMN_RECORDER_BURSTS";
/// Where it writes what it did, as `name=ns` pairs on its own clock.
const REPORT_VAR: &str = "OMN_RECORDER_REPORT";

/// The descriptors each burst opens: far above anything the idle program
/// holds, so the trigger's threshold separates the two with room either side.
const BURST_FDS: u64 = 300;
/// The threshold, half way.
const TRIGGER: &str = "value >= 150";
const POLL_MS: u64 = 50;
const RETAIN_MS: u64 = 1_000;
const AFTER_MS: u64 = 600;

fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a timespec this frame owns.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// The workload, when this binary was launched as one; a no-op otherwise.
///
/// Two bursts: `fds` descriptors opened on `/dev/null`, stamped before the
/// first open and after the last, held `hold_ms`, stamped again before the
/// first close and after the last.
#[test]
fn workload_runs_only_when_this_binary_was_launched_as_a_workload() {
    let Ok(spec) = std::env::var(BURSTS_VAR) else {
        return;
    };
    let numbers: Vec<u64> = spec
        .split(':')
        .map(|number| number.parse().expect("a number"))
        .collect();
    let [idle_ms, fds, hold_ms, gap_ms, tail_ms] = numbers[..] else {
        panic!("a workload is `idle_ms:fds:hold_ms:gap_ms:tail_ms`, not `{spec}`");
    };
    let sleep = |ms: u64| std::thread::sleep(std::time::Duration::from_millis(ms));
    let mut report = vec![("started".to_owned(), now_ns())];
    sleep(idle_ms);
    for burst in 1..=2 {
        if burst == 2 {
            sleep(gap_ms);
        }
        report.push((format!("burst{burst}_opening"), now_ns()));
        let opened: Vec<std::fs::File> = (0..fds)
            .map(|_| std::fs::File::open("/dev/null").expect("/dev/null opens"))
            .collect();
        report.push((format!("burst{burst}_opened"), now_ns()));
        sleep(hold_ms);
        report.push((format!("burst{burst}_closing"), now_ns()));
        drop(opened);
        report.push((format!("burst{burst}_closed"), now_ns()));
    }
    sleep(tail_ms);
    report.push(("ended".to_owned(), now_ns()));
    let text: String = report
        .iter()
        .map(|(name, ns)| format!("{name}={ns}\n"))
        .collect();
    std::fs::write(std::env::var(REPORT_VAR).expect("a report path"), text)
        .expect("the report is written");
}

/// How the spinning workload is told what to do: `fds:spin_ms:tail_ms`.
const SPIN_VAR: &str = "OMN_RECORDER_SPIN";

fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a timespec this frame owns.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// The second workload: `fds` descriptors held for the whole run, which is
/// what makes each read of them cost something, then `spin_ms` of its own
/// cpu time -- charged, not wall -- and then `tail_ms` of near idleness, a tenth of
/// a millisecond of cpu every hundred. It stamps when the spin ended.
#[test]
fn spinning_workload_runs_only_when_this_binary_was_launched_as_one() {
    let Ok(spec) = std::env::var(SPIN_VAR) else {
        return;
    };
    let numbers: Vec<u64> = spec
        .split(':')
        .map(|number| number.parse().expect("a number"))
        .collect();
    let [fds, spin_ms, tail_ms] = numbers[..] else {
        panic!("a spinning workload is `fds:spin_ms:tail_ms`, not `{spec}`");
    };
    let _held: Vec<std::fs::File> = (0..fds)
        .map(|_| std::fs::File::open("/dev/null").expect("/dev/null opens"))
        .collect();
    let spin = |ns: u64| {
        let from = thread_cpu_ns();
        while thread_cpu_ns() - from < ns {
            std::hint::spin_loop();
        }
    };
    spin(spin_ms * 1_000_000);
    let spun = now_ns();
    std::fs::write(
        std::env::var(REPORT_VAR).expect("a report path"),
        format!("spun={spun}\n"),
    )
    .expect("the report is written");
    let until = now_ns() + tail_ms * 1_000_000;
    while now_ns() < until {
        spin(100_000);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// What the workload said it did, on its own clock.
struct Truth(std::collections::BTreeMap<String, u64>);

impl Truth {
    fn read(path: &Path) -> Truth {
        let text = std::fs::read_to_string(path).expect("the workload wrote its report");
        Truth(
            text.lines()
                .filter_map(|line| {
                    let (name, ns) = line.split_once('=')?;
                    Some((name.to_owned(), ns.parse().ok()?))
                })
                .collect(),
        )
    }

    fn at(&self, name: &str) -> u64 {
        *self
            .0
            .get(name)
            .unwrap_or_else(|| panic!("the workload reported no `{name}`"))
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omn-recorder-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// The argv that launches this binary as the workload.
fn workload(spec: &str, report: &Path) -> String {
    let exe = std::env::current_exe().expect("this test binary has a path");
    format!(
        "[/bin/sh, -c, '{BURSTS_VAR}={spec} {REPORT_VAR}=\"$2\" exec \"$1\" --exact \
         --test-threads 1 workload_runs_only_when_this_binary_was_launched_as_a_workload \
         >/dev/null 2>&1', sh, {:?}, {:?}]",
        exe.display().to_string(),
        report.display().to_string()
    )
}

/// A recording session over the workload, with `record` as its block.
fn session(argv: &str, duration: &str, record: &str) -> String {
    format!(
        "duration: {duration}\n\
         requests:\n\
         \x20 - label: descriptors\n\
         \x20   source: proc.fd\n\
         \x20   target: {{launch: {argv}}}\n\
         \x20   mode: poll@{POLL_MS}ms\n\
         \x20   emit: [raw]\n\
         record:\n{record}"
    )
}

fn run(args: &[&str], stdin: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(BINARY)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn runs");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(stdin.as_bytes())
        .expect("the session is written");
    child.wait_with_output().expect("omn finishes")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn record_to(out: &Path, session: &str) -> Output {
    run(
        &[
            "-f",
            "-",
            "--input-format",
            "yaml",
            "--format",
            "json",
            "-o",
            out.to_str().expect("a utf-8 path"),
        ],
        session,
    )
}

fn json(path: &Path) -> Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn number(value: &Value) -> u64 {
    value
        .as_u64()
        .unwrap_or_else(|| panic!("not a number: {value}"))
}

/// Every `(ts_ns, open)` a document's raw output holds.
fn readings(document: &Value) -> Vec<(u64, u64)> {
    document["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("raw events")
        .iter()
        .map(|event| {
            let open = event["attrs"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|attr| attr["name"] == "open")
                .map(|attr| number(&attr["value"]))
                .expect("every proc.fd event carries `open`");
            (number(&event["ts_ns"]), open)
        })
        .collect()
}

/// The ground truth: two bursts, exactly two documents.
///
/// Each document's window reaches back exactly `retain` from the round that
/// fired and closes no earlier than `after` past it, so it holds the whole
/// burst -- its opening and its closing, on the workload's own clock -- and
/// the idle before it. Inside it the count is the idle baseline before the
/// workload's first open and the baseline plus 300 between its last open and
/// its first close, to the descriptor. What the retention evicted is counted
/// in rounds that fit the idle span the workload reports before the window,
/// at one event per round, and the second document counts only what came
/// after the first.
#[test]
fn two_bursts_write_exactly_two_documents_each_holding_its_burst() {
    let dir = scratch("two");
    let report = dir.join("report");
    let out = dir.join("out");
    let spec = format!("2500:{BURST_FDS}:400:2500:500");
    let text = session(
        &workload(&spec, &report),
        "30s",
        &format!(
            "  trigger: \"{TRIGGER}\"\n  on: descriptors\n  retain: {RETAIN_MS}ms\n  \
             after: {AFTER_MS}ms\n  documents: 5\n"
        ),
    );
    let output = record_to(&out, &text);
    let spoken = stderr(&output);
    assert!(output.status.success(), "{spoken}");
    let truth = Truth::read(&report);

    let recording = json(&out.join("recording.json"));
    let documents = recording["documents"].as_array().expect("documents");
    assert_eq!(
        documents.len(),
        2,
        "two bursts, two firings, and the bound of five left alone:\n{spoken}"
    );
    assert!(
        !out.join("trigger-3.json").exists(),
        "no third document on disk"
    );
    assert!(
        recording["stopped_because"]
            .as_str()
            .is_some_and(|why| why.contains("exited")),
        "the program's exit ended the run, not the bound: {}",
        recording["stopped_because"]
    );

    let mut previous_cut_ns = number(&recording["opened_ns"]);
    let mut baseline = None;
    for burst in 1..=2u64 {
        let document = json(&out.join(format!("trigger-{burst}.json")));
        let fired = &document["trigger"];
        assert_eq!(
            fired,
            &documents[burst as usize - 1],
            "the file and the record agree"
        );
        let fired_ns = number(&fired["fired_ns"]);
        let opened_ns = number(&fired["opened_ns"]);
        let closed_ns = number(&fired["closed_ns"]);
        let opening = truth.at(&format!("burst{burst}_opening"));
        let all_open = truth.at(&format!("burst{burst}_opened"));
        let closing = truth.at(&format!("burst{burst}_closing"));
        let closed = truth.at(&format!("burst{burst}_closed"));
        eprintln!(
            "document {burst}: fired {:.1} ms after the burst opened, window {:.1} ms before \
             it to {:.1} ms after it closed",
            fired_ns.saturating_sub(all_open) as f64 / 1e6,
            opening.saturating_sub(opened_ns) as f64 / 1e6,
            closed_ns as f64 / 1e6 - closed as f64 / 1e6,
        );

        // The trigger fired on the burst, on its own clock.
        assert!(
            fired_ns >= opening,
            "document {burst} fired at {fired_ns}, before the workload began opening at \
             {opening}"
        );
        assert!(
            fired_ns <= closing,
            "document {burst} fired at {fired_ns}, after the workload began closing at {closing}"
        );
        let event_ts = number(&fired["event_ts_ns"]);
        assert!(opening <= event_ts && event_ts <= fired_ns);
        // The retention bound, exactly: the window reaches back `retain`.
        assert_eq!(
            fired_ns - opened_ns,
            RETAIN_MS * 1_000_000,
            "the window reaches back exactly the retention bound"
        );
        assert!(closed_ns >= fired_ns + AFTER_MS * 1_000_000);
        // The window holds the whole burst, and the idle before it.
        assert!(
            opened_ns <= opening && closed <= closed_ns,
            "document {burst}'s window [{opened_ns}, {closed_ns}] does not hold the burst \
             [{opening}, {closed}]"
        );

        // To the descriptor, against the workload's own stamps.
        let readings = readings(&document);
        assert!(!readings.is_empty());
        assert!(readings
            .iter()
            .all(|(ts, _)| opened_ns <= *ts && *ts <= closed_ns));
        let idle: Vec<u64> = readings
            .iter()
            .filter(|(ts, _)| *ts < opening)
            .map(|(_, open)| *open)
            .collect();
        let held: Vec<u64> = readings
            .iter()
            .filter(|(ts, _)| all_open < *ts && *ts < closing)
            .map(|(_, open)| *open)
            .collect();
        assert!(
            !idle.is_empty() && !held.is_empty(),
            "the window holds readings of both the idle and the burst"
        );
        let base = idle[0];
        assert!(
            idle.iter().all(|open| *open == base),
            "the idle program holds a constant count: {idle:?}"
        );
        assert!(
            held.iter().all(|open| *open == base + BURST_FDS),
            "every reading between the last open and the first close is the baseline plus \
             {BURST_FDS}: baseline {base}, {held:?}"
        );
        assert_eq!(
            fired["value"].as_f64(),
            Some((base + BURST_FDS) as f64),
            "the value that fired is the burst's"
        );
        if let Some(first) = baseline {
            assert_eq!(base, first, "the two idles agree");
        }
        baseline = Some(base);

        // The eviction, against what the workload says it did before.
        let evicted = &fired["evicted"][0];
        let (events, rounds) = (number(&evicted["events"]), number(&evicted["rounds"]));
        assert_eq!(events, rounds, "one process, one event a round");
        let evicted_span_ns = opened_ns.saturating_sub(previous_cut_ns);
        let at_most = evicted_span_ns / (POLL_MS * 1_000_000) + 2;
        assert!(
            (1..=at_most).contains(&rounds),
            "document {burst} counts {rounds} evicted rounds over the {:.0} ms of idle before \
             its window, at one round every {POLL_MS} ms: between 1 and {at_most}",
            evicted_span_ns as f64 / 1e6
        );
        assert_eq!(
            number(&document["results"][0]["losses"]["evicted"]),
            events,
            "the document's losses count what its trigger record says was evicted"
        );
        assert!(
            document["results"][0]["losses"]["reasons"]
                .as_object()
                .is_some_and(|reasons| reasons.keys().any(|reason| reason.contains("evicted"))),
            "and say why"
        );
        previous_cut_ns = closed_ns;
    }
    // The first idle the workload reports is the one the first eviction
    // counts: it idled longer than the retention before its first burst.
    assert!(truth.at("burst1_opening") - truth.at("started") > RETAIN_MS * 1_000_000);
    assert!(out.join("resolved.json").exists() && out.join("cost.json").exists());
}

/// A trigger that never fires ends at the longest duration with no
/// document, and says so -- with what it read and put in none.
#[test]
fn a_trigger_that_never_fires_ends_at_the_longest_duration_with_no_document() {
    let dir = scratch("never");
    let report = dir.join("report");
    let out = dir.join("out");
    let spec = format!("100:{BURST_FDS}:100:100:8000");
    let text = session(
        &workload(&spec, &report),
        "2s",
        "  trigger: \"value >= 100000\"\n  retain: 500ms\n  documents: 3\n",
    );
    let started = std::time::Instant::now();
    let output = record_to(&out, &text);
    let spoken = stderr(&output);
    assert!(output.status.success(), "{spoken}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(7),
        "the run ended at its two seconds, not at the program's eight"
    );
    let recording = json(&out.join("recording.json"));
    assert_eq!(recording["documents"].as_array().map(Vec::len), Some(0));
    assert!(
        recording["stopped_because"]
            .as_str()
            .is_some_and(|why| why.contains("duration")),
        "{}",
        recording["stopped_because"]
    );
    assert!(!out.join("trigger-1.json").exists());
    assert!(spoken.contains("never fired"), "{spoken}");
    assert!(
        number(&recording["unrecorded"][0]["events"]) > 0,
        "what it read over the run is counted as in no document"
    );
}

/// Each bound is refused when absent, before the program is launched: the
/// workload writes its report as its first act, and there is none.
#[test]
fn a_recording_missing_a_bound_is_refused_before_the_launch() {
    for (record, says) in [
        (
            "  trigger: \"value >= 1\"\n  documents: 1\n",
            "needs `retain`",
        ),
        (
            "  trigger: \"value >= 1\"\n  retain: 1s\n",
            "needs `documents`",
        ),
        ("  retain: 1s\n  documents: 1\n", "names none"),
        (
            "  trigger: \"bytes >= 1\"\n  retain: 1s\n  documents: 1\n",
            "does not emit",
        ),
    ] {
        let dir = scratch("refused");
        let report = dir.join("report");
        let text = session(&workload("0:1:0:0:0", &report), "5s", record);
        let output = record_to(&dir.join("out"), &text);
        let spoken = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{record}\n{spoken}");
        assert!(spoken.contains(says), "`{says}` in:\n{spoken}");
        assert!(
            !report.exists(),
            "nothing was launched for a refused recording"
        );
    }
    // And the longest duration: the same refusal, from a session with none.
    let dir = scratch("refused-duration");
    let report = dir.join("report");
    let text = session(
        &workload("0:1:0:0:0", &report),
        "5s",
        "  trigger: \"value >= 1\"\n  retain: 1s\n  documents: 1\n",
    )
    .replacen("duration: 5s\n", "", 1);
    let output = record_to(&dir.join("out"), &text);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("needs `duration`"));
    assert!(!report.exists());
}

/// A recording writes one document per trigger, and one structured document
/// on stdout cannot hold several: refused, with the two ways that can.
#[test]
fn a_recording_to_one_structured_document_on_stdout_is_refused() {
    let dir = scratch("stdout");
    let report = dir.join("report");
    let text = session(
        &workload("0:1:0:0:0", &report),
        "5s",
        "  trigger: \"value >= 1\"\n  retain: 1s\n  documents: 1\n",
    );
    let output = run(
        &["-f", "-", "--input-format", "yaml", "--format", "json"],
        &text,
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("-o DIR"), "{}", stderr(&output));
    assert!(!report.exists());
}

/// The command line records the same way: `--until`, `--retain`,
/// `--documents`, and text on stdout, one document after another.
#[test]
fn the_command_line_records_as_a_session_does() {
    let dir = scratch("cli");
    let report = dir.join("report");
    let exe = std::env::current_exe().expect("this test binary has a path");
    let output = Command::new(BINARY)
        .args([
            "proc.fd",
            "-m",
            "poll@50ms",
            "-e",
            "summary",
            "-d",
            "20s",
            "--until",
            TRIGGER,
            "--retain",
            "800ms",
            "--documents",
            "1",
            "--",
            "/bin/sh",
            "-c",
        ])
        .arg(format!(
            "{BURSTS_VAR}=1500:{BURST_FDS}:300:300:3000 {REPORT_VAR}=\"$2\" exec \"$1\" --exact \
             --test-threads 1 workload_runs_only_when_this_binary_was_launched_as_a_workload \
             >/dev/null 2>&1"
        ))
        .args([
            "sh",
            &exe.display().to_string(),
            &report.display().to_string(),
        ])
        .output()
        .expect("omn runs");
    let spoken = stderr(&output);
    assert!(output.status.success(), "{spoken}");
    let text = String::from_utf8_lossy(&output.stdout);
    let headers: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("document "))
        .collect();
    assert_eq!(headers.len(), 1, "one document, in text:\n{text}");
    assert!(headers[0].starts_with("document 1 of at most 1:"), "{text}");
    assert!(
        spoken.contains("1 document(s), the most it was allowed"),
        "the bound of one ended the run, before the program's second burst:\n{spoken}"
    );
    assert!(
        !report.exists(),
        "the run ended at its first document and stopped the program before it finished"
    );
}

/// The budget holds over a run with no end, weighed over a rolling span.
///
/// The workload spins three seconds of its own cpu and then all but idles,
/// while `proc.fd` walks its 300 descriptors fifty times a second. Over the
/// whole window that walk is a few percent of what the program burned, and
/// an ordinary run of the same session holds a 20 % ceiling to its end. A
/// recording must not: once the spin is over the last two seconds are the
/// walk against almost nothing, and a recorder that idled for an hour cannot
/// have banked an hour of budget for what it does next. So the recording
/// ends for its budget, after the workload says its spin ended and within
/// the rolling span of it, and says over which span.
#[test]
fn a_recording_is_held_to_its_budget_over_a_rolling_span_not_the_whole_run() {
    let dir = scratch("budget");
    let exe = std::env::current_exe().expect("this test binary has a path");
    let report = dir.join("report");
    let argv = format!(
        "[/bin/sh, -c, '{SPIN_VAR}=300:3000:8000 {REPORT_VAR}=\"$2\" exec \"$1\" --exact \
         --test-threads 1 spinning_workload_runs_only_when_this_binary_was_launched_as_one \
         >/dev/null 2>&1', sh, {:?}, {:?}]",
        exe.display().to_string(),
        report.display().to_string()
    );
    let ordinary = format!(
        "duration: 6s\nbudget: 20%\nrequests:\n  - label: descriptors\n    source: proc.fd\n    \
         target: {{launch: {argv}}}\n    mode: poll@20ms\n    emit: [summary]\n"
    );
    let whole = dir.join("whole");
    let output = record_to(&whole, &ordinary);
    assert!(output.status.success(), "{}", stderr(&output));
    let cost = json(&whole.join("cost.json"));
    assert_eq!(
        cost[0]["within_budget"], true,
        "over the whole window the walk is within 20 % of what the program burned: {cost}"
    );
    let _ = std::fs::remove_file(&report);

    let recording = dir.join("recording");
    let text = format!(
        "{}record:\n  trigger: \"value >= 100000\"\n  retain: 1s\n  documents: 1\n",
        ordinary.replace("duration: 6s", "duration: 12s")
    );
    let output = record_to(&recording, &text);
    let spoken = stderr(&output);
    assert!(output.status.success(), "{spoken}");
    let spun = Truth::read(&report).at("spun");
    let recorded = json(&recording.join("recording.json"));
    let why = recorded["stopped_because"].as_str().unwrap_or_default();
    assert!(
        why.contains("over the last") && why.contains("ceiling of 20"),
        "the recording ended for its budget, over the rolling span: {why}"
    );
    let rolling_ns = number(&recorded["budget_interval_ns"]);
    assert_eq!(
        rolling_ns, 2_000_000_000,
        "the floor, since the retention is shorter"
    );
    let closed_ns = number(&recorded["closed_ns"]);
    eprintln!(
        "the recording ended for its budget {:.0} ms after the workload's spin ended",
        closed_ns.saturating_sub(spun) as f64 / 1e6
    );
    assert!(
        closed_ns > spun,
        "not during the spin, when the walk was a small share of what the program burned"
    );
    assert!(
        closed_ns < spun + rolling_ns + 3_000_000_000,
        "within the rolling span of the spin's end, with a second of slack for the checks"
    );
    let cost = json(&recording.join("cost.json"));
    assert_eq!(cost[0]["within_budget"], false, "{cost}");
    assert_eq!(number(&cost[0]["weighed_over_ns"]), rolling_ns);
}
