//! Scenarios for the `runtime` family.
//!
//! Every test here runs a session that is also a file in `examples/`, so a
//! reader can reproduce any of them by hand with `omn -f examples/<name>.yml`
//! -- except the two that cannot be a file: a `--pid` target, whose pid exists
//! only once the test has started it, and a binary without its capabilities,
//! which is a copy the test makes.
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
        "a reader has to be able to tell a filter that removed seven from a workload \
         that produced one: {filtered}"
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

/// `examples/runtime-not-built.yml`: the two backends of this family this
/// build does not implement, both valid and both exiting 3.
///
/// The session validates cleanly and resolves onto a backend before it stops,
/// which is what makes the answer "not written yet" rather than "you asked for
/// something impossible". USDT landed in the eighth wave, so the three sources
/// that used to be here collect now and have scenarios of their own below.
#[test]
fn the_backends_this_build_does_not_implement_exit_three_and_name_themselves() {
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

    // Each pinned road, asked for on its own, gives the same answer naming
    // itself.
    for (source, backend) in [
        ("runtime.jit", "bpf:usdt"),
        ("runtime.custom", "bpf:uprobe"),
    ] {
        let output = omn(&[
            source,
            "--backend",
            &format!("force:{backend}"),
            "--mode",
            "trace",
            "--emit",
            "summary",
            "--",
            "sh",
            "-c",
            "true",
        ]);
        assert_eq!(
            output.status.code(),
            Some(3),
            "`{source}` over `{backend}`: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(source) && stderr(&output).contains(backend),
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

/// Defect 2, fixed: `=~` is a substring test, and now it says so.
///
/// It is `str::contains`, and every other tool that spells a match operator
/// this way -- Perl, Ruby, awk, bash's `[[ =~ ]]` -- means a regular
/// expression. A regex written here used to be parsed, validated, reported as
/// applied, and keep nothing: the same silent-empty failure this source already
/// had once, when its `code_kind` attribute was called `kind`, and one an empty
/// series over a quiet workload is indistinguishable from.
///
/// `filter::literal_only` now refuses the patterns that can only have been
/// meant as a regex -- a character class, a group, an escape, and the two
/// anchors where they anchor -- and the refusal is the one place in a run that
/// has any reason to say what the operator does. The two halves asserted below
/// are that a literal substring still matches and that a regex no longer runs
/// silently. What is deliberately *not* widened is `.`, `+`, `*` and `?`:
/// `a.js:3:1` and `libc.so.6+0x27781` are ordinary method and frame text.
#[test]
fn defect_2_the_match_operator_is_a_substring_test_and_says_so() {
    let workload = "map=/tmp/perf-$$.map; \
                    for i in 1 2 3 4; do \
                    printf '4000%s 10 JS:~omniscientSwept%s a.js:%s:1\\n' $i $i $i >> $map; \
                    done; sleep 0.05; rm -f $map";
    let run = |filter: &str| {
        omn(&[
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
        ])
    };
    let with = |filter: &str| {
        let output = run(filter);
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
        with("method =~ 'omniscientSwept1'"),
        1,
        "and a longer substring narrows it, which is the useful half of the operator"
    );

    // A pattern that can only have been meant as a regular expression. It used
    // to keep zero of four and report that as an ordinary empty result; it is
    // now refused before anything is attached, and the refusal is what says
    // that `=~` is a substring test.
    for (pattern, meant) in [
        ("Swept[1-4]", "a character class"),
        ("(Swept1|Swept2)", "a group"),
        ("Swept\\d", "an escape"),
        ("^omniscientSwept1", "a leading anchor"),
        ("omniscientSwept1$", "a trailing anchor"),
    ] {
        let output = run(&format!("method =~ '{pattern}'"));
        assert!(
            !output.status.success(),
            "{meant} in `{pattern}` cannot match literally and must not run silently:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let said = stderr(&output);
        assert!(
            said.contains("substring test, not a regular expression"),
            "the refusal is the only place that says what `=~` does:\n{said}"
        );
    }

    // The anchored form names the exact predicate that was meant, since an
    // anchor is a reader asking for equality in the only spelling they knew.
    let anchored = stderr(&run("comm =~ '^sh$'"));
    assert!(
        anchored.contains("comm == 'sh'"),
        "an anchored pattern is an exact match written in regex, and the refusal has to \
         name the operator that is one:\n{anchored}"
    );

    // And the manual says it, which closed the rest of this defect in the
    // tenth wave: the operator's line is followed by what it does.
    let manual = std::fs::read_to_string(root().join("man/omn.1")).expect("the manual page");
    let lines: Vec<&str> = manual.lines().collect();
    let described = lines
        .windows(2)
        .any(|pair| pair[0].trim() == ".BR =~" && pair[1].contains("substring"));
    assert!(
        described,
        "the manual must say that `=~` is a substring test"
    );
}

// ---------------------------------------------------------------------------
// USDT -- `runtime.custom`, `runtime.gc`, `runtime.alloc`
// ---------------------------------------------------------------------------
//
// Ground truth for a probe is a program that fires it a number of times it
// chose, with arguments it chose, and says so. No runtime on the development
// machine is built with its probes, so the program is this fixture, built at
// test time against systemtap's own <sys/sdt.h> -- the notes are written by the
// header every USDT-carrying program uses, not by anything in this project.
// Where the fixture stands in for a runtime (CPython's `gc__start`, HotSpot's
// `object__alloc`) it publishes that runtime's exact probe shape, and the
// scenario says it is not that runtime.
//
// libc is the other target whose notes a machine can be relied on to have:
// glibc's own build put about eighty probes in it.

/// The fixture's C source. Between the two marker lines so that
/// `examples/runtime-usdt-*.yml` can say how to build it by hand with `sed`.
const FIXTURE_SOURCE: &str = r#"
/* BEGIN FIXTURE -- omn's USDT fixture. Every probe it fires, it also counts and
 * reports on stdout, which is what the scenarios check the tool against. */
#define _SDT_HAS_SEMAPHORES 1
#include <sys/sdt.h>
#include <pthread.h>
#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

unsigned short omn_fixture_tick_semaphore __attribute__((section(".probes")));
unsigned short omn_fixture_guarded_semaphore __attribute__((section(".probes")));
unsigned short python_gc__start_semaphore __attribute__((section(".probes")));
unsigned short python_gc__done_semaphore __attribute__((section(".probes")));
unsigned short hotspot_object__alloc_semaphore __attribute__((section(".probes")));

struct record { long pad; long value; };

static long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000000L + ts.tv_nsec;
}

static void __attribute__((noinline)) ticks(long n) {
    struct record *r = malloc(sizeof *r);
    long guarded = 0;
    for (long i = 0; i < n; i++) {
        r->value = 1000 + i;
        /* a register, a signed short, a constant, and a value behind a pointer */
        DTRACE_PROBE4(omn_fixture, tick, i, (short)-i, 7, r->value);
        if (omn_fixture_guarded_semaphore) {
            DTRACE_PROBE1(omn_fixture, guarded, i);
            guarded++;
        }
        __asm__ volatile("" ::: "memory");
    }
    printf("fixture: ticks=%ld guarded=%ld\n", n, guarded);
    free(r);
}

struct job { long n; };
static void *thread_ticks(void *arg) {
    long n = ((struct job *)arg)->n;
    for (long i = 0; i < n; i++) DTRACE_PROBE4(omn_fixture, tick, i, (short)-i, 7, i);
    return NULL;
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "ticks";
    long n = argc > 2 ? atol(argv[2]) : 10;
    long m = argc > 3 ? atol(argv[3]) : 0;
    printf("fixture: pid=%d semaphores tick=%u guarded=%u gc=%u\n", getpid(),
           omn_fixture_tick_semaphore, omn_fixture_guarded_semaphore, python_gc__start_semaphore);
    fflush(stdout);
    if (!strcmp(mode, "wait")) {
        /* For a --pid target: running before anything attaches, it waits for
         * its own semaphore to be raised -- which is the attach -- then ticks. */
        long waited = 0;
        while (!omn_fixture_tick_semaphore && waited < 10000) {
            struct timespec d = { 0, 1000000L };
            nanosleep(&d, NULL);
            waited++;
        }
        printf("fixture: waited_ms=%ld semaphores tick=%u guarded=%u\n", waited,
               omn_fixture_tick_semaphore, omn_fixture_guarded_semaphore);
        fflush(stdout);
        ticks(n);
    } else if (!strcmp(mode, "ticks")) {
        ticks(n);
    } else if (!strcmp(mode, "threads")) {  /* n threads, m ticks each */
        pthread_t t[64];
        struct job job = { m };
        for (long i = 0; i < n && i < 64; i++) pthread_create(&t[i], NULL, thread_ticks, &job);
        for (long i = 0; i < n && i < 64; i++) pthread_join(t[i], NULL);
        printf("fixture: threads=%ld each=%ld\n", n, m);
    } else if (!strcmp(mode, "gc")) {       /* n pauses of m ms, CPython's probe shape */
        for (long i = 0; i < n; i++) {
            int generation = (int)(i % 3);
            DTRACE_PROBE1(python, gc__start, generation);
            long t0 = now_ns();
            struct timespec d = { m / 1000, (m % 1000) * 1000000L };
            nanosleep(&d, NULL);
            long t1 = now_ns();
            long collected = 100 + i;
            DTRACE_PROBE1(python, gc__done, collected);
            printf("fixture: pause=%ld generation=%d collected=%ld own_ns=%ld\n", i, generation, collected, t1 - t0);
        }
    } else if (!strcmp(mode, "alloc")) {    /* n objects, sizes 16, 32, ...; HotSpot's probe shape */
        long total = 0;
        for (long i = 0; i < n; i++) {
            long size = 16 * (i + 1);
            DTRACE_PROBE4(hotspot, object__alloc, 1, "Fixture", 7, size);
            total += size;
        }
        printf("fixture: objects=%ld bytes=%ld\n", n, total);
    } else if (!strcmp(mode, "jumps")) {    /* n setjmp/longjmp pairs through libc's own probes */
        static jmp_buf buf;
        volatile long done = 0;
        for (long i = 0; i < n; i++) if (setjmp(buf) == 0) { done++; longjmp(buf, 3); }
        printf("fixture: jumps=%ld buf=%lu\n", (long)done, (unsigned long)&buf);
    }
    return 0;
}
/* END FIXTURE */
"#;

/// Where the fixture is built. The examples launch it by this path, relative to
/// the repository root, so it is not `CARGO_TARGET_DIR`.
fn fixture_path() -> PathBuf {
    root().join("target/omn-usdt-fixture/fixture")
}

/// Builds the fixture once per test process, or says why it cannot.
fn fixture() -> Result<PathBuf, String> {
    static BUILT: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let path = fixture_path();
            let dir = path.parent().expect("a directory");
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
            let source = dir.join("fixture.c");
            std::fs::write(&source, FIXTURE_SOURCE).map_err(|error| error.to_string())?;
            // Built beside and renamed, so a half-written binary is never at the
            // path the examples launch.
            let partial = dir.join(format!("fixture.{}", std::process::id()));
            let built = Command::new("cc")
                .args(["-O0", "-pthread", "-o"])
                .arg(&partial)
                .arg(&source)
                .output()
                .map_err(|error| {
                    format!("no C compiler (`cc`) to build the fixture with: {error}")
                })?;
            if !built.status.success() {
                return Err(format!(
                    "the fixture did not build -- most likely <sys/sdt.h> is missing, which \
                     systemtap's development headers provide:\n{}",
                    String::from_utf8_lossy(&built.stderr)
                ));
            }
            std::fs::rename(&partial, &path).map_err(|error| error.to_string())?;
            Ok(path)
        })
        .clone()
}

/// Whether this binary can attach a USDT program at all, answered by trying,
/// with the reason when it cannot. A binary rebuilt with `cargo build` rather
/// than `make` has lost its capabilities, and every scenario below would fail
/// for that one reason; they skip instead, and say so.
fn usdt_ready() -> Result<PathBuf, String> {
    static READY: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    READY
        .get_or_init(|| {
            let fixture = fixture()?;
            let probe = omn(&[
                "runtime.custom",
                "--mode",
                "count",
                "--emit",
                "summary",
                "--filter",
                "provider == 'omn_fixture' && probe == 'tick'",
                "--",
                fixture.to_str().expect("a UTF-8 path"),
                "ticks",
                "1",
            ]);
            if probe.status.success() {
                return Ok(fixture);
            }
            let message = stderr(&probe);
            if message.contains("CAP_BPF") {
                return Err(format!(
                    "this binary cannot load a USDT program; build with `make`, which grants \
                     CAP_BPF and CAP_PERFMON, rather than `cargo build`, which drops them:\n{message}"
                ));
            }
            panic!("a one-tick USDT run failed for a reason that is not privileges:\n{message}");
        })
        .clone()
}

macro_rules! needs_usdt {
    () => {
        match usdt_ready() {
            Ok(path) => path,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                return;
            }
        }
    };
}

