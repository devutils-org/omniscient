//! Each view's workload, held to its own manifest before any view draws it.
//!
//! `view/workloads/<view>/` holds a program whose load is known in advance and
//! a `manifest.json` that declares it -- per phase, thread, function, request,
//! file, socket and kernel, with how exact each figure is -- and a
//! `session.yml` that asks the view's sources of it. The program reads its
//! parameters from the manifest, so the two cannot drift apart. Each test here
//! runs one session and checks the sources against the manifest, within each
//! figure's stated trust: the workload's ground truth, the way every source
//! is held, before `tests/view_truth.rs` holds a view's drawing to it.
//!
//! The eBPF sources need `CAP_BPF` and `CAP_PERFMON` (`make` grants them);
//! without them a test skips, loudly, naming what is missing. A machine with
//! no GPU runs the lanes workload without its device part.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// The views that have a workload: every view in the registry, and `process`,
/// whose view is drawn from all of them.
const PROCESS: &str = "process";

const MIB: f64 = 1024.0 * 1024.0;
const MS: f64 = 1e6;
const S: f64 = 1e9;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Why a read of `path` issues no block request, if it issues none: the
/// filesystem it is on, found as the longest mount point above it in
/// `/proc/self/mountinfo`, is not backed by a `/dev` device -- virtiofs, NFS,
/// 9p, FUSE, a tmpfs. A checkout shared into a VM is one.
fn off_any_block_device(path: &std::path::Path) -> Option<String> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let table = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let (_, fstype, source) = table
        .lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let point = left.split_whitespace().nth(4)?;
            let mut right = right.split_whitespace();
            let fstype = right.next()?;
            let source = right.next()?;
            path.starts_with(point)
                .then(|| (point.len(), fstype.to_owned(), source.to_owned()))
        })
        .max_by_key(|(length, _, _)| *length)?;
    (!source.starts_with("/dev/") || fstype == "tmpfs").then(|| {
        format!(
            "{} is on {fstype} ({source}), which is no block device: no read of it is a block \
             request",
            path.display()
        )
    })
}

/// Why the workloads' files reach no device here, if they do not: they are
/// written under the checkout's `target/` (`view/workloads/common.py`).
fn workloads_off_block_device() -> Option<String> {
    off_any_block_device(&root().join("target"))
}

fn dir(view: &str) -> PathBuf {
    root().join("view/workloads").join(view)
}

