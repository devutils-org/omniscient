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

/// `examples/proc-not-built.yml`: valid, resolvable, and refused because this
/// build has no collector for the backend it names.
///
/// A caller that cannot tell "you asked for something impossible" from "this is
/// not written yet" cannot decide whether to fix the request or wait for a
/// release, and one that cannot tell either from "this machine cannot serve it"
/// raises privileges against a collector nobody has written. So the session
/// validates cleanly, resolves, and exits 3 with the sentence that belongs to
/// that answer.
///
/// **This used to be a two-way test, and the sixth wave settled it.** Until
/// then the file held the four `proc` sources with no collector at all; all
/// four declared `bpf:tp` and nothing else, a classic tracepoint is attached by
/// reading a numeric event id out of a root-only file under tracefs, and on a
/// machine where that file is unreadable -- this project's own -- the honest
/// answer was 1, "this machine cannot serve it". The test had to accept either.
/// All four now collect through `bpf:raw_tp`, which needs a name and the
/// kernel's own BTF; what is left unimplemented is the second backend of each
/// chain, and `--backend force:` reaches it while bypassing the resolver
/// entirely. The answer no longer depends on what this machine can read, which
/// is what an exit code meaning "this build does not have it" should be.
#[test]
fn a_source_this_build_does_not_implement_exits_three_and_names_the_backend() {
    let validated = omn(&["validate", "-f", &example("proc-not-built.yml")]);
    assert!(
        validated.status.success(),
        "the session is valid; it is the build that is missing: {}",
        stderr(&validated)
    );

    let output = omn(&["-f", &example("proc-not-built.yml")]);
    let message = stderr(&output);
    assert_eq!(
        output.status.code(),
        Some(3),
        "a valid request for a backend nothing collects is this build's gap, and `force:` \
         bypasses the resolver so no machine can turn it into any other answer:\n{message}"
    );
    assert!(
        message.contains("no collector is written"),
        "exit 3 is this build's gap and must say so: {message}"
    );
    // The way out is named too: the request is fine and can be written down for
    // a build that has it.
    assert!(
        message.contains("--dump-request"),
        "a reader has to be told what they can still do with a valid request: {message}"
    );
    assert!(
        message.contains("bpf:tp"),
        "the refusal must name the backend: {message}"
    );
    assert!(
        message.contains("proc.exec"),
        "and the source it was asked for: {message}"
    );

    // And the sibling answer stays distinct: the *other* backend of the same
    // chain is written, so the same source without the pin measures.
    let served = omn(&[
        "proc.exec",
        "--system",
        "--mode",
        "trace",
        "--emit",
        "summary",
        "-d",
        "100ms",
    ]);
    if !stderr(&served).contains("CAP_BPF") {
        assert!(
            served.status.success(),
            "without the pin the resolver picks the backend this build does have: {}",
            stderr(&served)
        );
    }
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

// ---------------------------------------------------------------------------
// The four event sources, over eBPF
// ---------------------------------------------------------------------------
//
// `proc.exec`, `proc.exit`, `proc.signals` and `proc.syscall` collect since the
// sixth wave, through `bpf:raw_tp`. Everything above this line measures a
// process from outside on a clock; everything below reads events the kernel
// publishes as they happen, which is a different shape of scenario.
//
// Three of the four watch the machine -- the catalog declares no `pid` target
// for them, because "watch this process's execs" is a question about a pid that
// is about to mean something else. So their ground truth cannot come from a
// launched target the way `proc.fd`'s does. It comes from a workload this file
// starts *while the run is open*, whose identity nothing else on the machine
// can accidentally share: a copy of `/bin/sh` under a name this test invented.
// The same trick `examples/cpu-one-busy-core.yml` documents for a pinned spin
// loop, for the same reason.

use std::io::Read;
use std::process::{Child, Stdio};

/// How long the workload waits before it starts, so that the run under test is
/// attached by the time it does.
///
/// Attaching means reading a few megabytes of BTF, building two programs and
/// having the verifier accept them, which is tens of milliseconds. This is a
/// margin over that rather than a measurement of it -- and when it is not
/// enough the test fails saying so, with both numbers, rather than reporting a
/// smaller count as a result.
const ATTACH_MARGIN: std::time::Duration = std::time::Duration::from_millis(700);

/// Starts a session without waiting for it, so a workload can be driven while
/// it runs.
fn session_in_background(name: &str) -> Child {
    Command::new(BINARY)
        .current_dir(root())
        .args(["-f", &example(name), "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should start")
}

fn finish(child: Child) -> Output {
    child.wait_with_output().expect("omn should finish")
}

/// A copy of `/bin/sh` under a name nothing else on this machine has.
///
/// The name is what makes a machine-wide scenario checkable: a count of execs
/// on a busy machine is whatever the machine was doing, and a count of execs
/// *of this path* is exactly what this test ran. It is kept to ten characters
/// because the kernel truncates `comm` to fifteen, and a name that is cut in
/// half is one the exit events cannot be matched on.
struct UniqueShell {
    path: PathBuf,
}

impl UniqueShell {
    fn new() -> Option<Self> {
        // The pid alone is not unique enough: `cargo test` runs these in
        // parallel threads of one process, so every test here would ask for the
        // same name and each would then count the others' programs as its own.
        // That is the same trap `docs/contributing.md` records for descriptor
        // counts, in a new place.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("omnx{}x{serial}", std::process::id() % 100_000));
        std::fs::copy("/bin/sh", &path).ok()?;
        let mut permissions = std::fs::metadata(&path).ok()?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&path, permissions).ok()?;
        Some(Self { path })
    }

    fn name(&self) -> String {
        self.path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned()
    }

    fn as_str(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    /// Runs the shell on one script and returns what it exited with.
    fn run(&self, script: &str) -> std::process::ExitStatus {
        Command::new(&self.path)
            .args(["-c", script])
            .status()
            .expect("the copied shell runs")
    }
}

impl Drop for UniqueShell {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Every event of one result, whatever emit carries them.
fn attr_f64(event: &serde_json::Value, name: &str) -> Option<f64> {
    attr(event, name).and_then(|value| value.as_f64())
}

fn attr_text(event: &serde_json::Value, name: &str) -> Option<String> {
    attr(event, name).and_then(|value| value.as_str().map(str::to_owned))
}

/// Whether a run refused because this machine will not load an eBPF program.
///
/// A skip that says so beats a test that passes because it did nothing -- and
/// it must be told apart from a run that attached and found nothing, which is a
/// failure.
fn skipped_without_bpf(output: &Output) -> bool {
    let message = stderr(output);
    if message.contains("CAP_BPF") || message.contains("Operation not permitted") {
        eprintln!("skipped: {message}");
        return true;
    }
    false
}

/// Class ground truth, `proc.exec`: a program this test ran, exactly as many
/// times as it ran it, under a path nothing else on the machine has.
///
/// The count is exact rather than a lower bound, and that is the point: the
/// path is unique to this test, so a machine executing a thousand other things
/// at the same time contributes none of them. An event more or less than the
/// number of `execve` calls this test made is a defect either way.
#[test]
fn every_exec_of_a_path_this_test_invented_is_reported_once() {
    const RUNS: usize = 5;
    let Some(shell) = UniqueShell::new() else {
        eprintln!("skipped: this machine has no /bin/sh to copy");
        return;
    };

    let run = session_in_background("proc-lifecycle.yml");
    std::thread::sleep(ATTACH_MARGIN);
    for _ in 0..RUNS {
        assert!(shell.run("exit 0").success());
    }
    let output = finish(run);
    if skipped_without_bpf(&output) {
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let doc = document(&output);
    let path = shell.as_str();
    let mine: Vec<_> = events(result(&doc, "what-ran"))
        .into_iter()
        .filter(|event| attr_text(event, "filename").as_deref() == Some(path.as_str()))
        .collect();
    assert_eq!(
        mine.len(),
        RUNS,
        "`{path}` was executed {RUNS} times and {} exec events carry it; losses {}",
        mine.len(),
        result(&doc, "what-ran")["losses"]
    );
    for event in &mine {
        assert_eq!(
            attr_text(event, "comm").as_deref(),
            Some(shell.name().as_str()),
            "the name a task takes at exec is the last component of its path"
        );
        assert_eq!(
            attr_f64(event, "truncated"),
            Some(0.0),
            "a path this short did not fill the record"
        );
    }
}

/// Class ground truth, `proc.exit`: a status this test chose, and a signal this
/// test sent, reported as the two different things they are.
///
/// The kernel packs both into one word -- a task killed by `SIGKILL` carries 9
/// in `exit_code` and a task that returned 9 carries 2304 -- so a build that
/// reported the raw word would call a killed program one that exited 9. Both
/// are driven here, in one run, against the same program.
#[test]
fn a_chosen_exit_status_and_a_sent_signal_are_reported_as_different_things() {
    let Some(shell) = UniqueShell::new() else {
        eprintln!("skipped: this machine has no /bin/sh to copy");
        return;
    };

    let run = session_in_background("proc-lifecycle.yml");
    std::thread::sleep(ATTACH_MARGIN);
    assert_eq!(shell.run("exit 7").code(), Some(7));
    // `kill` is a shell builtin, so this is the same process signalling itself
    // and no second program is executed.
    assert_eq!(shell.run("kill -9 $$").code(), None);
    let output = finish(run);
    if skipped_without_bpf(&output) {
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let doc = document(&output);
    let name = shell.name();
    let mine: Vec<_> = events(result(&doc, "what-ended"))
        .into_iter()
        .filter(|event| attr_text(event, "comm").as_deref() == Some(name.as_str()))
        .collect();
    assert_eq!(
        mine.len(),
        2,
        "two copies of `{name}` were run and left; {} exit events carry that name",
        mine.len()
    );

    let returned = mine
        .iter()
        .find(|event| attr_f64(event, "signal") == Some(0.0))
        .expect("one of the two returned a status");
    assert_eq!(
        attr_f64(returned, "code"),
        Some(7.0),
        "the shell was told to exit 7"
    );
    assert_eq!(attr_f64(returned, "status"), Some(7.0));
    assert_eq!(
        attr_f64(returned, "exit_code"),
        Some(f64::from(7 << 8)),
        "the kernel's own word is carried undecoded beside the decoded one"
    );

    let killed = mine
        .iter()
        .find(|event| attr_f64(event, "signal") == Some(9.0))
        .expect("the other was killed by SIGKILL");
    assert_eq!(
        attr_f64(killed, "code"),
        Some(137.0),
        "128 + the signal, which is the shell's convention and what `code` says it is"
    );
    assert_eq!(
        attr(killed, "status"),
        None,
        "a task a signal killed returned no status, and inventing one is the defect"
    );
    assert_eq!(
        attr_f64(killed, "exit_code"),
        Some(9.0),
        "the raw word is the signal number, which is why it is not reported as `code`"
    );
    assert_eq!(
        attr_f64(killed, "group_dead"),
        Some(1.0),
        "a single-threaded program's exit is the last thread of its process"
    );
}

/// Class ground truth, `proc.syscall`: a known number of calls, and a delay
/// this session injected.
///
/// Two independent properties, and neither comes from the tool. The workload
/// calls `getppid` exactly a thousand times -- glibc does not cache it and a
/// Python start-up makes none -- and it sleeps fifty milliseconds ten times.
/// The count must be exactly a thousand; the sleeps must be exactly ten and
/// their mean must be the delay that was injected.
#[test]
fn a_known_number_of_calls_and_an_injected_delay_are_both_reported_as_chosen() {
    needs_python!();
    let output = session("proc-syscall.yml");
    if skipped_without_bpf(&output) {
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);

    let calls = reported(&output, "CALLS ");
    assert_eq!(calls, 1000, "the workload says how many calls it made");

    // Each row of a kernel-side histogram stands for as many calls as its
    // `count` says, so the number of calls is the sum of those and not the
    // number of events.
    let cheap = result(&doc, "a-thousand-cheap-calls");
    let counted: f64 = events(cheap)
        .iter()
        .filter_map(|event| attr_f64(event, "count"))
        .sum();
    assert_eq!(
        counted as u64, calls,
        "the workload made {calls} calls and {counted} were counted; losses {}",
        cheap["losses"]
    );

    let delayed = result(&doc, "ten-injected-delays");
    let sleeps = events(delayed);
    let count: f64 = sleeps
        .iter()
        .filter_map(|event| attr_f64(event, "count"))
        .sum();
    let total: f64 = sleeps
        .iter()
        .filter_map(|event| attr_f64(event, "total_ns"))
        .sum();
    assert_eq!(count as u64, 10, "the workload sleeps ten times");

    // The mean comes from `total_ns`, which is the exact sum the kernel kept
    // beside the bucket; the bucket itself is a factor of two wide and could
    // not answer this. A sleep is a lower bound on its own duration -- the
    // kernel returns once the timer has expired *and* the thread is scheduled
    // -- so the separation is what is asserted rather than a threshold: at
    // least the delay that was asked for, and less than twice it.
    let mean = total / count;
    assert!(
        (50_000_000.0..100_000_000.0).contains(&mean),
        "ten sleeps of 50ms reported a mean of {mean}ns"
    );

    // And every one of them is in the bucket that holds fifty milliseconds,
    // which is [2^25, 2^26) -- the one a `hist` puts them in.
    for event in &sleeps {
        assert_eq!(
            attr_f64(event, "lat_ns"),
            Some(33_554_432.0),
            "a 50ms call belongs in [33.5ms, 67ms) and the value on the event is that \
             bucket's low bound"
        );
    }
}

/// The filter the resolved file says ran in the kernel is the one that did.
///
/// The resolver credits any backend whose id begins with `bpf` with pushing
/// predicates down, and writes `"placement": "kernel"` into the record kept
/// beside the numbers. For this source that claim is about cost as well as
/// about the numbers, because `raw_syscalls` fires on everything: a predicate
/// applied after the fact still pays for every other syscall on the machine. A
/// filter reported as pushed down and applied in userspace is what
/// `DECISIONS.md` calls the worst shape a bug takes here, so the claim and the
/// run's own account of itself are checked against each other.
#[test]
fn the_syscall_filter_the_resolved_file_claims_is_the_one_that_ran() {
    let resolved = omn(&[
        "proc.syscall",
        "--system",
        "--mode",
        "trace",
        "--emit",
        "hist",
        "--filter",
        "syscall == 'getppid'",
        "--dump-resolved",
    ]);
    assert!(resolved.status.success(), "{}", stderr(&resolved));
    let record: serde_json::Value =
        serde_json::from_str(&stdout(&resolved)).expect("one json document");
    assert_eq!(
        record["filter"]["placement"], "kernel",
        "the record kept beside the numbers claims the predicate ran in the program"
    );

    needs_python!();
    let output = session("proc-syscall-sweep.yml");
    if skipped_without_bpf(&output) {
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    let result = result(&doc, "syscall-every-emit");

    let account = result["degradations"]
        .as_array()
        .expect("a run says what qualifies its numbers")
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        account.contains("pushed into the program") && account.contains("getppid"),
        "the run has to say which predicate the program carried, or the resolved file's \
         claim is unverifiable: {account}"
    );

    // And nothing else was timed. The workload makes thousands of other
    // syscalls -- every `write`, every `mmap` of a Python start-up -- and none
    // of them may be in these numbers.
    for event in events(result) {
        assert_eq!(
            attr_text(&event, "syscall").as_deref(),
            Some("getppid"),
            "a syscall the program was narrowed away from is in the output, so the filter \
             did not run where the resolved file says it did"
        );
    }
}

/// Class the loss path: what the ring could not hold is counted, and the count
/// closes against a number the workload chose.
///
/// This is the only place in the family where a loss can be checked rather than
/// merely observed. The workload sends itself three hundred thousand real-time
/// signals -- real-time signals queue, so every one of them is generated -- and
/// the identity that must hold is `counted + dropped >= sent`: a signal that
/// was generated was either counted or dropped, and the drop counter is
/// machine-wide so it also holds whatever else was lost in the same window.
///
/// On a machine fast enough to drain the ring, `dropped` is zero and `counted`
/// is exactly `sent`. Both readings are correct and both are asserted, because
/// asserting that this machine must lose something would be asserting the
/// machine. What would be wrong -- and what this catches -- is a run that
/// reported fewer signals than were sent and called itself clean.
#[test]
fn what_the_signal_ring_could_not_hold_is_counted_and_closes_against_what_was_sent() {
    needs_python!();
    const SENT: u64 = 300_000;
    let storm = format!(
        "import os, signal, sys, time\n\
         signal.signal(signal.SIGRTMIN, lambda *a: None)\n\
         time.sleep(0.1)\n\
         pid = os.getpid()\n\
         for _ in range({SENT}):\n    os.kill(pid, signal.SIGRTMIN)\n\
         sys.stderr.write('SENT {SENT}\\n')\n"
    );

    let run = session_in_background("proc-signal-storm.yml");
    std::thread::sleep(ATTACH_MARGIN);
    let mut workload = Command::new("python3")
        .args(["-c", &storm])
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 runs");
    let mut said = String::new();
    workload
        .stderr
        .take()
        .expect("a pipe")
        .read_to_string(&mut said)
        .expect("readable");
    let target = workload.id();
    assert!(workload.wait().expect("the workload finishes").success());
    assert!(
        said.contains(&format!("SENT {SENT}")),
        "the workload has to say how many it sent: {said}"
    );

    let output = finish(run);
    if skipped_without_bpf(&output) {
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    let result = result(&doc, "storm");

    let dropped = result["losses"]["dropped"].as_u64().expect("a drop count");
    let counted: u64 = events(result)
        .iter()
        .filter(|event| attr_f64(event, "pid") == Some(f64::from(target)))
        .filter(|event| attr_f64(event, "sig").is_some_and(|sig| sig >= 32.0))
        .filter_map(|event| attr_f64(event, "count"))
        .sum::<f64>() as u64;

    assert!(
        counted <= SENT,
        "{counted} signals were counted and only {SENT} were sent"
    );
    if dropped == 0 {
        assert_eq!(
            counted, SENT,
            "nothing was dropped, so every signal sent has to have been counted"
        );
    } else {
        assert!(
            counted + dropped >= SENT,
            "{counted} counted plus {dropped} dropped is fewer than the {SENT} that were sent, \
             so events went missing without being counted as lost"
        );
        // The loss is a drop and not a late tick. A dropped event is a
        // measurement that happened and was not recorded; a late tick is a
        // series whose spacing is not what it claims, and a ring-backed source
        // has no clock to run late.
        assert_eq!(
            result["losses"]["late_ticks"].as_u64(),
            Some(0),
            "a full ring is a drop, not a tick that ran late: {}",
            result["losses"]
        );
        let reasons = result["losses"]["reasons"]
            .as_object()
            .expect("a loss says why");
        assert!(
            reasons
                .keys()
                .any(|reason| reason.contains("ring was full")),
            "the reason has to name the ring: {reasons:?}"
        );
    }
}

/// Class the capability sweep: every emit these three declare is produced, and
/// every filter field they declare is on the events.
///
/// The catalog is data and the CLI is a projection of it, so a combination the
/// catalog offers and the build cannot serve is a promise broken where nobody
/// looks: `omn list proc` would still say `yes`.
#[test]
fn every_emit_and_filter_field_the_three_machine_wide_sources_declare_works() {
    let Some(shell) = UniqueShell::new() else {
        eprintln!("skipped: this machine has no /bin/sh to copy");
        return;
    };

    let run = session_in_background("proc-bpf-sweep.yml");
    std::thread::sleep(ATTACH_MARGIN);
    // Something for each of the three to find: two programs start, two end,
    // and one of them signals itself.
    assert!(shell.run("exit 0").success());
    assert!(shell.run("kill -USR1 $$").code().is_none());
    let output = finish(run);
    if skipped_without_bpf(&output) {
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);

    for (label, emits, fields) in [
        (
            "exec-every-emit",
            &["raw", "timeline", "topk", "summary"][..],
            &["comm", "filename"][..],
        ),
        (
            "exit-every-emit",
            &["raw", "timeline", "topk", "summary"][..],
            &["comm", "code"][..],
        ),
        (
            "signals-every-emit",
            &["raw", "topk", "rate", "summary"][..],
            &["comm", "sig", "pid"][..],
        ),
    ] {
        let result = result(&doc, label);
        for emit in emits {
            let output = emitted(result, emit);
            let filled = match *emit {
                "raw" => !output["events"].as_array().expect("events").is_empty(),
                _ => !output["series"].as_array().expect("series").is_empty(),
            };
            assert!(
                filled,
                "`{label}` produced an empty `{emit}`, and the workload this test ran is one \
                 the source had to see"
            );
        }
        let events = events(result);
        assert!(!events.is_empty(), "`{label}` saw nothing");
        for event in &events {
            for field in fields {
                assert!(
                    attr(event, field).is_some(),
                    "`{label}`: the catalog offers a `{field}` filter and this event carries \
                     none, so a predicate over it would be false everywhere"
                );
            }
        }
    }
}

/// Class the refusal: the combinations these four sources do not have are
/// refused by name, and not approximated.
///
/// Each of these is the axis interaction working. A caller that gets a number
/// back from one of them has been told something that is not true.
#[test]
fn the_combinations_these_sources_do_not_have_are_refused_with_the_reason() {
    const EXIT_REJECTED: i32 = 2;

    // A mode that never sees an individual event cannot rank who produced them.
    let output = omn(&[
        "proc.signals",
        "--system",
        "--mode",
        "count",
        "--emit",
        "topk",
        "-d",
        "50ms",
    ]);
    assert_eq!(
        output.status.code(),
        Some(EXIT_REJECTED),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("cannot produce `topk`"),
        "{}",
        stderr(&output)
    );

    // A duration is the gap between two events, and a sample that catches one
    // of them without the other has no duration to report.
    let output = omn(&[
        "proc.syscall",
        "--system",
        "--mode",
        "sample@99hz",
        "--emit",
        "hist",
        "-d",
        "50ms",
    ]);
    assert_eq!(
        output.status.code(),
        Some(EXIT_REJECTED),
        "{}",
        stderr(&output)
    );

    // A pid is not a target these two have: an exec is the moment a pid stops
    // being one program and starts being another.
    let output = omn(&["proc.exec", "--pid", "1", "--emit", "raw", "-d", "50ms"]);
    assert_eq!(
        output.status.code(),
        Some(EXIT_REJECTED),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("pid"),
        "the refusal has to name the target: {}",
        stderr(&output)
    );

    // A syscall this build's table does not know matches nothing, which reads
    // exactly like a process that made no calls. It is refused instead.
    let output = omn(&[
        "proc.syscall",
        "--system",
        "--mode",
        "trace",
        "--emit",
        "hist",
        "--filter",
        "syscall == 'wriet'",
        "-d",
        "50ms",
    ]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "a misspelled syscall must not be served: {}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("wriet"),
        "and the refusal has to name it: {}",
        stderr(&output)
    );
}

// ---------------------------------------------------------------------------
// The silent degradations (the eleventh wave, stream S)
// ---------------------------------------------------------------------------
//
// Each of these was a request the tool accepted and then answered with a
// different question, saying nothing. Every one fails against the build that
// had the defect, and `docs/fixes/silent.md` has the command that found it.

/// Runs `omn` with these arguments in the background, for a machine-wide
/// source the test drives a workload under.
fn omn_in_background(args: &[&str]) -> Child {
    Command::new(BINARY)
        .current_dir(root())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should start")
}

/// `syscall == 110` counts the getppid calls a program made, and not zero.
///
/// The number was pushed into the program correctly -- only getppid was timed
/// -- and then a second, userspace pass compared 110 with the event's
/// `syscall`, which is the name, and dropped every row the kernel had kept:
/// "the predicate removed 2000 calls here and kept 0", on a workload that made
/// two thousand. Ground truth is the workload's own loop, and `getppid` is the
/// one call glibc does not cache and a Python start-up does not make.
#[test]
fn a_syscall_filter_by_number_counts_the_calls_the_program_made() {
    needs_python!();
    const CALLS: u64 = 2000;
    let getppid = if cfg!(target_arch = "x86_64") {
        "110"
    } else if cfg!(target_arch = "aarch64") {
        "173"
    } else {
        eprintln!("skipped: this scenario knows getppid's number on x86_64 and aarch64 only");
        return;
    };
    let filter = format!("syscall == {getppid}");
    let program = format!(
        "import os, sys\nfor _ in range({CALLS}):\n    os.getppid()\nsys.stderr.write('CALLS {CALLS}\\n')\n"
    );
    let output = omn(&[
        "proc.syscall",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--filter",
        &filter,
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        &program,
    ]);
    if skipped_without_bpf(&output) {
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(reported(&output, "CALLS "), CALLS);
    let doc = document(&output);
    let result = &doc["results"][0];
    let rows = events(result);
    let counted: f64 = rows.iter().filter_map(|row| attr_f64(row, "count")).sum();
    assert_eq!(
        counted as u64, CALLS,
        "the program called getppid {CALLS} times and `{filter}` counted {counted}; \
         degradations {}, losses {}",
        result["degradations"], result["losses"]
    );
    for row in &rows {
        assert_eq!(
            attr_f64(row, "nr"),
            getppid.parse().ok(),
            "a syscall other than the one filtered on is in the output"
        );
    }
}

/// The machine-wide `proc` sources apply the filter they accept.
///
/// `proc.exec --filter 'filename == "/nonexistent/path"'` reported every exec
/// on the machine and said nothing about the filter, and so did the same
/// predicate on `proc.exit` and `proc.signals`: the resolver credits a `bpf`
/// backend with filtering in the program, the runner then leaves its events
/// alone, and none of the three collectors applied it. A filter accepted and
/// not applied is the one outcome the invariants rule out.
///
/// Ground truth is a shell under a name nothing else on the machine has, run a
/// number of times this test chose, among execs of `/bin/true` that every one
/// of the three filters must remove. Three observers, one workload.
#[test]
fn a_filter_on_the_machine_wide_proc_sources_keeps_only_what_it_names() {
    const RUNS: usize = 4;
    let Some(shell) = UniqueShell::new() else {
        eprintln!("skipped: this machine has no /bin/sh to copy");
        return;
    };
    let path = shell.as_str();
    let name = shell.name();
    let observe = |source: &str, filter: String| {
        omn_in_background(&[
            source, "--system", "-d", "3s", "--mode", "trace", "--emit", "raw", "--filter",
            &filter, "--format", "json",
        ])
    };
    let execs = observe("proc.exec", format!("filename == '{path}'"));
    let exits = observe("proc.exit", format!("comm == '{name}' && group_dead == 1"));
    let signals = observe("proc.signals", format!("comm == '{name}' && sig == 10"));
    std::thread::sleep(ATTACH_MARGIN);
    for _ in 0..RUNS {
        assert!(shell.run("exit 0").success());
        assert!(shell.run("kill -USR1 $$").code().is_none());
        // What every filter has to remove.
        assert!(Command::new("/bin/true").status().expect("true").success());
    }

    for (child, what, expected) in [
        (execs, "exec", 2 * RUNS),
        (exits, "exit", 2 * RUNS),
        (signals, "signal", RUNS),
    ] {
        let output = finish(child);
        if skipped_without_bpf(&output) {
            return;
        }
        assert!(output.status.success(), "{what}: {}", stderr(&output));
        let doc = document(&output);
        let result = &doc["results"][0];
        let events = events(result);
        for event in &events {
            assert_eq!(
                attr_text(event, "comm").as_deref(),
                Some(name.as_str()),
                "a `{what}` the filter does not name is in the output: {event}"
            );
        }
        let clean = result["losses"]["dropped"] == 0;
        if clean {
            assert_eq!(
                events.len(),
                expected,
                "this test made {expected} `{what}` events the filter names and {} came back",
                events.len()
            );
        } else {
            assert!(events.len() <= expected, "{what}: {}", result["losses"]);
        }
        let said = result["degradations"].to_string();
        assert!(
            said.contains("applied here"),
            "a `{what}` run that filtered has to say where the predicate ran: {said}"
        );
    }
}