/// The `fixture: key=value ...` lines the fixture printed, in order.
fn fixture_says(output: &Output) -> Vec<std::collections::BTreeMap<String, String>> {
    stdout(output)
        .lines()
        .filter_map(|line| line.strip_prefix("fixture: "))
        .map(|line| {
            line.split_whitespace()
                .filter_map(|pair| pair.split_once('='))
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect()
        })
        .collect()
}

fn said(lines: &[std::collections::BTreeMap<String, String>], key: &str) -> i64 {
    lines
        .iter()
        .rev()
        .find_map(|line| line.get(key))
        .unwrap_or_else(|| panic!("the fixture never said `{key}`: {lines:?}"))
        .parse()
        .expect("a number")
}

fn int_attr(event: &serde_json::Value, name: &str) -> i64 {
    attr(event, name)
        .as_i64()
        .unwrap_or_else(|| panic!("`{name}` is not an integer on {event}"))
}

fn successful_session(name: &str) -> (Output, serde_json::Value) {
    let output = session(name);
    assert!(
        output.status.success(),
        "`{name}` failed:\n{}",
        stderr(&output)
    );
    let doc = document(&output);
    (output, doc)
}

/// `examples/runtime-usdt-probes.yml`: fifty hits of a probe the program fired
/// fifty times, each carrying the four arguments the program passed -- one in
/// a register, one a signed short, one a constant, one behind a pointer.
///
/// And the guarded probe: it only fires while its semaphore is raised, and the
/// fixture counts how often it saw it raised. The kernel maintains that
/// counter through the link, so for a launched program it is raised before
/// `main`, which the fixture reports from its first line.
#[test]
fn a_named_probe_arrives_once_per_hit_carrying_the_arguments_the_program_passed() {
    needs_usdt!();
    let (output, doc) = successful_session("runtime-usdt-probes.yml");
    let says = fixture_says(&output);
    assert_eq!(said(&says, "ticks"), 50);

    let ticks = events(result(&doc, "ticks"));
    assert_eq!(ticks.len(), 50, "one event per hit, no more and no fewer");
    let mut indices: Vec<i64> = ticks.iter().map(|event| int_attr(event, "arg0")).collect();
    indices.sort_unstable();
    assert_eq!(indices, (0..50).collect::<Vec<_>>());
    for event in &ticks {
        let index = int_attr(event, "arg0");
        assert_eq!(attr(event, "provider"), "omn_fixture");
        assert_eq!(attr(event, "probe"), "tick");
        assert_eq!(
            int_attr(event, "arg1"),
            -index,
            "a signed short, sign-extended: {event}"
        );
        assert_eq!(
            int_attr(event, "arg2"),
            7,
            "a constant folded into the descriptor: {event}"
        );
        assert_eq!(
            int_attr(event, "arg3"),
            1000 + index,
            "a value read from user memory: {event}"
        );
    }

    // The guarded probe fired exactly as often as the fixture saw its
    // semaphore raised, and it was raised from the program's first line.
    let guarded = said(&says, "guarded");
    assert_eq!(
        guarded, 50,
        "the kernel kept the semaphore raised for the whole run: {says:?}"
    );
    assert!(
        says[0].get("guarded").is_some_and(|value| value != "0"),
        "the semaphore must be raised before main runs: {says:?}"
    );
    let summary = emitted(result(&doc, "guarded"), "summary");
    let counted: f64 = summary["series"]
        .as_array()
        .expect("series")
        .iter()
        .map(|series| series["last"].as_f64().expect("a count"))
        .sum();
    assert_eq!(counted as i64, guarded, "{summary}");
}

