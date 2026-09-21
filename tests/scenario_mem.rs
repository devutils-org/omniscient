//! Scenarios for the `mem` family.
//!
//! A scenario is a workload with a property this file chose, a session in
//! `examples/mem-*.yml` that measures it, and a check of the answer against the
//! property. Forty megabytes made resident, twenty thousand allocations of four
//! thousand and one bytes, a gibibyte of reads: the number is in the workload,
//! which is what makes it ground truth. A test that checks the tool returned
//! what the tool computed proves nothing.
//!
//! Four classes, each catching a different kind of lie: the capability sweep
//! (does every axis the catalog declares work?), the backend comparison (do two
//! roads to the same number agree?), the loss path (does the tool say what it
//! did not see?) and the refusal (is the reason the right reason?).
//!
//! `docs/scenarios/mem.md` is the prose half of this file. Where a scenario
//! found a defect it is recorded there under "Defects found", with the session
//! and the output, and the test below pins the wrong behaviour rather than
//! working around it: a suite that quietly routed around three bugs has
//! measured nothing. Each such test says in its failure message what to do when
//! the defect is fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// This machine cannot serve the request.
const EXIT_NO_VIABLE_BACKEND: i32 = 1;
/// The request cannot mean anything.
const EXIT_REJECTED: i32 = 2;
/// This build has not implemented it.
const EXIT_NO_COLLECTOR: i32 = 3;

// ---------------------------------------------------------------------------
// Running the tool
// ---------------------------------------------------------------------------

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn example(name: &str) -> String {
    format!("{}/examples/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Runs one of the sessions this stream ships, by name.
///
/// The sessions are the deliverable as much as the assertions are: running the
/// file rather than a string built here is what keeps the two from drifting,
/// and what makes a reader able to reproduce a failure by hand.
fn run_example(name: &str) -> Output {
    run(&["-f", &example(name)])
}

/// Runs a session that is not shipped as a file, from standard input.
///
/// One scenario needs this. `tests/cli.rs::the_shipped_examples_validate` walks
/// `examples/` and requires every session in it to validate, which is right and
/// is also why the exit-2 scenario cannot live there: a request that cannot
/// mean anything is exactly a session that does not validate. The session is
/// written out in docs/scenarios/mem.md instead, and fed in here.
fn run_written_session(session: &str) -> Output {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(BINARY)
        .args(["-f", "-", "--input-format", "yaml"])
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

/// The exit-2 scenario, kept here rather than in `examples/`.
///
/// DRAM throughput attributed to one process is not a number that exists: the
/// memory controller counts what crosses it for the whole package and has no
/// idea which process asked. A tool that answered anyway would hand back the
/// package's number with a pid printed beside it, and nothing in the output
/// would say so, which is why the catalog writes the refusal in capitals.
const DRAM_THROUGHPUT_FOR_ONE_PROCESS: &str = "\
name: DRAM throughput for one process, which does not exist
format: text

requests:
  - label: per-process bandwidth
    source: mem.bandwidth
    target: {pid: 1}
    mode: count
    emit: [rate]
";

/// Both huge-page backends, read one round apart rather than half a second.
///
/// `examples/mem-machine-counters.yml` asks a great deal of one session and is
/// right to; what it cannot do is compare a *level* read by two backends,
/// because its reads are spread across its window. Every request here is a
/// `snapshot`, so the run is a single round and the two reads are microseconds
/// apart -- which is the only way a count of resident huge pages on a machine
/// that is promoting them is a comparison rather than two instants.
const BOTH_HUGE_PAGE_BACKENDS_AT_ONCE: &str = "\
name: both huge page backends at one instant
format: json

requests:
  - label: per-size statistics
    source: mem.thp
    backend: force:fs:sys
    target: system
    mode: snapshot
    emit: [raw]

  - label: vmstat
    source: mem.thp
    backend: force:fs:proc
    target: system
    mode: snapshot
    emit: [raw]
";

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("omn exits rather than signals")
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output)).unwrap_or_else(|error| {
        panic!(
            "a session with `format: json` must print one json document ({error})\n\
             stdout: {}\nstderr: {}",
            stdout(output),
            stderr(output)
        )
    })
}

/// Some machines withhold what a scenario needs. That is a skip, and the skip
/// has to name what is missing -- a machine that cannot serve a request and a
/// request that cannot mean anything are different answers.
///
/// A missing shim is neither. It is a build that was not run, and letting it
/// skip would mean the heap scenarios measure nothing on the machine where they
/// matter most.
fn unavailable(output: &Output) -> Option<String> {
    let message = stderr(output);
    assert!(
        !message.contains("heap shim was not found"),
        "the heap sources need the preload shim: build with `make`, not `cargo build`\n{message}"
    );
    let withheld = message.contains("not readable")
        || message.contains("perf_event_paranoid")
        || message.contains("no such file");
    withheld.then_some(message)
}

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

// ---------------------------------------------------------------------------
// Reading a run back
// ---------------------------------------------------------------------------

fn result<'a>(run: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
    run["results"]
        .as_array()
        .expect("a session produces results")
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("no request labelled `{label}` in {run}"))
}

fn output_of<'a>(result: &'a serde_json::Value, emit: &str) -> &'a serde_json::Value {
    result["outputs"]
        .as_array()
        .expect("a result carries its outputs")
        .iter()
        .find(|output| output["emit"] == emit)
        .unwrap_or_else(|| panic!("no `{emit}` output in {result}"))
}

/// The one series of a summary, as (count, min, max, last).
fn summary(result: &serde_json::Value) -> (u64, f64, f64, f64) {
    let series = &output_of(result, "summary")["series"][0];
    (
        series["count"].as_u64().expect("a count"),
        series["min"].as_f64().expect("a minimum"),
        series["max"].as_f64().expect("a maximum"),
        series["last"].as_f64().expect("a last value"),
    )
}

/// Every attribute of the first raw event, by name.
fn attrs(result: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    output_of(result, "raw")["events"][0]["attrs"]
        .as_array()
        .expect("a raw event carries attributes")
        .iter()
        .map(|attr| {
            (
                attr["name"].as_str().expect("a name").to_owned(),
                attr["value"].clone(),
            )
        })
        .collect()
}

fn attr_u64(result: &serde_json::Value, name: &str) -> u64 {
    attrs(result)
        .get(name)
        .unwrap_or_else(|| panic!("no `{name}` on the event of {result}"))
        .as_u64()
        .unwrap_or_else(|| panic!("`{name}` is not a whole number in {result}"))
}

fn losses(result: &serde_json::Value, kind: &str) -> u64 {
    result["losses"][kind]
        .as_u64()
        .unwrap_or_else(|| panic!("no `{kind}` in the losses of {result}"))
}

// ---------------------------------------------------------------------------
// The catalog, as the tool projects it
// ---------------------------------------------------------------------------

/// One catalog entry, reduced to the axes a sweep walks.
struct Entry {
    id: String,
    modes: Vec<String>,
    emits: Vec<String>,
    targets: BTreeSet<String>,
}

fn catalog() -> Vec<Entry> {
    let listing = run(&["list", "mem", "--format", "json"]);
    assert!(listing.status.success(), "{}", stderr(&listing));
    let entries: serde_json::Value =
        serde_json::from_str(&stdout(&listing)).expect("the listing is json");
    entries
        .as_array()
        .expect("an array of entries")
        .iter()
        .map(|entry| Entry {
            id: entry["id"].as_str().expect("an id").to_owned(),
            modes: strings(&entry["modes"]),
            emits: strings(&entry["emits"]),
            targets: entry["targets"]
                .as_object()
                .expect("a target map")
                .keys()
                .cloned()
                .collect(),
        })
        .collect()
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .expect("an array of strings")
        .iter()
        .map(|item| item.as_str().expect("a string").to_owned())
        .collect()
}

/// The backends of one source that this build can actually serve, read off the
/// tool's own listing rather than off a list kept here: a source gains a
/// collector by a line in `src/collect/mem/mod.rs`, and nothing should have to
/// be edited in this file when it does.
fn implemented_backends(source: &str) -> Vec<String> {
    let listing = run(&["list", source, "--color", "never"]);
    assert!(listing.status.success(), "{}", stderr(&listing));
    stdout(&listing)
        .lines()
        .filter(|line| line.contains("collector written"))
        .filter_map(|line| {
            let after_number = line.trim_start().split_once(". ")?.1;
            Some(after_number.split_whitespace().next()?.to_owned())
        })
        .collect()
}

