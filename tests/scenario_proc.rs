//! Scenarios for the `proc` family.
//!
//! Every test here runs a session that is also a file in `examples/`, so a
//! reader can reproduce any of them by hand with `omn -f examples/<name>.yml`
//! and see the same thing. The session is the scenario; this file is only the
//! check on it.
//!
//! The check is against a property the workload chose, never against what the
//! tool computed. The programs launched below open a fixed number of
//! descriptors and start a fixed number of threads, and they write those two
//! numbers to standard error before holding still, so every assertion here
//! compares the tool's answer with a number that is in the workload's source.
//! These two counts are the only ones in the catalog where that comparison can
//! be exact: a process holding twenty descriptors holds twenty.
//!
//! Some tests assert a **defect** rather than correct behaviour, and say so in
//! their name and their comment. They exist because this stream may not edit
//! `src/`, and a defect that is written down in a document and nowhere else is
//! a defect that gets fixed silently or not at all. Each one names the entry in
//! `docs/scenarios/proc.md` it pins. When the defect is fixed the test fails,
//! which is the intended way to find out: delete it and the document entry
//! together.

use std::path::PathBuf;
use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// The repository root, which is where the example sessions live.
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn example(name: &str) -> String {
    root()
        .join("examples")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn omn(args: &[&str]) -> Output {
    Command::new(BINARY)
        .current_dir(root())
        .args(args)
        .output()
        .expect("omn should run")
}

/// Runs one of the example sessions, asking for JSON whatever the file says.
fn session(name: &str) -> Output {
    omn(&["-f", &example(name), "--format", "json"])
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The JSON document a run prints, with whatever the launched program wrote to
/// standard output stripped from in front of it.
fn document(output: &Output) -> serde_json::Value {
    let text = stdout(output);
    let start = text
        .find('{')
        .unwrap_or_else(|| panic!("no json in `{text}`\n--- stderr ---\n{}", stderr(output)));
    serde_json::from_str(&text[start..]).expect("one json document")
}

/// One result of a session, by the label the session gave it.
fn result<'a>(run: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
    run["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("no request labelled `{label}` in {run}"))
}

/// One emit's output within a result, by its name.
fn emitted<'a>(result: &'a serde_json::Value, emit: &str) -> &'a serde_json::Value {
    result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .find(|candidate| candidate["emit"] == emit)
        .unwrap_or_else(|| panic!("no `{emit}` output in {result}"))
}

fn events(result: &serde_json::Value) -> Vec<serde_json::Value> {
    emitted(result, "raw")["events"]
        .as_array()
        .expect("raw events")
        .clone()
}

fn attr(event: &serde_json::Value, name: &str) -> Option<serde_json::Value> {
    event["attrs"]
        .as_array()
        .expect("attributes")
        .iter()
        .find(|attr| attr["name"] == name)
        .map(|attr| attr["value"].clone())
}

/// The one number a launched program wrote to standard error about itself.
///
/// This is the ground truth: it is produced by the measured process, from the
/// kernel's own accounting of that process, and it never passes through the
/// code under test.
fn reported(output: &Output, marker: &str) -> u64 {
    stderr(output)
        .lines()
        .find_map(|line| line.strip_prefix(marker))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "the workload did not report `{marker}`:\n{}",
                stderr(output)
            )
        })
}

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

/// A skip that names what is missing, because a test that skips is not a test
/// that passes and a reader has to be able to tell the two apart.
macro_rules! needs_python {
    () => {
        if !has_python() {
            eprintln!(
                "skipped: this machine has no python3 to launch, and every `proc` scenario \
                 measures a program that reports its own descriptor and thread counts"
            );
            return;
        }
    };
}

// ---------------------------------------------------------------------------
// Class A -- the capability sweep
// ---------------------------------------------------------------------------