fn manifest(view: &str) -> Value {
    let path = dir(view).join("manifest.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn f(value: &Value) -> f64 {
    value
        .as_f64()
        .unwrap_or_else(|| panic!("a number in the manifest, found {value}"))
}

fn spoken(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

fn have_python() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Whether this `python3` writes a perf map under `-X perf` (3.12 and later),
/// which is the only way a Python function has a name in a stack.
fn python_has_perf_map() -> bool {
    Command::new("python3")
        .args([
            "-c",
            "import sys; sys.exit(0 if hasattr(sys, 'activate_stack_trampoline') else 1)",
        ])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Whether the run was refused for want of the privileges the eBPF backends
/// need: the machine's limit, and the only reason a test here stops quietly.
fn unprivileged(text: &str) -> bool {
    text.contains("CAP_BPF") || text.contains("CAP_PERFMON")
}

/// Runs a session text through `omn -f -`, from the repository's root, where
/// the sessions' paths are relative to.
fn run_session(text: &str) -> Output {
    let mut child = Command::new(BINARY)
        .current_dir(root())
        .args(["-f", "-", "--input-format", "yaml", "--format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    child
        .stdin
        .take()
        .expect("a stdin")
        .write_all(text.as_bytes())
        .expect("the session is written");
    child.wait_with_output().expect("omn should finish")
}

fn session_text(view: &str) -> String {
    let path = dir(view).join("session.yml");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The results of a view's session, or `None` when this machine cannot run it
/// (said on stderr).
fn measure(view: &str) -> Option<Vec<Value>> {
    measure_text(view, &for_this_machine(&session_text(view)))
}

/// Whether this kernel publishes PSI: one built without `CONFIG_PSI`, or
/// booted with `psi=0`, has no `/proc/pressure` and no pressure in any cgroup.
fn psi_here() -> bool {
    std::path::Path::new("/proc/pressure").exists()
}

/// A session with the requests this machine cannot serve taken out, said on
/// stderr: a session is refused whole for one request the machine lacks, and
/// the rest of a view's panels still have something to check. The one taken
/// out is `mem.pressure` on a kernel without PSI.
fn for_this_machine(text: &str) -> String {
    if psi_here() {
        return text.to_owned();
    }
    let mut kept = Vec::new();
    let mut request: Vec<&str> = Vec::new();
    let flush = |request: &mut Vec<&str>, kept: &mut Vec<String>| {
        if request
            .iter()
            .any(|line| line.trim() == "source: mem.pressure")
        {
            eprintln!(
                "note: this kernel publishes no PSI, so the session runs without its \
                       `mem.pressure` request"
            );
        } else {
            kept.extend(request.iter().map(|line| (*line).to_owned()));
        }
        request.clear();
    };
    for line in text.lines() {
        // A request begins at `  - label:`, and the block ends at the next
        // one or at a line that is not indented under the list.
        if line.starts_with("  - ") || !line.starts_with("    ") {
            flush(&mut request, &mut kept);
        }
        if line.starts_with("  - ") || (line.starts_with("    ") && !request.is_empty()) {
            request.push(line);
        } else {
            kept.push(line.to_owned());
        }
    }
    flush(&mut request, &mut kept);
    kept.join("\n") + "\n"
}

fn measure_text(view: &str, text: &str) -> Option<Vec<Value>> {
    if !have_python() {
        eprintln!("skipped: the {view} workload is a python3 program, and this machine has none");
        return None;
    }
    let output = run_session(text);
    let said = spoken(&output);
    if !output.status.success() && unprivileged(&said) {
        eprintln!(
            "skipped: the {view} workload's sources need CAP_BPF and CAP_PERFMON, which this \
             binary does not carry (`make` grants them):\n{said}"
        );
        return None;
    }
    assert!(
        output.status.success(),
        "the {view} workload's session failed:\n{said}"
    );
    // Kept beside the workload's files, so a failure can be read after the
    // fact: `target/omn-view-workloads/<view>.json`.
    let kept = root().join("target/omn-view-workloads");
    if std::fs::create_dir_all(&kept).is_ok() {
        let _ = std::fs::write(kept.join(format!("{view}.json")), &output.stdout);
    }
    let document: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("the run is not one json document ({error}):\n{said}"));
    Some(
        document["results"]
            .as_array()
            .expect("a json run carries results")
            .clone(),
    )
}

fn labelled<'a>(results: &'a [Value], label: &str) -> &'a Value {
    results
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("no result labelled `{label}`"))
}

fn outputs<'a>(result: &'a Value, emit: &'a str) -> impl Iterator<Item = &'a Value> {
    result["outputs"]
        .as_array()
        .expect("a result carries outputs")
        .iter()
        .filter(move |output| output["emit"] == emit)
}

/// One raw event: its timestamp and its attributes by name.
struct Event {
    ts: u64,
    attrs: HashMap<String, Value>,
}

impl Event {
    fn num(&self, name: &str) -> f64 {
        self.attrs.get(name).and_then(Value::as_f64).unwrap_or(0.0)
    }
    fn text(&self, name: &str) -> &str {
        self.attrs.get(name).and_then(Value::as_str).unwrap_or("")
    }
}

fn raw(result: &Value) -> Vec<Event> {
    outputs(result, "raw")
        .flat_map(|output| output["events"].as_array().expect("raw events").iter())
        .map(|event| Event {
            ts: event["ts_ns"].as_u64().unwrap_or(0),
            attrs: event["attrs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|attr| Some((attr["name"].as_str()?.to_owned(), attr["value"].clone())))
                .collect(),
        })
        .collect()
}

/// The sum of `attr` over the raw events of the threads named `comm`.
fn sum_for(events: &[Event], comm: &str, attr: &str) -> f64 {
    events
        .iter()
        .filter(|event| event.text("comm") == comm)
        .map(|event| event.num(attr))
        .sum()
}

/// The pid of the process whose thread is named `comm`.
fn pid_of(events: &[Event], comm: &str) -> Option<f64> {
    events
        .iter()
        .find(|event| event.text("comm") == comm)
        .map(|event| event.num("pid"))
}

/// Bytes per (operation, descriptor) of a process, from `io.syscall` or
/// `net.socket`'s raw events (a socket's descriptor is absent: -1).
fn bytes_by_op_fd(events: &[Event], pid: f64) -> BTreeMap<(String, i64), f64> {
    let mut bytes = BTreeMap::new();
    for event in events.iter().filter(|event| event.num("pid") == pid) {
        let fd = event.attrs.get("fd").and_then(Value::as_i64).unwrap_or(-1);
        *bytes
            .entry((event.text("op").to_owned(), fd))
            .or_insert(0.0) += event.num("bytes");
    }
    bytes
}

/// The sum of every point of every series of a `timeline`, optionally of the
/// series whose key is `key`.
fn timeline_sum(result: &Value, key: Option<&str>) -> f64 {
    outputs(result, "timeline")
        .flat_map(|output| output["series"].as_array().expect("series").iter())
        .filter(|series| key.is_none_or(|key| series["key"] == key))
        .flat_map(|series| series["points"].as_array().expect("points").iter())
        .filter_map(|point| point["value"].as_f64())
        .sum()
}

fn timeline_points(result: &Value) -> Vec<f64> {
    outputs(result, "timeline")
        .flat_map(|output| output["series"].as_array().expect("series").iter())
        .flat_map(|series| series["points"].as_array().expect("points").iter())
        .filter_map(|point| point["value"].as_f64())
        .collect()
}

/// The weight under each Python function of a `stack` emit, inclusive: a
/// stack counts once for every function on it.
fn python_weights(result: &Value) -> HashMap<String, f64> {
    let stacks = &result["stacks"];
    let mut weights = HashMap::new();
    for output in outputs(result, "stack") {
        for entry in output["stacks"].as_array().expect("stacks") {
            let frames = stacks[entry["stack"].to_string()]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut seen: Vec<&str> = frames
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|frame| frame.strip_prefix("py::")?.split(':').next())
                .collect();
            seen.sort_unstable();
            seen.dedup();
            for function in seen {
                *weights.entry(function.to_owned()).or_insert(0.0) +=
                    entry["total"].as_f64().unwrap_or(0.0);
            }
        }
    }
    weights
}

fn weight(weights: &HashMap<String, f64>, function: &str) -> f64 {
    weights.get(function).copied().unwrap_or(0.0)
}

fn dropped(result: &Value) -> u64 {
    result["losses"]["dropped"].as_u64().unwrap_or(0)
}

/// An exact figure, held exactly unless the source counted a loss: then what
/// it saw is a lower bound, and only an excess is wrong. A loss the source did
/// not count is the defect this catches.
fn exact_unless_counted(measured: f64, truth: f64, result: &Value, what: &str) {
    if dropped(result) == 0 {
        assert_eq!(
            measured, truth,
            "{what}: {truth} exactly, and nothing was dropped"
        );
    } else {
        eprintln!(
            "{what}: {measured} of {truth}, with {} events counted as dropped: {}",
            dropped(result),
            result["losses"]
        );
        assert!(
            measured <= truth,
            "{what}: {measured} is more than the {truth} there were"
        );
    }
}

/// The bytes a manifest's `file` says were written through its descriptor,
/// held to the `syscalls` result.
fn file_bytes(results: &[Value], comm: &str, file: &Value) {
    let result = labelled(results, "syscalls");
    let events = raw(result);
    let pid = pid_of(&events, comm).expect("the program's own system calls");
    let fd = file["fd"].as_i64().expect("fd");
    exact_unless_counted(
        bytes_by_op_fd(&events, pid)
            .get(&("write".to_owned(), fd))
            .copied()
            .unwrap_or(0.0),
        f(&file["written"]),
        result,
        &format!("{} written through descriptor {fd}", file["name"]),
    );
}

/// The bytes a manifest's `socket` says were sent and received, held to the
/// `socket` result.
fn socket_bytes(results: &[Value], comm: &str, socket: &Value) {
    let result = labelled(results, "socket");
    let events = raw(result);
    let pid = pid_of(&events, comm).expect("the program's own socket calls");
    let bytes = bytes_by_op_fd(&events, pid);
    for (op, field) in [("send", "sent"), ("recv", "received")] {
        exact_unless_counted(
            bytes.get(&(op.to_owned(), -1)).copied().unwrap_or(0.0),
            f(&socket[field]),
            result,
            &format!("the socket's bytes {field}"),
        );
    }
}

/// What a sample at 997 Hz counts for `seconds` of cpu, within `band` of it:
/// the profile's sampling error is a few percent at these sizes, and the band
/// leaves a loaded machine room without letting a halved count through.
fn samples_close(samples: f64, seconds: f64, band: f64) -> bool {
    let expected = seconds * 997.0;
    (samples - expected).abs() <= expected * band
}

// ----------------------------------------------------------- the inventory

/// Every view has a workload, and every workload a manifest that names its
/// view, a session, and the program it runs.
#[test]
fn every_view_has_a_workload_with_a_manifest_and_a_session() {
    let mut views: Vec<String> = omn_view::views::registry()
        .iter()
        .map(|view| view.name().to_owned())
        .collect();
    if !views.iter().any(|view| view == PROCESS) {
        views.push(PROCESS.to_owned());
    }
    for view in &views {
        let truth = manifest(view);
        assert_eq!(
            truth["view"],
            view.as_str(),
            "{view}'s manifest names another view"
        );
        assert!(
            truth["about"]
                .as_str()
                .is_some_and(|about| !about.is_empty()),
            "{view}'s manifest says nothing of what its program does"
        );
        assert!(
            truth["trust"].is_object(),
            "{view}'s manifest says how exact nothing is"
        );
        let program = truth["run"]
            .as_array()
            .unwrap_or_else(|| panic!("{view}'s manifest has no `run`"))
            .iter()
            .filter_map(Value::as_str)
            .find(|word| word.ends_with(".py") || word.ends_with(".c"))
            .map(str::to_owned);
        let program = program.or_else(|| {
            truth["build"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .find(|word| word.ends_with(".c"))
                .map(str::to_owned)
        });
        let program = program.unwrap_or_else(|| panic!("{view}'s manifest names no program"));
        assert!(
            root().join(&program).is_file(),
            "{view}'s program `{program}` is missing"
        );
        assert!(
            dir(view).join("session.yml").is_file(),
            "{view} has no session.yml"
        );
    }
}

/// A session asks every source its view needs, or the manifest says why this
/// workload cannot give that source a known load.
#[test]
fn each_session_asks_its_views_sources_or_says_why_not() {
    for view in omn_view::views::registry() {
        let name = view.name();
        let session = session_text(name);
        let truth = manifest(name);
        for need in view.needs() {
            let asked = session.contains(&format!("source: {}\n", need.source));
            let excused = truth["not_exercised"][need.source].is_string();
            assert!(
                asked || excused,
                "the {name} view needs `{}`, which its workload's session does not ask and its \
                 manifest's `not_exercised` does not excuse",
                need.source
            );
        }
    }
}

/// Every session is understood. Validated against the catalog, which needs no
/// privilege, so that half holds on any machine; then resolved
/// (`--dump-request`), measuring nothing -- but resolving picks backends, and
/// on a binary without `CAP_BPF` and `CAP_PERFMON` most of these sources have
/// none, so that half skips, loudly, per session, when that is the reason.
#[test]
fn every_session_is_understood() {
    let mut views: Vec<String> = omn_view::views::registry()
        .iter()
        .map(|view| view.name().to_owned())
        .collect();
    views.push(PROCESS.to_owned());
    views.dedup();
    for view in views {
        let path = format!("view/workloads/{view}/session.yml");
        let validated = Command::new(BINARY)
            .current_dir(root())
            .args(["validate", "-f", &path])
            .output()
            .expect("omn should run");
        assert!(
            validated.status.success(),
            "{path}:\n{}",
            spoken(&validated)
        );

        let resolved = Command::new(BINARY)
            .current_dir(root())
            .args(["-f", &path, "--dump-request"])
            .output()
            .expect("omn should run");
        let said = spoken(&resolved);
        // The machine answering: a source this kernel or this hardware cannot
        // serve (no PSI, no GPU), every backend named with what it lacked.
        if !resolved.status.success()
            && said.contains("no viable backend")
            && said.contains("it is this machine that cannot serve it")
        {
            eprintln!("skipped resolving {path}: this machine cannot serve it:\n{said}");
            continue;
        }
        if !resolved.status.success() && said.contains("no viable backend") && unprivileged(&said) {
            eprintln!(
                "skipped resolving {path}: its sources need CAP_BPF and CAP_PERFMON, which this \
                 binary does not carry (`make` grants them):\n{said}"
            );
            continue;
        }
        assert!(resolved.status.success(), "{path}:\n{said}");
    }
}

/// The request session's plan is the manifest's: the server takes its plan on
/// its command line, not from the manifest, so this is what keeps the two
/// from drifting apart. Each kind's plan string must say the figures the
/// manifest gives it (`lock:reads[:gap]`, zero where the kind has none), and
/// the session must launch the kinds interleaved, `count` rounds, in the
/// manifest's order -- which is what `request_causes_are_the_manifests` reads
/// a request's id against.
fn request_plan_is_the_manifests() {
    let truth = manifest("request");
    let kinds = truth["requests"].as_array().expect("requests");
    let field = |kind: &Value, name: &str| kind[name].as_i64().unwrap_or(0);
    for kind in kinds {
        let plan = kind["plan"].as_str().expect("plan");
        let words: Vec<i64> = plan
            .split(':')
            .map(|word| word.parse().unwrap_or_else(|_| panic!("plan `{plan}`")))
            .collect();
        let said = |at: usize| words.get(at).copied().unwrap_or(0);
        assert_eq!(
            (said(0), said(1), said(2)),
            (
                field(kind, "lock_ms"),
                field(kind, "reads"),
                field(kind, "gap_ms")
            ),
            "the {} kind's plan `{plan}` is `lock:reads[:gap]`, and must say the manifest's \
             lock_ms, reads and gap_ms",
            kind["cause"]
        );
    }
    let rounds = field(&kinds[0], "count");
    assert!(
        kinds.iter().all(|kind| field(kind, "count") == rounds),
        "the kinds interleave, so each comes the same number of times"
    );
    let run: Vec<&str> = truth["run"]
        .as_array()
        .expect("run")
        .iter()
        .filter_map(Value::as_str)
        .take_while(|word| *word != "<plan...>")
        .collect();
    let mut expected: Vec<String> = run.iter().map(|word| (*word).to_owned()).collect();
    for _ in 0..rounds {
        expected.extend(
            kinds
                .iter()
                .map(|kind| kind["plan"].as_str().expect("plan").to_owned()),
        );
    }
    let session: serde_norway::Value =
        serde_norway::from_str(&session_text("request")).expect("the session is yaml");
    let launched: Vec<String> = session["requests"][0]["target"]["launch"]
        .as_sequence()
        .expect("the session launches the server")
        .iter()
        .map(|word| word.as_str().expect("a word").to_owned())
        .collect();
    assert_eq!(
        launched, expected,
        "view/workloads/request/session.yml must launch the manifest's plan"
    );
}

#[test]
fn request_session_runs_the_manifests_plan() {
    request_plan_is_the_manifests();
}

// ----------------------------------------------------------- one per view

#[test]
fn flame_functions_weigh_what_the_manifest_says() {
    if !python_has_perf_map() {
        eprintln!("skipped: this python3 writes no perf map (`-X perf`, 3.12 and later), so no Python function has a name in a stack");
        return;
    }
    let truth = manifest("flame");
    let Some(results) = measure("flame") else {
        return;
    };

    let on = python_weights(labelled(&results, "profile"));
    let slices = truth["threads"].as_array().expect("threads");
    for thread in slices {
        for (function, seconds) in thread["on_cpu"].as_object().expect("on_cpu") {
            let samples = weight(&on, function);
            assert!(
                samples_close(samples, f(seconds), 0.25),
                "`{function}` burns {} s of cpu, which is about {:.0} samples at 997 Hz; the \
                 profile has {samples}",
                f(seconds),
                f(seconds) * 997.0
            );
        }
    }
    // The ratio is the sharper figure: one machine's load stretches both.
    let ratio = weight(&on, "heavy") / weight(&on, "light").max(1.0);
    assert!(
        (3.0..=5.0).contains(&ratio),
        "`heavy` burns four times what `light` does; the profile says {ratio:.2}"
    );

    let blocked = labelled(&results, "blocked");
    let off = python_weights(blocked);
    for thread in slices {
        for (function, seconds) in thread["off_cpu"].as_object().expect("off_cpu") {
            let measured = weight(&off, function) / S;
            assert!(
                measured >= f(seconds) * 0.98 && measured <= f(seconds) + 0.3,
                "`{function}` sleeps {} s in ten sleeps; off the cpu under it: {measured:.3} s",
                f(seconds)
            );
        }
    }
    // Each thread runs alone, so neither waits for the other's interpreter
    // lock: a burning function is (almost) never off the cpu.
    for function in ["heavy", "light", "medium"] {
        let measured = weight(&off, function) / S;
        assert!(
            measured < 0.05,
            "`{function}` never waits for anything, and was off the cpu {measured:.3} s"
        );
    }
}

#[test]
fn timebar_states_split_the_wall_clock_as_the_manifest_says() {
    let truth = manifest("timebar");
    let Some(results) = measure("timebar") else {
        return;
    };
    let phase = |name: &str| -> f64 {
        truth["phases"]
            .as_array()
            .expect("phases")
            .iter()
            .find(|phase| phase["name"] == name)
            .map(|phase| f(&phase["seconds"]))
            .unwrap_or_else(|| panic!("no phase {name}"))
    };
    let worker = truth["thread"].as_str().expect("thread");

    let blocked = sum_for(&raw(labelled(&results, "blocked")), worker, "blocked_ns") / S;
    let expected = phase("sleep") + phase("lock");
    assert!(
        blocked >= expected * 0.98 && blocked <= expected + 0.3,
        "`{worker}` sleeps {} s and waits {} s for a lock: {expected} s blocked; measured \
         {blocked:.3} s",
        phase("sleep"),
        phase("lock")
    );

    let futex = labelled(&results, "futex");
    let waited = sum_for(&raw(futex), worker, "total_ns") / S;
    assert!(
        waited >= phase("lock") * 0.95 || dropped(futex) > 0,
        "`{worker}` waits {} s for the lock, and the futex wait says {waited:.3} s with \
         nothing dropped",
        phase("lock")
    );
    assert!(
        waited <= phase("lock") + 0.2,
        "the futex wait of `{worker}` is {waited:.3} s, beyond the {} s lock",
        phase("lock")
    );

    let queued = sum_for(&raw(labelled(&results, "runqueue")), worker, "total_ns") / S;
    assert!(
        queued >= phase("contended") * 0.5,
        "`{worker}` shares a core with a rival for {} s of cpu each, so it waits about as long \
         on the run queue; measured {queued:.3} s",
        phase("contended")
    );

    let on_cpu = timeline_sum(labelled(&results, "on-cpu"), None) / S;
    let burnt = phase("compute") + phase("contended");
    assert!(
        on_cpu >= burnt * 0.95 && on_cpu <= burnt + 0.6,
        "the program burns {burnt} s of cpu; its on-cpu time is {on_cpu:.3} s"
    );

    if python_has_perf_map() {
        let on = python_weights(labelled(&results, "profile"));
        // `compute` is on the stack in both burning phases.
        assert!(
            samples_close(weight(&on, "compute"), burnt, 0.25),
            "`compute` burns {burnt} s; the profile has {} samples",
            weight(&on, "compute")
        );
    }
}

#[test]
fn wake_hand_offs_follow_the_ring_the_manifest_draws() {
    let truth = manifest("wake");
    let Some(results) = measure("wake") else {
        return;
    };
    let events = raw(labelled(&results, "wakeups"));
    // The wakee's name is the event's; the waker's is the one its tid had when
    // it was woken itself.
    let names: HashMap<i64, String> = events
        .iter()
        .map(|event| (event.num("wakee_tid") as i64, event.text("comm").to_owned()))
        .collect();
    let mut edges: HashMap<(String, String), f64> = HashMap::new();
    for event in &events {
        let waker = names
            .get(&(event.num("waker_tid") as i64))
            .cloned()
            .unwrap_or_default();
        *edges
            .entry((waker, event.text("comm").to_owned()))
            .or_insert(0.0) += event.num("count").max(1.0);
    }
    let laps = f(&truth["laps"]);
    for edge in truth["edges"].as_array().expect("edges") {
        let (waker, wakee) = (
            edge["waker"].as_str().expect("waker"),
            edge["wakee"].as_str().expect("wakee"),
        );
        let count = edges
            .get(&(waker.to_owned(), wakee.to_owned()))
            .copied()
            .unwrap_or(0.0);
        assert!(
            count >= f(&edge["count"]) * 0.5 && count <= f(&edge["count"]),
            "{waker} hands the token to {wakee} {} times; {count} wakeups between them",
            f(&edge["count"])
        );
        let back = edges
            .get(&(wakee.to_owned(), waker.to_owned()))
            .copied()
            .unwrap_or(0.0);
        assert!(
            back <= laps * 0.02,
            "the ring goes {waker} -> {wakee} and never back; {back} wakeups went back"
        );
    }
}

#[test]
fn flows_bytes_per_file_and_socket_are_the_manifests() {
    let truth = manifest("flows");
    let Some(results) = measure("flows") else {
        return;
    };

    let result = labelled(&results, "syscalls");
    let syscalls = raw(result);
    let pid = pid_of(&syscalls, "flows-main").expect("the program's own system calls");
    let files = bytes_by_op_fd(&syscalls, pid);
    for file in truth["files"].as_array().expect("files") {
        let fd = file["fd"].as_i64().expect("fd");
        for (op, field) in [("write", "written"), ("read", "read")] {
            exact_unless_counted(
                files.get(&(op.to_owned(), fd)).copied().unwrap_or(0.0),
                f(&file[field]),
                result,
                &format!("{} {field} through descriptor {fd}", file["name"]),
            );
        }
    }
    socket_bytes(&results, "flows-main", &truth["socket"]);

    // The machine's: lower bounds.
    let written: f64 = truth["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| f(&file["written"]))
        .sum();
    let devices = timeline_sum(labelled(&results, "devices"), None);
    assert!(
        workloads_off_block_device().is_some() || devices >= written,
        "{written} bytes are fsynced to a disk; the devices moved {devices}"
    );
    let carried = timeline_sum(labelled(&results, "network"), Some("iface:lo"));
    let exchanged = f(&truth["socket"]["sent"]) + f(&truth["socket"]["received"]);
    assert!(
        carried >= exchanged,
        "{exchanged} bytes cross loopback; `lo` carried {carried}"
    );
}

#[test]
fn heatmap_regimes_are_the_manifests_phases_in_order() {
    let truth = manifest("heatmap");
    let Some(results) = measure("heatmap") else {
        return;
    };
    let socket = raw(labelled(&results, "socket"));
    let pid = pid_of(&socket, "heatmap-main").expect("the program's own socket calls");
    let phases = truth["phases"].as_array().expect("phases");
    let fast_ms = f(&phases[0]["pause_ms"]);
    let slow_ms = f(&phases[1]["pause_ms"]);
    // The two regimes are told apart at their geometric mean, a separation
    // rather than a threshold: on a loaded machine a fast receive waits longer
    // than its pause, and is still nearer it than the slow one.
    let split = (fast_ms * slow_ms).sqrt() * MS;
    let mut fast_events = Vec::new();
    let mut slow_events = Vec::new();
    let mut other_events = Vec::new();
    let mut slow_at_least_pause = 0.0;
    let mut received = 0.0;
    for event in socket
        .iter()
        .filter(|event| event.num("pid") == pid && event.text("op") == "recv")
    {
        let count = event.num("count").max(1.0);
        let mean = event.num("total_ns") / count;
        received += count;
        if mean < split {
            fast_events.push((event.ts, count));
        }
        if mean >= slow_ms * MS * 0.98 {
            slow_events.push((event.ts, count));
            slow_at_least_pause += count;
        } else {
            other_events.push((event.ts, count));
        }
    }
    let count = |events: &[(u64, f64)]| events.iter().map(|(_, count)| count).sum::<f64>();
    let fast_requests = f(&phases[0]["requests"]) + f(&phases[2]["requests"]);
    let slow_requests = f(&phases[1]["requests"]);
    exact_unless_counted(
        received,
        fast_requests + slow_requests,
        labelled(&results, "socket"),
        "one receive per request",
    );
    assert!(
        count(&fast_events) >= fast_requests * 0.8,
        "{fast_requests} requests wait {fast_ms} ms; {} receives waited under {:.2} ms",
        count(&fast_events),
        split / MS
    );
    assert!(
        slow_at_least_pause + dropped(labelled(&results, "socket")) as f64 >= slow_requests,
        "{slow_requests} requests wait {slow_ms} ms; {slow_at_least_pause} receives waited as long"
    );
    // In order: fast, slow, fast again. The slow phase's bounds are where the
    // tenth and the ninetieth percentile of the receives that waited its pause
    // fall, so that one fast receive a loaded machine stretched that far does
    // not move them; every other receive, stretched or not, is a fast phase's.
    // A window that straddles a boundary carries both at one timestamp.
    let mut slow_times: Vec<u64> = slow_events
        .iter()
        .flat_map(|(ts, count)| std::iter::repeat_n(*ts, *count as usize))
        .collect();
    slow_times.sort_unstable();
    assert!(!slow_times.is_empty(), "no slow regime at all");
    let early = slow_times[slow_times.len() / 10];
    let late = slow_times[slow_times.len() * 9 / 10];
    let before: Vec<_> = other_events
        .iter()
        .copied()
        .filter(|(ts, _)| *ts <= early)
        .collect();
    let after: Vec<_> = other_events
        .iter()
        .copied()
        .filter(|(ts, _)| *ts >= late)
        .collect();
    for (name, events, expected) in [
        ("before", before, f(&phases[0]["requests"])),
        ("after", after, f(&phases[2]["requests"])),
    ] {
        assert!(
            count(&events) >= expected * 0.9,
            "{expected} fast requests come {name} the slow phase; {} were seen there",
            count(&events)
        );
    }
}

#[test]
fn ledger_costs_are_the_manifests() {
    let truth = manifest("ledger");
    let Some(results) = measure("ledger") else {
        return;
    };

    let on_cpu = timeline_sum(labelled(&results, "on-cpu"), None) / S;
    let burnt = f(&truth["cpu_s"]);
    assert!(
        on_cpu >= burnt * 0.95 && on_cpu <= burnt + 0.6,
        "the program burns {burnt} s and touches its memory; on the cpu {on_cpu:.3} s"
    );

    let memory = &truth["memory"];
    let resident = timeline_points(labelled(&results, "rss"));
    let floor = resident
        .iter()
        .copied()
        .filter(|bytes| *bytes > MIB)
        .fold(f64::INFINITY, f64::min);
    let peak = resident.iter().copied().fold(0.0, f64::max);
    assert!(
        peak - floor >= f(&memory["bytes"]),
        "{} bytes are touched and held; the resident size rose by {}",
        f(&memory["bytes"]),
        peak - floor
    );

    let faults: f64 = outputs(labelled(&results, "faults"), "summary")
        .flat_map(|output| output["series"].as_array().expect("series").iter())
        .filter_map(|series| series["count"].as_f64())
        .fold(0.0, f64::max);
    let pages = f(&memory["bytes"]) / f(&memory["page"]);
    let huge = f(&memory["bytes"]) / (2.0 * MIB);
    assert!(
        faults >= huge && faults <= pages + 10_000.0,
        "touching {pages} pages is between {huge} and {pages} faults, plus the interpreter's; \
         counted {faults}"
    );

    file_bytes(&results, "ledger-main", &truth["file"]);
    socket_bytes(&results, "ledger-main", &truth["socket"]);
}

#[test]
fn system_panels_carry_at_least_the_manifests_cost() {
    let truth = manifest("system");
    let Some(results) = measure("system") else {
        return;
    };

    // `fs:proc` reads the launched process alone; `bpf:raw_tp` follows its
    // descendants, so the child's burn is in its rows too.
    let result = labelled(&results, "on-cpu");
    let on_cpu = timeline_sum(result, None) / S;
    let followed = result["backend"] == "bpf:raw_tp";
    let burnt = f(&truth["burn_s"]) * if followed { 2.0 } else { 1.0 };
    assert!(
        on_cpu >= burnt * 0.95 && on_cpu <= burnt + 0.6,
        "the program and its child burn {} s each; through {} the on-cpu time holds {burnt} s \
         of it; on the cpu {on_cpu:.3} s",
        f(&truth["burn_s"]),
        result["backend"]
    );

    let cores = std::thread::available_parallelism().map_or(1.0, |cores| cores.get() as f64);
    let busiest = timeline_points(labelled(&results, "cpu"))
        .into_iter()
        .fold(0.0, f64::max);
    assert!(
        busiest >= 1.5 / cores,
        "two processes burn at once on {cores} cores, at least {:.3} of the machine; the \
         busiest poll says {busiest:.3}",
        2.0 / cores
    );

    let written = f(&truth["file"]["written"]);
    let devices = timeline_sum(labelled(&results, "devices"), None);
    assert!(
        workloads_off_block_device().is_some() || devices >= written,
        "{written} bytes fsynced; the devices moved {devices}"
    );

    let sent = f(&truth["loopback"]["bytes"]);
    let carried = timeline_sum(labelled(&results, "network"), Some("iface:lo"));
    assert!(
        carried >= sent,
        "{sent} bytes over loopback; `lo` carried {carried}"
    );

    assert!(
        !psi_here() || !timeline_points(labelled(&results, "pressure")).is_empty(),
        "the memory pressure panel has no point at all"
    );
}

#[test]
fn lanes_schedule_is_the_manifests() {
    let truth = manifest("lanes");
    let mut session = session_text("lanes");
    let mut gpu = true;
    if !Path::new("/dev/nvidia0").exists() {
        eprintln!(
            "the lanes workload runs without its device part: this machine has no NVIDIA device"
        );
        let cut = session
            .find("  # Last, so that a machine with no device")
            .expect("the kernels request is last");
        session.truncate(cut);
        gpu = false;
    }
    let Some(results) = measure_text("lanes", &session) else {
        return;
    };
    let rounds = f(&truth["rounds"]);
    let turn = f(&truth["turn_s"]);
    let lanes: Vec<&str> = truth["lanes"]
        .as_array()
        .expect("lanes")
        .iter()
        .filter_map(Value::as_str)
        .collect();

    let switches = raw(labelled(&results, "switches"));
    let blocked = raw(labelled(&results, "blocked"));
    let wakeups = raw(labelled(&results, "wakeups"));
    let names: HashMap<i64, String> = wakeups
        .iter()
        .map(|event| (event.num("wakee_tid") as i64, event.text("comm").to_owned()))
        .collect();
    for (index, lane) in lanes.iter().enumerate() {
        let left = sum_for(&switches, lane, "count");
        assert!(
            left >= rounds,
            "`{lane}` waits for the baton {rounds} times, so it leaves the cpu at least as often; \
             counted {left}"
        );
        let waited = sum_for(&blocked, lane, "blocked_ns") / S;
        let least = (rounds - 1.0) * 2.0 * turn;
        assert!(
            waited >= least * 0.95,
            "`{lane}` waits for two other lanes' turns between its own, at least {least:.2} s; \
             blocked {waited:.3} s"
        );
        let before = lanes[(index + lanes.len() - 1) % lanes.len()];
        let woken: f64 = wakeups
            .iter()
            .filter(|event| event.text("comm") == *lane)
            .filter(|event| {
                names
                    .get(&(event.num("waker_tid") as i64))
                    .map(String::as_str)
                    == Some(before)
            })
            .map(|event| event.num("count").max(1.0))
            .sum();
        let expected = if index == 0 { rounds - 1.0 } else { rounds };
        assert!(
            woken >= expected * 0.9,
            "`{before}` passes `{lane}` the baton {expected} times; {woken} wakeups between them"
        );
    }

    if gpu {
        let kernels = labelled(&results, "kernels");
        let name = truth["gpu"]["kernel"].as_str().expect("kernel");
        let launched = raw(kernels)
            .iter()
            .filter(|event| event.text("op") == "kernel" && event.text("kernel") == name)
            .count() as f64;
        assert_eq!(
            launched,
            f(&truth["gpu"]["launches"]),
            "the workload launches {} kernels",
            f(&truth["gpu"]["launches"])
        );
    }
}

/// The request server, built once per test process, or why it cannot be.
fn request_server() -> Result<(), String> {
    static BUILT: OnceLock<Result<(), String>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let truth = manifest("request");
            let build: Vec<String> = truth["build"]
                .as_array()
                .expect("build")
                .iter()
                .filter_map(|word| word.as_str().map(str::to_owned))
                .collect();
            let output_at = build
                .iter()
                .position(|word| word == "-o")
                .map(|at| root().join(&build[at + 1]))
                .expect("the build names its output");
            std::fs::create_dir_all(output_at.parent().expect("a directory"))
                .map_err(|error| error.to_string())?;
            // Built beside and renamed, so a half-written binary is never at
            // the path the session launches.
            let partial = output_at.with_extension(std::process::id().to_string());
            let mut args: Vec<String> = build[1..].to_vec();
            let at = args.iter().position(|word| word == "-o").expect("-o");
            args[at + 1] = partial.display().to_string();
            let built = Command::new(&build[0])
                .current_dir(root())
                .args(&args)
                .output()
                .map_err(|error| format!("no C compiler (`{}`): {error}", build[0]))?;
            if !built.status.success() {
                return Err(format!(
                    "the request server did not build -- most likely <sys/sdt.h> is missing, \
                     which systemtap's development headers provide:\n{}",
                    spoken(&built)
                ));
            }
            std::fs::rename(&partial, &output_at).map_err(|error| error.to_string())
        })
        .clone()
}

#[test]
fn request_causes_are_the_manifests() {
    request_plan_is_the_manifests();
    let truth = manifest("request");
    if let Err(why) = request_server() {
        eprintln!("skipped: {why}");
        return;
    }
    let Some(results) = measure("request") else {
        return;
    };
    let result = labelled(&results, "requests");
    let kinds = truth["requests"].as_array().expect("requests");
    let total: f64 = kinds.iter().map(|kind| f(&kind["count"])).sum();
    let events: Vec<(String, Event)> = outputs(result, "raw")
        .flat_map(|output| output["events"].as_array().expect("events").iter())
        .map(|event| {
            let id = event["keys"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|key| key["req"]["id"].as_str())
                .unwrap_or_default()
                .to_owned();
            let attrs = event["attrs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|attr| Some((attr["name"].as_str()?.to_owned(), attr["value"].clone())))
                .collect();
            (id, Event { ts: 0, attrs })
        })
        .collect();
    assert_eq!(
        events.len() as f64,
        total,
        "the plan is {total} requests, every one ended"
    );
    let lock_ms = kinds
        .iter()
        .find_map(|kind| kind["lock_ms"].as_f64())
        .expect("a kind behind the lock");
    let gap = kinds
        .iter()
        .find_map(|kind| kind["gap_ms"].as_f64())
        .expect("a kind with a pause");
    for (id, event) in &events {
        // `a001` is the plan's first request: the kinds go round in order.
        let place = i64::from_str_radix(id.trim_start_matches('a'), 16)
            .unwrap_or_else(|_| panic!("a request id the server chose: {id}"));
        let kind = &kinds[(place as usize - 1) % kinds.len()];
        let cause = kind["cause"].as_str().expect("cause");
        let sleep = event.num("sleep_ns") / MS;
        let net_rx = event.num("net_rx_ns") / MS;
        let blocks = event.num("blocks");
        match cause {
            "lock_wait" => {
                let lock = lock_ms;
                assert!(
                    sleep >= lock * 0.75 && sleep <= lock * 1.5,
                    "{id} waits {lock} ms for the lock; asleep {sleep:.2} ms"
                );
            }
            "block_io" if workloads_off_block_device().is_some() => {
                eprintln!(
                    "{id}: its reads are not checked against the disk: {}",
                    workloads_off_block_device().unwrap_or_default()
                );
            }
            "block_io" => {
                assert!(
                    blocks >= f(&kind["reads"]) && event.num("disk_ns") > 0.0,
                    "{id} reads {} cold pages: as many blocks on the disk, saw {blocks} and \
                     {:.2} ms of disk",
                    f(&kind["reads"]),
                    event.num("disk_ns") / MS
                );
            }
            "net_rx" => {
                assert!(
                    net_rx >= gap * 0.95 && net_rx <= gap * 1.5,
                    "{id}'s client pauses {gap} ms halfway through; net_rx {net_rx:.2} ms"
                );
            }
            _ => {}
        }
        if cause != "lock_wait" {
            assert!(
                sleep < lock_ms / 2.0,
                "{id} ({cause}) waits for no lock, and the lock is held {lock_ms} ms; asleep \
                 {sleep:.2} ms"
            );
        }
        // A request sent at once can still wait for the server to be
        // scheduled on a loaded machine: held apart from the pause, not to zero.
        if cause != "net_rx" {
            assert!(
                net_rx < gap / 2.0,
                "{id} ({cause}) is sent at once, and the pause is {gap} ms; net_rx {net_rx:.2} ms"
            );
        }
    }
}

#[test]
fn process_carries_every_cost_at_once() {
    let truth = manifest("process");
    let Some(results) = measure("process") else {
        return;
    };
    let main = truth["thread"].as_str().expect("thread");

    if python_has_perf_map() {
        let on = python_weights(labelled(&results, "profile"));
        let ratio = weight(&on, "crunch") / weight(&on, "tidy").max(1.0);
        assert!(
            (3.0..=5.0).contains(&ratio),
            "`crunch` burns four times what `tidy` does; the profile says {ratio:.2}"
        );
        assert!(
            samples_close(weight(&on, "crunch"), f(&truth["cpu"]["seconds"]), 0.25),
            "`crunch` burns {} s; the profile has {} samples",
            f(&truth["cpu"]["seconds"]),
            weight(&on, "crunch")
        );
        let off = python_weights(labelled(&results, "blocked"));
        for (function, seconds) in [
            ("doze", f(&truth["sleep"]["seconds"])),
            ("queue_up", f(&truth["lock"]["seconds"])),
        ] {
            let measured = weight(&off, function) / S;
            assert!(
                measured >= seconds * 0.95 && measured <= seconds + 0.2,
                "`{function}` is off the cpu {seconds} s; measured {measured:.3} s"
            );
        }
    }

    let on_cpu = timeline_sum(labelled(&results, "on-cpu"), None) / S;
    let burnt = f(&truth["cpu"]["seconds"]) + f(&truth["cpu"]["minor"]["seconds"]);
    assert!(
        on_cpu >= burnt * 0.95 && on_cpu <= burnt + 0.6,
        "the program burns {burnt} s; on the cpu {on_cpu:.3} s"
    );

    let futex = labelled(&results, "futex");
    let waited = sum_for(&raw(futex), main, "total_ns") / S;
    let lock = f(&truth["lock"]["seconds"]);
    assert!(
        waited >= lock * 0.95 || dropped(futex) > 0,
        "`{main}` waits {lock} s for the lock; the futex wait says {waited:.3} s, nothing dropped"
    );

    let resident = timeline_points(labelled(&results, "rss"));
    let floor = resident
        .iter()
        .copied()
        .filter(|bytes| *bytes > MIB)
        .fold(f64::INFINITY, f64::min);
    let peak = resident.iter().copied().fold(0.0, f64::max);
    assert!(
        peak - floor >= f(&truth["memory"]["bytes"]),
        "{} bytes are touched and held; the resident size rose by {}",
        f(&truth["memory"]["bytes"]),
        peak - floor
    );

    file_bytes(&results, main, &truth["file"]);
    socket_bytes(&results, main, &truth["socket"]);
}