/// `examples/runtime-usdt-libc.yml`: libc's own probes, in a program that
/// asked for none of them.
///
/// Twenty-five round trips on one `jmp_buf`: exactly twenty-five `setjmp` and
/// twenty-five `longjmp` hits name that buffer, and every such `longjmp` passes
/// the value 3. The run also sees glibc's own `setjmp` from before `main`, on
/// a different buffer -- which is why the check keys on the address.
#[test]
fn libcs_own_probes_count_the_round_trips_on_the_programs_own_buffer() {
    needs_usdt!();
    let (output, doc) = successful_session("runtime-usdt-libc.yml");
    let says = fixture_says(&output);
    assert_eq!(said(&says, "jumps"), 25);
    let buffer = said(&says, "buf");

    let all = events(result(&doc, "jumps"));
    let on_buffer = |probe: &str| -> Vec<serde_json::Value> {
        all.iter()
            .filter(|event| attr(event, "probe") == probe && int_attr(event, "arg0") == buffer)
            .cloned()
            .collect()
    };
    assert_eq!(on_buffer("setjmp").len(), 25, "{all:?}");
    let longjmps = on_buffer("longjmp");
    assert_eq!(longjmps.len(), 25);
    assert!(
        longjmps.iter().all(|event| int_attr(event, "arg1") == 3),
        "{longjmps:?}"
    );
    assert!(
        all.iter()
            .any(|event| attr(event, "probe") == "setjmp" && int_attr(event, "arg0") != buffer),
        "glibc sets up a jmp_buf of its own before main; a run that does not see it has missed \
         the start of the program"
    );
}