fn declared_backends(source: &str) -> Vec<String> {
    let listing = run(&["list", source, "--color", "never"]);
    stdout(&listing)
        .lines()
        .filter(|line| {
            line.contains("cost ") && line.trim_start().starts_with(|c: char| c.is_ascii_digit())
        })
        .filter_map(|line| {
            let after_number = line.trim_start().split_once(". ")?.1;
            Some(after_number.split_whitespace().next()?.to_owned())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// A. The capability sweep
// ---------------------------------------------------------------------------

/// What one (source, mode, emit) did when it was asked for.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Combination {
    source: String,
    mode: String,
    emit: String,
    exit: i32,
    reason: String,
}

fn spelled(mode: &str) -> &'static str {
    match mode {
        "poll" => "poll@50ms",
        "sample" => "sample@997hz",
        "snapshot" => "snapshot",
        "count" => "count",
        "trace" => "trace",
        other => panic!("no spelling for mode `{other}`"),
    }
}

/// Asks for every mode against every emit, for the named sources.
///
/// The workload is deliberately the cheapest thing that is still a process: a
/// sweep is about whether a combination can be served at all, and making it
/// measure something real as well would multiply a hundred runs by the time
/// each takes.
fn sweep(sources: &[&str]) -> Vec<Combination> {
    let mut walked = Vec::new();
    for entry in catalog().into_iter().filter(|e| sources.contains(&&*e.id)) {
        if implemented_backends(&entry.id).is_empty() {
            continue;
        }
        for mode in &entry.modes {
            for emit in &entry.emits {
                let mut args = vec![entry.id.clone(), "-m".into(), spelled(mode).into()];
                args.push("-e".into());
                args.push(emit.clone());
                if entry.targets.contains("launch") {
                    args.extend([
                        "--".into(),
                        "/bin/sh".into(),
                        "-c".into(),
                        "sleep 0.05".into(),
                    ]);
                } else if entry.targets.contains("system") {
                    args.extend(["--system".into(), "-d".into(), "60ms".into()]);
                } else {
                    continue;
                }
                let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
                let output = run(&borrowed);
                let reason = stderr(&output)
                    .lines()
                    .find(|line| line.starts_with("error:"))
                    .unwrap_or_default()
                    .to_owned();
                walked.push(Combination {
                    source: entry.id.clone(),
                    mode: mode.clone(),
                    emit: emit.clone(),
                    exit: code(&output),
                    reason,
                });
            }
        }
    }
    assert!(
        !walked.is_empty(),
        "the sweep walked nothing: {sources:?} have no collector in this build"
    );
    walked
}

/// Every combination refused as a machine limit, written as the sweep prints
/// them, for the sources one test covers.
fn blamed_on_the_machine(walked: &[Combination]) -> Vec<String> {
    let mut named: Vec<String> = walked
        .iter()
        .filter(|combination| combination.exit == EXIT_NO_VIABLE_BACKEND)
        .map(|combination| {
            format!(
                "{} {} {}",
                combination.source, combination.mode, combination.emit
            )
        })
        .collect();
    named.sort();
    named
}

/// The pinned list, per group of sources. See "Defects found", D5.
///
/// Every entry here is a combination the catalog offers, that this build
/// refuses at attach time with exit 1 -- "this machine cannot serve it" -- for a
/// reason that has nothing to do with the machine. Nothing is missing from this
/// machine for any of them: the collector does not implement the mode.
///
/// Thirteen of the sixteen D5 recorded have left this list: a refusal raised
/// before the target exists is now read as this build's gap and exits 3. What
/// is left is the three raised from `Collector::start`, where the runner cannot
/// tell a mode nobody wrote from a counter that would not open, and the
/// collector has to say which with `collect::not_built`.
fn assert_machine_blame_is_exactly(walked: &[Combination], expected: &[&str]) {
    let found = blamed_on_the_machine(walked);
    let expected: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(
        found, expected,
        "\nthe set of combinations refused with exit {EXIT_NO_VIABLE_BACKEND} has changed.\n\
         Exit {EXIT_NO_VIABLE_BACKEND} means this machine cannot serve the request; exit \
         {EXIT_NO_COLLECTOR} means this build has not implemented it.\n\
         If a combination left the list because a collector now serves it, delete it here.\n\
         If it left because the exit code was corrected to {EXIT_NO_COLLECTOR}, delete it here \
         and say so in docs/scenarios/mem.md under D5.\n\
         If a new one appeared, a mode the catalog declares has stopped being served.\n\
         Every one of these is recorded in docs/scenarios/mem.md.\n"
    );
}

#[test]
fn every_mode_and_emit_of_the_footprint_sources_is_served_or_refused_by_the_right_code() {
    let walked = sweep(&["mem.rss", "mem.pss", "mem.uss", "mem.cgroup", "mem.wss"]);
    assert_modes_and_emits_are_coherent(&walked);
    assert_machine_blame_is_exactly(&walked, &[]);
}

#[test]
fn every_mode_and_emit_of_the_counter_sources_is_served_or_refused_by_the_right_code() {
    let walked = sweep(&[
        "mem.faults",
        "mem.cache_l1",
        "mem.cache_llc",
        "mem.cache_tlb",
    ]);
    assert_modes_and_emits_are_coherent(&walked);
    // The nine cache combinations are gone: their refusal is raised by the
    // factory, before anything is opened, so it is now read as this build's gap
    // and exits 3. The three `mem.faults` ones remain, and the reason they do
    // is worth knowing -- `mem.faults` refuses in `Collector::start`, against a
    // target that already exists, where an unmarked refusal is a failure of the
    // run. `docs/fixes/runner.md` has the one-line change that moves them.
    assert_machine_blame_is_exactly(
        &walked,
        &[
            "mem.faults count rate",
            "mem.faults count raw",
            "mem.faults count summary",
        ],
    );
}

#[test]
fn every_mode_and_emit_of_the_machine_sources_is_served_or_refused_by_the_right_code() {
    let walked = sweep(&["mem.thp", "mem.swap", "mem.pressure", "mem.numa"]);
    assert_modes_and_emits_are_coherent(&walked);
    assert_machine_blame_is_exactly(&walked, &[]);
}

#[test]
fn every_mode_and_emit_of_the_heap_sources_is_served_or_refused_by_the_right_code() {
    let walked = sweep(&["mem.heap.alloc", "mem.heap.live", "mem.heap.retained"]);
    assert_modes_and_emits_are_coherent(&walked);
    // Nothing here blames the machine any more: a snapshot the shim cannot
    // serve is refused by the factory, which is where this build's own gaps are
    // told from this machine's.
    assert_machine_blame_is_exactly(&walked, &[]);
}

/// Whatever a combination did, it did not do it silently or by accident.
///
/// A rejection has to name the mode and the emit it refused, because a reader
/// who asked for one of twelve shapes needs to know which one was wrong; and no
/// combination may end in a code this vocabulary has no meaning for.
fn assert_modes_and_emits_are_coherent(walked: &[Combination]) {
    for combination in walked {
        assert!(
            (0..=3).contains(&combination.exit),
            "{combination:?} exited with a code outside the four this tool defines"
        );
        if combination.exit == EXIT_REJECTED {
            assert!(
                combination.reason.contains(&combination.mode)
                    || combination.reason.contains(&combination.emit),
                "a rejection has to name what it refused: {combination:?}"
            );
        }
    }
}

/// A source that declares an emit no mode it declares can produce has promised
/// something no build could ever keep.
///
/// The mode-to-emit matrix is a property of the model, not of a family: `rate`
/// comes out of `count` and `trace`, `timeline` out of `trace` and `poll`,
/// `summary` out of everything except `sample`. A catalog entry that crosses
/// those two lists and finds an emit unreachable is not a machine limit and not
/// a missing collector -- it is a line of YAML that cannot be true anywhere.
///
/// See "Defects found", D3.
#[test]
fn every_emit_a_mem_source_declares_can_be_produced_by_a_mode_it_declares() {
    let matrix: BTreeMap<&str, &[&str]> = BTreeMap::from([
        ("count", &["rate", "summary", "raw"][..]),
        ("sample", &["stack", "topk", "hist", "raw"][..]),
        (
            "trace",
            &[
                "raw", "hist", "topk", "timeline", "stack", "rate", "summary",
            ][..],
        ),
        ("poll", &["raw", "timeline", "summary"][..]),
        ("snapshot", &["summary", "raw"][..]),
    ]);

    let mut unreachable = Vec::new();
    for entry in catalog() {
        for emit in &entry.emits {
            let reachable = entry
                .modes
                .iter()
                .any(|mode| matrix[mode.as_str()].contains(&emit.as_str()));
            if !reachable {
                unreachable.push(format!("{} declares `{emit}`", entry.id));
            }
        }
    }
    unreachable.sort();

    // The matrix itself has to be right, or this test proves nothing: one
    // combination that is known to work is checked against the tool.
    let served = run(&[
        "mem.swap",
        "--system",
        "-m",
        "poll@50ms",
        "-e",
        "timeline",
        "-d",
        "100ms",
    ]);
    assert_eq!(
        code(&served),
        0,
        "`mem.swap` in poll mode emitting a timeline is the matrix's own example \
         of a combination that works: {}",
        stderr(&served)
    );

    assert!(
        unreachable.is_empty(),
        "\nthe set of `mem` emits that no declared mode can produce has changed.\n\
         Each of these is a promise in catalog/mem.yaml that no build can keep: the source \
         declares the emit, and every mode it declares refuses it with exit {EXIT_REJECTED}.\n\
         D3 was the two this file used to record -- `mem.sharing` declaring `summary` over its \
         only mode `sample`, and `mem.swap` declaring `rate` over `poll` and `snapshot`.\n\
         Both are out of the catalog; what is left here is: {unreachable:?}\n"
    );

    // And the two that were removed are now refused by the source rather than
    // by the matrix, which is the message that names the line of YAML to look
    // at. Against the old catalog both of these said `mode ... cannot produce`.
    for (source, mode, emit, target) in [
        ("mem.swap", "poll@50ms", "rate", "--system"),
        ("mem.sharing", "sample@997hz", "summary", "--system"),
    ] {
        let refused = run(&[source, target, "-m", mode, "-e", emit, "-d", "100ms"]);
        assert_eq!(code(&refused), EXIT_REJECTED, "{}", stderr(&refused));
        assert!(
            stderr(&refused).contains(&format!("`{source}` cannot emit `{emit}`")),
            "`{source}` should refuse `{emit}` as its own, not as the matrix's: {}",
            stderr(&refused)
        );
    }
}

/// The shapes the family can produce, all at once, over one program.
///
/// This is the readable half of the sweep: six sources, one launch, one window,
/// every emit the family has a producer for. A combination that resolves and
/// then renders nothing passes the sweep above and fails here.
#[test]
fn the_capability_sweep_session_produces_every_shape_it_asks_for() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run_example("mem-capability-sweep.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let run = json(&output);

    let wanted: [(&str, &[&str]); 6] = [
        ("footprint over time", &["timeline", "summary"]),
        ("private footprint, one number", &["summary"]),
        ("what it came back to", &["timeline", "summary"]),
        (
            "faults, several ways at once",
            &["hist", "topk", "rate", "summary"],
        ),
        ("allocation sizes", &["hist", "topk", "summary"]),
        ("l1 misses, counted", &["summary", "rate"]),
    ];
    for (label, emits) in wanted {
        let result = result(&run, label);
        let produced: Vec<&str> = result["outputs"]
            .as_array()
            .expect("outputs")
            .iter()
            .map(|output| output["emit"].as_str().expect("an emit name"))
            .collect();
        assert_eq!(produced, emits, "`{label}` produced the wrong shapes");
        assert!(
            result["events_seen"].as_u64().expect("a count") > 0,
            "`{label}` rendered its emits over no events at all"
        );
    }
}

/// Every output format that claims it can carry a timeline has to carry one.
///
/// A format that claims an emit it cannot express is the same lie as a mode
/// that resolves and then refuses, one layer further out, and `--format` is the
/// axis a reader is least likely to test before piping the result somewhere.
#[test]
fn a_footprint_timeline_reaches_every_format_that_can_carry_it() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    for format in ["json", "yaml", "toml", "ndjson", "chrome", "speedscope"] {
        let output = run(&[
            "mem.rss",
            "-m",
            "poll@50ms",
            "-e",
            "timeline",
            "--format",
            format,
            "--",
            "python3",
            "-c",
            "import time; block = bytearray(8 << 20); time.sleep(0.2)",
        ]);
        if let Some(reason) = unavailable(&output) {
            eprintln!("skipped: {format}: {reason}");
            continue;
        }
        assert_eq!(code(&output), 0, "`{format}`: {}", stderr(&output));
        assert!(
            !stdout(&output).trim().is_empty(),
            "`{format}` succeeded and wrote nothing"
        );
    }

    // And one that says no rather than approximating: `folded` carries stacks.
    let refused = run(&[
        "mem.rss",
        "-m",
        "poll@50ms",
        "-e",
        "timeline",
        "--format",
        "folded",
        "--",
        "/bin/sh",
        "-c",
        "sleep 0.05",
    ]);
    assert_eq!(code(&refused), EXIT_REJECTED, "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("folded") && stderr(&refused).contains("timeline"),
        "the refusal has to name the format and the emit: {}",
        stderr(&refused)
    );
}

// ---------------------------------------------------------------------------
// A. Ground truth: the workload chooses the number
// ---------------------------------------------------------------------------

