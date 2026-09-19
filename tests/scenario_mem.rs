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
    assert_machine_blame_is_exactly(
        &walked,
        &[
            "mem.cache_l1 sample raw",
            "mem.cache_l1 sample stack",
            "mem.cache_l1 sample topk",
            "mem.cache_llc sample raw",
            "mem.cache_llc sample stack",
            "mem.cache_llc sample topk",
            "mem.cache_tlb sample raw",
            "mem.cache_tlb sample stack",
            "mem.cache_tlb sample topk",
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
    assert_machine_blame_is_exactly(
        &walked,
        &[
            "mem.heap.live snapshot raw",
            "mem.heap.live snapshot summary",
            "mem.heap.retained snapshot raw",
            "mem.heap.retained snapshot summary",
        ],
    );
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

    assert_eq!(
        unreachable,
        vec![
            "mem.sharing declares `summary`".to_owned(),
            "mem.swap declares `rate`".to_owned(),
        ],
        "\nthe set of `mem` emits that no declared mode can produce has changed.\n\
         Each of these is a promise in catalog/mem.yaml that no build can keep: the source \
         declares the emit, and every mode it declares refuses it with exit {EXIT_REJECTED}.\n\
         Removing one from the catalog -- or giving the source a mode that can produce it -- \
         is the fix; delete it here and from docs/scenarios/mem.md under D3 when that happens.\n"
    );
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
/// The `statm` series additionally contains a value that is not a measurement.
/// See "Defects found", D1.
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

    // D1: `statm` publishes one more point than the rollup, and it is a zero
    // for a process that has already exited.
    let (rollup_points, ..) = summary(result(&run, "rss via the rollup"));
    let (statm_points, statm_low, _, statm_last) = summary(result(&run, "rss via statm"));
    assert_eq!(
        (statm_points, statm_low, statm_last),
        (rollup_points + 1, 0.0, 0.0),
        "\n`mem.rss` through `fs:proc:statm` used to publish one value more than the rollup, \
         and that value was a fabricated 0 B for a process that had already exited (D1 in \
         docs/scenarios/mem.md).\n\
         If it now stops where the rollup stops, the defect is fixed: replace this with the \
         assertion below it, which is what the scenario is actually about.\n"
    );
    assert!(
        statm_low == 0.0,
        "a live process is never 0 bytes resident; when D1 is fixed this becomes the check"
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
/// A third field disagrees and nothing declares it. See "Defects found", D6.
#[test]
fn both_huge_page_backends_read_the_same_promotion_counter() {
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

    // D6: a field both carry, that neither the catalog nor the event explains.
    let (theirs, ours) = (
        sysfs["resident_huge_pages"].as_u64().expect("a count"),
        vmstat["resident_huge_pages"].as_u64().expect("a count"),
    );
    assert!(
        theirs >= ours,
        "\n`fs:sys` reads `nr_anon` out of the per-size statistics and `fs:proc` reads \
         `nr_anon_transparent_hugepages` out of /proc/vmstat, and on this machine the first is \
         consistently the larger (D6 in docs/scenarios/mem.md).\n\
         If the two now agree, one of them was corrected and this becomes an equality.\n\
         {theirs} against {ours}\n"
    );
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
/// room. So seen plus dropped has to come to the number the workload chose.
///
/// It does not. See "Defects found", D2.
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

    let accounted = seen + dropped;
    assert!(
        accounted < CHOSEN,
        "\nseen plus dropped now comes to {accounted} against the {CHOSEN} allocations the \
         workload made, which is the arithmetic this scenario is about and which used to be \
         short by tens of thousands (D2 in docs/scenarios/mem.md).\n\
         If the shim's drop accounting has been fixed, replace this with `assert_eq!(accounted, \
         CHOSEN)` and delete D2 from the document.\n\
         seen {seen}, dropped {dropped}\n"
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
        reasons.iter().any(|reason| reason.contains("longer than")),
        "a late tick has to say how long the round took and how long it had: {reasons:?}"
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
/// over a mechanism nobody has written. `mem.reclaim` is valid, this machine
/// has the tracepoints and the capability for it, and `src/bpf/` attaches only
/// one way.
#[test]
fn a_source_this_build_does_not_collect_exits_three_and_says_what_is_missing() {
    let output = run_example("mem-refused-not-built.yml");
    assert_eq!(code(&output), EXIT_NO_COLLECTOR, "{}", stderr(&output));
    let message = stderr(&output);
    assert!(
        message.contains("mem.reclaim") && message.contains("bpf:tp"),
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
        "`omn validate` checks the request against the catalog, where `mem.reclaim` is a source \
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
    let not_built = run(&[
        "mem.compaction",
        "--system",
        "-m",
        "trace",
        "-e",
        "hist",
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

/// A sampling request that lands on a backend that cannot sample.
///
/// `mem.numa` declares two modes. The first backend samples memory accesses
/// through PEBS and is not written; the second reads `numa_maps`, which is a
/// file, and a file has no sampling rate. The resolver falls through to it for
/// a `sample@997hz` request and the run succeeds, reporting a single read of
/// page placement as though it were a second of sampling.
///
/// See "Defects found", D7.
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
        .any(|note| note.contains("sample") || note.contains("rate") || note.contains("poll"));

    assert!(
        events * 100 < asked_for && !says_so,
        "\n`mem.numa -m sample@997hz` used to resolve onto the poll-only `fs:proc` backend and \
         return a single read while reporting success, with nothing in the output about the \
         sampling rate it did not serve (D7 in docs/scenarios/mem.md).\n\
         If the run now either samples, or says which part of the request it could not honour, \
         delete this and D7.\n\
         {events} events over {observed_ns} ns, where {asked_for} were asked for; degradations \
         {degradations:?}\n"
    );
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