/// `examples/runtime-usdt-gc.yml`: five injected pauses in CPython's probe
/// shape.
///
/// Each reported pause must contain the fixture's own measurement of its
/// sleep, and exceed it by less than half of it -- the separation between
/// "this pause" and "not this pause", not a calibrated threshold. The
/// generation and the collected count are the program's own arguments and
/// must match exactly.
#[test]
fn a_pause_is_the_time_between_the_runtimes_own_two_probes() {
    needs_usdt!();
    let (output, doc) = successful_session("runtime-usdt-gc.yml");
    let says = fixture_says(&output);
    let own: Vec<&std::collections::BTreeMap<String, String>> = says
        .iter()
        .filter(|line| line.contains_key("pause"))
        .collect();
    assert_eq!(own.len(), 5);

    let result = result(&doc, "pauses");
    let mut pauses = events(result);
    assert_eq!(pauses.len(), 5, "one event per pause, not per probe");
    pauses.sort_by_key(|event| event["ts_ns"].as_u64());
    for (event, line) in pauses.iter().zip(own) {
        let own_ns: i64 = line["own_ns"].parse().unwrap();
        let pause_ns = int_attr(event, "pause_ns");
        assert!(
            pause_ns >= own_ns && pause_ns - own_ns < own_ns / 2,
            "the pause is {pause_ns} ns and the fixture timed its own sleep at {own_ns} ns: {event}"
        );
        assert_eq!(
            int_attr(event, "generation"),
            line["generation"].parse::<i64>().unwrap()
        );
        assert_eq!(
            int_attr(event, "collected"),
            line["collected"].parse::<i64>().unwrap()
        );
        assert_eq!(attr(event, "provider"), "python");
        assert_eq!(attr(event, "phase"), "collect");
    }
    // The emits that make a pause readable are there and agree on the count.
    assert_eq!(emitted(result, "hist")["series"][0]["count"], 5);
    assert!(emitted(result, "timeline").is_object());
}

