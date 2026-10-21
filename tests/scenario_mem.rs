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

/// What this machine lacks for `backend` of `source`, as `omn list` probes it
/// -- the requirements it calls missing -- or `None` when nothing it needs is
/// missing here. A pinned backend is refused for the machine before the build
/// (`DECISIONS.md`, "one order for unbuilt and missing"), so which refusal a
/// pin earns is this answer's.
fn lacks_here(source: &str, backend: &str) -> Option<Vec<String>> {
    let listing = run(&["list", source, "--format", "json"]);
    assert!(listing.status.success(), "{}", stderr(&listing));
    let entries = json(&listing);
    let probed = entries[0]["probes"]
        .as_array()
        .expect("the listing carries each backend's probes")
        .iter()
        .find(|probed| probed["backend"] == backend)
        .unwrap_or_else(|| panic!("`{source}` lists no backend `{backend}`"));
    let missing: Vec<String> = probed["requirements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|requirement| requirement["probe"]["state"] == "missing")
        .filter_map(|requirement| requirement["needs"].as_str().map(str::to_owned))
        .collect();
    (!missing.is_empty()).then_some(missing)
}

/// Asserts the refusal a pinned backend this build has no collector for
/// earns here: the machine's (exit 1) where it lacks something the backend
/// needs, as the walk says it, and the build's (exit 3) where it lacks nothing.
fn assert_refused_in_the_chains_order(source: &str, backend: &str, output: &Output) {
    let message = stderr(output);
    assert!(
        message.contains(backend) && message.contains(source),
        "the refusal has to name the source and the backend: {message}"
    );
    match lacks_here(source, backend) {
        Some(lacks) => {
            assert_eq!(
                code(output),
                EXIT_NO_VIABLE_BACKEND,
                "`{source}` pinned to `{backend}`, which lacks {lacks:?} here, is the machine's \
                 refusal before the build's: {message}"
            );
            assert!(
                lacks
                    .iter()
                    .all(|needs| message.contains(&format!("{needs}: missing"))),
                "the machine's refusal names what it lacks: {message}"
            );
            assert!(
                !message.contains("no collector is written"),
                "a machine that cannot serve the backend is not sent after its collector: \
                 {message}"
            );
        }
        None => assert_eq!(
            code(output),
            EXIT_NO_COLLECTOR,
            "`{source}` pinned to `{backend}`, which nothing implements and this machine lacks \
             nothing for, has to say so with exit {EXIT_NO_COLLECTOR}: {message}"
        ),
    }
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
        !message.contains(SHIM_MISSING),
        "the heap sources need the preload shim: build with `make`, not `cargo build`\n{message}"
    );
    // A run that succeeded was served, whatever its caveats say: the
    // `mem.faults` caveat names `perf_event_paranoid` on every run, and until
    // the sixteenth wave that word alone turned a served run into a skip, so
    // `the_tool_finds_the_faults_the_program_counted_itself` never compared
    // anything.
    if output.status.success() {
        return None;
    }
    // The skip line starts at the refusal: a launch's warnings come first on
    // stderr (`json` is one document...), and the CI's skip gate reads the
    // first line as the reason.
    // A line that starts `error:`, not the word anywhere: a warning or a
    // caveat that quotes it would otherwise start the skip line mid-sentence.
    let from_the_error = message
        .match_indices("error:")
        .find(|(at, _)| *at == 0 || message[..*at].ends_with('\n'))
        .map_or(message.as_str(), |(at, _)| &message[at..]);
    // The resolver's own word for a machine's limit is the widest of these:
    // the CI's second runner has no hardware PMU, and `mem.cache_l1` was
    // refused there with "`L1-dcache-load-misses` is not available on this
    // cpu" under it.
    let withheld = from_the_error.contains("not readable")
        || from_the_error.contains("perf_event_paranoid")
        || from_the_error.contains("no such file")
        || from_the_error.contains("has no viable backend on this machine");
    withheld.then(|| from_the_error.to_owned())
}

/// The words the tool uses when the shim is not where it looks
/// (`collect::mem::heap::shim_in`: *"the preload shim was not found at ..."*).
/// Until the sixteenth wave the check read "heap shim was not found", which the
/// tool has not printed since the ninth, so a missing shim read as whatever
/// failure came after it.
const SHIM_MISSING: &str = "shim was not found";

/// Runs a copy of the binary from a directory that holds no shim, beside it or
/// in `../lib/omn/`, with `OMN_PRELOAD_LIB` unset: a build that was not run
/// through `make`, made on purpose.
fn run_without_the_shim(args: &[&str]) -> Output {
    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("no-shim-{}", std::process::id()))
        .join("bin");
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let copy = directory.join("omn");
    std::fs::copy(BINARY, &copy).expect("copying the binary");
    // Ground truth, outside the tool: neither place it looks holds a shim.
    for place in ["libomn_preload.so", "../lib/omn/libomn_preload.so"] {
        assert!(
            !directory.join(place).exists(),
            "{} exists",
            directory.join(place).display()
        );
    }
    // `ETXTBSY` is retried: another test thread that forks while the copy
    // is still open for writing holds that descriptor until its child execs,
    // and the kernel refuses to run a file someone may still be writing. The
    // descriptor is close-on-exec, so the refusal ends within a fork-exec of
    // another test; failed once in this file's parallel run.
    let mut busy = 0;
    let output = loop {
        match Command::new(&copy)
            .args(args)
            .env_remove("OMN_PRELOAD_LIB")
            .output()
        {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && busy < 100 => {
                busy += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => break other.expect("the copy should run"),
        }
    };
    let _ = std::fs::remove_dir_all(directory.parent().expect("the scratch's parent"));
    output
}

/// A missing shim fails the heap tests rather than skipping them, and the
/// check that says so matches what the tool prints.
#[test]
fn a_binary_without_its_shim_fails_the_heap_tests_rather_than_skipping_them() {
    let output = run_without_the_shim(&["mem.heap.alloc", "-e", "summary", "--", "true"]);
    assert_ne!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(SHIM_MISSING),
        "the tool's words for a missing shim changed: {}",
        stderr(&output)
    );
    let caught = std::panic::catch_unwind(|| unavailable(&output));
    assert!(
        caught.is_err(),
        "a missing shim was read as a machine that cannot serve the request"
    );
}

/// Whether the run says it sampled page faults in user mode only: the
/// `perf_event_paranoid` 2 path, where the faults the kernel takes on the
/// program's behalf are not in the samples (the source's note, in the
/// result's `notes`).
fn sampled_in_user_mode_only(result: &serde_json::Value) -> bool {
    result["notes"].as_array().is_some_and(|notes| {
        notes
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|note| note.contains("page faults in user mode only"))
    })
}