/// `examples/proc-sweep.yml`: every emit both sources declare, in one run,
/// against a workload that says what it holds.
///
/// Ground truth: the program opens seventeen files on top of the three every
/// process starts with and runs five workers beside its main thread, and it
/// writes `HELD 20` and `RUNNING 6`. The peak of each series must be exactly
/// that. `max` rather than `last` or `min` deliberately -- see the two defects
/// pinned below, which own both ends of the series.
#[test]
fn the_sweep_reports_the_counts_the_workload_chose_through_every_emit() {
    needs_python!();
    let output = session("proc-sweep.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);

    let held = reported(&output, "HELD ");
    let running = reported(&output, "RUNNING ");
    assert_eq!(
        held, 20,
        "the workload opens seventeen files plus the three every process starts with"
    );
    assert_eq!(running, 6, "five workers and the main thread");

    for (label, expected) in [("descriptors", held), ("threads", running)] {
        let result = result(&doc, label);

        // Every emit the catalog declares for this source is present and is
        // shaped like itself. A combination the catalog offers and the build
        // cannot serve is a promise broken where nobody looks.
        for emit in ["timeline", "raw", "summary"] {
            let output = emitted(result, emit);
            match emit {
                "raw" => assert!(!output["events"].as_array().unwrap().is_empty()),
                _ => assert!(!output["series"].as_array().unwrap().is_empty()),
            }
        }

        let series = &emitted(result, "summary")["series"][0];
        assert_eq!(
            series["max"].as_f64().unwrap() as u64,
            expected,
            "`{label}`: the program reported {expected} and the peak of the series is \
             {}",
            series["max"]
        );

        // The timeline and the summary are two views of one series and must
        // agree about its extremes, or one of them is being computed from
        // something the other is not.
        let peak = emitted(result, "timeline")["series"][0]["points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|point| point["value"].as_f64().unwrap() as u64)
            .max()
            .unwrap();
        assert_eq!(
            peak, expected,
            "`{label}`: the timeline disagrees with the summary"
        );
    }
}

/// The `comm` filter both sweep requests carry selects rather than merely
/// being accepted.
///
/// A filter field nothing emits is false everywhere, which empties the series
/// and looks exactly like a program that did nothing. The sweep filters on
/// `comm =~ 'python'` and the launched program is python, so the events must
/// survive -- and each must carry the attribute the predicate was written
/// against.
#[test]
fn the_declared_filter_field_is_on_the_events_and_keeps_them() {
    needs_python!();
    let output = session("proc-sweep.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);

    for label in ["descriptors", "threads"] {
        let events = events(result(&doc, label));
        assert!(
            !events.is_empty(),
            "`{label}`: the filter emptied a series it should have kept"
        );
        for event in &events {
            let comm = attr(event, "comm").expect("`comm` is a declared filter field");
            assert!(
                comm.as_str().unwrap().contains("python"),
                "the filter kept an event the predicate excludes: {comm}"
            );
        }
    }
}

/// Every output format that can carry these emits does, and every one that
/// cannot is refused by name rather than approximated.
///
/// `text` is the default and proves nothing about the encoders; the catalog
/// offers eight more and a format that claims an emit it cannot carry is the
/// kind of thing only a sweep finds.
#[test]
fn every_format_either_carries_these_emits_or_refuses_them_by_name() {
    needs_python!();
    let program = "import time; time.sleep(0.1)";
    let measure = |emit: &str, format: &str| {
        omn(&[
            "proc.fd",
            "--mode",
            "poll@50ms",
            "--emit",
            emit,
            "--format",
            format,
            "--",
            "python3",
            "-c",
            program,
        ])
    };

    // `timeline`, `raw` and `summary` are structural: every structured format
    // carries them.
    for emit in ["timeline", "raw", "summary"] {
        for format in ["json", "yaml", "toml", "ndjson"] {
            let output = measure(emit, format);
            assert!(
                output.status.success(),
                "`{format}` cannot carry `{emit}`: {}",
                stderr(&output)
            );
            assert!(
                !stdout(&output).trim().is_empty(),
                "`{format}`/`{emit}` wrote nothing"
            );
        }
    }

    // The stack formats cannot, and must say so rather than writing an empty
    // profile. `folded` and `pprof` carry stacks, and this source has none.
    for (format, emit) in [
        ("folded", "timeline"),
        ("folded", "raw"),
        ("folded", "summary"),
        ("pprof", "timeline"),
        ("speedscope", "raw"),
        ("chrome", "summary"),
    ] {
        let output = measure(emit, format);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`{format}` with `{emit}` must be refused as a request that cannot mean \
             anything: {}",
            stderr(&output)
        );
        let message = stderr(&output);
        assert!(
            message.contains(format) && message.contains(emit),
            "the refusal must name both the format and the emit: {message}"
        );
    }
}

/// `snapshot` is one read, and the validator knows a series of one point is
/// not a series.
///
/// `examples/proc-one-reading.yml` asks for `raw` and `summary` because those
/// are the only emits `snapshot` can carry; asking it for `timeline` is
/// refused, and that refusal is correct.
#[test]
fn a_snapshot_is_a_single_read_and_cannot_be_asked_for_a_timeline() {
    needs_python!();
    let output = session("proc-one-reading.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    for label in ["descriptors", "threads"] {
        let result = result(&doc, label);
        assert_eq!(result["events_seen"], 1, "`snapshot` is a single read");
        assert_eq!(result["stopped_because"], "snapshot: a single read");
    }

    let refused = omn(&[
        "proc.fd",
        "--mode",
        "snapshot",
        "--emit",
        "timeline",
        "--",
        "python3",
        "-c",
        "import time; time.sleep(0.05)",
    ]);
    assert_eq!(refused.status.code(), Some(2));
    let message = stderr(&refused);
    assert!(
        message.contains("snapshot") && message.contains("timeline"),
        "the refusal must name the mode and the emit it cannot produce: {message}"
    );
}

// ---------------------------------------------------------------------------
// Class B -- the backend comparison
// ---------------------------------------------------------------------------

/// `proc.fd` and `proc.threads` have one backend each, so there is no second
/// road to compare with. What can be checked is that the road is named, that
/// pinning it says so, and that pinning one that does not exist is refused
/// with the chain rather than accepted and ignored.
///
/// See "What cannot be verified here" in `docs/scenarios/proc.md`: this family
/// contributes nothing to class B, which is why the effort went to class D.
#[test]
fn the_only_backend_these_sources_have_is_named_and_a_pin_says_it_was_pinned() {
    needs_python!();
    let pinned = omn(&[
        "proc.fd",
        "--mode",
        "poll@50ms",
        "--emit",
        "raw",
        "--backend",
        "force:fs:proc",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        "import time; time.sleep(0.1)",
    ]);
    assert!(pinned.status.success(), "{}", stderr(&pinned));
    assert_eq!(document(&pinned)["results"][0]["backend"], "fs:proc");
    assert!(
        stderr(&pinned).contains("pinned"),
        "a pinned backend is not a measurement to keep, and the run has to say so: {}",
        stderr(&pinned)
    );

    let absent = omn(&[
        "proc.fd",
        "--mode",
        "poll@50ms",
        "--emit",
        "raw",
        "--backend",
        "force:bpf:tp",
        "--",
        "python3",
        "-c",
        "import time; time.sleep(0.05)",
    ]);
    assert_eq!(absent.status.code(), Some(2));
    assert!(
        stderr(&absent).contains("fs:proc"),
        "a pin on a backend the chain does not have must print the chain: {}",
        stderr(&absent)
    );
}

// ---------------------------------------------------------------------------
// Class C -- the loss path
// ---------------------------------------------------------------------------

/// `examples/proc-loss.yml`: a poller that cannot keep up says so.
///
/// Ground truth twice over. The workload raises its own soft limit and opens
/// exactly 30000 descriptors on top of the three it starts with, and reports
/// `OPEN 30003`; the peak of the series must be that number exactly. And a
/// 30000-entry directory walk cannot be done in the millisecond the session
/// asks for, so the run must count the ticks that ran late -- a series whose
/// spacing is not what it claims is the failure this source can actually have.
///
/// This scenario is the slowest in the family, around three quarters of a
/// second, and nearly all of that is python opening thirty thousand files.
#[test]
fn a_poller_that_cannot_keep_up_counts_and_names_the_ticks_that_ran_late() {
    needs_python!();
    let output = session("proc-loss.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    let result = result(&doc, "thirty-thousand-handles");

    let open = reported(&output, "OPEN ");
    assert_eq!(open, 30003, "the workload chooses the number it holds");
    assert_eq!(
        emitted(result, "summary")["series"][0]["max"]
            .as_f64()
            .unwrap() as u64,
        open,
        "the peak of the series must be the number the workload reported"
    );

    let losses = &result["losses"];
    assert!(
        losses["late_ticks"].as_u64().unwrap() > 0,
        "a 30000-entry directory cannot be walked in a millisecond, and a series that \
         claims a spacing it never had is a wrong number that looks right: {losses}"
    );

    // A late tick is not a gap in the data. Nothing was dropped, nothing failed
    // to read, and reporting either would send a reader looking for events that
    // were never missed.
    assert_eq!(losses["dropped"], 0, "this source has no ring to drop from");
    assert_eq!(
        losses["failed_reads"], 0,
        "every read of a live process succeeded"
    );

    let reasons = losses["reasons"]
        .as_object()
        .expect("the reasons behind the counts");
    assert!(
        reasons.keys().any(|reason| reason.contains("longer than")),
        "the reason has to say the read outran the tick: {reasons:?}"
    );
}

/// `examples/proc-exit.yml`: a program that ends is not a program that lost
/// data.
///
/// The workload chooses when: it holds still for 150 ms and exits 0, while the
/// session asks for two seconds. The run must stop at the exit rather than the
/// deadline, name it, and leave every loss counter at zero -- procfs answers a
/// read of a departed process with ESRCH or ENOENT, and counting either as a
/// lost read would make every clean run look lossy.
#[test]
fn a_target_that_exits_ends_the_run_cleanly_and_is_not_counted_as_loss() {
    needs_python!();
    let output = session("proc-exit.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);

    for label in ["descriptors", "threads"] {
        let result = result(&doc, label);
        let stopped = result["stopped_because"].as_str().unwrap();
        assert!(
            stopped.contains("exited"),
            "`{label}` must stop because the program ended, not at the deadline: {stopped}"
        );
        let losses = &result["losses"];
        for counter in [
            "dropped",
            "failed_reads",
            "late_ticks",
            "undecoded",
            "throttled",
        ] {
            assert_eq!(
                losses[counter], 0,
                "`{label}`: a program that ends has lost nothing, and `{counter}` says \
                 otherwise: {losses}"
            );
        }
        // The run is far shorter than the two seconds it asked for, which is
        // the other half of the same claim.
        assert!(
            result["observed_ns"].as_u64().unwrap() < 1_000_000_000,
            "the run should have ended with the program, well inside its 2s duration"
        );
    }

    assert_eq!(
        emitted(result(&doc, "descriptors"), "summary")["series"][0]["max"]
            .as_f64()
            .unwrap() as u64,
        reported(&output, "HELD "),
        "what it held while it was alive is still the number it reported"
    );
}

// ---------------------------------------------------------------------------
// Class D -- the refusal
// ---------------------------------------------------------------------------

/// Three requests that cannot mean anything, and three reasons that name the
/// thing that is actually missing.
///
/// A refusal that sends a reader to the wrong target, the wrong mode or the
/// wrong emit costs more than no message at all. `validate` is the surface that
/// answers for all three at once instead of stopping at the first, which is why
/// this is one session rather than three command lines.
///
/// The session is written here rather than shipped in `examples/`, because
/// `tests/cli.rs` defines that directory as sessions that validate -- and this
/// one cannot, by construction. `docs/scenarios/proc.md` carries it verbatim so
/// that a reader still has it. It is the one place in this stream where the
/// scenario and the example had to part company.
#[test]
fn a_request_that_cannot_mean_anything_is_refused_with_the_reason_it_cannot() {
    let scratch = std::env::temp_dir().join("omn-scenario-proc-refusals");
    std::fs::create_dir_all(&scratch).expect("a directory to write a session into");
    let path = scratch.join("proc-refusals.yml");
    std::fs::write(&path, REFUSALS).expect("the test owns this file");

    let output = omn(&["validate", "-f", &path.to_string_lossy()]);
    let message = stderr(&output);
    let code = output.status.code();
    let _ = std::fs::remove_dir_all(&scratch);

    assert_eq!(code, Some(2), "a request that cannot mean anything exits 2");

    // The target it does not accept, with the ones it does.
    assert!(
        message.contains("no `system` support") && message.contains("accepts launch, pid, tid"),
        "the refusal must name the targets that would work: {message}"
    );
    // The mode it does not have, with the ones it has.
    assert!(
        message.contains("cannot be observed in `trace` mode")
            && message.contains("supports poll, snapshot"),
        "the refusal must name the modes this source has: {message}"
    );
    // The emit it does not declare, with the ones it does.
    assert!(
        message.contains("cannot emit `hist`") && message.contains("timeline, raw, summary"),
        "the refusal must name the emits this source declares: {message}"
    );

    // All three, not just the first: a reader fixing a session wants every
    // reason it is wrong, not one per run.
    assert_eq!(
        message.matches("error:").count(),
        3,
        "validate reports every rejection rather than stopping at the first: {message}"
    );
}

/// The session the test above writes, kept here so the two cannot drift.
///
/// Each request is rejected for a different reason, and the three must not be
/// confused with each other or with the other two ways this tool says no --
/// `proc-not-built.yml` is "this build does not implement it" and exits 3, and
/// "this machine cannot serve it" should exit 1 and does not (defect 4).
const REFUSALS: &str = "\
name: three ways a request cannot mean anything
format: text

requests:
  # A descriptor table belongs to a process. The machine has no descriptor
  # count, so `--system` is not a harder measurement, it is a different
  # sentence.
  - label: no-such-target
    source: proc.fd
    target: system
    mode: poll@100ms
    emit: [raw]

  # `proc.fd` is a counter read on a clock, not an event stream. There is no
  # descriptor-open event for `trace` to follow -- that is `proc.syscall`, a
  # different source with a different mechanism.
  - label: no-such-mode
    source: proc.fd
    target: {launch: [sleep, \"0.1\"]}
    mode: trace
    emit: [raw]

  # A histogram of a polled gauge is a histogram of when the poller happened to
  # look, which is a property of the tick and not of the program.
  - label: no-such-emit
    source: proc.threads
    target: {launch: [sleep, \"0.1\"]}
    mode: poll@100ms
    emit: [hist]
";

/// `examples/proc-not-built.yml`: valid, resolvable, and not written yet.
///
/// This is the third answer, and it must not be confused with the other two. A
/// caller that cannot tell "you asked for something impossible" from "this is
/// not written yet" cannot decide whether to fix the request or wait for a
/// release. The session validates cleanly, resolves onto a backend, and exits
/// 3 naming that backend.
#[test]
fn a_source_this_build_does_not_implement_exits_three_and_names_the_backend() {
    let validated = omn(&["validate", "-f", &example("proc-not-built.yml")]);
    assert!(
        validated.status.success(),
        "the session is valid; it is the build that is missing: {}",
        stderr(&validated)
    );

    let output = omn(&["-f", &example("proc-not-built.yml")]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "not implemented is not the same answer as invalid: {}",
        stderr(&output)
    );
    let message = stderr(&output);
    assert!(
        message.contains("no collector is written") && message.contains("bpf:tp"),
        "the refusal must name the backend that is missing: {message}"
    );
    assert!(
        message.contains("proc.exec"),
        "and the source it was asked for: {message}"
    );
    // The way out is named too: the request is fine and can be written down.
    assert!(
        message.contains("--dump-request"),
        "a reader has to be told what they can still do with a valid request: {message}"
    );
}

// ---------------------------------------------------------------------------
// The defects this family found
// ---------------------------------------------------------------------------
//
// Each of these asserts the wrong behaviour on purpose, because this stream
// may not edit `src/` and a defect recorded only in prose is a defect that
// gets fixed silently or not at all. When one of them starts failing, the
// defect is fixed: delete the test and its entry in `docs/scenarios/proc.md`
// together.

/// Defect 1: the first reading of a launched target describes this tool.
///
/// The runner releases the child from its gate and reads immediately, and the
/// child has not reached its `execve` yet, so `/proc/<pid>/fd` is still the
/// forked image of `omn`. The pid is right and everything else is wrong: the
/// count is this tool's three descriptors and `comm` says `omn`.
///
/// It is worst in `snapshot` mode, where that reading is the only one there
/// is: `examples/proc-one-reading.yml` against a program holding twenty
/// descriptors answers three.
#[test]
fn defect_1_the_first_reading_of_a_launched_target_is_this_tool_not_the_program() {
    needs_python!();

    // What the workload holds, established the only way a snapshot run cannot
    // establish it: by polling the same program long enough for it to say. A
    // `snapshot` run ends in microseconds, so the program it is measuring has
    // not reached its first statement, let alone written to standard error --
    // which is itself the shape of the defect.
    let polled = session("proc-sweep.yml");
    assert!(polled.status.success(), "{}", stderr(&polled));
    let held = reported(&polled, "HELD ");
    assert_eq!(
        held, 20,
        "the workload opens seventeen files plus its three"
    );

    let output = session("proc-one-reading.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);

    let event = &events(result(&doc, "descriptors"))[0];
    let measured = attr(event, "open").unwrap().as_u64().unwrap();
    assert!(
        measured <= held,
        "the read lands somewhere inside the program's startup, so it cannot find more than the \
         {held} descriptors that program ever holds: {event}"
    );

    // The defect is a race with three outcomes, not two, and a test must not
    // assert which one a run got. The child is released from its gate and read
    // immediately; it may not have reached `execve` (`comm=omn`, three
    // descriptors), it may have exec'd and not yet opened its files
    // (`comm=python3`, three descriptors -- seen under this suite, and the case
    // the first version of this test did not expect), or it may occasionally
    // have got further.
    //
    // What makes it a defect is that the answer is *unspecified*, so that is
    // what is asserted: over several runs of the same session against the same
    // program, at least one must disagree with what the program holds. A
    // snapshot that always found twenty would be a reliable measurement, and
    // this test would be telling the truth by failing.
    const RUNS: usize = 5;
    let mut readings = vec![measured];
    for _ in 1..RUNS {
        let again = session("proc-one-reading.yml");
        assert!(again.status.success(), "{}", stderr(&again));
        let doc = document(&again);
        let event = &events(result(&doc, "descriptors"))[0];
        readings.push(attr(event, "open").unwrap().as_u64().unwrap());
    }
    assert!(
        readings.iter().any(|open| *open != held),
        "DEFECT 1 IS FIXED: {RUNS} single reads of a launched target all found the {held} \
         descriptors the program holds ({readings:?}). Delete this test and defect 1 of \
         docs/scenarios/proc.md."
    );

    // The same reading is the first point of every polled launch too, where it
    // is merely the first of several -- which is why `proc-handles.yml` tells a
    // reader to start from the second point rather than the first.
    //
    // `proc-sweep.yml` is not the file to show it in: its `comm =~ 'python'`
    // filter removes the pre-exec reading, because that reading says `omn`.
    // A filter written to select a program silently repairs this defect, and a
    // session without one does not.
    let unfiltered = session("proc-handles.yml");
    assert!(unfiltered.status.success(), "{}", stderr(&unfiltered));
    let points = emitted(result(&document(&unfiltered), "descriptors"), "timeline")["series"][0]
        ["points"]
        .as_array()
        .expect("a series of readings")
        .clone();
    assert_eq!(
        points[0]["value"].as_f64().unwrap() as u64,
        3,
        "the first point is this tool's own three descriptors, not the program's {held}"
    );
    assert_eq!(points[1]["value"].as_f64().unwrap() as u64, held);
}

/// Defect 2: the last reading of a launched target reports a zombie as holding
/// nothing.
///
/// A process that has exited but has not been reaped still has a
/// `/proc/<pid>/fd`, and it is an empty directory that reads successfully --
/// neither the ESRCH nor the ENOENT the collector treats as "the target left".
/// So the run takes one more sample and records zero open descriptors with
/// full headroom, no loss, and a clean bill of health, for a program that
/// never closed a handle in its life. `min` and `last` of every polled launch
/// are that zero.
#[test]
fn defect_2_the_last_reading_of_a_launched_target_reports_a_zombie_as_empty() {
    needs_python!();
    let output = session("proc-exit.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);

    let held = reported(&output, "HELD ");
    let result = result(&doc, "descriptors");
    let summary = &emitted(result, "summary")["series"][0];

    assert_eq!(
        summary["last"].as_f64().unwrap() as u64,
        0,
        "DEFECT 2 IS FIXED: the series no longer ends on a zombie's empty directory. \
         Delete this test and defect 2 of docs/scenarios/proc.md."
    );
    assert_eq!(summary["min"].as_f64().unwrap() as u64, 0);
    assert_eq!(
        summary["max"].as_f64().unwrap() as u64,
        held,
        "the peak is still right, which is why only `max` can be trusted today"
    );

    // And the reading looks entirely healthy: full headroom, no loss reported.
    let last = events(result).last().unwrap().clone();
    assert_eq!(attr(&last, "open").unwrap(), 0);
    assert_eq!(
        attr(&last, "headroom").unwrap(),
        attr(&last, "limit").unwrap(),
        "a departed process reads as one with its whole limit available"
    );
    assert_eq!(result["losses"]["failed_reads"], 0);
}

/// Defect 3: a session takes its tick from the first request and discards
/// every other request's mode.
///
/// The runner asks the collectors for a tick, and these pollers declare none,
/// so it falls back to `resolved[0].request.mode` -- the *first* request --
/// and applies it to all of them. Three consequences, all silent, and the
/// project's own rule is that an invalid combination is refused with the
/// reason and never degraded quietly:
///
///   - `poll@200ms` first, `poll@50ms` second: both run at 200 ms, and the
///     second request is downsampled by four without being told.
///   - `poll@50ms` first, `snapshot` second: the snapshot becomes a series.
///   - `snapshot` first, `poll@50ms` second: the interval is zero and the run
///     becomes a busy loop against procfs for every source in it.
#[test]
fn defect_3_a_session_takes_its_tick_from_the_first_request_only() {
    let scratch = std::env::temp_dir().join("omn-scenario-proc-defect-3");
    std::fs::create_dir_all(&scratch).expect("a directory to write sessions into");

    let write = |name: &str, first: &str, second: &str| {
        let path = scratch.join(name);
        std::fs::write(
            &path,
            format!(
                "format: json\n\
                 requests:\n\
                 \x20 - label: first\n\
                 \x20   source: proc.threads\n\
                 \x20   target: {{launch: [sh, -c, \"sleep 0.4\"]}}\n\
                 \x20   mode: {first}\n\
                 \x20   emit: [raw]\n\
                 \x20 - label: second\n\
                 \x20   source: proc.fd\n\
                 \x20   target: {{launch: [sh, -c, \"sleep 0.4\"]}}\n\
                 \x20   mode: {second}\n\
                 \x20   emit: [raw]\n"
            ),
        )
        .expect("the test owns this file");
        let output = omn(&["-f", &path.to_string_lossy(), "--format", "json"]);
        assert!(output.status.success(), "{}", stderr(&output));
        let doc = document(&output);
        let seen = |label: &str| {
            let result = result(&doc, label);
            assert!(
                result["events_seen"].as_u64().unwrap() > 0,
                "`{label}` of {name} read nothing at all, which is a different failure \
                 from the one this test pins: {result}"
            );
            result["events_seen"].as_u64().unwrap()
        };
        (seen("first"), seen("second"))
    };

    // A slow first request downsamples a fast second one.
    let (slow, fast) = write("downsampled.yml", "poll@200ms", "poll@50ms");
    assert_eq!(
        slow, fast,
        "DEFECT 3 IS FIXED: the two requests now keep their own rates. Delete this test \
         and defect 3 of docs/scenarios/proc.md."
    );
    assert!(
        fast < 6,
        "`poll@50ms` over 400ms is around nine readings, and this run took {fast} \
         because the first request asked for 200ms"
    );

    // A `snapshot` beside a `poll` stops being a single read.
    let (_, snapshot) = write("not-a-snapshot.yml", "poll@50ms", "snapshot");
    assert!(
        snapshot > 1,
        "a `snapshot` next to a `poll` produced {snapshot} readings, and `snapshot` \
         means one"
    );

    // And a `snapshot` first turns the whole session into a busy loop: the
    // fallback interval for `Mode::Snapshot` is zero, and it is applied to
    // every request.
    // The threshold is deliberately far below what an idle machine produces --
    // thousands of rounds in 400ms -- because the spinning thread's share of
    // the cpu is whatever the rest of the suite leaves it. What is being
    // claimed is not a rate but an order of magnitude: no request in this
    // session asked for more than nine readings, and anything past fifty can
    // only be an interval of zero.
    let (spun, _) = write("busy-loop.yml", "snapshot", "poll@50ms");
    assert!(
        spun > 50,
        "a session led by a `snapshot` should spin: {spun} rounds where `poll@50ms` \
         over 400ms asks for nine is what a zero interval looks like"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

/// Defect 4: "this machine cannot serve it" exits 2, the code that means "the
/// request cannot mean anything".
///
/// `BRIEF.md` defines three answers and three exit codes, and only two of the
/// codes exist. `src/main.rs` returns `EXIT_REJECTED` when the resolver finds
/// no viable backend, one line after printing a hint that says the opposite:
/// "the request is valid; it is this machine that cannot serve it". A caller
/// cannot tell a request it should rewrite from one it should run elsewhere.
///
/// No `proc` or `runtime` source can reach this state on a real machine --
/// every implemented backend needs only procfs or `/tmp` -- so the scenario
/// gets there through `--catalog`, with a copy of the catalog in which
/// `proc.fd` asks for a file that does not exist. That is a supported flag and
/// the resolver is the code under test either way.
#[test]
fn defect_4_no_viable_backend_exits_two_rather_than_one() {
    let scratch = std::env::temp_dir().join("omn-scenario-proc-defect-4");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("a directory to write a catalog into");

    // A copy of the real catalog with one requirement no machine can satisfy.
    for entry in std::fs::read_dir(root().join("catalog")).expect("the catalog is in the tree") {
        let entry = entry.expect("a catalog file");
        let text = std::fs::read_to_string(entry.path()).expect("readable");
        let text = text.replace(
            "requires: [\"/proc/[pid]/fd\"]",
            "requires: [\"/proc/[pid]/fd\", \"/proc/omniscient-no-such-file\"]",
        );
        std::fs::write(scratch.join(entry.file_name()), text).expect("the test owns this file");
    }

    let output = omn(&[
        "proc.fd",
        "--catalog",
        &scratch.to_string_lossy(),
        "--mode",
        "poll@50ms",
        "--emit",
        "raw",
        "--",
        "sh",
        "-c",
        "sleep 0.05",
    ]);
    let message = stderr(&output);
    let _ = std::fs::remove_dir_all(&scratch);

    // The message is right: it says the request is valid and the machine is
    // not able, and it names the requirement that failed.
    assert!(
        message.contains("no viable backend") && message.contains("omniscient-no-such-file"),
        "the refusal must name the requirement this machine does not meet: {message}"
    );
    assert!(
        message.contains("it is this machine that cannot serve it"),
        "and must say which of the three answers it is: {message}"
    );

    // The exit code is not.
    assert_eq!(
        output.status.code(),
        Some(2),
        "DEFECT 4 IS FIXED: `no viable backend` no longer exits 2. Delete this test and \
         defect 4 of docs/scenarios/proc.md."
    );
}

/// Defect 5, fixed: `proc.fd` declared the `file` key and no event carried it.
///
/// The catalog said `keys: [task, file]`, the validator accepted `--keys file`
/// and told a reader that "`proc.fd` carries task, file", and every event this
/// collector produces carries `task` alone. A join written on `file` matched
/// nothing and reported that it attributed zero events, which reads as a
/// workload that did nothing rather than as a key that was never there.
///
/// The entry now declares `task` alone, so the promise and the events agree and
/// the request is refused before anything is attached. Both halves are asserted
/// here -- the refusal, and that the events carry exactly the key that is left
/// -- because a fix that only edited the refusal would be the same lie the
/// other way round.
///
/// It matters beyond this family: phase two's premise is that every session
/// which could carry a key carries it, and a key the catalog promises and the
/// build does not emit makes that impossible to check from the catalog alone.
#[test]
fn the_only_key_proc_fd_declares_is_the_only_one_its_events_carry() {
    needs_python!();
    let refused = omn(&[
        "proc.fd",
        "--mode",
        "poll@50ms",
        "--emit",
        "raw",
        "--keys",
        "task,file",
        "--",
        "true",
    ]);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "`--keys file` on a source that carries no file is a request that cannot mean \
         anything: {}",
        stderr(&refused)
    );
    let complaint = stderr(&refused);
    assert!(
        complaint.contains("does not carry the `file` key"),
        "the refusal has to name the key: {complaint}"
    );
    assert!(
        complaint.contains("carries task") && !complaint.contains("carries task, file"),
        "and the hint has to name what is left, without the key it just refused: {complaint}"
    );

    // And what the entry does declare is on every event.
    let output = omn(&[
        "proc.fd",
        "--mode",
        "poll@50ms",
        "--emit",
        "raw",
        "--keys",
        "task",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        "import time; time.sleep(0.1)",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    for event in events(&doc["results"][0]) {
        let keys = event["keys"].as_array().expect("keys");
        assert_eq!(keys.len(), 1, "one key per event, and it is task: {event}");
        assert!(
            keys[0].get("task").is_some(),
            "the key on the event is not `task`: {event}"
        );
    }
}

/// Defect 6: a late-tick reason carries its own nanosecond count, so the loss
/// summary never deduplicates.
///
/// `Losses::note` counts identical reasons, which is what turns a thousand
/// dropped events into one line saying `1000x`. The late-tick note embeds the
/// exact duration of the round that overran, so no two are ever equal: a run
/// with seventy-seven late ticks prints seventy-seven `1x` lines. Losses are
/// surfaced to the user always, and this is the one place where surfacing them
/// buries the count instead of reporting it.
#[test]
fn defect_6_late_tick_reasons_never_deduplicate() {
    needs_python!();
    let output = session("proc-loss.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    let losses = &result(&doc, "thirty-thousand-handles")["losses"];

    let late = losses["late_ticks"].as_u64().unwrap();
    let distinct = losses["reasons"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|reason| reason.contains("longer than"))
        .count() as u64;

    assert!(late > 10, "the scenario has to actually run late: {losses}");
    assert!(
        distinct > late / 2,
        "DEFECT 6 IS FIXED: {late} late ticks now collapse into {distinct} reason(s). \
         Delete this test and defect 6 of docs/scenarios/proc.md."
    );
}