/// Sixty-four megabytes, written a byte per page, seen by all three footprints.
///
/// The three differ only in how they count shared pages, and a buffer this
/// program just wrote to is shared with nobody: the rise is the same number
/// three times over. `malloc` would have moved none of them, which is why the
/// workload writes.
#[test]
fn sixty_four_megabytes_made_resident_show_up_in_all_three_footprints() {
    const TOUCHED: f64 = 64.0 * 1024.0 * 1024.0;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run_example("mem-footprints.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let run = json(&output);

    let mut maxima = Vec::new();
    for label in ["resident", "proportional", "private"] {
        let (count, low, high, _) = summary(result(&run, label));
        assert!(
            count >= 10,
            "`{label}` polled at 50 ms over a program that lives 700 ms is a dozen points, not \
             {count}"
        );
        let rise = high - low;
        assert!(
            rise >= TOUCHED,
            "`{label}`: the program made {TOUCHED} bytes resident and the rise was {rise}"
        );
        assert!(
            rise <= 2.0 * TOUCHED,
            "`{label}`: a rise of {rise} is far more than the {TOUCHED} bytes the program touched"
        );
        maxima.push(high);
    }

    // Private is a subset of proportional is a subset of resident. The three
    // are read one after another inside a tick, so a few pages may appear
    // between two of them.
    let slack = 0.01 * maxima[0];
    assert!(
        maxima[2] <= maxima[1] + slack && maxima[1] <= maxima[0] + slack,
        "uss <= pss <= rss has to hold: {maxima:?}"
    );
}

/// The kernel counts this program's page faults, and so does the tool.
///
/// `getrusage` is the independent witness: the program asks the kernel how many
/// minor faults it took and prints the answer, and the run has to find the same
/// faults. Both gaps are in the same direction and both are startup: the
/// interpreter faults before the collector is attached, and the program faults
/// between the last drain and its own final read.
#[test]
fn the_tool_finds_the_faults_the_program_counted_itself() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run_example("mem-faults-ground-truth.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let kernels_own = stderr(&output)
        .lines()
        .find_map(|line| line.strip_prefix("MINFLT=").map(str::to_owned))
        .expect("the workload prints its own ru_minflt on standard error")
        .trim()
        .parse::<f64>()
        .expect("a fault count");
    assert!(
        kernels_own > 500.0,
        "touching 256 MiB has to fault more than {kernels_own} times, or the workload did nothing"
    );

    let traced = result(&json(&output), "faults")["events_seen"]
        .as_u64()
        .expect("a count") as f64;
    let shortfall = (kernels_own - traced) / kernels_own;
    assert!(
        (0.0..0.10).contains(&shortfall),
        "the kernel counted {kernels_own} minor faults for this program and the run traced \
         {traced}: a shortfall of {:.1} %",
        shortfall * 100.0
    );
}

/// A gibibyte of reads costs at least one L1 miss per cache line.
///
/// This is the one thing about a cache counter that is portable. The buffer is
/// twice this machine's whole last-level cache, every page of it has been
/// written so none is a shared zero page, and it is read from end to end eight
/// times: sixteen million seven hundred and seventy-seven thousand two hundred
/// and sixteen lines have to come in, and a prefetcher decides when a line
/// arrives, not whether it is needed.
///
/// The TLB is read beside it for the opposite reason. The same scan crosses a
/// quarter of a million four-kibibyte pages and five hundred and twelve
/// two-mebibyte ones, so a TLB miss count near the first number would mean the
/// huge pages are not there -- which is a property of the machine's THP mode,
/// and why the scenario reads `mem.thp` before believing this one.
#[test]
fn a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line() {
    const BYTES_SCANNED: f64 = 128.0 * 1024.0 * 1024.0 * 8.0;
    const LINES: f64 = BYTES_SCANNED / 64.0;
    const SMALL_PAGES: f64 = BYTES_SCANNED / 4096.0;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run_example("mem-cache-lines.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let session = json(&output);

    let misses = attr_u64(result(&session, "l1 misses"), "count") as f64;
    assert!(
        misses >= LINES,
        "scanning {BYTES_SCANNED} bytes brings in {LINES} cache lines and the counter saw only \
         {misses} L1 misses"
    );
    assert!(
        misses <= 2.5 * LINES,
        "{misses} L1 misses for {LINES} cache lines is more than the scan can account for"
    );

    // The loads are counted in the same group and travel on the event, because
    // a miss count without them is not a miss rate.
    let loads = attr_u64(result(&session, "l1 misses"), "l1d_loads") as f64;
    assert!(
        loads > misses,
        "every miss is a load: {loads} loads against {misses} misses"
    );

    let thp_is_on = run(&[
        "mem.thp", "--system", "-m", "snapshot", "-e", "raw", "--format", "json",
    ]);
    let mode = if thp_is_on.status.success() {
        attrs(&json(&thp_is_on)["results"][0])
            .get("thp_mode")
            .and_then(|value| value.as_str().map(str::to_owned))
    } else {
        None
    };
    match mode.as_deref() {
        Some("always") => {
            let tlb = attr_u64(result(&session, "tlb misses"), "count") as f64;
            assert!(
                tlb < SMALL_PAGES / 4.0,
                "with transparent huge pages set to `always`, a scan crossing {SMALL_PAGES} \
                 four-kibibyte pages must cross far fewer huge ones, and the TLB counter saw \
                 {tlb} misses"
            );
        }
        other => eprintln!(
            "skipped the TLB half: this machine's transparent huge page mode is {other:?}, and \
             the claim is about `always`"
        ),
    }
}

/// Twenty thousand allocations of four thousand and one bytes, exactly.
///
/// Not "about twenty thousand". The shim sees every call, 4001 is a size
/// nothing else in an interpreter asks for, and a count that is short is a
/// count that lost something without saying so.
#[test]
fn an_exact_number_of_allocations_is_reported_exactly() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run_example("mem-allocation-exact.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let session = json(&output);
    let result = result(&session, "allocations");

    let (count, low, high, _) = summary(result);
    assert_eq!(count, 20_000, "the program called malloc(4001) 20000 times");
    assert_eq!(low, 4001.0, "every one of them asked for 4001 bytes");
    assert_eq!(high, 4001.0, "every one of them asked for 4001 bytes");
    assert_eq!(
        losses(result, "dropped"),
        0,
        "twenty thousand records fit in the pipe: {result}"
    );
}

// ---------------------------------------------------------------------------
// B. The backend comparison
// ---------------------------------------------------------------------------

/// Two files, one resident set; two files, one proportional set.
///
/// `smaps_rollup` is the kernel's own rollup and `statm` is seven numbers in
/// pages; the catalog says the second degrades nothing about the resident size
/// itself. The maxima are what is compared, not the two series point by point:
/// reading either file faults in pages of the reader's own, so two reads are
/// two instants of a process that grows while it is read.
///
/// The `statm` series used to contain a value that was not a measurement: a
/// zero for a process that had already exited. See "Defects found", D1, fixed.
#[test]
fn the_two_roads_to_a_footprint_arrive_at_the_same_number() {
    const TOUCHED: f64 = 64.0 * 1024.0 * 1024.0;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run_example("mem-backend-agreement.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let run = json(&output);

    for (left, right, tolerance) in [
        ("rss via the rollup", "rss via statm", 0.01),
        ("pss via the rollup", "pss via the per-mapping walk", 0.01),
        ("uss via the rollup", "uss via the per-mapping walk", 0.01),
    ] {
        let (_, _, left_max, _) = summary(result(&run, left));
        let (_, _, right_max, _) = summary(result(&run, right));
        assert!(
            left_max - TOUCHED > 0.0 && right_max - TOUCHED > 0.0,
            "both backends have to see the {TOUCHED} bytes the program made resident: \
             {left} peaked at {left_max}, {right} at {right_max}"
        );
        let apart = (left_max - right_max).abs() / left_max;
        assert!(
            apart <= tolerance,
            "`{left}` and `{right}` are two routes to one number and their peaks are \
             {:.2} % apart ({left_max} against {right_max})",
            apart * 100.0
        );
    }

    // D1, fixed. A zombie keeps its `/proc/<pid>/statm` and that file reads
    // `0 0 0 0 0 0 0`, which is not an error and used to be parsed and
    // published as a resident set of zero bytes -- the one answer a footprint
    // must never invent. The same zombie's `smaps_rollup` answers ESRCH, so
    // the two backends of one chain ended the run one reading apart and the
    // self-benchmark reported them 6.25 % apart on a value that was not a
    // measurement.
    let (rollup_points, _, _, rollup_last) = summary(result(&run, "rss via the rollup"));
    let (statm_points, statm_low, _, statm_last) = summary(result(&run, "rss via statm"));
    assert!(
        statm_low > 0.0,
        "no live process is 0 bytes resident: `statm` published {statm_low} B, which is a \
         departed process parsed as a measurement"
    );
    assert_eq!(
        statm_points, rollup_points,
        "both roads watch one process over one window, so they end on the same reading: \
         {statm_points} against {rollup_points}"
    );
    // And the mean the self-benchmark compares is no longer dragged by it: the
    // two roads end on the same last reading of the same live process.
    let apart = (statm_last - rollup_last).abs() / rollup_last;
    assert!(
        apart <= 0.01,
        "the last reading of one process through two files: {statm_last} against {rollup_last}"
    );
}

/// The machine's own files and cgroup2 are two routes to the same counters.
///
/// `mem.pressure` and `mem.swap` each declare their second backend
/// `equivalent: true`, which the loader takes to mean it degrades nothing. At
/// the root cgroup the two even cover the same tasks, so the totals have to
/// match -- and this test does not take the tool's word for either of them: it
/// reads `/proc/pressure/memory` and `/proc/vmstat` itself and checks both
/// routes against the kernel.
///
/// Every counter here is monotone and the machine is not idle while the suite
/// runs, so the kernel is read before the sessions and again after, and what
/// the tool reports has to fall inside that bracket. Comparing against a single
/// read would be pretending two instants were one, which is the mistake the
/// first `smaps` comparison in this repository made.
#[test]
fn the_machine_and_the_root_cgroup_are_two_routes_to_one_counter() {
    fn psi_some_ns() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/pressure/memory").ok()?;
        text.lines()
            .find_map(|line| line.strip_prefix("some "))
            .and_then(|rest| rest.split("total=").nth(1))
            .and_then(|total| total.trim().parse::<u64>().ok())
            .map(|microseconds| microseconds * 1_000)
    }
    fn vmstat_pages(name: &str) -> Option<u64> {
        let text = std::fs::read_to_string("/proc/vmstat").ok()?;
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{name} ")))
            .and_then(|value| value.trim().parse::<u64>().ok())
    }

    let (Some(stall_before), Some(out_before)) = (psi_some_ns(), vmstat_pages("pswpout")) else {
        eprintln!(
            "skipped: this kernel publishes no /proc/pressure/memory or no `pswpout` in \
             /proc/vmstat, so there is nothing independent to check either route against"
        );
        return;
    };
    if !std::path::Path::new("/sys/fs/cgroup/memory.pressure").exists() {
        eprintln!(
            "skipped: the root cgroup publishes no memory.pressure, so cgroup2 is not mounted \
             here or is mounted without it"
        );
        return;
    }

    let machine = run_example("mem-machine-counters.yml");
    let cgroup = run_example("mem-cgroup-counters.yml");
    let (Some(stall_after), Some(out_after)) = (psi_some_ns(), vmstat_pages("pswpout")) else {
        eprintln!("skipped: the kernel's counters stopped being readable mid-test");
        return;
    };

    for output in [&machine, &cgroup] {
        if let Some(reason) = unavailable(output) {
            eprintln!("skipped: {reason}");
            return;
        }
        assert!(output.status.success(), "{}", stderr(output));
    }
    let machine = json(&machine);
    let cgroup = json(&cgroup);

    const PAGE: u64 = 4096;
    for (name, run) in [("machine", &machine), ("cgroup", &cgroup)] {
        let stall = attr_u64(result(run, "stall time since boot"), "some_total_ns");
        assert!(
            (stall_before..=stall_after).contains(&stall),
            "the {name} route reports {stall} ns of `some` memory stall; the kernel's own \
             counter went from {stall_before} to {stall_after} across the run, and the answer \
             has to be inside that"
        );

        // `pswpin` and `pswpout` are in pages while almost every neighbouring
        // field in both files is in bytes or kilobytes, so this is the
        // assertion that catches a scale error: reading them as anything else
        // is wrong by exactly the page size and looks entirely plausible.
        let out = attr_u64(result(run, "swap since boot"), "out_bytes_since_boot");
        assert!(
            (out_before * PAGE..=out_after * PAGE).contains(&out),
            "the {name} route reports {out} bytes swapped out; the kernel counted \
             {out_before} to {out_after} pages across the run, which is \
             {} to {} bytes",
            out_before * PAGE,
            out_after * PAGE
        );
    }

    // And the two routes against each other. The machine session runs first, so
    // a monotone counter can only have grown by the time the cgroup one reads
    // it, and a quiet moment leaves them equal.
    let machine_stall = attr_u64(result(&machine, "stall time since boot"), "some_total_ns");
    let cgroup_stall = attr_u64(result(&cgroup, "stall time since boot"), "some_total_ns");
    assert!(
        cgroup_stall >= machine_stall,
        "the two routes read one monotone counter moments apart: {machine_stall} then \
         {cgroup_stall}"
    );
    let machine_in = attr_u64(result(&machine, "swap since boot"), "in_bytes_since_boot");
    let cgroup_in = attr_u64(result(&cgroup, "swap since boot"), "in_bytes_since_boot");
    assert!(
        cgroup_in >= machine_in,
        "`fs:cgroup` declares itself equivalent to `fs:proc`, and at the root they are reading \
         the same accounting: {machine_in} then {cgroup_in}"
    );
}

/// Both THP backends read the same promotion counter; only one reads the mode.
///
/// That is the whole of the declared degradation -- "promotion counters only,
/// not the configured mode" -- and it is checkable in both directions: the
/// counter must agree, and the mode must be absent from the fallback rather
/// than guessed.
///
/// A third field used to disagree with nothing declaring it. See "Defects
/// found", D6, fixed.
#[test]
fn both_huge_page_backends_read_the_same_promotion_counter() {
    // The kernel's own answer to the question both backends are asked, read
    // either side of the run so that what moves during it is visible rather
    // than assumed away. `AnonHugePages` is the fully-mapped anonymous huge
    // pages, in kilobytes.
    fn anon_huge_kilobytes() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        text.lines()
            .find_map(|line| line.strip_prefix("AnonHugePages:"))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    let before = anon_huge_kilobytes();
    let output = run_example("mem-machine-counters.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let run = json(&output);

    let sysfs = attrs(result(&run, "huge pages since boot, with the mode"));
    let vmstat = attrs(result(&run, "huge pages since boot, counters only"));

    // Monotone and read moments apart, exactly like the PSI pair above: the
    // two sessions are separate reads of one counter that has been rising since
    // boot, so the later one is at least the earlier and the gap is whatever
    // the machine promoted in between. Measured here at one page, under the
    // rest of this suite. Asserting equality made the scenario a test of how
    // busy the machine was.
    let sysfs_promotions = sysfs["promotions_since_boot"].as_u64().expect("a count");
    let vmstat_promotions = vmstat["promotions_since_boot"].as_u64().expect("a count");
    assert!(
        vmstat_promotions >= sysfs_promotions,
        "one monotone kernel counter reached through two files cannot go backwards: \
         {sysfs_promotions} then {vmstat_promotions}"
    );
    // And the two must be the same counter, not two plausible ones. A thousand
    // promotions between two reads in the same run would mean they are not.
    assert!(
        vmstat_promotions - sysfs_promotions < 1_000,
        "these are meant to be one counter through two files, and they are \
         {} apart: {sysfs_promotions} then {vmstat_promotions}",
        vmstat_promotions - sysfs_promotions
    );
    assert_eq!(
        sysfs["hpage_bytes"].as_u64(),
        vmstat["hpage_bytes"].as_u64(),
        "both report the PMD-sized huge page, and every event says how large that is"
    );
    assert!(
        sysfs.contains_key("thp_mode"),
        "`fs:sys` is preferred precisely because it reads the configured mode"
    );
    assert!(
        !vmstat.contains_key("thp_mode"),
        "`fs:proc` declares that it cannot see the mode; inventing one would be worse than \
         the degradation it declares"
    );

    // D6, fixed. `fs:sys` used to read `nr_anon` out of the per-size
    // statistics, which counts every anonymous huge page of that size, while
    // `fs:proc` read `nr_anon_transparent_hugepages` out of /proc/vmstat,
    // which counts only the ones still wholly mapped. The two were 8 % apart
    // on this machine, systematically, under one attribute name and with no
    // caveat anywhere saying they were answering different questions.
    //
    // A resident count is a *level*, so the comparison needs the two reads to
    // be one instant. The session above mixes modes and spreads its reads over
    // half a second, and under this suite the machine promotes and splits huge
    // pages the whole time, so this half is asked of a session of its own in
    // which every request is a `snapshot` and the two reads are one round
    // apart.
    //
    // A round apart is still not an instant on a loaded machine -- the reading
    // thread can be descheduled between the two -- and that is why the session
    // is run five times and the *closest* pair is the one asserted on. The
    // drift between two reads is noise either way; the defect was a bias, the
    // same 8 % in the same direction on every run, and a biased pair can never
    // produce a close one. Measured here: nineteen pages apart on the worst of
    // five runs under the full suite, none on the best.
    const TRIES: usize = 5;
    let mut pairs = Vec::new();
    let mut partial = None;
    for _ in 0..TRIES {
        let at_one_instant = run_written_session(BOTH_HUGE_PAGE_BACKENDS_AT_ONCE);
        assert!(
            at_one_instant.status.success(),
            "{}",
            stderr(&at_one_instant)
        );
        let instant = json(&at_one_instant);
        let sysfs = attrs(result(&instant, "per-size statistics"));
        let vmstat = attrs(result(&instant, "vmstat"));

        // What only `fs:sys` can see travels beside the comparable number
        // rather than inside it, and `fs:proc` says nothing it cannot see.
        partial = sysfs
            .get("partially_mapped_huge_pages")
            .and_then(serde_json::Value::as_u64);
        assert!(
            partial.is_some(),
            "`fs:sys` reads the per-size statistics, which count the partially mapped huge \
             pages: {sysfs:?}"
        );
        assert!(
            !vmstat.contains_key("partially_mapped_huge_pages"),
            "/proc/vmstat has no such counter, and inventing one is the degradation this \
             source declares, in reverse"
        );
        pairs.push((
            sysfs["resident_huge_pages"].as_u64().expect("a count"),
            vmstat["resident_huge_pages"].as_u64().expect("a count"),
            partial.expect("read a moment ago"),
        ));
    }
    let after = anon_huge_kilobytes();
    let partial = partial.expect("five runs read it");
    // How fast this machine's huge pages are moving, measured rather than
    // assumed: the change in one backend's own answer between consecutive
    // rounds. The two backends inside a pair are read one after the other, not
    // at one instant, so whatever moves in that gap is in their difference --
    // and under the rest of this suite it moves. Without this the test is a
    // measurement of how busy the machine was.
    let drift = pairs
        .windows(2)
        .map(|pair| pair[0].0.abs_diff(pair[1].0))
        .max()
        .unwrap_or(0);
    let (theirs, ours, partial_then) = *pairs
        .iter()
        .min_by_key(|(theirs, ours, _)| theirs.abs_diff(*ours))
        .expect("five runs");

    if partial == 0 {
        eprintln!(
            "skipped the agreement half: this machine has no partially mapped huge pages right \
             now, so the two backends cannot disagree and nothing here would catch it if they \
             did. Both said {theirs}"
        );
    } else {
        // The bound is the size of the error plus the size of the machine's own
        // movement, and both are measured here rather than chosen to pass: the
        // gap between the two used to be exactly the partially-mapped count,
        // and the pages themselves move between the two reads of a pair.
        let bound = partial_then / 2 + drift;
        assert!(
            theirs.abs_diff(ours) <= bound,
            "\nnot one of {TRIES} pairs of reads of one kernel's huge pages, taken one round \
             apart through the two backends of one source, came within {bound} of each other \
             ({} for the partially-mapped pages only `fs:sys` sees, {drift} for how far this \
             machine's own count moved between rounds): {pairs:?}. The difference used to be \
             exactly the partially-mapped count.\n",
            partial_then / 2
        );
    }

    // Ground truth, and the reason this is not two numbers agreeing on a
    // mistake: `/proc/meminfo` publishes the same count, and it is read here
    // rather than taken from the tool. It is a level and the machine is not
    // idle while the suite runs, so it is read either side of the session and
    // what moved in between is carried as the slack -- the pattern the PSI
    // pair above uses for the same reason.
    let page_size = sysfs["hpage_bytes"].as_u64().expect("the huge page size") / 1024;
    let (Some(before), Some(after)) = (before, after) else {
        eprintln!("skipped the kernel cross-check: this kernel publishes no AnonHugePages line");
        return;
    };
    let (before, after) = (before / page_size, after / page_size);
    let drift = before.abs_diff(after);
    if drift > 64 {
        eprintln!(
            "skipped the kernel cross-check: AnonHugePages moved from {before} to {after} huge \
             pages while the session ran, which is wider than the 8 % this is looking for. The \
             tool said {theirs} and {ours}"
        );
        return;
    }
    let slack = drift + 4;
    let (low, high) = (before.min(after) - slack, before.max(after) + slack);
    for (backend, reported) in [("fs:sys", theirs), ("fs:proc", ours)] {
        assert!(
            (low..=high).contains(&reported),
            "`{backend}` reports {reported} fully-mapped huge pages and /proc/meminfo says \
             between {before} and {after} over the same window. {partial} of them are \
             partially mapped, which is the size of the error this once made"
        );
    }
}

/// Forcing a backend nothing implements is exit 3, and it names the backend.
///
/// The comparison `--backend force:` exists for is not always available: where
/// a chain has one implemented backend and others declared, the comparison is
/// with the refusal instead, and the refusal has to say that the build is
/// missing rather than the machine.
#[test]
fn forcing_a_backend_with_no_collector_exits_three_and_names_it() {
    let mut checked = 0;
    for source in [
        "mem.faults",
        "mem.heap.alloc",
        "mem.heap.live",
        "mem.numa",
        "mem.wss",
    ] {
        let implemented = implemented_backends(source);
        for backend in declared_backends(source) {
            if implemented.contains(&backend) {
                continue;
            }
            let forced = format!("force:{backend}");
            let output = run(&[
                source,
                "--backend",
                &forced,
                "-m",
                "poll@50ms",
                "-e",
                "summary",
                "--",
                "/bin/sh",
                "-c",
                "sleep 0.05",
            ]);
            // A mode the source does not declare is refused before the backend
            // is ever reached; ask in a mode it does declare.
            let output = if code(&output) == EXIT_REJECTED {
                run(&[
                    source,
                    "--backend",
                    &forced,
                    "-m",
                    "trace",
                    "-e",
                    "summary",
                    "--",
                    "/bin/sh",
                    "-c",
                    "sleep 0.05",
                ])
            } else {
                output
            };
            assert_eq!(
                code(&output),
                EXIT_NO_COLLECTOR,
                "`{source}` pinned to `{backend}`, which nothing implements, has to say so with \
                 exit {EXIT_NO_COLLECTOR}: {}",
                stderr(&output)
            );
            assert!(
                stderr(&output).contains(&backend) && stderr(&output).contains(source),
                "the refusal has to name the source and the backend: {}",
                stderr(&output)
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 5,
        "the `mem` chains hold more than {checked} declared backends without a collector; \
         the listing this reads must have changed shape"
    );
}

// ---------------------------------------------------------------------------
// C. The loss path
// ---------------------------------------------------------------------------

/// Two hundred thousand allocations, and the arithmetic that has to close.
///
/// The shim never blocks the measured program: when the pipe is full it throws
/// the record away and counts it, and reports the count as soon as there is
/// room. So what the run saw plus what it counted lost has to cover every
/// record the program caused the shim to write.
///
/// It did not: the counter reported seven drops for thirty-one thousand lost
/// records, because the record that met a full pipe while a backlog was
/// already waiting left through an early `return` without being counted. D2 of
/// `docs/scenarios/mem.md`, fixed.
///
/// The closure is over *records*, not over the filtered series, and the
/// difference is the whole of what this test had to learn. The session keeps
/// only the 4001-byte allocations, and a drop is a record that never reached
/// the reader at all -- so it can be the `free` of one of those allocations,
/// or the interpreter's own traffic, and the two numbers are not commensurable
/// until the filter's own count is added back. What the workload guarantees is
/// the floor: `free(malloc(4001))` two hundred thousand times is four hundred
/// thousand records, whatever else `python3` does around them.
#[test]
fn the_shim_counts_every_record_it_could_not_carry() {
    const CHOSEN: u64 = 200_000;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run_example("mem-allocation-storm.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let session = json(&output);
    let result = result(&session, "allocations");

    let (seen, low, high, _) = summary(result);
    assert_eq!(
        (low, high),
        (4001.0, 4001.0),
        "every allocation asked for 4001 bytes"
    );
    let dropped = losses(result, "dropped");

    if dropped == 0 && seen == CHOSEN {
        // A machine fast enough to drain the pipe as fast as the shim fills it
        // has nothing to lose, and then the accounting is trivially exact.
        return;
    }

    assert!(
        dropped > 0,
        "the run is {} records short of the {CHOSEN} this program made and reports no loss at \
         all",
        CHOSEN - seen
    );
    assert!(
        result["losses"]["reasons"]
            .as_object()
            .is_some_and(|reasons| reasons.keys().any(|reason| reason.contains("shim"))),
        "a loss has to say what was lost and why: {result}"
    );

    // Every record the shim wrote or threw away, which is the quantity the
    // accounting is about: what the filter kept, what it dropped, and what
    // never arrived for it to judge.
    let filtered = &result["filtered"];
    let kept = filtered["kept"]
        .as_u64()
        .expect("a filtered run counts what it kept");
    let filtered_out = filtered["dropped"].as_u64().expect("and what it did not");
    assert_eq!(kept, seen, "the series is what the filter kept");
    let attempted = kept + filtered_out + dropped;

    // The floor is the workload's: two hundred thousand `malloc`s of 4001
    // bytes and the two hundred thousand `free`s that follow them, one record
    // each. Before the fix this came to about three hundred and forty
    // thousand -- sixty thousand records that were neither delivered nor
    // counted, in a run that reported seven drops.
    assert!(
        attempted >= 2 * CHOSEN,
        "\n`free(malloc(4001))` {CHOSEN} times is {} records the shim was asked to write, and \
         the run accounts for {attempted} of them. The rest were neither delivered nor counted, \
         which is the invariant this scenario exists for.\n\
         kept {seen}, filtered out {filtered_out}, dropped {dropped}\n",
        2 * CHOSEN
    );

    // And not much more than the floor, which is what says a drop is counted
    // once rather than twice: everything above four hundred thousand is
    // `python3`'s own allocation traffic around the loop, six thousand records
    // of it here.
    assert!(
        attempted < 3 * CHOSEN,
        "{attempted} records for a workload that causes {} is not the interpreter's overhead; \
         a backlog counted twice looks exactly like this",
        2 * CHOSEN
    );

    // The strongest form of the same statement, and the one that needs no
    // number from outside: the deterministic program is run twice, the two
    // runs lose wildly different amounts to a pipe that fills differently
    // every time, and they account for *the same total*. Measured at 406018
    // records on this machine across ten runs, with the drop count ranging
    // from 62178 to 144753.
    let again = run_example("mem-allocation-storm.yml");
    assert!(again.status.success(), "{}", stderr(&again));
    let repeated_run = json(&again);
    let repeated = self::result(&repeated_run, "allocations");
    let repeated_total = repeated["filtered"]["kept"].as_u64().unwrap()
        + repeated["filtered"]["dropped"].as_u64().unwrap()
        + losses(repeated, "dropped");
    let apart = attempted.abs_diff(repeated_total);
    assert!(
        apart * 200 < attempted,
        "\none program, run twice, causes the same records to be written both times: {attempted} \
         and {repeated_total}. A total that moves with how much was lost is a loss that is not \
         being counted -- which is D2 -- or one being counted twice.\n\
         dropped {dropped} then {}\n",
        losses(repeated, "dropped")
    );
}

/// A table of freed ranges that overflows, and says how much it evicted.
///
/// Retained memory missing from the table looks exactly like retained memory
/// that does not exist, which is why the table is bounded and why what it drops
/// is a loss rather than a silently smaller number. The workload frees a
/// hundred and forty thousand ranges into a table that holds a hundred and
/// thirty-one thousand and seventy-two.
#[test]
fn a_freed_range_table_that_overflows_says_how_much_it_dropped() {
    const FREED: u64 = 140_000;
    const CAPACITY: u64 = 131_072;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run_example("mem-retained-eviction.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let session = json(&output);
    let result = result(&session, "retained");

    let reasons = result["losses"]["reasons"]
        .as_object()
        .map(|reasons| reasons.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let eviction = reasons
        .iter()
        .find(|reason| reason.contains("freed-range table"))
        .unwrap_or_else(|| {
            panic!(
                "freeing {FREED} ranges into a table of {CAPACITY} has to evict, and the \
                 eviction has to reach the output: {result}"
            )
        });
    assert!(
        eviction.contains(&CAPACITY.to_string()),
        "the loss has to say how large the table is, or a reader cannot tell how much is \
         missing: {eviction}"
    );
    assert!(
        eviction.contains("bytes of address space"),
        "the loss has to say how much address space it stopped watching: {eviction}"
    );
    assert!(
        losses(result, "dropped") >= FREED - CAPACITY,
        "at least {} ranges had to be evicted and the run counted {}",
        FREED - CAPACITY,
        losses(result, "dropped")
    );
}

/// A poll faster than the file can be read, and the ticks that were missed.
///
/// The `smaps` walk prints a block per mapping, and two thousand mappings at a
/// thousand polls a second does not fit. What matters is that the run says so:
/// a reader who divides the series by the requested interval is computing a
/// rate over points that were never taken.
#[test]
fn a_poll_faster_than_the_file_can_be_read_counts_its_late_ticks() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run_example("mem-poll-too-fast.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let session = json(&output);
    let result = result(&session, "proportional, the slow way");

    let observed_ms = result["observed_ns"].as_u64().expect("a duration") / 1_000_000;
    let points = output_of(result, "timeline")["series"][0]["points"]
        .as_array()
        .expect("a timeline")
        .len() as u64;
    let asked_for = observed_ms; // one per millisecond

    if points >= asked_for {
        eprintln!(
            "skipped: this machine walked {points} smaps files in {observed_ms} ms and kept up \
             with a poll every millisecond, so there is no late tick to check"
        );
        return;
    }
    assert!(
        losses(result, "late_ticks") > 0,
        "the run produced {points} points where {asked_for} were asked for and reports no late \
         tick at all: {result}"
    );
    let reasons = result["losses"]["reasons"]
        .as_object()
        .map(|reasons| reasons.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    assert!(
        reasons.iter().any(|reason| reason.contains("outran")),
        "a late tick has to say that the round outran its tick: {reasons:?}"
    );
    // One reason for however many late ticks there were. The duration used to
    // be in the key, which made every occurrence a reason of its own. Other
    // kinds of loss keep their own reasons; what is counted here is the late
    // ticks' share of the table.
    assert_eq!(
        reasons
            .iter()
            .filter(|reason| reason.contains("outran"))
            .count(),
        1,
        "the late ticks have to share one reason: {reasons:?}"
    );
    let overrun = &result["losses"]["overrun"];
    assert!(
        overrun["worst_ns"].as_u64().unwrap_or_default()
            > overrun["tick_ns"].as_u64().unwrap_or_default(),
        "and the magnitude has to travel beside the count: {overrun}"
    );
}

/// A target that exits ends the run, rather than the run ending the target.
///
/// The session asks for five seconds and the program lives for a fifth of one.
/// A collector that returned five seconds of series would be inventing four of
/// them, and one that stopped without saying why would leave a reader unable to
/// tell a short run from a truncated one.
#[test]
fn a_target_that_exits_ends_the_run_and_says_why() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run(&[
        "mem.rss",
        "-m",
        "poll@50ms",
        "-e",
        "timeline",
        "-d",
        "5s",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        "import time; time.sleep(0.2)",
    ]);
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let result = &json(&output)["results"][0];

    let observed_ms = result["observed_ns"].as_u64().expect("a duration") / 1_000_000;
    assert!(
        (150..1500).contains(&observed_ms),
        "the program lives about 200 ms after the interpreter starts, and the run observed \
         {observed_ms} ms of a five-second request"
    );
    let why = result["stopped_because"].as_str().expect("a reason");
    assert!(
        why.contains("exit"),
        "a run that ends early has to say that the target went, not merely that it ended: {why}"
    );
    assert_eq!(
        losses(result, "failed_reads"),
        0,
        "a process that dies between two reads is the end of the run, not a lost sample: \
         {result}"
    );
}

// ---------------------------------------------------------------------------
// D. The refusal
// ---------------------------------------------------------------------------

/// A request that cannot mean anything is rejected, and the reason is the
/// request.
///
/// Exit 2 is the code for a request no machine and no build could serve. Each
/// of these is a different way of asking for a number that does not exist.
#[test]
fn a_request_that_cannot_mean_anything_is_rejected_with_the_reason() {
    let refused = run_written_session(DRAM_THROUGHPUT_FOR_ONE_PROCESS);
    assert_eq!(code(&refused), EXIT_REJECTED, "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("package"),
        "the uncore is package-scoped, and the refusal has to say so rather than mention a \
         privilege: {}",
        stderr(&refused)
    );

    let cases: [(&[&str], &str); 4] = [
        (
            &["mem.rss", "-m", "count", "-e", "summary", "--pid", "1"],
            "poll",
        ),
        (
            &["mem.wss", "-m", "snapshot", "-e", "summary", "--pid", "1"],
            "poll",
        ),
        (
            &["mem.thp", "--pid", "1", "-m", "poll@50ms", "-e", "summary"],
            "system",
        ),
        (
            &["mem.rss", "--system", "-m", "poll@50ms", "-e", "summary"],
            "system",
        ),
    ];
    for (args, hint) in cases {
        let output = run(args);
        assert_eq!(
            code(&output),
            EXIT_REJECTED,
            "`{args:?}` names something that does not exist: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(hint),
            "the refusal of `{args:?}` has to point at what would work, and `{hint}` is not in \
             it: {}",
            stderr(&output)
        );
    }
}

/// A machine that cannot serve a request says so without blaming the build.
///
/// `mem.cache_llc`'s first backend names the last-level cache directly, and AMD
/// family 17h publishes no descriptor for it -- which the catalog says. Left
/// alone the resolver falls through to the architectural counter and the run
/// succeeds; pinning the first backend is what reaches the machine's limit, and
/// the limit is a property of the hardware rather than of this build.
#[test]
fn a_machine_that_cannot_serve_a_backend_says_so_in_its_own_terms() {
    let output = run_example("mem-refused-unavailable.yml");
    match code(&output) {
        0 => eprintln!(
            "skipped: this cpu exposes a last-level cache descriptor, so the pinned backend runs \
             and there is no machine limit here to check"
        ),
        EXIT_NO_VIABLE_BACKEND => {
            let message = stderr(&output);
            assert!(
                message.contains("perf_event_paranoid") || message.contains("cpu"),
                "the message has to send the reader at the machine -- a paranoia setting or a \
                 counter the cpu does not have: {message}"
            );
            assert!(
                !message.contains("no collector is written"),
                "a counter the hardware lacks is not a collector this build is missing: {message}"
            );
        }
        other => panic!(
            "pinning a backend the hardware may not have is either a measurement or exit \
             {EXIT_NO_VIABLE_BACKEND}, not exit {other}: {}",
            stderr(&output)
        ),
    }
}

/// A source this build does not collect exits 3, and says what is missing.
///
/// Exit 1 would send a reader to their kernel configuration for an afternoon
/// over a mechanism nobody has written. The source has to be one this machine
/// can genuinely serve, or the honest answer is exit 1 and this scenario is
/// asserting the machine rather than the build. `mem.latency` is that source on
/// any x86: it declares PEBS and IBS, one of which every such cpu has, and
/// neither has a collector.
///
/// It used to be `mem.reclaim`, which stopped qualifying twice over: first when
/// the `tracepoint:` probe became honest, because it declared only the classic
/// attach whose event id lives in a root-only file, and then for good when the
/// sixth wave wrote its collector. The session file records both.
#[test]
fn a_source_this_build_does_not_collect_exits_three_and_says_what_is_missing() {
    let output = run_example("mem-refused-not-built.yml");
    assert_eq!(code(&output), EXIT_NO_COLLECTOR, "{}", stderr(&output));
    let message = stderr(&output);
    assert!(
        message.contains("mem.latency") && message.contains("pmu:"),
        "the refusal has to name the source and the backend that is missing: {message}"
    );
    assert!(
        message.contains("--dump-request"),
        "a valid request that nothing collects should say how to see what it would resolve to: \
         {message}"
    );

    // And the request really is valid: the validator, which consults the
    // catalog and not the collectors, passes it.
    let validated = run(&["validate", "-f", &example("mem-refused-not-built.yml")]);
    assert_eq!(
        code(&validated),
        0,
        "`omn validate` checks the request against the catalog, where `mem.latency` is a source \
         like any other: {}",
        stderr(&validated)
    );
}

/// The three refusals are never confused with each other.
///
/// One command line per code, asked of the same family in the same window, so
/// that the three answers can be read side by side. A refusal that sends a
/// reader to the wrong sysctl, the wrong capability or the wrong flag costs
/// more than no message at all.
#[test]
fn the_three_refusals_are_told_apart() {
    let rejected = run(&["mem.bandwidth", "--pid", "1", "-m", "count", "-e", "rate"]);
    // `mem.sharing` rather than one of the tracefs-bound sources: the three
    // refusals can only be told apart where the machine is not itself a fourth
    // answer, and its PEBS-or-IBS chain is served by every x86 cpu and written
    // for none.
    let not_built = run(&[
        "mem.sharing",
        "--system",
        "-m",
        "sample",
        "-e",
        "topk",
        "-d",
        "1s",
    ]);

    assert_eq!(code(&rejected), EXIT_REJECTED, "{}", stderr(&rejected));
    assert_eq!(
        code(&not_built),
        EXIT_NO_COLLECTOR,
        "{}",
        stderr(&not_built)
    );

    assert!(
        !stderr(&rejected).contains("no collector"),
        "a request that cannot mean anything is not a collector nobody wrote: {}",
        stderr(&rejected)
    );
    assert!(
        !stderr(&not_built).contains("cannot be measured"),
        "a source nothing collects is not an invalid request: {}",
        stderr(&not_built)
    );
}

/// `mem.heap.*` with a pid: the right code, and a sentence that came apart.
///
/// The preloaded shim reaches a program only at exec, so a running process
/// cannot be given one; the catalog declares `pid: native` because the `usdt`
/// and `uprobe` backends would serve it, and neither is written. Exit 3 is
/// therefore the honest answer. The message is not.
///
/// See "Defects found", D4.
#[test]
fn asking_the_shim_for_a_running_process_is_refused_by_the_build() {
    let output = run(&[
        "mem.heap.live",
        "--pid",
        "1",
        "-m",
        "poll@50ms",
        "-e",
        "summary",
    ]);
    assert_eq!(
        code(&output),
        EXIT_NO_COLLECTOR,
        "the shim cannot be handed to a running process and the two backends that could are not \
         written: {}",
        stderr(&output)
    );
    let message = stderr(&output);
    assert!(
        message.contains("exec"),
        "the refusal has to say why a running process cannot be served: {message}"
    );

    // D4: the reason is spliced into a template that ends in " target yet".
    assert!(
        message.contains("running process) target yet"),
        "\nthe refusal for `mem.heap.live --pid` used to read \"... rather than an already \
         running process) target yet\", a whole sentence pushed into the `{{target}}` slot of \
         `\u{0060}{{source_id}}\u{0060} cannot be collected for a {{target}} target yet` (D4 in \
         docs/scenarios/mem.md).\n\
         If it now reads as one sentence, delete this assertion and D4.\n\
         {message}\n"
    );
}

/// A sampling request that lands on a backend that cannot sample says so.
///
/// `mem.numa` declares two modes. The first backend samples memory accesses
/// through PEBS and is not written; the second reads `numa_maps`, which is a
/// file, and a file has no sampling rate. The resolver falls through to it for
/// a `sample@997hz` request, and the run used to succeed, reporting a single
/// read of page placement as though it were a second of sampling -- D7 of
/// `docs/scenarios/mem.md`, and the reason it mattered more than a missing
/// number is that the number it produced was *plausible*.
///
/// The runner names it now. A request in `sample` or `trace` mode whose
/// collector declares no cadence of its own is being read at a poll interval
/// nobody asked for, and that is a degradation of the request rather than of
/// the measurement: it travels with the results like every other one.
///
/// The reproduction is the session that found it; what it asserts is the other
/// answer. Either honest answer passes -- a run that samples, or a run that
/// says which part of the request it could not honour.
#[test]
fn a_sampling_request_that_no_backend_can_sample_is_not_answered_with_a_poll() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run(&[
        "mem.numa",
        "-m",
        "sample@997hz",
        "-e",
        "topk",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        "import time; block = bytearray(32 << 20); block[::4096] = b'A' * len(block[::4096]); time.sleep(0.5)",
    ]);
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    if code(&output) != 0 {
        // The honest answers are exit 3 (no backend here can sample) or a run
        // that says it degraded the rate. Either is a fix.
        assert_eq!(
            code(&output),
            EXIT_NO_COLLECTOR,
            "a sampling request nothing can sample is refused, not failed: {}",
            stderr(&output)
        );
        return;
    }

    let result = &json(&output)["results"][0];
    let events = result["events_seen"].as_u64().expect("a count");
    let observed_ns = result["observed_ns"].as_u64().expect("a duration");
    let asked_for = 997 * observed_ns / 1_000_000_000;
    let degradations = result["degradations"]
        .as_array()
        .map(|list| list.iter().filter_map(|d| d.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    let says_so = degradations
        .iter()
        .any(|note| note.contains("has no sampling of its own"));

    // Either the rate was served, or the run said it was not. What is refused
    // is the third answer: three orders of magnitude fewer readings than were
    // asked for, reported as a clean run.
    assert!(
        events * 100 >= asked_for || says_so,
        "\n`mem.numa -m sample@997hz` resolved onto the poll-only `fs:proc` backend and \
         answered with {events} events over {observed_ns} ns, where {asked_for} were asked \
         for, and said nothing about the rate it could not serve. That is D7 of \
         docs/scenarios/mem.md.\n\
         degradations {degradations:?}\n"
    );
    if says_so {
        let note = degradations
            .iter()
            .find(|note| note.contains("has no sampling of its own"))
            .expect("just found");
        assert!(
            note.contains("sample@997hz") && note.contains("fs:proc"),
            "the degradation has to name the rate that was asked for and the backend that \
             could not serve it: {note}"
        );
    }
}

// ---------------------------------------------------------------------------
// The sessions themselves
// ---------------------------------------------------------------------------

/// Every session this family ships validates against the catalog.
///
/// `omn validate` consults the catalog and not the machine, so this is the one
/// check that means the same thing everywhere: a session that names an emit its
/// source does not declare, or a key it cannot carry, is broken on every
/// machine and not only on the ones that cannot run it.
///
/// The three sessions written to be refused are named here with the code they
/// are written to produce, because a refusal scenario that started passing has
/// stopped testing anything.
#[test]
fn every_mem_session_says_what_it_means() {
    let directory = format!("{}/examples", env!("CARGO_MANIFEST_DIR"));
    let mut seen = 0;
    for file in std::fs::read_dir(&directory).expect("examples/ is readable") {
        let path = file.expect("a directory entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        if !name.starts_with("mem-") || !name.ends_with(".yml") {
            continue;
        }
        seen += 1;
        let validated = run(&["validate", "-f", path.to_str().expect("a path")]);
        assert_eq!(
            code(&validated),
            0,
            "`{name}`: {}\n{}",
            stdout(&validated),
            stderr(&validated)
        );
    }
    assert!(
        seen >= 12,
        "this stream ships more than {seen} sessions; examples/mem-*.yml has lost some"
    );
}

// ---------------------------------------------------------------------------
// The four that go through the kernel
// ---------------------------------------------------------------------------
//
// `mem.page_alloc`, `mem.kmalloc`, `mem.reclaim` and `mem.compaction` count in
// a BPF hash map and hand up rows of a histogram rather than one event per
// measurement, which is a shape nothing else in this family has. Their four
// classes are below, and two of them need something the others do not: a
// workload running *beside* the session rather than launched by it. None of
// these sources offers a `launch` target -- an allocator is a property of the
// machine, and one run observes one target -- so the sessions watch everything
// and the attribution is checked on the events.

/// How long the sessions below are given before their workload starts.
///
/// The programs are loaded and attached in `Collector::start`, and an
/// allocation made before that happened is one the kernel never told anyone
/// about. Every ground truth here is a lower bound -- the tool must find *at
/// least* what the workload did -- so starting the workload early would fail
/// the assertion rather than weaken it, which is the right direction for a
/// margin to be wrong in.
const ATTACH_MARGIN: std::time::Duration = std::time::Duration::from_millis(700);

/// Mebibytes touched by the workload in `a_known_number_of_megabytes_...`.
const TOUCHED_MIB: u64 = 64;
/// Datagrams, and their size, in the same scenario.
const DATAGRAMS: u64 = 2000;
const DATAGRAM_BYTES: u64 = 2048;

/// A session run beside a workload, and how long the workload took.
///
/// The second half is not bookkeeping. These sessions have a fixed duration and
/// the workload has none, so on a loaded machine the workload can outlive the
/// window that was supposed to contain it -- and a measurement of the first half
/// of a workload, compared against the whole workload's number, fails in a way
/// that looks exactly like a collector losing events. The run says which it was.
struct Beside {
    output: Output,
    workload_ns: u64,
}

/// Held for the length of a session that drives the machine hard.
///
/// The three scenarios below fork thousands of processes, write a hundred
/// mebibytes through a squeezed cgroup and fault in sixty-four mebibytes of
/// transparent huge pages. `cargo test` runs the tests of one binary in
/// parallel threads of one process, so without this they run *at the same time
/// as each other* and as
/// `a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line`, whose ground truth
/// is a machine with cpu and contiguous memory to spare. That is the trap
/// `STATE.md` names: a measurement of a shared machine is not repeatable inside
/// a parallel suite.
///
/// One at a time is not a fix for that -- nothing makes a scenario immune to
/// the rest of the suite -- but it removes the largest avoidable part of it,
/// and it costs only wall time in a file that already spends ten seconds.
static ONE_HEAVY_SESSION_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs one of this stream's sessions while a workload drives the machine.
///
/// The session is started first and given [`ATTACH_MARGIN`] to load and attach
/// its programs; the workload then runs to completion, ideally inside the
/// session's own window. [`Beside::contained_the_workload`] is what says
/// whether it did.
fn run_example_beside(name: &str, workload: impl FnOnce()) -> Beside {
    // A poisoned lock means another of these panicked, which is a failure
    // being reported rather than a reason to fail differently here.
    let _one_at_a_time = ONE_HEAVY_SESSION_AT_A_TIME
        .lock()
        .unwrap_or_else(|held| held.into_inner());
    use std::process::Stdio;
    use std::time::Instant;

    let child = Command::new(BINARY)
        .args(["-f", &example(name)])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    std::thread::sleep(ATTACH_MARGIN);
    let started = Instant::now();
    workload();
    let workload_ns = started.elapsed().as_nanos() as u64;
    Beside {
        output: child.wait_with_output().expect("omn should finish"),
        workload_ns,
    }
}

impl Beside {
    /// Whether the session was still observing when the workload finished.
    ///
    /// The margin is counted in because the workload starts after it: a window
    /// of two seconds that spent seven hundred milliseconds waiting has one and
    /// a third left, and a workload that took longer than that was cut off.
    fn contained_the_workload(&self, result: &serde_json::Value) -> bool {
        let observed = result["observed_ns"].as_u64().unwrap_or(0);
        ATTACH_MARGIN.as_nanos() as u64 + self.workload_ns <= observed
    }

    /// What to print when it did not.
    fn cut_off(&self, result: &serde_json::Value) -> String {
        format!(
            "the workload took {:.1} ms and the session observed {:.1} ms of which the first \
             {:.0} ms were spent waiting for the programs to attach, so the window did not \
             contain the workload and its number cannot be checked against one that is whole",
            self.workload_ns as f64 / 1e6,
            result["observed_ns"].as_u64().unwrap_or(0) as f64 / 1e6,
            ATTACH_MARGIN.as_millis(),
        )
    }
}

/// Every `raw` event of a result, as (attributes by name).
fn raw_events(result: &serde_json::Value) -> Vec<BTreeMap<String, serde_json::Value>> {
    result["outputs"]
        .as_array()
        .expect("a result carries outputs")
        .iter()
        .filter(|output| output["emit"] == "raw")
        .flat_map(|output| {
            output["events"]
                .as_array()
                .expect("a raw output carries events")
                .clone()
        })
        .map(|event| {
            event["attrs"]
                .as_array()
                .expect("an event carries attributes")
                .iter()
                .map(|attr| {
                    (
                        attr["name"].as_str().expect("a name").to_owned(),
                        attr["value"].clone(),
                    )
                })
                .collect()
        })
        .collect()
}

/// Adds up one attribute over the rows whose `pid` is this one.
fn total_for_pid(result: &serde_json::Value, pid: u32, attribute: &str) -> u64 {
    raw_events(result)
        .iter()
        .filter(|attrs| {
            attrs.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(pid))
        })
        .filter_map(|attrs| attrs.get(attribute).and_then(serde_json::Value::as_u64))
        .sum()
}

/// Whether this machine refused for want of the capabilities every eBPF backend
/// here needs. That is a machine's limit and the only thing in this section
/// allowed to stop a test quietly.
fn unprivileged(output: &Output) -> bool {
    let message = stderr(output);
    message.contains("CAP_BPF") || message.contains("CAP_PERFMON")
}

// --------------------------------------------------- A. the capability sweep

#[test]
fn every_mode_and_emit_of_the_kernel_sources_is_served_or_refused_by_the_right_code() {
    let walked = sweep(&[
        "mem.page_alloc",
        "mem.kmalloc",
        "mem.reclaim",
        "mem.compaction",
    ]);
    assert_modes_and_emits_are_coherent(&walked);
    // Nothing here blames the machine. Every refusal these four raise is
    // marked: a mode or a target that cannot mean anything exits 2, a shape
    // this build has not written exits 3, and the machine is only ever blamed
    // for the capabilities, which are checked separately.
    assert_machine_blame_is_exactly(&walked, &[]);
}

/// The readable half of the sweep: five requests, one window, every emit these
/// four sources declare.
///
/// Two of them will render nothing, and that is the right answer rather than a
/// failure -- reclaim happens when a machine has no free memory and compaction
/// when it has no contiguous memory, and an idle machine has both. What is
/// checked here is that every shape was *produced*: a combination that resolves
/// and then renders nothing at all passes the sweep above and fails here.
#[test]
fn the_kernel_sweep_session_produces_every_shape_it_asks_for() {
    let output = run_example("mem-kernel-sweep.yml");
    if unprivileged(&output) {
        eprintln!(
            "skipped: this machine does not grant CAP_BPF and CAP_PERFMON, which every \
             kernel-side mem backend needs"
        );
        return;
    }
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let run = json(&output);

    let wanted: [(&str, &[&str]); 5] = [
        (
            "pages, every shape",
            &["hist", "topk", "rate", "raw", "summary"],
        ),
        ("pages, counted", &["rate", "raw", "summary"]),
        (
            "slab, every shape",
            &["hist", "topk", "rate", "raw", "summary"],
        ),
        (
            "reclaim, every shape",
            &["hist", "topk", "rate", "raw", "summary"],
        ),
        (
            "compaction, every shape",
            &["hist", "rate", "raw", "summary"],
        ),
    ];
    for (label, emits) in wanted {
        let result = result(&run, label);
        let produced: Vec<&str> = result["outputs"]
            .as_array()
            .expect("outputs")
            .iter()
            .map(|output| output["emit"].as_str().expect("an emit name"))
            .collect();
        assert_eq!(produced, emits, "`{label}` produced the wrong shapes");
    }

    // The two that always see something must have seen something, or the
    // session proves nothing about the shapes it rendered: a machine allocates
    // pages and slab objects continuously, and a run of half a second that saw
    // neither is a collector that is not attached.
    for label in ["pages, every shape", "slab, every shape"] {
        assert!(
            result(&run, label)["events_seen"]
                .as_u64()
                .expect("a count")
                > 0,
            "`{label}` saw no allocations at all in half a second, which no running machine does"
        );
    }
}

// ------------------------------------------------------- C. ground truth

/// One program, two numbers it chose, and the kernel's ledger of both.
///
/// The workload touches [`TOUCHED_MIB`] mebibytes one byte per page, and sends
/// [`DATAGRAMS`] datagrams of [`DATAGRAM_BYTES`] bytes through a unix socket
/// pair. Neither number is read off the tool:
///
/// * anonymous memory comes from the page allocator and from nowhere else, so a
///   process that made 64 MiB resident was handed at least 64 MiB of pages;
/// * a datagram smaller than a page is copied into one slab allocation of the
///   kernel's own choosing, so 2000 of 2048 bytes are at least 2000 allocations
///   and at least 4 MiB of slab.
///
/// Both are lower bounds, and both have an upper bound too, because a lower
/// bound alone is passed by a collector that reports a huge constant.
#[test]
fn a_known_number_of_megabytes_and_datagrams_is_found_in_the_allocators_ledger() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to drive the allocators");
        return;
    }
    let toucher = format!(
        "block = bytearray({TOUCHED_MIB} * 1024 * 1024)\n\
         for offset in range(0, len(block), 4096):\n    block[offset] = 1\n"
    );
    let sender = format!(
        "import socket\n\
         a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM)\n\
         payload = b'x' * {DATAGRAM_BYTES}\n\
         for _ in range({DATAGRAMS}):\n    a.send(payload)\n    b.recv({DATAGRAM_BYTES})\n"
    );

    let (mut touching, mut sending) = (None, None);
    let beside = run_example_beside("mem-allocator-ledger.yml", || {
        let mut one = Command::new("python3")
            .args(["-c", &toucher])
            .spawn()
            .expect("python3 should run");
        let mut two = Command::new("python3")
            .args(["-c", &sender])
            .spawn()
            .expect("python3 should run");
        touching = Some(one.id());
        sending = Some(two.id());
        one.wait().expect("the toucher should finish");
        two.wait().expect("the sender should finish");
    });
    let output = &beside.output;
    if unprivileged(output) {
        eprintln!("skipped: the allocator sources need CAP_BPF and CAP_PERFMON");
        return;
    }
    assert_eq!(code(output), 0, "{}", stderr(output));
    let run = json(output);
    if !beside.contained_the_workload(result(&run, "pages handed out")) {
        eprintln!(
            "skipped: {}",
            beside.cut_off(result(&run, "pages handed out"))
        );
        return;
    }

    let pages = total_for_pid(
        result(&run, "pages handed out"),
        touching.expect("a pid"),
        "total_pages",
    );
    let mib = pages * 4096 / (1024 * 1024);
    assert!(
        mib >= TOUCHED_MIB,
        "a program that made {TOUCHED_MIB} MiB resident must have been handed at least that \
         many pages, and the ledger has {mib} MiB ({pages} pages). If this is short, the \
         session's programs attached after the workload had already run"
    );
    assert!(
        mib <= TOUCHED_MIB + 32,
        "and not much more: {mib} MiB for a program that touched {TOUCHED_MIB} MiB is an \
         interpreter's worth of startup plus something unaccounted for"
    );

    let slab = result(&run, "slab bytes");
    let bytes = total_for_pid(slab, sending.expect("a pid"), "total_bytes");
    let asked = total_for_pid(slab, sending.expect("a pid"), "requested_bytes");
    assert!(
        bytes >= DATAGRAMS * DATAGRAM_BYTES,
        "{DATAGRAMS} datagrams of {DATAGRAM_BYTES} bytes are copied into at least {} bytes of \
         slab, and the ledger has {bytes}",
        DATAGRAMS * DATAGRAM_BYTES
    );
    assert!(
        bytes <= 8 * DATAGRAMS * DATAGRAM_BYTES,
        "and not eight times it: {bytes} bytes of slab for {DATAGRAMS} datagrams"
    );
    // The allocator never gives less than it was asked for, and the difference
    // is the rounding -- a number no other source in this tool can produce.
    assert!(
        asked <= bytes,
        "the slab handed out {bytes} bytes for {asked} asked for, which is the wrong way round"
    );

    // And the rows are a histogram of allocations, not of map entries: the
    // weight on each row is what makes a summary of them come out the right
    // height.
    let weighted: u64 = raw_events(result(&run, "pages handed out"))
        .iter()
        .filter_map(|attrs| attrs.get("weight").and_then(serde_json::Value::as_u64))
        .sum();
    assert!(
        weighted > 0,
        "every row has to carry the allocations it stands for, or a histogram of these events \
         is a histogram of the shape of a BPF map"
    );
}

/// Direct reclaim, against the kernel's own count of the pages it freed.
///
/// `examples/mem-reclaim-squeeze.sh` builds a cgroup with a 32 MiB limit,
/// writes 96 MiB of page cache through it, and reads that cgroup's own
/// `memory.stat` before and after. `pgsteal_direct` is the kernel counting the
/// pages reclaimed; `pages_reclaimed` is what this tool read off the `end`
/// tracepoint. Two counters, incremented by different code in different places,
/// that have to agree.
///
/// This is also the scenario that justifies attaching both halves of the
/// source. The kernel publishes global reclaim and cgroup-limit reclaim under
/// two names, the catalog's chain requires only the first pair, and every
/// episode below is the second -- a build that attached only what the entry
/// names would report zero here while the cgroup reclaimed ninety thousand
/// pages.
#[test]
fn a_cgroup_squeezed_until_it_reclaims_reports_what_the_kernel_counted() {
    let mut ground_truth = String::new();
    let beside = run_example_beside("mem-reclaim-squeeze.yml", || {
        let squeeze = Command::new("sh")
            .arg(example("mem-reclaim-squeeze.sh"))
            .output()
            .expect("sh should run");
        ground_truth = String::from_utf8_lossy(&squeeze.stderr).into_owned();
    });
    let output = &beside.output;
    if unprivileged(output) {
        eprintln!("skipped: mem.reclaim needs CAP_BPF and CAP_PERFMON");
        return;
    }
    if let Some(reason) = ground_truth.lines().find(|line| line.contains("skip")) {
        eprintln!("skipped: the workload could not create the shortage: {reason}");
        return;
    }
    assert_eq!(code(output), 0, "{}", stderr(output));

    let number = |name: &str| -> u64 {
        ground_truth
            .lines()
            .find_map(|line| line.strip_prefix(&format!("omn-scenario {name} ")))
            .unwrap_or_else(|| panic!("the workload prints `{name}`; it said:\n{ground_truth}"))
            .trim()
            .parse()
            .expect("a number")
    };
    let cgroup_id = number("cgroup_id");
    let stolen = number("pgsteal_direct");
    assert!(
        stolen > 0,
        "the workload wrote 96 MiB through a 32 MiB cgroup and the kernel reclaimed nothing; \
         there is no shortage here to measure"
    );

    let run = json(output);
    let result = result(&run, "reclaim");
    if !beside.contained_the_workload(result) {
        eprintln!("skipped: {}", beside.cut_off(result));
        return;
    }
    let mine: Vec<_> = raw_events(result)
        .into_iter()
        .filter(|attrs| {
            attrs.get("cgroup_id").and_then(serde_json::Value::as_u64) == Some(cgroup_id)
        })
        .collect();
    let sum = |attribute: &str| -> u64 {
        mine.iter()
            .filter_map(|attrs| attrs.get(attribute).and_then(serde_json::Value::as_u64))
            .sum()
    };
    let reclaimed = sum("pages_reclaimed");
    let episodes = sum("count");

    assert!(
        episodes > 0,
        "the cgroup reclaimed {stolen} pages and this run measured no episode of it at all. \
         The kernel fires `mm_vmscan_memcg_reclaim_*` for a cgroup that hits its own limit and \
         never `mm_vmscan_direct_reclaim_*`, so a build attached to one of the two pairs sees \
         exactly this"
    );
    // The tool may see a little more than the workload's own reading: the
    // script reads memory.stat before it exits, and the run drains again after
    // it has. It cannot see less: every page counted here was counted there.
    assert!(
        reclaimed >= stolen,
        "the kernel's own memory.stat counted {stolen} pages reclaimed and this run found \
         {reclaimed}; a measurement short of the counter it is read against is a measurement \
         that missed episodes"
    );
    assert!(
        reclaimed <= stolen + stolen / 20 + 1024,
        "and it cannot be far more: {reclaimed} pages against the kernel's {stolen}"
    );
    // Every one of them is the cgroup half of the source, which is the half the
    // catalog entry does not name.
    let scopes: BTreeSet<&str> = mine
        .iter()
        .filter_map(|attrs| attrs.get("scope").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(
        scopes,
        BTreeSet::from(["cgroup"]),
        "a cgroup reclaiming under its own limit is the `cgroup` scope, and nothing else"
    );
}

// ----------------------------------------------------------- D. the loss path

/// A kernel-side histogram driven past its capacity says what it could not
/// count.
///
/// The map holds 4096 rows between two drains, and a row is keyed by a task and
/// a size, so filling it takes tasks: `examples/mem-kernel-storm.sh` forks
/// about three thousand short-lived processes from four shells at once. When the
/// update fails there is nothing anywhere else that records the allocation --
/// the helper returns an error in the kernel and the event is gone -- so the
/// program counts it per cpu and the run reports it.
///
/// A machine that forks more slowly than this one may not reach the bound, and
/// that is a fact about the machine. It is skipped loudly, with the numbers.
#[test]
fn a_histogram_map_driven_past_its_capacity_says_what_it_could_not_count() {
    let beside = run_example_beside("mem-kernel-storm.yml", || {
        Command::new("sh")
            .arg(example("mem-kernel-storm.sh"))
            .output()
            .expect("sh should run");
    });
    let output = &beside.output;
    if unprivileged(output) {
        eprintln!("skipped: mem.kmalloc needs CAP_BPF and CAP_PERFMON");
        return;
    }
    assert_eq!(code(output), 0, "{}", stderr(output));
    let run = json(output);
    let result = result(&run, "slab under a fork storm");

    let seen = result["events_seen"].as_u64().expect("a count");
    let dropped = losses(result, "dropped");
    if dropped == 0 {
        eprintln!(
            "skipped: the storm produced {seen} rows over the run and never more than the \
             4096 the map holds in one drain, so the bound was not reached on this machine"
        );
        return;
    }

    // The loss reaches the output as a count and as a sentence, and the
    // sentence names the capacity -- a reader told only that something was
    // dropped cannot tell whether to change the request or the machine.
    let reasons = result["losses"]["reasons"]
        .as_object()
        .expect("a loss carries its reasons")
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        reasons.contains("4096"),
        "the reason has to name the capacity that was reached: {reasons}"
    );
    assert!(
        reasons.contains("never counted"),
        "and what became of the allocations behind it: {reasons}"
    );
    // A loss is not a corruption: what the run did see is still exactly what
    // the kernel counted, and it is not nothing.
    assert!(
        seen > 0,
        "a run that dropped {dropped} allocations still counted the rows it managed to create"
    );
}

// ------------------------------------------------------------ E. the refusals

/// The four kernel-side sources refuse for the right reason, with the right
/// code, in four different ways.
///
/// The table is the point. A request that cannot mean anything, a machine that
/// cannot serve a backend and a build that has not written one are three
/// different answers, and the cost of confusing them is a reader spending an
/// afternoon on their kernel configuration over a collector nobody wrote.
#[test]
fn the_kernel_sources_tell_their_three_refusals_apart() {
    // 2: the catalog declares no `pid` target for compaction, because
    // compaction is work done for a whole memory zone.
    let per_process = run(&["mem.compaction", "--pid", "1", "-m", "trace", "-e", "raw"]);
    assert_eq!(
        code(&per_process),
        EXIT_REJECTED,
        "{}",
        stderr(&per_process)
    );
    assert!(
        stderr(&per_process).contains("mem.compaction") && stderr(&per_process).contains("pid"),
        "{}",
        stderr(&per_process)
    );

    // 2: a polled distribution of an instant. The mode is not one the entry
    // declares, and the refusal says which modes are.
    let polled = run(&[
        "mem.reclaim",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "timeline",
        "-d",
        "200ms",
    ]);
    assert_eq!(code(&polled), EXIT_REJECTED, "{}", stderr(&polled));
    assert!(
        stderr(&polled).contains("trace") && stderr(&polled).contains("count"),
        "a refused mode has to name the ones that would work: {}",
        stderr(&polled)
    );

    // 3: the classic tracepoint backend, which this machine can reach only
    // through tracefs and which no collector serves either way. Forcing it is
    // the backend comparison these four have -- each has exactly one
    // implemented backend, so the comparison is with the refusal.
    for source in [
        "mem.page_alloc",
        "mem.kmalloc",
        "mem.reclaim",
        "mem.compaction",
    ] {
        assert_eq!(
            implemented_backends(source),
            vec!["bpf:raw_tp".to_owned()],
            "{source} has one implemented backend, so its comparison is with the refusal"
        );
        assert!(
            declared_backends(source).contains(&"bpf:tp".to_owned()),
            "{source} declares the classic tracepoint backend"
        );
        let forced = run(&[
            source,
            "--system",
            "-m",
            "trace",
            "-e",
            "summary",
            "-d",
            "200ms",
            "--backend",
            "force:bpf:tp",
        ]);
        assert_eq!(
            code(&forced),
            EXIT_NO_COLLECTOR,
            "forcing an unimplemented backend is this build's gap, not this machine's: {}",
            stderr(&forced)
        );
        assert!(
            stderr(&forced).contains(source) && stderr(&forced).contains("bpf:tp"),
            "the refusal names the source and the backend: {}",
            stderr(&forced)
        );
    }

    // 3: a key the catalog offers and an aggregating collector cannot carry.
    // The identity of one allocation is exactly what counting in the kernel
    // discards, and answering with an unkeyed series would put every task's
    // allocations in one anonymous heap.
    let keyed = run(&[
        "mem.kmalloc",
        "--system",
        "-m",
        "trace",
        "-e",
        "hist",
        "-k",
        "mem",
        "-d",
        "200ms",
    ]);
    if !unprivileged(&keyed) {
        assert_eq!(code(&keyed), EXIT_NO_COLLECTOR, "{}", stderr(&keyed));
        assert!(
            stderr(&keyed).contains("mem:[base,len)") && stderr(&keyed).contains("task"),
            "the refusal names the key it cannot carry and the one to use: {}",
            stderr(&keyed)
        );
    }
}

/// Compaction, which this machine will not perform on request.
///
/// `mem.compaction` is the one collector in this family that is written here
/// and exercised elsewhere, and saying so is the point of this test rather than
/// an excuse for it. Compaction happens when the allocator wants a contiguous
/// block and the free pages are scattered; a developer machine with free memory
/// never needs to, and the two ways to make it are both out of reach:
/// `/proc/sys/vm/compact_memory` is mode 0200 and root-only, and fragmenting a
/// shared machine's memory badly enough that a huge-page fault has to compact
/// is not something a test may do to a machine it does not own. Measured here:
/// three gibibytes of transparent huge pages faulted in, and `compact_stall`
/// did not move.
///
/// So the check is conditional on the kernel's own counters, and it is a real
/// check wherever they move: `/proc/vmstat` says whether compaction ran in the
/// window, and if it did, this source has to have seen it. Where it did not,
/// the test skips with the numbers that show why -- which is the honest form of
/// "the property could not be created here", and is worth more than a threshold
/// that always passes.
#[test]
fn compaction_is_seen_where_the_kernel_says_it_happened_and_skipped_loudly_where_it_did_not() {
    fn vmstat(name: &str) -> u64 {
        std::fs::read_to_string("/proc/vmstat")
            .unwrap_or_default()
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} ")))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0)
    }
    // Three counters rather than one. `compact_stall` counts only the direct
    // case -- a thread that had to compact inside its own allocation -- and
    // `kcompactd` fires the same tracepoint from a kernel thread, which this
    // source sees and that counter does not.
    // Three counters rather than one. `compact_stall` counts only the direct
    // case -- a thread that had to compact inside its own allocation -- and
    // `kcompactd` fires the same tracepoint from a kernel thread, which this
    // source sees and that counter does not.
    let counters = [
        "compact_stall",
        "compact_daemon_wake",
        "compact_migrate_scanned",
    ];
    // Read around the workload rather than around the test, because the
    // session's window is what the source could have seen: a compaction that
    // happened while the programs were still loading is in the counters and in
    // nothing else, and comparing the two would fail for the wrong reason.
    let (mut before, mut after) = (Vec::new(), Vec::new());
    // Read around the session's own window and nothing else, because that
    // window is what the source could have seen: a compaction that happened
    // while the programs were still loading is in the counters and in nothing
    // else, and comparing the two would fail for the wrong reason.
    //
    // **And no workload.** An earlier version faulted in a quarter of a
    // gibibyte of transparent huge pages to try to provoke compaction. It never
    // provoked any -- this machine has free contiguous memory and simply does
    // not need to compact -- and it took the huge pages that
    // `a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line` needs for its own
    // scan, which then measured a gibibyte walked in four-kibibyte pages and
    // failed beside it. A workload that cannot create the property and can
    // break the neighbours is worse than no workload: what is left is a real
    // check on any machine that does compact, and an honest skip here.
    let beside = run_example_beside("mem-compaction-watch.yml", || {
        before = counters.iter().map(|name| vmstat(name)).collect();
        std::thread::sleep(std::time::Duration::from_secs(2));
        after = counters.iter().map(|name| vmstat(name)).collect();
    });
    let output = &beside.output;
    if unprivileged(output) {
        eprintln!("skipped: mem.compaction needs CAP_BPF and CAP_PERFMON");
        return;
    }
    assert_eq!(code(output), 0, "{}", stderr(output));

    let run = json(output);
    let result = result(&run, "compaction");
    if !beside.contained_the_workload(result) {
        eprintln!("skipped: {}", beside.cut_off(result));
        return;
    }

    let moved: Vec<String> = counters
        .iter()
        .zip(before.iter().zip(after.iter()))
        .filter(|(_, (before, after))| after > before)
        .map(|(name, (before, after))| format!("{name} {before} -> {after}"))
        .collect();
    let episodes: u64 = raw_events(result)
        .iter()
        .filter_map(|attrs| attrs.get("count").and_then(serde_json::Value::as_u64))
        .sum();

    if moved.is_empty() {
        let writable = std::fs::OpenOptions::new()
            .write(true)
            .open("/proc/sys/vm/compact_memory")
            .is_ok();
        eprintln!(
            "skipped: this machine did not compact. None of {counters:?} moved over the \
             session's window, so there was nothing for the source to see, and \
             `/proc/sys/vm/compact_memory` is {}, which is the other way to ask and needs \
             root. `mem.compaction` attached and reported zero, which is the right answer \
             and not a measurement",
            if writable { "writable" } else { "not writable" }
        );
        assert_eq!(
            episodes, 0,
            "the kernel's counters say compaction did not run, and this source reported \
             {episodes} episodes of it"
        );
        return;
    }

    assert!(
        episodes > 0,
        "the kernel says compaction ran inside this session's window ({}) and \
         `mem.compaction` saw none of it",
        moved.join(", ")
    );
}