/// `examples/runtime-usdt-alloc.yml`: a hundred objects of sizes the fixture
/// chose, in HotSpot's probe shape; their count and their bytes exactly.
#[test]
fn every_allocation_the_runtime_reported_is_counted_with_its_size() {
    needs_usdt!();
    let (output, doc) = successful_session("runtime-usdt-alloc.yml");
    let says = fixture_says(&output);
    let hist = &emitted(result(&doc, "objects"), "hist")["series"][0];
    assert_eq!(hist["count"].as_i64(), Some(said(&says, "objects")));
    assert_eq!(
        hist["sum"].as_f64().map(|sum| sum as i64),
        Some(said(&says, "bytes"))
    );
    assert_eq!(hist["min"].as_f64(), Some(16.0));
    assert_eq!(hist["max"].as_f64(), Some(1600.0));
}

/// `examples/runtime-usdt-threads.yml`: eight threads created after the run
/// started, five hundred hits each, all of them this run's.
#[test]
fn a_probe_follows_every_thread_of_the_process() {
    needs_usdt!();
    let (output, doc) = successful_session("runtime-usdt-threads.yml");
    let says = fixture_says(&output);
    let (threads, each) = (said(&says, "threads"), said(&says, "each"));
    let pid = said(&says, "pid");

    let all = events(result(&doc, "threads"));
    assert_eq!(all.len() as i64, threads * each);
    let mut per_thread = std::collections::BTreeMap::new();
    for event in &all {
        *per_thread.entry(int_attr(event, "tid")).or_insert(0i64) += 1;
        assert_eq!(int_attr(event, "pid"), pid);
    }
    assert_eq!(per_thread.len() as i64, threads, "{per_thread:?}");
    assert!(
        per_thread.values().all(|count| *count == each),
        "{per_thread:?}"
    );
    assert!(
        !per_thread.contains_key(&pid),
        "the main thread fired none of them"
    );
}

