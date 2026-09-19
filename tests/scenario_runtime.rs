//! Scenarios for the `runtime` family.
//!
//! Every test here runs a session that is also a file in `examples/`, so a
//! reader can reproduce any of them by hand with `omn -f examples/<name>.yml`.
//!
//! Ground truth in this family comes in two shapes, and both are used below
//! because neither is sufficient alone.
//!
//! Against a **written map**, the workload is a shell script that writes its
//! own `/tmp/perf-<pid>.map`, so the number of entries, their sizes, their
//! names and the exact moment the file is truncated or deleted are all chosen
//! rather than observed. That gives exact counts, and it is the only way to
//! reach the loss paths on purpose.
//!
//! Against a **real V8**, the workload compiles a function whose name no
//! plausible constant could produce. The count is not ground truth there --
//! how many times V8 decides to compile a function is V8's business -- but the
//! name is: an event naming `omniscientGroundTruth` came from a real JIT's
//! real map.
//!
//! Some tests assert a **defect** rather than correct behaviour, and say so in
//! their name. This stream may not edit `src/`, and a defect recorded only in
//! prose is one that gets fixed silently or not at all. When one starts
//! failing, the defect is fixed: delete the test and its entry in
//! `docs/scenarios/runtime.md` together.

use std::path::PathBuf;
use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

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

fn result<'a>(doc: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
    doc["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("no request labelled `{label}` in {doc}"))
}

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

fn attr(event: &serde_json::Value, name: &str) -> serde_json::Value {
    event["attrs"]
        .as_array()
        .expect("attributes")
        .iter()
        .find(|attr| attr["name"] == name)
        .unwrap_or_else(|| panic!("no `{name}` on {event}"))["value"]
        .clone()
}

fn methods(result: &serde_json::Value) -> Vec<String> {
    events(result)
        .iter()
        .map(|event| attr(event, "method").as_str().unwrap().to_owned())
        .collect()
}

fn has_node() -> bool {
    Command::new("node").arg("--version").output().is_ok()
}

/// Removes what a launched runtime left behind, by the pid the events say it
/// had. A test that scatters files and leaves them there is a test that fails
/// differently the second time.
fn clean_up_after(doc: &serde_json::Value) {
    let Some(pid) = doc["results"][0]["outputs"][0]["events"][0]["keys"][0]["task"]["tid"].as_i64()
    else {
        return;
    };
    let _ = std::fs::remove_file(format!("/tmp/perf-{pid}.map"));
    let _ = std::fs::remove_file(format!("/tmp/omn-v8-{pid}.log"));
}

/// A skip that names what is missing. A test that skips is not a test that
/// passes, and a reader has to be able to tell the two apart.
macro_rules! needs_node {
    () => {
        if !has_node() {
            eprintln!(
                "skipped: this machine has no node to launch, and the scenarios that check \
                 this source against a real JIT need a runtime that writes \
                 /tmp/perf-<pid>.map -- node --perf-basic-prof, or a JVM with an agent \
                 that maintains one. The written-map scenarios in this file do run here \
                 and cover everything except whether a real runtime's format is read."
            );
            return;
        }
    };
}

// ---------------------------------------------------------------------------
// Class A -- the capability sweep
// ---------------------------------------------------------------------------

