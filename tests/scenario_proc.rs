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
    let late = reasons
        .iter()
        .find(|(reason, _)| reason.contains("outran"))
        .unwrap_or_else(|| panic!("the reason has to say the read outran the tick: {reasons:?}"));
    // One reason for the whole run, carrying the count it explains. The
    // magnitudes used to be in the key -- one reason per late tick, each with a
    // count of one -- which is defect 6 of `docs/scenarios/proc.md`.
    assert_eq!(
        late.1.as_u64().unwrap(),
        losses["late_ticks"].as_u64().unwrap(),
        "the reason has to carry the count it explains: {reasons:?}"
    );
    assert_eq!(
        reasons.len(),
        1,
        "a run whose only loss is late ticks has one reason, not one per tick: {reasons:?}"
    );
    // And the measurement is beside the count rather than in place of it: the
    // worst overrun is what says whether the series is usable at all.
    let overrun = &losses["overrun"];
    assert!(
        overrun["worst_ns"].as_u64().unwrap() >= overrun["tick_ns"].as_u64().unwrap(),
        "a tick counted as late has to have overrun the tick: {overrun}"
    );
    assert!(
        overrun["total_ns"].as_u64().unwrap() >= overrun["worst_ns"].as_u64().unwrap(),
        "the total cannot be less than the worst one in it: {overrun}"
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
/// `proc-not-built.yml` is "this build does not implement it" where the machine
/// can reach the backend, and "this machine cannot serve it" where it cannot;
/// the scenario below takes either, and neither may be confused with these.
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

/// `examples/proc-not-built.yml`: valid, resolvable, and refused for one of the
/// two reasons that are not the request's fault.
///
/// A caller that cannot tell "you asked for something impossible" from "this is
/// not written yet" cannot decide whether to fix the request or wait for a
/// release, and one that cannot tell either from "this machine cannot serve it"
/// raises privileges against a collector nobody has written. The session
/// validates cleanly and resolves, and whichever of the two refusals it gets
/// must be the one whose sentence it prints.
///
/// **Which one depends on the machine, and this family cannot choose.** All
/// four unwritten `proc` sources declare `bpf:tp` and nothing else, and a
/// classic tracepoint is attached by reading a numeric event id out of a
/// root-only file under tracefs. Where that file is readable the answer is 3;
/// where it is not, as here, the machine genuinely cannot serve the request and
/// 1 is the honest answer. Asserting either one alone would be asserting the
/// machine. Finding 9 of `docs/open-findings.md` is what would settle it in one
/// direction: all four tracepoints are published in this kernel's own BTF, so a
/// `bpf:raw_tp` backend ahead of the classic one puts this family back on 3
/// everywhere.
#[test]
fn a_source_this_build_does_not_implement_exits_three_and_names_the_backend() {
    let validated = omn(&["validate", "-f", &example("proc-not-built.yml")]);
    assert!(
        validated.status.success(),
        "the session is valid; it is the build or the machine that is missing: {}",
        stderr(&validated)
    );

    let output = omn(&["-f", &example("proc-not-built.yml")]);
    let message = stderr(&output);
    match output.status.code() {
        Some(3) => {
            assert!(
                message.contains("no collector is written"),
                "exit 3 is this build's gap and must say so: {message}"
            );
            // The way out is named too: the request is fine and can be written
            // down for a build that has it.
            assert!(
                message.contains("--dump-request"),
                "a reader has to be told what they can still do with a valid request: {message}"
            );
        }
        Some(1) => assert!(
            message.contains("has no viable backend on this machine"),
            "exit 1 is this machine's gap and must say so: {message}"
        ),
        other => panic!(
            "a valid request for an unwritten source is the build's gap or the machine's, \
             never a request that cannot mean anything; got {other:?}:\n{message}"
        ),
    }
    assert!(
        message.contains("bpf:tp"),
        "the refusal must name the backend either way: {message}"
    );
    assert!(
        message.contains("proc.exec"),
        "and the source it was asked for: {message}"
    );
}

// ---------------------------------------------------------------------------
// The defects this family found
// ---------------------------------------------------------------------------
//
// Each of these asserted the wrong behaviour on purpose, because the stream
// that found them could not edit `src/`, and a defect recorded only in prose
// is a defect that gets fixed silently or not at all.
//
// Defects 1 and 2 have since been fixed, and their tests now assert the
// behaviour that replaced the defect -- each one fails against the build that
// had it. The rest still hold their defect, because it belongs to a layer this
// family does not own.

/// Defect 1, fixed: the first reading of a launched target is of the program.
///
/// The runner released the child from its gate and read immediately, and the
/// child had not reached its `execve` yet, so `/proc/<pid>/fd` was still the
/// forked image of `omn`: the pid was right and everything else was wrong --
/// the count was this tool's three descriptors and `comm` said `omn`. It was
/// worst in `snapshot` mode, where that reading is the only one there is, and
/// it was a race with three outcomes, so the answer was not merely wrong but
/// *unspecified*.
///
/// `proc.fd` now waits for the launched pid to stop being this tool's own
/// image before its first read. What it does not do -- and what no measurement
/// tool can do -- is wait for the program to have done something: a snapshot
/// of a launched program is a reading of the instant it began, and that is
/// three or four descriptors for a `python3` that has not yet run its first
/// statement. The fix is that the reading is *of the program*, and that is
/// what is asserted here.
#[test]
fn defect_1_the_first_reading_of_a_launched_target_is_of_the_program() {
    needs_python!();

    // What the workload holds, established the only way a snapshot run cannot
    // establish it: by polling the same program long enough for it to say.
    let polled = session("proc-sweep.yml");
    assert!(polled.status.success(), "{}", stderr(&polled));
    let held = reported(&polled, "HELD ");
    assert_eq!(
        held, 20,
        "the workload opens seventeen files plus its three"
    );

    // Five single reads of the same session. Every one of them has to be about
    // the launched program: `comm` is the tell, because it says which image
    // the pid is running, and before the fix it said `omn` on about four runs
    // in five. And every one of them has to find no more than the program ever
    // holds, because a reading of this tool's table is not bounded by that.
    const RUNS: usize = 5;
    let mut readings = Vec::new();
    for _ in 0..RUNS {
        let output = session("proc-one-reading.yml");
        assert!(output.status.success(), "{}", stderr(&output));
        let doc = document(&output);
        let event = &events(result(&doc, "descriptors"))[0];
        let comm = attr(event, "comm")
            .and_then(|value| value.as_str().map(str::to_owned))
            .expect("`comm` travels on every event");
        assert_eq!(
            comm, "python3",
            "the single read of a launched program named `{comm}`, which is this tool and not \
             the program it launched: {event}"
        );
        let measured = attr(event, "open").unwrap().as_u64().unwrap();
        assert!(
            measured <= held,
            "the read lands at the program's first instant, so it cannot find more than the \
             {held} descriptors that program ever holds: {event}"
        );
        readings.push(measured);
    }

    // And the answer is now specified rather than a race: five runs of one
    // session against one program report the same count, give or take the one
    // descriptor `python3 -c` opens and closes while it starts.
    let (low, high) = (
        *readings.iter().min().unwrap(),
        *readings.iter().max().unwrap(),
    );
    assert!(
        high - low <= 1,
        "a snapshot of a launched program is meant to be a reading of one defined instant -- \
         the exec -- and these five disagree: {readings:?}"
    );

    // The same wait is the first point of every polled launch too, where the
    // reading used to be this tool's own three descriptors under the program's
    // pid. `proc-handles.yml` is the file that shows it: `proc-sweep.yml`
    // carries a `comm =~ 'python'` filter, which removed the pre-exec reading
    // because that reading said `omn` -- so a session written to select a
    // program repaired this defect and a session without one did not.
    let unfiltered = session("proc-handles.yml");
    assert!(unfiltered.status.success(), "{}", stderr(&unfiltered));
    let doc = document(&unfiltered);
    let points = emitted(self::result(&doc, "descriptors"), "timeline")["series"][0]["points"]
        .as_array()
        .expect("a series of readings")
        .clone();
    assert!(
        points[0]["value"].as_f64().unwrap() as u64 <= held,
        "the first point is the program at its first instant, not this tool's own table"
    );
    assert_eq!(points[1]["value"].as_f64().unwrap() as u64, held);

    // And the filtered session now sees the same run as the unfiltered one:
    // its `comm =~ 'python'` predicate has no pre-exec reading left to remove,
    // so it drops nothing and the two sessions agree on how many readings
    // there were.
    let swept = document(&polled);
    let filtered = self::result(&swept, "descriptors");
    assert_eq!(
        filtered["filtered"]["dropped"].as_u64(),
        Some(0),
        "a filter that selects the program used to repair this defect by removing the reading \
         that said `omn`, which made the defect intermittent across sessions that look \
         equivalent: {}",
        filtered["filtered"]
    );
    assert_eq!(
        filtered["filtered"]["kept"].as_u64(),
        Some(points.len() as u64),
        "the filtered and unfiltered sessions poll the same program at the same rate"
    );
}

/// Defect 2, fixed: the run no longer ends on a zombie's empty directory.
///
/// A process that has exited but has not been reaped still has a
/// `/proc/<pid>/fd`, and it is an empty directory that reads *successfully* --
/// neither the ESRCH nor the ENOENT `classify()` treats as "the target left".
/// So the run took one more sample and recorded zero open descriptors, with
/// the whole limit as headroom, no loss and a clean bill of health, for a
/// program that never closed a handle in its life. `min` and `last` of every
/// polled launch were that zero, and only `max` was usable.
///
/// The collector now reads the process's state and reports nothing for a
/// target that has departed, so `last` is the last reading the program was
/// alive for -- which is the number a reader takes off a `summary` to answer
/// "how many was it holding when it finished".
#[test]
fn defect_2_the_last_reading_of_a_launched_target_is_one_the_program_was_alive_for() {
    needs_python!();
    let output = session("proc-exit.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);

    let held = reported(&output, "HELD ");
    let result = result(&doc, "descriptors");
    let summary = &emitted(result, "summary")["series"][0];

    assert_eq!(
        summary["last"].as_f64().unwrap() as u64,
        held,
        "the program held {held} descriptors to the end and never closed one, so the last \
         reading is {held}; a zero there is a departed process reported as an empty one"
    );
    assert_eq!(
        summary["max"].as_f64().unwrap() as u64,
        held,
        "the peak was always right; it is the rest of the series that was not"
    );
    assert!(
        summary["min"].as_f64().unwrap() as u64 > 0,
        "no reading of a live process is zero open descriptors: it holds at least its own \
         standard streams"
    );

    // No event of the run describes the departed process, and the run ends for
    // the reason the runner knows -- which includes the exit status.
    for event in events(result) {
        assert!(
            attr(&event, "open").unwrap().as_u64().unwrap() > 0,
            "a reading of zero open descriptors is a zombie reported as a living process: \
             {event}"
        );
    }
    assert!(
        result["stopped_because"]
            .as_str()
            .is_some_and(|reason| reason.contains("exited")),
        "the run still ends when the program does: {}",
        result["stopped_because"]
    );
    assert_eq!(result["losses"]["failed_reads"], 0);

    // `proc.threads` had the same shape and a worse tell: a zombie's `status`
    // keeps its `Threads:` line, so the extra reading said one thread rather
    // than zero, which is indistinguishable by value from the single-threaded
    // program that was there a moment earlier. What it is distinguishable by
    // is the count: the two requests share one window and one target, so the
    // round that read the zombie added a point to both series, and the round
    // that no longer happens removes it from both.
    let threads = self::result(&doc, "threads");
    assert_eq!(
        emitted(threads, "timeline")["series"][0]["points"]
            .as_array()
            .map(Vec::len),
        emitted(result, "timeline")["series"][0]["points"]
            .as_array()
            .map(Vec::len),
        "both requests are driven over one window against one process, so a round that reads \
         the departed target adds a point to both series or to neither"
    );
    assert_eq!(
        emitted(threads, "summary")["series"][0]["last"]
            .as_f64()
            .unwrap() as u64,
        1,
        "the workload is single-threaded from its first instant to its last"
    );
}

/// Every request in a session keeps the rate it asked for.
///
/// The runner used to ask the collectors for a tick and, where none declared
/// one -- which is every poller in this family -- fall back to
/// `resolved[0].request.mode`, the *first* request, and apply it to all of
/// them. Three consequences, all silent, and all of them defect 3 of
/// `docs/scenarios/proc.md`:
///
///   - `poll@200ms` first, `poll@50ms` second: both ran at 200 ms, and the
///     second request was downsampled by four without being told.
///   - `poll@50ms` first, `snapshot` second: the snapshot became a series.
///   - `snapshot` first, `poll@50ms` second: the interval was zero and the run
///     became a busy loop against procfs for every source in it.
///
/// The reproduction is kept and what it asserts is turned round: the three
/// sessions below are the same three, and each now has to produce the
/// measurement its file describes.
#[test]
fn every_request_in_a_session_keeps_the_rate_it_asked_for() {
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

    // A slow first request no longer downsamples a fast second one. Over a
    // 400 ms window `poll@200ms` is two or three readings and `poll@50ms` is
    // eight or nine; the assertion is the separation rather than either
    // number, because this machine runs eight suites at once and a reading
    // that lands late is the tool being honest about a busy machine.
    let (slow, fast) = write("downsampled.yml", "poll@200ms", "poll@50ms");
    assert!(
        fast > slow,
        "`poll@50ms` took {fast} readings and `poll@200ms` took {slow}: the second \
         request is still being served at the first one's rate"
    );
    assert!(
        fast >= 5,
        "`poll@50ms` over 400ms is around nine readings, and this run took {fast}"
    );
    assert!(
        slow <= 4,
        "`poll@200ms` over 400ms is two or three readings, and this run took {slow}"
    );

    // A `snapshot` beside a `poll` is still a single read. The snapshot is a
    // property of the request, not of the session that happens to hold it.
    let (_, snapshot) = write("not-a-snapshot.yml", "poll@50ms", "snapshot");
    assert_eq!(
        snapshot, 1,
        "`snapshot` means one read, whatever is written beside it"
    );

    // And a `snapshot` written first no longer turns the session into a busy
    // loop: its zero interval used to become the interval of every request.
    // The threshold is an order of magnitude rather than a rate, because the
    // spinning thread's share of the cpu was whatever the rest of the suite
    // left it: no request in this session asks for more than nine readings,
    // and anything past fifty could only be an interval of zero.
    let (spun, polled) = write("busy-loop.yml", "snapshot", "poll@50ms");
    assert_eq!(spun, 1, "the snapshot is one read: {spun}");
    assert!(
        polled < 50,
        "`poll@50ms` over 400ms asks for nine readings and took {polled}, which is what \
         an interval of zero looks like"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

/// "This machine cannot serve it" exits 1, and not 2.
///
/// `BRIEF.md` defines three answers and three exit codes. Two of the codes used
/// to do the work of three: `src/main.rs` returned `EXIT_REJECTED` when the
/// resolver found no viable backend, one line after printing a hint saying the
/// opposite -- "the request is valid; it is this machine that cannot serve it"
/// -- so a caller could not tell a request it should rewrite from one it should
/// run elsewhere. That was defect 4 of `docs/scenarios/proc.md`, and the
/// reproduction is kept below.
///
/// No `proc` or `runtime` source can reach this state on a real machine --
/// every implemented backend needs only procfs or `/tmp` -- so the scenario
/// gets there through `--catalog`, with a copy of the catalog in which
/// `proc.fd` asks for a file that does not exist. That is a supported flag and
/// the resolver is the code under test either way.
#[test]
fn a_machine_that_cannot_serve_a_valid_request_has_its_own_exit_code() {
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

    // And the exit code says the same thing the sentence does. It used to say
    // 2 -- the code for a request that cannot mean anything -- one line under
    // the hint above, which is defect 4 of `docs/scenarios/proc.md`.
    assert_eq!(
        output.status.code(),
        Some(1),
        "a valid request this machine cannot serve is not a request that cannot mean \
         anything, and a caller that has only the code has to be able to tell them apart"
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

/// A late tick is one reason with a count, not one reason per tick.
///
/// `Losses::note` counts identical reasons, which is what turns a thousand
/// dropped events into one line saying `1000x`. The late-tick note used to
/// embed the exact duration of the round that overran, so no two were ever
/// equal: a run with seventy-seven late ticks printed seventy-seven `1x`
/// lines, carried seventy-seven keys into its JSON, and buried the one count it
/// existed to report. That was defect 6 of `docs/scenarios/proc.md`; the
/// reproduction below is the session that found it.
///
/// The reason is the kind now and the durations are magnitudes beside it, in
/// `losses.overrun`. This is the reproduction asserting the other answer: the
/// same session, the same late ticks, and one reason carrying all of them.
#[test]
fn a_late_tick_is_one_reason_with_a_count_rather_than_one_reason_each() {
    needs_python!();
    let output = session("proc-loss.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    let losses = &result(&doc, "thirty-thousand-handles")["losses"];

    let late = losses["late_ticks"].as_u64().unwrap();
    let reasons = losses["reasons"].as_object().unwrap();
    let distinct = reasons
        .keys()
        .filter(|reason| reason.contains("outran"))
        .count() as u64;

    assert!(late > 10, "the scenario has to actually run late: {losses}");
    assert_eq!(
        distinct, 1,
        "{late} late ticks have to collapse into one reason: {losses}"
    );
    assert_eq!(
        reasons
            .iter()
            .find(|(reason, _)| reason.contains("outran"))
            .and_then(|(_, count)| count.as_u64()),
        Some(late),
        "and that one reason has to carry the count it explains: {losses}"
    );

    // The measurement did not disappear with the key it was in: it moved
    // beside the count, where it can be aggregated instead of enumerated.
    let overrun = &losses["overrun"];
    assert!(
        overrun["worst_ns"].as_u64().unwrap() > overrun["tick_ns"].as_u64().unwrap(),
        "the worst overrun is what says whether the series is usable: {losses}"
    );
    assert!(
        overrun["total_ns"].as_u64().unwrap() >= overrun["worst_ns"].as_u64().unwrap(),
        "the total has to hold the worst one: {losses}"
    );
}