/// Whether a counted run says it counted page faults in user mode only: the
/// same `perf_event_paranoid` 2 path as [`sampled_in_user_mode_only`], taken by
/// the counting group (`CounterCollector::start`), which words its note after
/// the source rather than after page faults.
fn counted_in_user_mode_only(result: &serde_json::Value) -> bool {
    result["notes"].as_array().is_some_and(|notes| {
        notes
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|note| {
                note.contains("would only open `mem.faults` for user mode")
                    || note.contains("page faults in user mode only")
            })
    })
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
    /// Whether the refusal names the capabilities the eBPF and per-cpu perf
    /// backends need, anywhere in it: the first `error:` line says only that
    /// no backend is viable, and the lines after it say why.
    for_want_of_capabilities: bool,
    /// Whether the resolver itself refused it -- *"has no viable backend on
    /// this machine"* -- before anything was launched: every backend's probe
    /// answered missing, which is the machine's answer by the resolver's own
    /// classification rather than a build's gap that exits 1.
    withheld_by_the_resolver: bool,
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
                // The user's locked-memory budget for sampling rings is shared
                // by every perf session the user holds, so the scenarios
                // sampling beside this one can leave none for a moment: eight
                // `omn mem.faults` started together were five refused
                // (2026-10-21). That refusal is the moment's, not the
                // combination's, and is asked again before it counts.
                // A scenario profiling the whole machine holds its rings for
                // seconds, so the question is asked for up to fifteen.
                let mut output = run(&borrowed);
                let asked = std::time::Instant::now();
                while stderr(&output).contains("did not fit what this user may still lock")
                    && asked.elapsed() < std::time::Duration::from_secs(15)
                {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    output = run(&borrowed);
                }
                let reason = stderr(&output)
                    .lines()
                    .find(|line| line.starts_with("error:"))
                    .unwrap_or_default()
                    .to_owned();
                let said = stderr(&output);
                walked.push(Combination {
                    source: entry.id.clone(),
                    mode: mode.clone(),
                    emit: emit.clone(),
                    exit: code(&output),
                    reason,
                    for_want_of_capabilities: said.contains("CAP_BPF")
                        || said.contains("CAP_PERFMON"),
                    withheld_by_the_resolver: said
                        .contains("has no viable backend on this machine"),
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
///
/// A combination the resolver refused is left out and printed: D5 is a
/// refusal raised *after* the resolver chose a backend, for a reason that is
/// the build's. The CI's runners -- no hardware PMU, `perf_event_paranoid` 2
/// and 4 -- refused every cache source and most of `mem.faults` at the
/// resolver, which is the machine answering, and the list read 31 there.
fn blamed_on_the_machine(walked: &[Combination]) -> Vec<String> {
    let withheld: Vec<String> = walked
        .iter()
        .filter(|combination| combination.withheld_by_the_resolver)
        .map(|combination| {
            format!(
                "{} {} {}",
                combination.source, combination.mode, combination.emit
            )
        })
        .collect();
    if !withheld.is_empty() {
        eprintln!(
            "note: {} combinations have no viable backend on this machine and are not asked \
             about here: {withheld:?}",
            withheld.len()
        );
    }
    let mut named: Vec<String> = walked
        .iter()
        .filter(|combination| !combination.withheld_by_the_resolver)
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
    // and exits 3. The three `mem.faults count` ones went in the eleventh wave:
    // the catalog declared the mode and the collector refused it after the
    // launch, and it is served now, by the kernel's page-fault counter.
    assert_machine_blame_is_exactly(&walked, &[]);
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
         only mode `sample`, and `mem.swap` declaring `rate` over `poll` and `snapshot`, which \
         `count` now serves.\n\
         What is left here is: {unreachable:?}\n"
    );

    // And the two that were removed are now refused by the source rather than
    // by the matrix, which is the message that names the line of YAML to look
    // at. Against the old catalog both of these said `mode ... cannot produce`.
    // `mem.swap` answers `rate` now, over the counted window its `count`
    // mode reports (open-requests 2619), and a poll still cannot.
    let counted = run(&[
        "mem.swap", "--system", "-m", "count", "-e", "rate", "-d", "100ms",
    ]);
    assert_eq!(code(&counted), 0, "{}", stderr(&counted));
    let refused = run(&[
        "mem.sharing",
        "--system",
        "-m",
        "sample@997hz",
        "-e",
        "summary",
        "-d",
        "100ms",
    ]);
    assert_eq!(code(&refused), EXIT_REJECTED, "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("`mem.sharing` cannot emit `summary`"),
        "`mem.sharing` should refuse `summary` as its own, not as the matrix's: {}",
        stderr(&refused)
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

    let document = json(&output);
    let faults = result(&document, "faults");
    let traced = faults["events_seen"].as_u64().expect("a count") as f64;
    // `ru_minflt` counts the faults the kernel took on the program's behalf
    // too, and a user-mode-only event cannot see them, so on that path the
    // bound is widened by their share -- measured at 17 of 1610 for this
    // workload with `perf stat -e page-faults:u,page-faults:k`, about 1 %;
    // five points are allowed for it. The kernel's count stays the ground
    // truth either way: what a user-mode event misses is the kernel's own
    // faults, not the program's.
    let user_mode_only = sampled_in_user_mode_only(faults);
    let bound = if user_mode_only { 0.15 } else { 0.10 };
    let shortfall = (kernels_own - traced) / kernels_own;
    assert!(
        (0.0..bound).contains(&shortfall),
        "the kernel counted {kernels_own} minor faults for this program and the run traced \
         {traced}{}: a shortfall of {:.1} %, over the {:.0} % allowed",
        if user_mode_only {
            " in user mode only"
        } else {
            ""
        },
        shortfall * 100.0,
        bound * 100.0
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
/// The TLB is read for the opposite reason. The same scan crosses a quarter of
/// a million four-kibibyte pages and five hundred and twelve two-mebibyte
/// ones, so it is run twice, once advised onto huge pages and once off them,
/// and the two counts have to separate; the program's own `AnonHugePages` --
/// not the machine's THP mode -- says which run had them.
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

    // The TLB half is a separation, not a threshold. A count asserted against
    // a fixed quarter of the small pages held on a quiet machine and failed in
    // this file's own parallel run with the whole buffer on huge pages
    // (131072 KiB of AnonHugePages, 72296 misses against a bound of 65536):
    // every time the scan is descheduled, whatever ran on that cpu evicted its
    // translations, and that cost is the machine's load, not the page size.
    // So the program scans the same 128 MiB twice, in two runs that differ in
    // one `madvise`, and each says how much of its memory the kernel gave it
    // on huge pages. The small-page scan crosses a new four-kibibyte
    // translation every 4096 bytes, 262144 times over eight passes, more than
    // any second-level TLB holds -- arithmetic, not a measurement -- and the
    // huge-page one must miss far less than that same run does. Measured
    // here at a load average of 96: 26749 to 30897 against 413344 to 448939.
    let mut scans = Vec::new();
    for advice in ["MADV_HUGEPAGE", "MADV_NOHUGEPAGE"] {
        let output = run_written_session(&tlb_scan_session(advice));
        if let Some(reason) = unavailable(&output) {
            eprintln!("skipped the TLB half: {reason}");
            return;
        }
        assert!(output.status.success(), "{advice}: {}", stderr(&output));
        let huge_kb = stderr(&output)
            .lines()
            .find_map(|line| line.strip_prefix("ANON_HUGE_KB=").map(str::to_owned))
            .and_then(|value| value.trim().parse::<f64>().ok());
        let Some(huge_kb) = huge_kb else {
            eprintln!(
                "skipped the TLB half: this kernel publishes no AnonHugePages in \
                 /proc/self/smaps_rollup, so which scan had huge pages is unknown"
            );
            return;
        };
        let tlb = attr_u64(result(&json(&output), "tlb"), "count") as f64;
        scans.push((advice, huge_kb, tlb));
    }
    let [(_, huge_kb, huge_tlb), (_, small_kb, small_tlb)] = scans[..] else {
        unreachable!("two scans were run");
    };
    const BUFFER_KB: f64 = 128.0 * 1024.0;
    if huge_kb < 0.75 * BUFFER_KB || small_kb > 0.25 * BUFFER_KB {
        eprintln!(
            "skipped the TLB half: the kernel gave the advised scan {huge_kb} KiB of huge pages \
             and the other {small_kb} KiB, of a {BUFFER_KB} KiB buffer each, so the two runs do \
             not differ in the one thing this compares (THP `never`, or no huge page free)"
        );
        return;
    }
    assert!(
        small_tlb >= SMALL_PAGES,
        "a scan of {BYTES_SCANNED} bytes on small pages crosses {SMALL_PAGES} four-kibibyte \
         translations, and the TLB counter saw only {small_tlb} misses"
    );
    assert!(
        huge_tlb < small_tlb / 4.0,
        "the same scan on huge pages ({huge_kb} KiB of them) missed the TLB {huge_tlb} times, \
         against {small_tlb} on small pages: huge pages are what the counter is meant to see"
    );
}

/// The scan of `a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line`, on a
/// mapping advised one way or the other, and the program's own count of its
/// huge pages printed when the scan is done.
fn tlb_scan_session(advice: &str) -> String {
    format!(
        "\
name: one gibibyte of reads, {advice}
format: json

requests:
  - label: tlb
    source: mem.cache_tlb
    target:
      launch:
        - python3
        - -c
        - \"import mmap, sys\\nsize = 128 << 20\\nm = mmap.mmap(-1, size, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)\\nm.madvise(mmap.{advice})\\nfor i in range(0, size, 4096): m[i] = 65\\nfor _ in range(8): m.find(b'Z')\\nhuge = [line.split()[1] for line in open('/proc/self/smaps_rollup') if line.startswith('AnonHugePages:')]\\nprint('ANON_HUGE_KB=' + (huge[0] if huge else '?'), file=sys.stderr)\\n\"
    mode: count
    emit: [raw]
"
    )
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
    // Both roads watch one process over one window, read one after the other
    // on each tick. The process can exit between the two reads of the last
    // tick -- more often on a loaded machine -- and then the road read second
    // finds a zombie and, rightly, publishes nothing: one reading fewer, never
    // more than one.
    assert!(
        statm_points.abs_diff(rollup_points) <= 1,
        "both roads watch one process over one window, so they end at most one reading \
         apart: {statm_points} against {rollup_points}"
    );
    // And the mean the self-benchmark compares is no longer dragged by it:
    // where both ended on the same tick, they end on the same last reading of
    // the same live process. One tick apart, the extra reading is of a
    // process tearing down, and the zero check above is what holds it.
    if statm_points == rollup_points {
        let apart = (statm_last - rollup_last).abs() / rollup_last;
        assert!(
            apart <= 0.01,
            "the last reading of one process through two files: {statm_last} against \
             {rollup_last}"
        );
    } else {
        eprintln!(
            "the process ended between the two roads' reads of one tick: {statm_points} \
             readings against {rollup_points}"
        );
    }
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
    // `/proc/meminfo` read either side of every round, so that each pair is
    // bracketed by the kernel's own level around that pair and nothing else:
    // the endpoints of the whole session were narrower than the level's
    // excursion inside it (the flaky tally's #21).
    let mut brackets = Vec::new();
    let mut folio_counts = Vec::new();
    for _ in 0..TRIES {
        let level_before = anon_huge_kilobytes();
        let at_one_instant = run_written_session(BOTH_HUGE_PAGE_BACKENDS_AT_ONCE);
        let level_after = anon_huge_kilobytes();
        brackets.push(level_before.zip(level_after));
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
        // Every folio, beside the whole pages: a PMD-mapped page and a
        // partially mapped one are both folios, and never the same one.
        let folios = sysfs
            .get("huge_folios")
            .and_then(serde_json::Value::as_u64)
            .expect("`fs:sys` carries the per-size statistics' own folio count");
        pairs.push((
            sysfs["resident_huge_pages"].as_u64().expect("a count"),
            vmstat["resident_huge_pages"].as_u64().expect("a count"),
            partial.expect("read a moment ago"),
        ));
        folio_counts.push(folios);
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
    let closest = (0..pairs.len())
        .min_by_key(|at| pairs[*at].0.abs_diff(pairs[*at].1))
        .expect("five runs");
    let (theirs, ours, partial_then) = pairs[closest];

    // The bound is the machine's own movement between rounds, measured above.
    // The gap was once exactly the partially-mapped count, and after the
    // fifteenth wave's subtraction it was the folios whose PMD had been split
    // into page-table entries -- 134 to 304 of them on this machine, the same
    // in every round, which no amount of drift explains.
    assert!(
        theirs.abs_diff(ours) <= drift,
        "\nnot one of {TRIES} pairs of reads of one kernel's huge pages, taken one round \
         apart through the two backends of one source, came within {drift} of each other \
         (how far this machine's own count moved between rounds): {pairs:?}. A gap the same \
         in every round is a bias: the two backends are answering different questions.\n"
    );
    let folios = folio_counts[closest];
    assert!(
        folios + drift >= theirs + partial_then,
        "{folios} huge folios cannot hold {theirs} mapped whole and {partial_then} partially \
         mapped: those are two disjoint kinds of folio"
    );

    // Ground truth, and the reason this is not two numbers agreeing on a
    // mistake: `/proc/meminfo` publishes the same count, and it is read here
    // rather than taken from the tool. It is a level and the machine is not
    // idle while the suite runs, so it is read either side of the session and
    // what moved in between is carried as the slack -- the pattern the PSI
    // pair above uses for the same reason.
    let page_size = sysfs["hpage_bytes"].as_u64().expect("the huge page size") / 1024;
    let (Some((before, after)), Some(_), Some(_)) = (brackets[closest], before, after) else {
        eprintln!("skipped the kernel cross-check: this kernel publishes no AnonHugePages line");
        return;
    };
    let (before, after) = (before / page_size, after / page_size);
    let drift = before.abs_diff(after);
    if drift > 64 {
        eprintln!(
            "skipped the kernel cross-check: AnonHugePages moved from {before} to {after} huge \
             pages across the one round being checked, which is wider than the 8 % this is \
             looking for. The tool said {theirs} and {ours}"
        );
        return;
    }
    let slack = drift + 4;
    let (low, high) = (
        before.min(after).saturating_sub(slack),
        before.max(after) + slack,
    );
    for (backend, reported) in [("fs:sys", theirs), ("fs:proc", ours)] {
        assert!(
            (low..=high).contains(&reported),
            "`{backend}` reports {reported} fully-mapped huge pages and /proc/meminfo says \
             between {before} and {after} over the same window. {partial} of them are \
             partially mapped, which is the size of the error this once made"
        );
    }
}

/// Forcing a backend nothing implements is refused by name, in the chain's
/// order.
///
/// The comparison `--backend force:` exists for is not always available: where
/// a chain has one implemented backend and others declared, the comparison is
/// with the refusal instead. The refusal says the machine where it lacks
/// something the backend needs, as the walk does (exit 1), and the build where
/// it lacks nothing (exit 3) -- never the build for a backend this machine
/// could not serve whoever wrote it (`DECISIONS.md`, "one order for unbuilt and
/// missing").
#[test]
fn forcing_a_backend_with_no_collector_names_it_in_the_chains_order() {
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
            assert_refused_in_the_chains_order(source, &backend, &output);
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
        message.contains("mem.heap.alloc") && message.contains("bpf:usdt"),
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
        "`omn validate` checks the request against the catalog, where a pinned backend is a \
         backend like any other: {}",
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
    // A pinned USDT probe rather than a source with no collector at all: the
    // three refusals can only be told apart where the machine is not itself a
    // fourth answer, and this family's last two sources nothing collected --
    // `mem.latency` and `mem.sharing` -- are collected through IBS since the
    // ninth wave. It was the uprobe until the twenty-fifth wave wrote it.
    let not_built = run(&[
        "mem.heap.alloc",
        "--backend",
        "force:bpf:usdt",
        "-m",
        "trace",
        "-e",
        "summary",
        "--",
        "/bin/sh",
        "-c",
        "sleep 0.05",
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
    // for the capabilities -- which a binary without them is refused for, by
    // name, and which are taken out here and said, rather than read as a
    // mode that stopped being served (stream P's list: the CI runs without
    // them).
    let (for_want_of_capabilities, rest): (Vec<Combination>, Vec<Combination>) =
        walked.into_iter().partition(|combination| {
            combination.exit == EXIT_NO_VIABLE_BACKEND && combination.for_want_of_capabilities
        });
    if !for_want_of_capabilities.is_empty() {
        eprintln!(
            "skipped {} of {} combinations: this binary lacks CAP_BPF and CAP_PERFMON, and each \
             was refused for them by name ({} {} {}: {})",
            for_want_of_capabilities.len(),
            for_want_of_capabilities.len() + rest.len(),
            for_want_of_capabilities[0].source,
            for_want_of_capabilities[0].mode,
            for_want_of_capabilities[0].emit,
            for_want_of_capabilities[0].reason,
        );
    }
    assert_machine_blame_is_exactly(&rest, &[]);
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
    // Not every page the cgroup lost was reclaimed by a task inside it. A
    // kernel worker that allocates on the cgroup's behalf -- a loop device's
    // worker charges the backing file's pages to the cgroup that issued the
    // I/O -- reclaims *for* the cgroup while being in the root one, and the
    // event carries the reclaiming task's cgroup. Measured in the
    // portability matrix's 6.6 guest, whose disk is a loop device on tmpfs:
    // `dd` reclaimed 3838 pages, two `kworker/u8` 2081 and 1856, and the
    // three add up to the cgroup's own `pgsteal_direct`, 7775, exactly. On a
    // real disk the second part is empty.
    let remote: u64 = raw_events(result)
        .iter()
        .filter(|attrs| {
            attrs.get("cgroup_id").and_then(serde_json::Value::as_u64) != Some(cgroup_id)
                && attrs.get("scope").and_then(serde_json::Value::as_str) == Some("cgroup")
                && attrs
                    .get("comm")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|comm| comm.starts_with("kworker"))
        })
        .filter_map(|attrs| {
            attrs
                .get("pages_reclaimed")
                .and_then(serde_json::Value::as_u64)
        })
        .sum();
    // The tool may see a little more than the workload's own reading: the
    // script reads memory.stat before it exits, and the run drains again after
    // it has. It cannot see less: every page counted there was reclaimed by
    // the cgroup's own tasks or by a worker charging to it.
    assert!(
        reclaimed + remote >= stolen,
        "the kernel's own memory.stat counted {stolen} pages reclaimed and this run found \
         {reclaimed} by the cgroup's tasks and {remote} by kernel workers; a measurement short \
         of the counter it is read against is a measurement that missed episodes"
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
    // The kernel's skipped runs are losses too, and a loaded machine has them
    // without the map ever filling: a run whose only reason is that one did
    // not reach the bound this test is about.
    let reason_count = result["losses"]["reasons"]
        .as_object()
        .map_or(0, |reasons| reasons.len());
    if reason_count == 1 && reasons.contains("skipped a run of this program") {
        eprintln!(
            "skipped: the {dropped} dropped were the kernel's skipped runs, not the map's \
             capacity: the storm produced {seen} rows and never more than the 4096 the map \
             holds in one drain"
        );
        return;
    }
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

    // 1 or 3: the classic tracepoint backend, which this machine can reach
    // only through tracefs and which no collector serves either way. Forcing
    // it is the backend comparison these four have -- each has exactly one
    // implemented backend, so the comparison is with the refusal: the
    // machine's where tracefs is not readable here, the build's where it is.
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
        assert_refused_in_the_chains_order(source, "bpf:tp", &forced);
    }

    // 3: a key an aggregating collector cannot carry, refused before anything
    // is attached. The identity of one allocation is exactly what counting in
    // the kernel discards, and answering with an unkeyed series would put every
    // task's allocations in one anonymous heap.
    //
    // This refusal moved a layer earlier in the seventh wave, and the movement
    // is the improvement: the entry used to declare `keys: [task, mem]`, so the
    // validator accepted `-k mem`, the resolver chose a backend, and the
    // collector refused at attach with exit 3. The key is no longer declared,
    // so the validator refuses it with exit 2 -- before the machine is touched,
    // and while naming the key that does exist. An invalid combination is
    // refused with the reason, and the earliest layer that can see it is the
    // one that should.
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
    assert_eq!(
        code(&keyed),
        EXIT_REJECTED,
        "a key the source does not carry is a rejected request, not a missing collector: {}",
        stderr(&keyed)
    );
    assert!(
        stderr(&keyed).contains("`mem` key") && stderr(&keyed).contains("carries task"),
        "the refusal names the key it cannot carry and the one it has: {}",
        stderr(&keyed)
    );
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
    // Two of these are evidence that compaction *ran* and one is not, which is
    // the distinction this list was missing. `compact_stall` is a thread that
    // had to compact inside its own allocation, and `compact_migrate_scanned`
    // counts pages the compaction machinery actually walked: either one means
    // there were episodes for this source to see. `compact_daemon_wake` counts
    // kcompactd being *woken*, and a woken kcompactd that finds nothing worth
    // moving goes back to sleep without raising `mm_compaction_begin` at all.
    //
    // Treating a wake as proof is how this test failed inside the full suite
    // while passing alone: some neighbour's allocation woke the daemon, the
    // daemon did nothing, and the source was asked to account for an episode
    // that never happened.
    let proof = ["compact_stall", "compact_migrate_scanned"];
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
    //
    // **Where this process may ask, it asks.** Writing `/proc/sys/vm/
    // compact_memory` compacts every zone from the writing thread, through the
    // same `compact_zone` that raises `mm_compaction_begin`, so as root -- a
    // portability-matrix guest -- the property is created rather than hoped
    // for, and `compact_migrate_scanned` says whether it happened. Without
    // root the write is refused and the test is the passive watch above.
    let beside = run_example_beside("mem-compaction-watch.yml", || {
        before = counters.iter().map(|name| vmstat(name)).collect();
        let asked = std::fs::OpenOptions::new()
            .write(true)
            .open("/proc/sys/vm/compact_memory")
            .and_then(|mut file| std::io::Write::write_all(&mut file, b"1"))
            .is_ok();
        if !asked {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
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
        .filter(|(name, _)| proof.contains(name))
        .map(|(name, (before, after))| format!("{name} {before} -> {after}"))
        .collect();
    let woken_only: Vec<String> = counters
        .iter()
        .zip(before.iter().zip(after.iter()))
        .filter(|(_, (before, after))| after > before)
        .filter(|(name, _)| !proof.contains(name))
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
            "skipped: this machine did not compact. None of {proof:?} moved over the \
             session's window, so there was nothing for the source to see, and \
             `/proc/sys/vm/compact_memory` is {}, which is the other way to ask and needs \
             root. `mem.compaction` attached and reported zero, which is the right answer \
             and not a measurement.{}",
            if writable { "writable" } else { "not writable" },
            if woken_only.is_empty() {
                String::new()
            } else {
                format!(
                    " kcompactd was woken in the window ({}) and moved nothing, which is not \
                     an episode and is why a wake is not in the proof list",
                    woken_only.join(", ")
                )
            }
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

// ---------------------------------------------------------------------------
// Memory by IBS: `mem.latency` and `mem.sharing`
// ---------------------------------------------------------------------------

/// Where the two workloads put their memory, as their sessions write it.
const TIB: u64 = 1 << 40;

fn has_ibs() -> bool {
    std::path::Path::new("/sys/bus/event_source/devices/ibs_op/type").exists()
}

/// A result's raw events as attributes by name, with the task key lifted
/// beside them as `tid`.
fn ibs_events(result: &serde_json::Value) -> Vec<BTreeMap<String, serde_json::Value>> {
    output_of(result, "raw")["events"]
        .as_array()
        .expect("a raw output carries events")
        .iter()
        .map(|event| {
            let mut fields: BTreeMap<String, serde_json::Value> = event["attrs"]
                .as_array()
                .expect("a raw event carries attributes")
                .iter()
                .map(|attr| {
                    (
                        attr["name"].as_str().unwrap().to_owned(),
                        attr["value"].clone(),
                    )
                })
                .collect();
            if let Some(task) = event["keys"]
                .as_array()
                .and_then(|keys| keys.iter().find_map(|key| key["task"]["tid"].as_u64()))
            {
                fields.insert("tid".to_owned(), task.into());
            }
            fields
        })
        .collect()
}

fn degradation(result: &serde_json::Value, needle: &str) -> Option<String> {
    result["degradations"]
        .as_array()?
        .iter()
        .filter_map(|note| note.as_str())
        .find(|note| note.contains(needle))
        .map(str::to_owned)
}

/// The whole number just before `marker` in `text`.
fn number_before(text: &str, marker: &str) -> Option<u64> {
    let head = &text[..text.find(marker)?];
    head.trim_end().rsplit(' ').next()?.parse().ok()
}

/// What the collector counted over the accesses the request's filter kept:
/// loads, timed loads, and accesses answered in HITM.
fn ibs_tally(result: &serde_json::Value) -> (u64, u64, u64) {
    let note = degradation(result, "IBS tagged")
        .unwrap_or_else(|| panic!("an IBS run says what it tagged: {result}"));
    let loads = number_before(&note, "were loads").expect("the loads the filter kept");
    let timed = number_before(&note, "of them timed").expect("the timed loads");
    let hitm = number_before(&note, "were answered by a line").expect("the HITM count");
    (loads, timed, hitm)
}

/// The sampled loads by the level that answered them, timed or not, from the
/// note that carries them: on family 17h an L1 hit is never an event, so this
/// is the only place it is counted.
fn loads_by_level(result: &serde_json::Value) -> BTreeMap<String, u64> {
    let Some(note) = degradation(result, "sampled loads by level:") else {
        return BTreeMap::new();
    };
    note.rsplit("sampled loads by level:")
        .next()
        .unwrap()
        .split(',')
        .filter_map(|pair| {
            let (level, count) = pair.trim().split_once(' ')?;
            Some((level.to_owned(), count.parse().ok()?))
        })
        .collect()
}

/// A walk that fits in L1 and one that waits on memory, told apart by the
/// hardware.
///
/// The working set decides which level answers, and the workload chose both:
/// sixty-four lines, and a million. So every sampled load of the small walk has
/// to be an L1 hit, and on this cpu an L1 hit carries no latency and is no
/// event -- the collector counts it by level instead, over the loads the filter
/// kept. The large walk's loads have to miss, be timed, and be answered mostly
/// by memory, and a load from memory is at least tens of nanoseconds on any
/// machine, which at any clock above one gigahertz is fifty cycles.
///
/// Asserted as a separation: the two walks run in one program, back to back,
/// and share whatever else the machine is doing.
#[test]
fn a_walk_that_fits_in_l1_is_never_timed_and_one_that_does_not_waits_on_memory() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    if !has_ibs() {
        eprintln!(
            "skipped: this machine has no `ibs_op` unit, and the pebs backend is not written"
        );
        return;
    }
    let output = run_example("mem-load-latency.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let session = json(&output);
    let small = result(&session, "small walk");
    let large = result(&session, "large walk");

    let (small_loads, small_timed, _) = ibs_tally(small);
    let (large_loads, large_timed, _) = ibs_tally(large);
    if small_loads < 15 || large_loads < 15 {
        eprintln!(
            "skipped: the unit tagged {small_loads} of the small walk's loads and {large_loads} \
             of the large one's, too few to say which level answered; a loaded machine \
             time-shares the unit between every request that opened it"
        );
        return;
    }

    // The small walk: sampled, answered by L1, and so almost never timed.
    let levels = loads_by_level(small);
    let in_l1 = levels.get("l1").copied().unwrap_or(0);
    eprintln!(
        "small walk: {small_loads} loads, {small_timed} timed, by level {levels:?}; \
         large walk: {large_loads} loads, {large_timed} timed"
    );
    assert!(
        in_l1 * 2 > small_loads,
        "a 4 KiB chain is answered by L1, and only {in_l1} of its {small_loads} sampled loads were"
    );
    assert!(
        small_timed * 4 < small_loads,
        "an L1 hit carries no latency on this cpu, and {small_timed} of {small_loads} of the \
         small walk's loads did"
    );

    // The large walk: timed, and waiting on memory.
    let events = ibs_events(large);
    assert_eq!(
        events.len() as u64,
        large_timed,
        "every timed load is an event"
    );
    assert!(
        large_timed * 2 > large_loads,
        "a 64 MiB chain misses L1, and only {large_timed} of its {large_loads} sampled loads were \
         timed"
    );
    let range = 48 * TIB..49 * TIB;
    let mut from_memory = Vec::new();
    for event in &events {
        let addr = event["addr"].as_u64().unwrap();
        assert!(
            range.contains(&addr),
            "the filter kept {addr:#x}, outside the large walk"
        );
        if matches!(event["level"].as_str(), Some("ram" | "remote_ram")) {
            from_memory.push(event["latency_cycles"].as_u64().unwrap());
        }
    }
    assert!(
        from_memory.len() * 2 > events.len(),
        "a million-line chain four times this core's L3 waits on memory, and only {} of its {} \
         timed loads came from there",
        from_memory.len(),
        events.len()
    );
    from_memory.sort_unstable();
    let median = from_memory[from_memory.len() / 2];
    assert!(
        median >= 50,
        "a load from memory takes tens of nanoseconds, which is at least fifty cycles at any \
         clock above a gigahertz, and the median here is {median}"
    );
}

/// Two processes on one line take it from each other; on two lines they do not.
///
/// The workload chose the only thing that differs: the distance between two
/// bytes each child increments, 8 on one page and 128 on the other -- 128 and
/// not 64 so that the two lines are not a pair the adjacent-line prefetcher
/// fetches together. So the one-line page must be answered in HITM, to both
/// tasks, on the one line; and the two-line page must be sampled and answered
/// in HITM hardly at all.
#[test]
fn two_processes_on_one_line_take_it_from_each_other_and_on_two_lines_do_not() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    if !has_ibs() {
        eprintln!(
            "skipped: this machine has no `ibs_op` unit, and the pebs backend is not written"
        );
        return;
    }
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    if cpus < 4 {
        eprintln!(
            "skipped: two cores that share no L1 need four cpus, and this machine has {cpus}"
        );
        return;
    }
    let output = run_example("mem-false-sharing.yml");
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let session = json(&output);
    let one = result(&session, "one line");
    let two = result(&session, "two lines");

    let (_, _, one_hitm) = ibs_tally(one);
    let (two_loads, _, two_hitm) = ibs_tally(two);
    eprintln!("one line: {one_hitm} HITM; two lines: {two_hitm} HITM over {two_loads} loads");
    if one_hitm < 5 || two_loads < 15 {
        eprintln!(
            "skipped: the unit saw {one_hitm} HITM on the shared line and {two_loads} loads on \
             the other page, too few to tell the two apart"
        );
        return;
    }

    let events = ibs_events(one);
    let lines: BTreeSet<u64> = events.iter().map(|e| e["line"].as_u64().unwrap()).collect();
    assert_eq!(
        lines,
        BTreeSet::from([40 * TIB]),
        "both children write the first line of the page at 40 TiB, and nothing else there"
    );
    let tasks: BTreeSet<u64> = events.iter().map(|e| e["tid"].as_u64().unwrap()).collect();
    assert!(
        tasks.len() >= 2,
        "a line two processes fight over is taken by both, and HITM came from {tasks:?}"
    );
    assert!(
        degradation(one, "answered modified to more than one task").is_some(),
        "the run names the line more than one task fought over: {one}"
    );

    assert!(
        two_hitm * 4 < one_hitm,
        "two writers on two lines take nothing from each other, and the second page was answered \
         in HITM {two_hitm} times against {one_hitm} for the shared line"
    );
    assert_eq!(
        ibs_events(two).len() as u64,
        two_hitm,
        "every HITM is an event"
    );
}

/// A rate at the kernel's ceiling is throttled, and a run that was says so.
///
/// `kernel.perf_event_max_sample_rate` is what decides this, and the kernel
/// lowers it on its own when sampling costs too much, so the ceiling is read
/// here rather than assumed. Above it the open is refused, by name; at it the
/// kernel throttles the unit and the run has to count that as a loss and mark
/// what it reports as a lower bound.
#[test]
fn a_rate_the_kernel_will_not_sustain_is_refused_or_counted_as_a_loss() {
    if !has_ibs() {
        eprintln!(
            "skipped: this machine has no `ibs_op` unit, and the pebs backend is not written"
        );
        return;
    }
    let ceiling: u64 = std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .expect("the kernel publishes its sampling ceiling");

    let above = format!("sample@{}hz", ceiling * 2);
    let refused = run(&[
        "mem.latency",
        "-m",
        &above,
        "-e",
        "hist",
        "--",
        "/bin/sh",
        "-c",
        "sleep 0.05",
    ]);
    assert_ne!(
        code(&refused),
        0,
        "{above} is above the ceiling and was accepted"
    );
    assert!(
        stderr(&refused).contains("perf_event_max_sample_rate"),
        "a rate above the ceiling is refused by naming the ceiling: {}",
        stderr(&refused)
    );

    let at = format!("sample@{ceiling}hz");
    let throttled = run(&[
        "mem.latency",
        "-m",
        &at,
        "-e",
        "hist",
        "--format",
        "json",
        "--",
        "/bin/sh",
        "-c",
        "i=0; while [ $i -lt 400000 ]; do i=$((i+1)); done",
    ]);
    assert!(throttled.status.success(), "{}", stderr(&throttled));
    let session = json(&throttled);
    let run_result = &session["results"][0];
    let lost = losses(run_result, "throttled") + losses(run_result, "dropped");
    if lost == 0 {
        eprintln!(
            "skipped: the kernel sustained {ceiling} samples a second on this run without \
             throttling or dropping, so there was no loss to report"
        );
        return;
    }
    let hist = output_of(run_result, "hist");
    assert!(
        hist.to_string().contains("short"),
        "a throttled run marks its distribution as a lower bound: {hist}"
    );
}

/// Every mode and emit the two IBS sources declare is served here.
#[test]
fn every_mode_and_emit_of_the_sampled_sources_is_served_or_refused_by_the_right_code() {
    if !has_ibs() {
        eprintln!("skipped: this machine has no `ibs_op` unit, so there is nothing to serve these");
        return;
    }
    let walked = sweep(&["mem.latency", "mem.sharing"]);
    assert_modes_and_emits_are_coherent(&walked);
    assert_machine_blame_is_exactly(&walked, &[]);
    for combination in &walked {
        assert_eq!(combination.exit, 0, "{combination:?}");
    }
}

/// A stack is symbolized through one process's mappings, so a machine-wide
/// request for one is this build's gap and says so; the same request without
/// the stack is served.
#[test]
fn a_stack_of_every_process_is_refused_by_the_build_and_the_rest_is_served() {
    if !has_ibs() {
        eprintln!("skipped: this machine has no `ibs_op` unit");
        return;
    }
    let stacked = run(&[
        "mem.latency",
        "--system",
        "-m",
        "sample@999hz",
        "-e",
        "stack",
        "-d",
        "100ms",
    ]);
    assert_eq!(code(&stacked), EXIT_NO_COLLECTOR, "{}", stderr(&stacked));
    assert!(
        stderr(&stacked).contains("mappings"),
        "the refusal says what is missing: {}",
        stderr(&stacked)
    );
    let ranked = run(&[
        "mem.latency",
        "--system",
        "-m",
        "sample@999hz",
        "-e",
        "topk",
        "-d",
        "100ms",
    ]);
    if let Some(reason) = unavailable(&ranked) {
        eprintln!("skipped the machine-wide half: {reason}");
        return;
    }
    assert!(ranked.status.success(), "{}", stderr(&ranked));
}

// ---------------------------------------------------------------------------
// The silent degradations (the eleventh wave, stream S)
// ---------------------------------------------------------------------------
//
// Each of these was a request the tool accepted and then answered with a
// different question, saying nothing. Every one fails against the build that
// had the defect, and `docs/fixes/silent.md` has the command that found it.

/// A number the workload printed about itself on standard error, as
/// `NAME=value`.
fn printed(output: &Output, name: &str) -> f64 {
    stderr(output)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}=")).map(str::to_owned))
        .unwrap_or_else(|| panic!("the workload did not print {name}: {}", stderr(output)))
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("{name} is not a number ({error})"))
}

/// A backend pinned with `--backend force:` whose requirements probe missing
/// is refused before anything is launched, with the probes' reasons.
///
/// It used to be accepted -- `--dump-resolved` printed a resolved file with the
/// two `missing` probes in it -- and the run then launched the target and
/// failed at attach with "perf_event_open ... refused (No such file or
/// directory)", which is finding 15's shape reached through the pin instead of
/// the probe. Ground truth is the machine's own answer: `omn list` probes the
/// same requirements, and this scenario only runs where it says they are
/// missing.
#[test]
fn a_pinned_backend_this_machine_cannot_serve_is_refused_before_the_launch() {
    let listed = run(&["list", "mem.cache_llc", "--format", "json"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let listing = stdout(&listed);
    if !listing.contains("LLC-loads") || !listing.contains("missing") {
        eprintln!(
            "skipped: this cpu publishes the last-level cache descriptors `pmu` needs, so a \
             pin on it measures and there is no refusal to check here: {listing}"
        );
        return;
    }

    let resolved = run(&[
        "mem.cache_llc",
        "--backend",
        "force:pmu",
        "-m",
        "count",
        "-e",
        "summary",
        "--dump-resolved",
        "--",
        "true",
    ]);
    assert_eq!(
        code(&resolved),
        EXIT_NO_VIABLE_BACKEND,
        "a pin past two missing probes is this machine's gap, before anything runs: {}{}",
        stdout(&resolved),
        stderr(&resolved)
    );
    assert!(
        stdout(&resolved).trim().is_empty(),
        "no resolved file is printed for a request the machine cannot serve: {}",
        stdout(&resolved)
    );

    let marker = std::env::temp_dir().join(format!("omn-pinned-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = format!("touch {}", marker.display());
    let output = run(&[
        "mem.cache_llc",
        "--backend",
        "force:pmu",
        "-m",
        "count",
        "-e",
        "summary",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    let ran = marker.exists();
    let _ = std::fs::remove_file(&marker);
    let message = stderr(&output);
    assert_eq!(code(&output), EXIT_NO_VIABLE_BACKEND, "{message}");
    assert!(
        message.contains("pinned") && message.contains("LLC-loads"),
        "the refusal names the pin and the probe that is missing: {message}"
    );
    assert!(
        !message.contains("cannot start collecting"),
        "the refusal came from the attach, after the launch, rather than from the resolver: \
         {message}"
    );
    assert!(!ran, "the program ran although the request was refused");
}

/// `mem.faults -m count` counts, and the count is the kernel's own.
///
/// The catalog declared `count`, the validator accepted it, and the collector
/// refused it after the launch -- "`count` mode does not sample page faults".
/// A `PERF_COUNT_SW_PAGE_FAULTS` counting event is the cheapest thing the
/// source could do, and is what serves it now. Ground truth is `getrusage`:
/// the program asks the kernel how many faults it took. Both gaps are in one
/// direction -- the faults between the fork and the launch gate are the
/// program's in `getrusage` and before the counter, and the ones after its own
/// read are the counter's alone and a handful -- so the counter may be a little
/// below the program's number and not above it by more than those.
#[test]
fn a_counted_run_of_page_faults_is_the_kernels_own_count() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let program = "import mmap, resource, sys\n\
                   m = mmap.mmap(-1, 64 * 1024 * 1024)\n\
                   m.madvise(mmap.MADV_NOHUGEPAGE)\n\
                   for offset in range(0, len(m), 4096):\n    m[offset] = 65\n\
                   usage = resource.getrusage(resource.RUSAGE_SELF)\n\
                   sys.stderr.write('FAULTS=%d\\n' % (usage.ru_minflt + usage.ru_majflt))\n";
    let output = run(&[
        "mem.faults",
        "-m",
        "count",
        "-e",
        "summary",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        program,
    ]);
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let kernels_own = printed(&output, "FAULTS");
    assert!(
        kernels_own > 16_384.0,
        "sixty-four megabytes of small pages is sixteen thousand faults, and the program took \
         {kernels_own}"
    );
    let document = json(&output);
    let counted = document["results"][0]["outputs"][0]["series"][0]["last"]
        .as_f64()
        .unwrap_or_else(|| panic!("a counted value: {document}"));
    if counted_in_user_mode_only(&document["results"][0]) {
        // At perf_event_paranoid 2 the counter opens for user mode only, and
        // the program's `getrusage` still counts the faults the kernel took on
        // its behalf -- the exec's, a read() into a fresh buffer -- which a
        // user-mode event cannot see. Its own stores it can: one per page of a
        // mapping advised against huge pages, each a fault its code took. And
        // the counter is still never above the kernel's count by more than
        // the handful after the program's own read.
        const PAGES_STORED: f64 = 64.0 * 1024.0 * 1024.0 / 4096.0;
        assert!(
            counted >= PAGES_STORED && counted <= kernels_own * 1.01,
            "counted in user mode only, the run counted {counted} faults; the program's own \
             stores took {PAGES_STORED} and the kernel counted {kernels_own} in all: {}",
            document["results"][0]["notes"]
        );
        return;
    }
    let difference = (kernels_own - counted) / kernels_own;
    assert!(
        (-0.01..0.05).contains(&difference),
        "the kernel counted {kernels_own} faults for this program and the run counted \
         {counted}: {:.2} % apart",
        difference * 100.0
    );
}

/// The faults a worker thread takes are in a traced run, attributed to it.
///
/// A sampling event was not inherited, so `mem.faults` followed the task it
/// was opened on: a program that faulted its memory in from a worker read as a
/// program that faulted almost nothing, and said only that a sampling event is
/// not inherited. Ground truth is the worker's own `getrusage(RUSAGE_THREAD)`,
/// which counts that thread's faults and nobody else's, and the thread's id,
/// which the worker prints beside it.
#[test]
fn the_faults_a_worker_thread_takes_are_traced_to_that_thread() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let program = "import mmap, resource, sys, threading\n\
                   def touch():\n    \
                       m = mmap.mmap(-1, 64 * 1024 * 1024)\n    \
                       m.madvise(mmap.MADV_NOHUGEPAGE)\n    \
                       for offset in range(0, len(m), 4096):\n        m[offset] = 65\n    \
                       usage = resource.getrusage(resource.RUSAGE_THREAD)\n    \
                       sys.stderr.write('WORKER=%d\\nWORKER_FAULTS=%d\\n' % \
                       (threading.get_native_id(), usage.ru_minflt + usage.ru_majflt))\n\
                   worker = threading.Thread(target=touch)\n\
                   worker.start()\n\
                   worker.join()\n";
    let output = run(&[
        "mem.faults",
        "-m",
        "trace",
        "-e",
        "raw",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        program,
    ]);
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let worker = printed(&output, "WORKER");
    let its_own = printed(&output, "WORKER_FAULTS");
    assert!(
        its_own > 16_384.0,
        "sixty-four megabytes of small pages is sixteen thousand faults, and the worker took \
         {its_own}"
    );
    let document = json(&output);
    let result = &document["results"][0];
    let traced = result["outputs"][0]["events"]
        .as_array()
        .expect("raw events")
        .iter()
        .filter(|event| event["keys"][0]["task"]["tid"].as_f64() == Some(worker))
        .count() as f64;
    let dropped = result["losses"]["dropped"].as_f64().unwrap_or(0.0);
    if sampled_in_user_mode_only(result) {
        // The worker's own number counts faults the kernel took for it as
        // well, which a user-mode event cannot see. Its own stores it can: one
        // per page of a mapping advised against huge pages, each a fault the
        // worker's code took.
        const PAGES_STORED: f64 = 64.0 * 1024.0 * 1024.0 / 4096.0;
        assert!(
            traced + dropped >= PAGES_STORED && traced <= its_own + 64.0,
            "sampled in user mode only, the run traced {traced} faults to thread {worker} with \
             {dropped} lost; the worker's own stores took {PAGES_STORED} and it counted \
             {its_own} in all: {}",
            result["degradations"]
        );
        return;
    }
    // What the worker took is either traced to it or counted as lost. It
    // faults a few more times after its own read -- formatting and writing the
    // line that carries it -- so the trace may exceed its number by those and
    // not by more: the resolution of the reference is its own tail.
    const AFTER_ITS_OWN_READ: f64 = 64.0;
    assert!(
        traced + dropped >= its_own && traced <= its_own + AFTER_ITS_OWN_READ,
        "the worker counted {its_own} faults of its own and the run traced {traced} to thread \
         {worker}, with {dropped} lost: {}",
        result["degradations"]
    );
}