/// `examples/runtime-jit-sweep.yml`: every emit and every filter field this
/// source declares, against a map whose every entry the workload chose.
///
/// Ground truth: the script writes exactly eight entries of 0x10 bytes each,
/// named `omniscientSwept1` through `omniscientSwept8`. Eight events, those
/// names, that size. Nothing else writes that file.
#[test]
fn the_sweep_reads_back_exactly_the_map_the_workload_wrote() {
    const WRITTEN: usize = 8;

    let output = session("runtime-jit-sweep.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    let result = result(&doc, "everything");

    // Every emit the catalog declares, present and shaped like itself.
    for emit in ["raw", "rate", "topk", "timeline", "summary"] {
        let output = emitted(result, emit);
        assert!(
            output.get("events").is_some() || output.get("series").is_some(),
            "`{emit}` produced neither events nor series: {output}"
        );
    }

    assert_eq!(
        result["events_seen"].as_u64().unwrap() as usize,
        WRITTEN,
        "the workload wrote {WRITTEN} entries"
    );

    let methods = methods(result);
    for index in 1..=WRITTEN {
        assert!(
            methods
                .iter()
                .any(|method| method.contains(&format!("omniscientSwept{index}"))),
            "entry {index} was written and is not in {methods:?}"
        );
    }
    for event in &events(result) {
        assert_eq!(
            attr(event, "bytes"),
            0x10,
            "every entry declared 0x10 bytes"
        );
        assert_eq!(attr(event, "code_kind"), "js");
    }
}

/// The filters the same session carries select rather than merely being
/// accepted.
///
/// Eight entries are written; one matches the name filter and all eight match
/// the kind filter. A predicate that is parsed, validated, reported as applied
/// and matches nothing is worse than one that is refused -- this source has
/// already had one, which is why its attribute is called `code_kind` and not
/// `kind`.
#[test]
fn the_declared_filter_fields_select_rather_than_emptying_the_series() {
    let output = session("runtime-jit-sweep.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);

    let by_name = result(&doc, "filtered-by-name");
    assert_eq!(
        by_name["events_seen"], 1,
        "one of the eight entries is named `omniscientSwept1`"
    );
    assert!(methods(by_name)[0].contains("omniscientSwept1"));

    let by_kind = result(&doc, "filtered-by-kind");
    assert_eq!(
        by_kind["events_seen"], 8,
        "all eight entries the workload wrote are JavaScript"
    );

    // The run says what the filter did, which is the half that makes an empty
    // series readable rather than mysterious: the predicate as it was
    // understood, what it kept and what it removed.
    let filtered = &by_name["filtered"];
    assert_eq!(filtered["predicate"], "method =~ 'omniscientSwept1'");
    assert_eq!(filtered["kept"], 1);
    assert_eq!(
        filtered["dropped"], 7,
        "a reader has to be able to tell a filter that removed seven from a workload          that produced one: {filtered}"
    );
}

/// The formats this source's emits can be carried in, and the ones they cannot.
///
/// `runtime.jit` has no stacks, so the stack formats must refuse it by name
/// rather than write an empty profile.
#[test]
fn every_format_either_carries_these_emits_or_refuses_them_by_name() {
    let workload = "map=/tmp/perf-$$.map; \
                    printf '40001 10 JS:~omniscientFormatted a.js:1:1\\n' >> $map; \
                    sleep 0.05; rm -f $map";
    let measure = |emit: &str, format: &str| {
        omn(&[
            "runtime.jit",
            "--mode",
            "trace",
            "--emit",
            emit,
            "--format",
            format,
            "--",
            "sh",
            "-c",
            workload,
        ])
    };

    for emit in ["raw", "rate", "topk", "timeline", "summary"] {
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

    for (format, emit) in [("folded", "raw"), ("pprof", "topk"), ("speedscope", "raw")] {
        let output = measure(emit, format);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`{format}` has no stacks to carry here and must refuse: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(format) && stderr(&output).contains(emit),
            "the refusal must name both: {}",
            stderr(&output)
        );
    }
}

/// `examples/runtime-jit.yml`, against a real V8 rather than a written file.
///
/// Ground truth against a name only this scenario could have produced: the
/// program defines and calls `fib`, and V8 publishes what it compiled. The
/// count is V8's business and is not asserted; the name is not, and no
/// plausible constant produces it.
///
/// This also checks the caveat the source exists for. Most of a node map is
/// V8's own precompiled builtins, which are not compilations this program
/// caused, and the session filters them out on `code_kind` -- so both kinds
/// have to be present and distinguishable in an unfiltered run, or the caveat
/// that says to filter on them is useless.
#[test]
fn a_function_a_real_runtime_compiled_is_named_in_the_events() {
    needs_node!();

    let output = session("runtime-jit.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    clean_up_after(&doc);
    let result = result(&doc, "runtime.jit");

    let methods = methods(result);
    assert!(
        methods.iter().any(|method| method.contains("fib")),
        "the function the program compiled is not in the {} events read",
        methods.len()
    );
    for event in &events(result) {
        assert_eq!(
            attr(event, "code_kind"),
            "js",
            "the session filters on `code_kind == 'js'` and this survived it"
        );
    }

    // Unfiltered, the builtins are there too and are marked as what they are.
    let unfiltered = omn(&[
        "runtime.jit",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--format",
        "json",
        "--",
        "node",
        "--perf-basic-prof",
        "--logfile=/tmp/omn-v8-%p.log",
        "-e",
        "function omniscientGroundTruth(n) { return n < 2 ? n : \
         omniscientGroundTruth(n - 1) + omniscientGroundTruth(n - 2); }\n\
         let total = 0;\n\
         for (let i = 0; i < 25; i++) total += omniscientGroundTruth(20);\n\
         if (total !== 4228125) throw new Error('unexpected ' + total);\n",
    ]);
    assert!(unfiltered.status.success(), "{}", stderr(&unfiltered));
    let doc = document(&unfiltered);
    clean_up_after(&doc);
    let events = doc["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("raw events")
        .clone();

    assert!(
        events.iter().any(|event| attr(event, "method")
            .as_str()
            .is_some_and(|method| method.contains("omniscientGroundTruth"))),
        "a name only this test could have produced is missing from {} events",
        events.len()
    );
    let kinds: Vec<String> = events
        .iter()
        .map(|event| attr(event, "code_kind").as_str().unwrap().to_owned())
        .collect();
    assert!(kinds.iter().any(|kind| kind == "js"), "no JS was named");
    assert!(
        kinds.iter().any(|kind| kind == "builtin"),
        "node publishes its builtins through the same map, and they must be \
         distinguishable or the caveat telling a reader to filter them out is useless"
    );
}

// ---------------------------------------------------------------------------
// Class B -- the backend comparison
// ---------------------------------------------------------------------------

/// `examples/runtime-backends.yml` and the half of it that cannot share its
/// session: the chain has two backends and one of them is written, so the
/// comparison is between a measurement and a refusal.
///
/// Both halves must behave differently and for the right reason. The written
/// one measures and declares what it degrades; the unwritten one exits 3 --
/// "this build does not implement it" -- rather than 1, which would read as a
/// run that failed.
#[test]
fn forcing_the_written_backend_measures_and_forcing_the_other_refuses_by_name() {
    const WRITTEN: usize = 6;

    let output = session("runtime-backends.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    let result = result(&doc, "through-the-map");

    assert_eq!(result["backend"], "fs:jit-map");
    assert_eq!(
        result["events_seen"].as_u64().unwrap() as usize,
        WRITTEN,
        "the workload wrote {WRITTEN} entries"
    );
    let methods = methods(result);
    for index in 1..=WRITTEN {
        assert!(
            methods
                .iter()
                .any(|method| method.contains(&format!("omniscientPinned{index}"))),
            "entry {index} is missing from {methods:?}"
        );
    }

    // A pinned backend is not a measurement to keep, and the run says so twice:
    // as a warning before it and as a degradation in it.
    assert!(
        stderr(&output).contains("pinned"),
        "the warning is missing: {}",
        stderr(&output)
    );
    let degradations = result["degradations"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|note| note.as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(
        degradations.iter().any(|note| note.contains("pinned")),
        "the output has to record that the chain was not walked: {degradations:?}"
    );
    // And what this backend costs the reader, which is why it is second in the
    // chain rather than first.
    assert!(
        degradations
            .iter()
            .any(|note| note.contains("does not time the compilation")),
        "the declared degradation must travel with the numbers: {degradations:?}"
    );

    // The other road: the same source, pinned to the backend nobody wrote.
    let refused = omn(&[
        "runtime.jit",
        "--backend",
        "force:bpf:usdt",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--",
        "sh",
        "-c",
        "sleep 0.05",
    ]);
    assert_eq!(
        refused.status.code(),
        Some(3),
        "an unwritten backend is not a failed run: {}",
        stderr(&refused)
    );
    let message = stderr(&refused);
    assert!(
        message.contains("bpf:usdt") && message.contains("no collector is written"),
        "the refusal must name the backend that is missing: {message}"
    );
}

// ---------------------------------------------------------------------------
// Class C -- the loss path
// ---------------------------------------------------------------------------

/// `examples/runtime-jit-loss.yml`: four ways this source loses data, all four
/// in one run, all four with a right answer the workload chose.
///
/// The map is a plain file, so the workload can abuse it on purpose: six
/// well-formed lines, two that are not the format, a truncation, and a
/// deletion while the program is still running. Each counter is separate
/// because each failure is different, and a collector that folded them into
/// one number would be telling a reader to look in the wrong place.
#[test]
fn the_four_ways_this_source_loses_data_are_each_counted_and_named() {
    const WELL_FORMED: u64 = 6;
    const GARBAGE: u64 = 2;

    let output = session("runtime-jit-loss.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    let result = &doc["results"][0];
    let losses = &result["losses"];

    // The six good lines, plus the one written after the truncation.
    assert_eq!(
        result["events_seen"],
        WELL_FORMED + 1,
        "six entries, then one more after the map was replaced: {result}"
    );

    assert_eq!(
        losses["undecoded"], GARBAGE,
        "two lines were written in no format this build knows, and a map it cannot \
         read must not come back as a program that compiled nothing: {losses}"
    );
    assert!(
        losses["dropped"].as_u64().unwrap() >= 1,
        "the map was truncated, which means it was replaced and what stood between the \
         two reads is gone: {losses}"
    );
    assert!(
        losses["failed_reads"].as_u64().unwrap() >= 1,
        "the map was deleted while the program was still running: {losses}"
    );

    // Each counter comes with a reason that names what happened, because a
    // number on its own sends a reader looking in the wrong place.
    let reasons: Vec<String> = losses["reasons"]
        .as_object()
        .expect("the reasons behind the counts")
        .keys()
        .cloned()
        .collect();
    for expected in [
        "is not `<hex address> <hex size> <symbol>`",
        "it was replaced",
        "disappeared while the program was still running",
    ] {
        assert!(
            reasons.iter().any(|reason| reason.contains(expected)),
            "no reason says `{expected}`: {reasons:?}"
        );
    }
}

/// The absence that is not a loss, and the absence that is.
///
/// A map that has never been read yet is not a gap: a runtime creates its map
/// as it starts, and looking a few milliseconds early costs nothing. A map
/// that is *never* read at all is something else entirely -- this run watched
/// a program and saw nothing, which is not the same as a program that compiled
/// nothing, and it is the one statement this source exists to avoid making.
#[test]
fn a_program_that_publishes_no_map_says_it_read_none_rather_than_reporting_none() {
    let output = omn(&[
        "runtime.jit",
        "--mode",
        "trace",
        "--emit",
        "summary",
        "--format",
        "json",
        "--",
        "sh",
        "-c",
        "sleep 0.2",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    let result = &document(&output)["results"][0];
    assert_eq!(result["events_seen"], 0);
    assert!(
        result["losses"]["failed_reads"].as_u64().unwrap() > 0,
        "every read that never found a map is a read that failed: {result}"
    );
    let reasons = result["losses"]["reasons"]
        .as_object()
        .expect("the reasons behind the counts");
    assert!(
        reasons
            .keys()
            .any(|reason| reason.contains("does not exist")),
        "the reason must name the absent map: {reasons:?}"
    );
    // And it must name the thing a reader can do about it, which is start the
    // runtime differently rather than run this tool differently.
    assert!(
        reasons
            .keys()
            .any(|reason| reason.contains("perf-basic-prof")),
        "the reason has to say how a map gets written: {reasons:?}"
    );
}

/// The uncertainty this source cannot remove, stated rather than hidden.
///
/// The file records that code exists, never when it was created. A line is
/// known to have appeared between two reads and no more precisely, so it
/// carries that gap as `ts_err_ns`; a line already there at the first read is
/// older than the run by an amount nothing records, and carries the largest
/// error the event model can express. The workload writes three lines, waits,
/// and writes two more, so which line is in which class is chosen rather than
/// observed.
#[test]
fn an_entry_is_timestamped_no_more_precisely_than_the_reads_allow() {
    const FIRST: usize = 3;
    const SECOND: usize = 2;

    let script = format!(
        "map=/tmp/perf-$$.map\n\
         for i in $(seq 1 {FIRST}); do echo \"1000$i 10 JS:~first$i a.js:$i:1\" >> $map; done\n\
         sleep 0.1\n\
         for i in $(seq 1 {SECOND}); do echo \"2000$i 20 JS:~second$i a.js:$i:1\" >> $map; done\n\
         sleep 0.1\n\
         rm -f $map\n"
    );
    let output = omn(&[
        "runtime.jit",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--format",
        "json",
        "--",
        "sh",
        "-c",
        &script,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    let doc = document(&output);
    let events = doc["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("raw events")
        .clone();
    assert_eq!(events.len(), FIRST + SECOND);

    let already_there: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| attr(event, "first_read") == 1)
        .collect();
    assert_eq!(already_there.len(), FIRST);
    assert_eq!(
        already_there[0]["ts_err_ns"],
        u32::MAX,
        "how long before the run these were compiled is not in the file"
    );
    assert_eq!(attr(already_there[0], "bytes"), 0x10);

    let arrived: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| attr(event, "first_read") == 0)
        .collect();
    assert_eq!(arrived.len(), SECOND);
    let error_ns = arrived[0]["ts_err_ns"].as_u64().expect("an uncertainty");
    assert!(
        (1..300_000_000).contains(&error_ns),
        "a line seen between two reads is bounded by the gap between them, got {error_ns}ns"
    );
    assert_eq!(attr(arrived[0], "bytes"), 0x20);
}

// ---------------------------------------------------------------------------
// Class D -- the refusal
// ---------------------------------------------------------------------------

/// `examples/runtime-not-built.yml`: three sources and one backend this build
/// does not implement, all valid and all exiting 3.
///
/// The session validates cleanly and resolves onto a backend before it stops,
/// which is what makes the answer "not written yet" rather than "you asked for
/// something impossible".
#[test]
fn the_sources_this_build_does_not_implement_exit_three_and_name_the_backend() {
    let validated = omn(&["validate", "-f", &example("runtime-not-built.yml")]);
    assert!(
        validated.status.success(),
        "the session is valid; it is the build that is missing: {}",
        stderr(&validated)
    );

    let output = omn(&["-f", &example("runtime-not-built.yml")]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "not implemented is a different answer from invalid: {}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("bpf:usdt"),
        "the refusal must name the backend that is missing: {}",
        stderr(&output)
    );

    // And each of the three unimplemented sources, asked for on its own, gives
    // the same answer naming itself.
    for source in ["runtime.gc", "runtime.alloc", "runtime.custom"] {
        let output = omn(&[
            source, "--mode", "trace", "--emit", "summary", "--", "sh", "-c", "true",
        ]);
        assert_eq!(
            output.status.code(),
            Some(3),
            "`{source}`: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(source) && stderr(&output).contains("bpf:usdt"),
            "`{source}`: the refusal must name the source and the backend: {}",
            stderr(&output)
        );
    }
}

/// A request that cannot mean anything, in this family, is refused for the
/// reason it cannot -- and with exit 2, not the 3 that means "not written".
///
/// `runtime.alloc` does not declare `raw`: it is an allocation profile, and
/// its emits are the ones that aggregate. Asking it for `raw` is not a build
/// that is missing something, and the two must not be confused.
#[test]
fn an_emit_the_source_does_not_declare_is_rejected_rather_than_reported_unwritten() {
    let output = omn(&[
        "runtime.alloc",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--",
        "sh",
        "-c",
        "true",
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "this request cannot mean anything, which is not the same as unwritten: {}",
        stderr(&output)
    );
    let message = stderr(&output);
    assert!(
        message.contains("cannot emit `raw`") && message.contains("hist, topk, stack"),
        "the refusal must name the emits this source declares: {message}"
    );

    // A backend the chain does not have is also a rejection, and prints the
    // chain so a reader can see what they could have pinned.
    let absent = omn(&[
        "runtime.jit",
        "--backend",
        "force:nonsense",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--",
        "sh",
        "-c",
        "true",
    ]);
    assert_eq!(absent.status.code(), Some(2));
    assert!(
        stderr(&absent).contains("bpf:usdt, fs:jit-map"),
        "the chain must be printed: {}",
        stderr(&absent)
    );
}

// ---------------------------------------------------------------------------
// The defects this family found
// ---------------------------------------------------------------------------

/// Defect 1: `topk` cannot rank what this source is about.
///
/// `topk` ranks *series*, and a series is a distinct set of keys. `runtime.jit`
/// carries one key, `task`, so a run of it answers `topk:10` with exactly one
/// row per process however many methods were compiled -- and the row's total
/// is just the event count, which `summary` already gives.
///
/// The question a JIT map exists to answer is which methods dominated, and
/// `method` is an attribute rather than a key, so no emit can group on it.
/// `examples/runtime-jit.yml` promised in an earlier wave that `topk` would
/// answer it "when the shared aggregation work lands". The aggregation landed
/// and the question is still unanswerable.
#[test]
fn defect_1_topk_ranks_processes_rather_than_the_methods_that_were_compiled() {
    const WRITTEN: usize = 8;

    let output = session("runtime-jit-sweep.yml");
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = document(&output);
    let result = result(&doc, "everything");

    assert_eq!(result["events_seen"].as_u64().unwrap() as usize, WRITTEN);
    let ranked = emitted(result, "topk")["series"]
        .as_array()
        .expect("a ranking")
        .clone();

    assert_eq!(
        ranked.len(),
        1,
        "DEFECT 1 IS FIXED: `topk` now ranks more than one series over a single \
         process, so it may be able to rank methods. Delete this test and defect 1 of \
         docs/scenarios/runtime.md."
    );
    assert!(
        ranked[0]["key"].as_str().unwrap().starts_with("task:"),
        "the only thing it can rank by is the task: {ranked:?}"
    );
    // Eight distinct methods were compiled and the ranking names none of them.
    let names: Vec<String> = methods(result);
    assert_eq!(names.len(), WRITTEN);
    let ranking = format!("{ranked:?}");
    assert!(
        names.iter().all(|name| !ranking.contains(name)),
        "no method name reaches the ranking: {ranking}"
    );
}

/// Defect 2: `=~` is a substring test, and nothing says so.
///
/// The manual lists `=~` among the operators and leaves the reader to assume
/// the meaning it has in every other tool that spells it this way. It is
/// `str::contains`. A predicate written as a regular expression is parsed,
/// validated, reported as applied, and keeps nothing -- the same silent-empty
/// failure this source already had once, when its `code_kind` attribute was
/// called `kind`.
#[test]
fn defect_2_the_match_operator_is_a_substring_test_and_says_nothing_about_it() {
    let workload = "map=/tmp/perf-$$.map; \
                    for i in 1 2 3 4; do \
                    printf '4000%s 10 JS:~omniscientSwept%s a.js:%s:1\\n' $i $i $i >> $map; \
                    done; sleep 0.05; rm -f $map";
    let with = |filter: &str| {
        let output = omn(&[
            "runtime.jit",
            "--mode",
            "trace",
            "--emit",
            "raw",
            "--filter",
            filter,
            "--format",
            "json",
            "--",
            "sh",
            "-c",
            workload,
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        document(&output)["results"][0]["events_seen"]
            .as_u64()
            .unwrap()
    };

    assert_eq!(with("bytes >= 0"), 4, "four entries are written");
    assert_eq!(
        with("method =~ 'omniscientSwept'"),
        4,
        "a plain substring matches, which is all this operator does"
    );
    assert_eq!(
        with("method =~ 'Swept[1-4]'"),
        0,
        "DEFECT 2 IS FIXED: `=~` now evaluates a regular expression. Delete this test \
         and defect 2 of docs/scenarios/runtime.md."
    );

    // And nothing in the tool's own help distinguishes it from a regex.
    let manual = std::fs::read_to_string(root().join("man/omn.1")).expect("the manual page");
    let described = manual.lines().any(|line| {
        line.contains("=~") && (line.contains("substring") || line.contains("contains"))
    });
    assert!(
        !described,
        "DEFECT 2 IS PARTLY FIXED: the manual now says what `=~` does. Update defect 2 \
         of docs/scenarios/runtime.md."
    );
}