/// `examples/runtime-usdt-flood.yml`: two million hits through both shapes.
///
/// Streamed, the hits delivered plus the hits the program counted as dropped
/// are exactly two million; whether this run overflowed is a fact about the
/// machine at that moment, so it is printed, not asserted. Counted in the
/// kernel, it is exactly two million with nothing lost.
#[test]
fn hits_taken_plus_hits_lost_is_hits_fired() {
    needs_usdt!();
    let (output, doc) = successful_session("runtime-usdt-flood.yml");
    let fired = said(&fixture_says(&output), "ticks");

    let streamed = result(&doc, "streamed");
    let taken = emitted(streamed, "summary")["series"][0]["count"]
        .as_i64()
        .unwrap_or(0);
    let lost = streamed["losses"]["dropped"]
        .as_i64()
        .expect("a drop count");
    eprintln!("flood: {fired} fired, {taken} streamed, {lost} dropped by the ring");
    assert_eq!(taken + lost, fired, "{streamed}");

    let counted = result(&doc, "counted");
    let total = emitted(counted, "summary")["series"][0]["last"]
        .as_f64()
        .expect("a count") as i64;
    assert_eq!(total, fired, "{counted}");
    assert_eq!(counted["losses"]["dropped"], 0, "{counted}");
}

/// A process that was running before anything attached.
///
/// The fixture waits for its own semaphore to be raised -- which is the
/// attach -- and only then fires. So it reports, from inside, that the kernel
/// raised the semaphore of a running process, and how long it waited; and
/// every tick after that is this run's.
#[test]
fn a_running_process_is_reached_and_its_semaphore_raised_from_outside() {
    let fixture = needs_usdt!();
    let child = Command::new(&fixture)
        .args(["wait", "20"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the fixture starts");
    let pid = child.id().to_string();
    let output = omn(&[
        "runtime.custom",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--format",
        "json",
        "--duration",
        "3s",
        "--filter",
        "provider == 'omn_fixture' && probe == 'tick'",
        "--pid",
        &pid,
    ]);
    let fixture_output = child.wait_with_output().expect("the fixture finishes");
    assert!(output.status.success(), "{}", stderr(&output));
    let says = fixture_says(&fixture_output);
    assert!(
        said(&says, "waited_ms") < 10_000,
        "the fixture gave up waiting: nothing raised its semaphore: {says:?}"
    );
    let doc = document(&output);
    let ticks = &doc["results"][0]["outputs"][0]["events"];
    assert_eq!(ticks.as_array().map(Vec::len), Some(20), "{doc}");
}

/// The extension point without a probe named is a request that cannot mean
/// anything, and the refusal lists what the target carries -- which is what
/// the reader needs to write the filter.
#[test]
fn a_custom_request_that_names_no_probe_is_refused_with_the_inventory() {
    let fixture = needs_usdt!();
    let output = omn(&[
        "runtime.custom",
        "--mode",
        "trace",
        "--emit",
        "summary",
        "--",
        fixture.to_str().unwrap(),
        "ticks",
        "1",
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let message = stderr(&output);
    for expected in ["names none", "omn_fixture:tick", "libc.so.6", "libc:setjmp"] {
        assert!(
            message.contains(expected),
            "missing `{expected}`: {message}"
        );
    }
}

/// `examples/runtime-usdt-absent.yml`: a real runtime.
///
/// On a machine whose python3 is built without --with-dtrace -- most of them,
/// and the development machine -- the request is refused, and the refusal
/// says what each object's ELF actually contained, python3's own included.
/// Where python3 has its probes, five `gc.collect()` calls must come back as
/// five full collections. Either way this asserts something; neither branch
/// is a skip.
#[test]
fn a_runtime_without_its_probes_is_refused_naming_what_its_elf_contained() {
    needs_usdt!();
    let Ok(python) = Command::new("sh")
        .args(["-c", "command -v python3"])
        .output()
    else {
        eprintln!("skipped: no shell to look for python3 with");
        return;
    };
    let python = String::from_utf8_lossy(&python.stdout).trim().to_owned();
    if python.is_empty() {
        eprintln!(
            "skipped: no python3 on PATH, so there is no real runtime to check; the fixture \
             scenarios above cover the mechanism"
        );
        return;
    }
    let output = session("runtime-usdt-absent.yml");
    if output.status.success() {
        let doc = document(&output);
        let pauses = events(result(&doc, "python-pauses"));
        let full = pauses
            .iter()
            .filter(|event| int_attr(event, "generation") == 2)
            .count();
        assert!(
            full >= 5,
            "five gc.collect() calls are five full collections: {pauses:?}"
        );
        return;
    }
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let message = stderr(&output);
    assert!(
        message.contains("python:gc__start"),
        "the refusal names what it looked for: {message}"
    );
    assert!(
        message.contains("python3"),
        "the refusal names the runtime it read: {message}"
    );
    assert!(
        message.contains("no `.note.stapsdt`") || message.contains("probe site"),
        "the refusal says what the ELF contained: {message}"
    );
}

/// The same request from a copy of the binary, which carries no file
/// capabilities -- a copy never does. It must be refused naming the two
/// capabilities and how to grant them, never served by something lesser.
#[test]
fn without_its_capabilities_the_binary_refuses_and_names_them() {
    let fixture = needs_usdt!();
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: running as root, which has every capability a copy could lack");
        return;
    }
    let copy = std::env::temp_dir().join(format!("omn-uncapable-{}", std::process::id()));
    std::fs::copy(BINARY, &copy).expect("a copy of the binary");
    let output = Command::new(&copy)
        .args([
            "runtime.custom",
            "--mode",
            "trace",
            "--emit",
            "summary",
            "--filter",
            "provider == 'omn_fixture' && probe == 'tick'",
            "--",
            fixture.to_str().unwrap(),
            "ticks",
            "3",
        ])
        .output()
        .expect("the copy runs");
    let _ = std::fs::remove_file(&copy);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let message = stderr(&output);
    assert!(
        message.contains("CAP_BPF") && message.contains("setcap"),
        "{message}"
    );
}
