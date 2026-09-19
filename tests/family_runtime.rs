//! Ground-truth tests for the `runtime` family.
//!
//! Owned by that family; see docs/contributing.md. A test here checks a
//! measurement against something independently known -- a kernel counter, an
//! injected delay, a known size, a fixed iteration count. A test that checks
//! the code returns what the code computed proves nothing, which is the whole
//! difficulty of testing a tracer.

use std::process::{Command, Output, Stdio};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("omn should run")
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
        .unwrap_or_else(|| panic!("no json in `{text}`"));
    serde_json::from_str(&text[start..]).expect("one json document")
}

fn events(run: &serde_json::Value) -> Vec<serde_json::Value> {
    run["results"][0]["outputs"][0]["events"]
        .as_array()
        .unwrap_or_else(|| panic!("raw events: {run}"))
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

fn has_node() -> bool {
    Command::new("node").arg("--version").output().is_ok()
}

/// Removes what a launched runtime left behind, by the pid the events say it
/// had. A test that scatters files and leaves them there is a test that fails
/// differently the second time.
fn clean_up_after(run: &serde_json::Value) {
    let pid = run["results"][0]["outputs"][0]["events"][0]["keys"][0]["task"]["tid"]
        .as_i64()
        .expect("the task key carries the pid");
    let _ = std::fs::remove_file(format!("/tmp/perf-{pid}.map"));
    let _ = std::fs::remove_file(format!("/tmp/omn-v8-{pid}.log"));
}

/// Ground truth, against a name only this test could have produced.
///
/// The program compiles a function called `omniscientGroundTruth` and calls it
/// enough times for V8 to be sure of it. If the collector reports an event
/// naming that function, it read a real JIT's real map; no plausible constant
/// can produce that string.
#[test]
fn a_function_the_program_compiled_is_named_in_the_events() {
    if !has_node() {
        eprintln!("skipped: this machine has no node to launch");
        return;
    }

    let program = "function omniscientGroundTruth(n) { return n < 2 ? n : \
                   omniscientGroundTruth(n - 1) + omniscientGroundTruth(n - 2); }\n\
                   let total = 0;\n\
                   for (let i = 0; i < 25; i++) total += omniscientGroundTruth(20);\n\
                   if (total !== 4228125) throw new Error('unexpected ' + total);\n";
    let output = run(&[
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
        // V8's own log would otherwise be written into the working directory,
        // which for a test is the repository.
        "--logfile=/tmp/omn-v8-%p.log",
        "-e",
        program,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    let run = document(&output);
    clean_up_after(&run);
    let events = events(&run);

    let compiled: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| {
            attr(event, "method")
                .as_str()
                .is_some_and(|method| method.contains("omniscientGroundTruth"))
        })
        .collect();
    assert!(
        !compiled.is_empty(),
        "the function the program compiled is not in the {} events read",
        events.len()
    );

    // The runtime's own builtins arrive through the same file and are not
    // compilations this program caused. Both kinds must be present and
    // distinguishable, or the caveat that says to filter on them is useless.
    let kinds: Vec<String> = events
        .iter()
        .filter_map(|event| attr(event, "code_kind").as_str().map(str::to_owned))
        .collect();
    assert!(
        kinds.iter().any(|kind| kind == "js"),
        "no JS code was named"
    );
    assert!(
        kinds.iter().any(|kind| kind == "builtin"),
        "node publishes its builtins through the same map"
    );
}

/// A map written in two batches, with a known number of lines in each.
///
/// This is the timestamp claim, checked: the first batch was already there when
/// the collector first looked, so it carries the largest error the model can
/// express, and the second appeared between two reads and carries that gap.
#[test]
fn entries_are_timestamped_no_more_precisely_than_the_reads_allow() {
    const FIRST: usize = 3;
    const SECOND: usize = 2;

    let script = format!(
        "map=/tmp/perf-$$.map\n\
         for i in $(seq 1 {FIRST}); do echo \"1000$i 10 JS:~first$i script.js:$i:1\" >> $map; done\n\
         sleep 0.25\n\
         for i in $(seq 1 {SECOND}); do echo \"2000$i 20 JS:~second$i script.js:$i:1\" >> $map; done\n\
         sleep 0.25\n\
         rm -f $map\n"
    );
    let output = run(&[
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

    let run = document(&output);
    let events = events(&run);
    assert_eq!(
        events.len(),
        FIRST + SECOND,
        "{FIRST} lines then {SECOND} lines were written: {events:?}"
    );

    let first: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| attr(event, "first_read") == 1)
        .collect();
    assert_eq!(first.len(), FIRST, "the first read saw the first batch");
    assert_eq!(
        first[0]["ts_err_ns"],
        u32::MAX,
        "lines already in the map when it was first read are older than the run by an \
         amount nothing records"
    );

    let later: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| attr(event, "first_read") == 0)
        .collect();
    assert_eq!(
        later.len(),
        SECOND,
        "the second batch arrived while reading"
    );
    let error_ns = later[0]["ts_err_ns"].as_u64().expect("an uncertainty");
    assert!(
        (1..300_000_000).contains(&error_ns),
        "a line seen between two reads is bounded by the gap between them, got {error_ns}ns"
    );

    // The value of every event is one compilation, and the sizes are the ones
    // the map declared: 0x10 for the first batch, 0x20 for the second.
    assert_eq!(attr(first[0], "bytes"), 0x10);
    assert_eq!(attr(later[0], "bytes"), 0x20);
}

/// The trap this family exists to avoid.
///
/// A program with no JIT publishes no map, and the run has to say that it read
/// none. Reporting zero compilations with a clean bill of health would tell the
/// reader that nothing was compiled, which is a different statement and one
/// this run cannot make.
#[test]
fn a_program_that_publishes_no_map_says_so_rather_than_reporting_no_compilation() {
    let output = run(&[
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

    let run = document(&output);
    let result = &run["results"][0];
    assert_eq!(result["events_seen"], 0);
    assert!(
        result["losses"]["failed_reads"].as_u64().unwrap() > 0,
        "every read that found no map is a read that failed: {result}"
    );
    let reasons = result["losses"]["reasons"]
        .as_object()
        .expect("the reasons behind the counts");
    assert!(
        reasons
            .keys()
            .any(|reason| reason.contains("does not exist")),
        "the reasons must name the absent map: {reasons:?}"
    );
}

/// The other target this source accepts: a program that is already running.
///
/// A JIT's map outlives the poll that reads it, so attaching to a live process
/// must report what is already in the file — marked as what it is, code that
/// was compiled before anything was watching, with an uncertainty that says the
/// file does not record when.
#[test]
fn attaching_to_a_running_process_reports_the_map_it_already_published() {
    const ENTRIES: usize = 4;

    let mut child = Command::new("sh")
        .args(["-c", "sleep 5"])
        .spawn()
        .expect("sh should run");
    let pid = child.id();
    let map = format!("/tmp/perf-{pid}.map");

    let lines: String = (0..ENTRIES)
        .map(|index| {
            format!(
                "{:x} 40 JS:~already{index} before.js:{index}:1\n",
                0x40000 + index
            )
        })
        .collect();
    std::fs::write(&map, lines).expect("the test owns this file");

    let output = run(&[
        "runtime.jit",
        "--pid",
        &pid.to_string(),
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--duration",
        "200ms",
        "--format",
        "json",
    ]);

    let _ = std::fs::remove_file(&map);
    let _ = child.kill();
    let _ = child.wait();

    assert!(output.status.success(), "{}", stderr(&output));
    let run = document(&output);
    let events = events(&run);
    assert_eq!(
        events.len(),
        ENTRIES,
        "the map held {ENTRIES} entries: {events:?}"
    );
    for event in &events {
        assert_eq!(
            attr(event, "first_read"),
            1,
            "everything in the file predates this run"
        );
        assert_eq!(
            event["ts_err_ns"],
            u32::MAX,
            "how long before the run it was compiled is not in the file"
        );
    }

    // Nothing was missed: the map was there from the first poll to the last.
    assert_eq!(run["results"][0]["losses"]["failed_reads"], 0);
    assert_eq!(run["results"][0]["losses"]["dropped"], 0);
}

/// A filter on this source's own fields has to select, not just be accepted.
///
/// The map is mostly the runtime's builtins, and the question worth asking of
/// it is "what did *my* program compile". That is a filter, and the first
/// version of this source made it unanswerable: the attribute was called
/// `kind`, which the filter grammar reads as the event's source id, so
/// `kind == 'js'` was parsed, validated, reported as applied, and matched
/// nothing. A predicate that quietly keeps nothing is worse than one that is
/// refused.
#[test]
fn filtering_on_the_code_kind_keeps_that_kind_and_drops_the_rest() {
    if !has_node() {
        eprintln!("skipped: this machine has no node to launch");
        return;
    }

    let program = "function omniscientFiltered(n) { return n < 2 ? n : \
                   omniscientFiltered(n - 1) + omniscientFiltered(n - 2); }\n\
                   let total = 0;\n\
                   for (let i = 0; i < 20; i++) total += omniscientFiltered(18);\n";
    let node = |filter: &str| {
        run(&[
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
            "node",
            "--perf-basic-prof",
            "--logfile=/tmp/omn-v8-%p.log",
            "-e",
            program,
        ])
    };

    let all = node("bytes >= 0");
    assert!(all.status.success(), "{}", stderr(&all));
    let all = document(&all);
    let total = events(&all).len();
    clean_up_after(&all);
    assert!(total > 100, "a node run publishes its builtins and more");

    let compiled = node("code_kind == 'js'");
    assert!(compiled.status.success(), "{}", stderr(&compiled));
    let compiled = document(&compiled);
    let kept = events(&compiled);
    clean_up_after(&compiled);

    assert!(!kept.is_empty(), "the program compiled JavaScript");
    assert!(
        kept.len() < total,
        "the builtins are not JavaScript this program compiled: {} of {total} kept",
        kept.len()
    );
    for event in &kept {
        assert_eq!(attr(event, "code_kind"), "js");
    }
}
