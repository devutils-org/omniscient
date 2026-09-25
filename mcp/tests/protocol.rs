//! A small MCP client that speaks the protocol to `omn-mcp`, against runs the
//! real `omn` wrote.
//!
//! The ground truth for a server that only reads is the documents themselves:
//! each test runs an example session with `omn -f ... -o DIR`, points the
//! server at `DIR`, and checks that what comes back over the protocol is the
//! file's own value -- every number, loss, shortfall, evidence level and
//! timestamp error -- with only the two additions the server documents
//! (`evidence_means`, `elided`) taken out first. Where a property of the
//! machine is the point (a lossy profile, a clock from elsewhere, the
//! capabilities on the binary), the test checks that the property was created
//! and skips loudly, with the numbers, when it could not be.
//!
//! These run `target/debug/omn`, which `make` builds and grants its
//! capabilities to. The server's own tests never relink it.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn omn() -> PathBuf {
    let path = repo().join("target/debug/omn");
    assert!(
        path.is_file(),
        "{} does not exist: run `make` first, which builds it and grants its capabilities",
        path.display()
    );
    path
}

/// A directory of its own for each test, emptied first.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omn-mcp-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs an example session through the real binary, writing JSON to `out`.
fn run_example(example: &str, out: &Path) -> std::process::Output {
    let output = Command::new(omn())
        .current_dir(repo())
        .args([
            "-f",
            &format!("examples/{example}.yml"),
            "--format",
            "json",
            "-o",
        ])
        .arg(out)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "omn -f examples/{example}.yml exited {:?}:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Client {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_omn-mcp"))
            .current_dir(repo())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            stdin,
            stdout,
            next: 0,
        };
        let init = client.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "omn-mcp-tests", "version": "0"},
            }),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        client.notify("notifications/initialized");
        client
    }

    fn over(dir: &Path) -> Self {
        Self::start(&["--dir", dir.to_str().unwrap()])
    }

    fn send(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str) {
        self.send(&json!({"jsonrpc": "2.0", "method": method}));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("not a JSON-RPC line ({error}): {line}"));
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    /// A tool call that must succeed; returns its structured content, after
    /// checking the text content says the same thing.
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let reply = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        let result = &reply["result"];
        assert_eq!(result["isError"], false, "{tool} refused: {reply}");
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["structuredContent"], "the two contents differ");
        result["structuredContent"].clone()
    }

    /// A tool call that must be refused; returns the reason.
    fn refused(&mut self, tool: &str, arguments: Value) -> String {
        let reply = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        assert_eq!(
            reply["result"]["isError"], true,
            "{tool} was expected to refuse: {reply}"
        );
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Takes out the keys the server documents as its own additions.
fn strip_additions(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("evidence_means");
            object.remove("elided");
            for (_, child) in object.iter_mut() {
                strip_additions(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_additions),
        _ => {}
    }
}

/// The first path at which two values differ, for a readable failure.
fn first_difference(path: &str, left: &Value, right: &Value) -> Option<String> {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys()) {
                let (x, y) = (a.get(key), b.get(key));
                match (x, y) {
                    (Some(x), Some(y)) => {
                        if let Some(found) = first_difference(&format!("{path}.{key}"), x, y) {
                            return Some(found);
                        }
                    }
                    _ => return Some(format!("{path}.{key}: {x:?} against {y:?}")),
                }
            }
            None
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Some(format!("{path}: {} items against {}", a.len(), b.len()));
            }
            a.iter()
                .zip(b)
                .enumerate()
                .find_map(|(i, (x, y))| first_difference(&format!("{path}[{i}]"), x, y))
        }
        _ => (left != right).then(|| format!("{path}: {left} against {right}")),
    }
}

fn assert_same(what: &str, served: &Value, written: &Value) {
    if let Some(difference) = first_difference(what, served, written) {
        panic!("the server changed what omn wrote: {difference}");
    }
}

/// Every leaf of a document that is a number, with its path: the count the
/// comparison covered, so a vacuous equality cannot pass.
fn numbers(value: &Value) -> usize {
    match value {
        Value::Number(_) => 1,
        Value::Object(object) => object.values().map(numbers).sum(),
        Value::Array(items) => items.iter().map(numbers).sum(),
        _ => 0,
    }
}

/// Checks a whole `read_run` answer against the three files, key by key.
fn assert_run_unchanged(answer: &Value, dir: &Path) -> usize {
    let results = read_json(&dir.join("results.json"));
    let resolved = read_json(&dir.join("resolved.json"));
    let cost = read_json(&dir.join("cost.json"));
    let requests = answer["requests"].as_array().unwrap();
    assert_eq!(requests.len(), results["results"].as_array().unwrap().len());
    let mut compared = 0;
    for (index, request) in requests.iter().enumerate() {
        let mut result = request["result"].clone();
        strip_additions(&mut result);
        assert_same(
            &format!("results[{index}]"),
            &result,
            &results["results"][index],
        );
        let mut served = request["resolved"].clone();
        served["environment"] = answer["environment"].clone();
        assert_same(&format!("resolved[{index}]"), &served, &resolved[index]);
        assert_same(&format!("cost[{index}]"), &request["cost"], &cost[index]);
        compared +=
            numbers(&results["results"][index]) + numbers(&resolved[index]) + numbers(&cost[index]);
    }
    for key in ["joins", "decompositions"] {
        match results.get(key) {
            Some(written) => {
                let mut served = answer[key].clone();
                strip_additions(&mut served);
                assert_same(key, &served, written);
                compared += numbers(written);
            }
            None => assert!(answer.get(key).is_none(), "{key} appeared from nowhere"),
        }
    }
    compared
}

#[test]
fn every_number_of_a_run_comes_back_unchanged() {
    let dir = scratch("unchanged");
    let run = dir.join("decompose");
    run_example("join-decompose-compute", &run);

    let mut client = Client::over(&dir);
    let listed = client.call("list_runs", json!({}));
    assert_eq!(listed["runs"][0]["run"], "decompose", "{listed}");

    let answer = client.call("read_run", json!({"run": "decompose", "max_events": 0}));
    let compared = assert_run_unchanged(&answer, &run);
    assert!(
        compared > 100,
        "only {compared} numbers were compared: the run is not the one this test meant"
    );
    assert!(answer["decompositions"][0]["tables"][0]["residue"].is_number());
    assert_eq!(answer["missing"], json!([]), "{}", answer["missing"]);

    // The resources are the files byte for byte.
    for file in ["resolved.json", "results.json", "cost.json"] {
        let reply = client.request(
            "resources/read",
            json!({"uri": format!("omn://runs/decompose/{file}")}),
        );
        assert_eq!(
            reply["result"]["contents"][0]["text"].as_str().unwrap(),
            std::fs::read_to_string(run.join(file)).unwrap(),
            "{file}"
        );
    }
}

#[test]
fn an_answer_cut_short_says_how_short() {
    let dir = scratch("elided");
    let run = dir.join("between");
    run_example("join-between-residency", &run);
    let written = read_json(&run.join("results.json"));

    let mut client = Client::over(&dir);
    let answer = client.call("read_run", json!({"run": "between", "max_events": 2}));
    let mut cut = 0;
    for (index, request) in answer["requests"].as_array().unwrap().iter().enumerate() {
        for (at, output) in request["result"]["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let original = &written["results"][index]["outputs"][at];
            let Some(events) = original["events"].as_array() else {
                continue;
            };
            if events.len() <= 2 {
                continue;
            }
            cut += 1;
            assert_eq!(output["elided"]["events_in_the_document"], events.len());
            assert_same(
                "events kept",
                &output["events"],
                &Value::Array(events[..2].to_vec()),
            );
        }
    }
    assert!(cut > 0, "no raw output had more than two events to cut");
    assert_eq!(
        answer["missing"].as_array().unwrap().len(),
        cut,
        "one line per cut: {}",
        answer["missing"]
    );
}

#[test]
fn a_lossy_run_answers_with_its_shortfall() {
    let dir = scratch("lossy");
    let run = dir.join("lossy");
    run_example("cross-compute-lossy-profile", &run);
    let written = read_json(&run.join("results.json"));
    let profile = &written["results"][0];
    let losses = &profile["losses"];
    let dropped = losses["dropped"].as_u64().unwrap();
    let throttled = losses["throttled"].as_u64().unwrap();
    if dropped == 0 && throttled == 0 {
        let rate = std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
            .unwrap_or_default();
        eprintln!(
            "SKIP: a 100 kHz profile lost nothing here (dropped 0, throttled 0, \
             kernel.perf_event_max_sample_rate {}): the property this test needs was not \
             created",
            rate.trim()
        );
        return;
    }

    let mut client = Client::over(&dir);
    let listed = client.call("list_runs", json!({}));
    let marked = &listed["runs"][0]["short_or_lossy"];
    assert!(
        marked
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["label"] == "on-cpu" && entry["carries_a_shortfall"] == true),
        "the index does not mark the lossy profile: {marked}"
    );

    // Narrowed to the one request, and with its raw events cut, the answer
    // still carries every mark the document put on it.
    let answer = client.call(
        "read_run",
        json!({"run": "lossy", "label": "on-cpu", "max_events": 1}),
    );
    let request = &answer["requests"][0];
    let missing = &request["what_it_is_missing"];
    assert_eq!(missing["losses"]["dropped"], dropped);
    assert_eq!(missing["losses"]["throttled"], throttled);
    assert_eq!(missing["loss_reasons"], losses["reasons"]);
    let marks: Vec<&Value> = profile["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|output| output["series"].as_array().into_iter().flatten())
        .filter_map(|series| series.get("short"))
        .collect();
    assert!(
        !marks.is_empty(),
        "the lossy profile carries no `short` mark"
    );
    let hoisted: Vec<&Value> = missing["shortfalls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| &entry["short"])
        .collect();
    assert_eq!(hoisted, marks, "every mark, hoisted, unchanged");
    let mut result = request["result"].clone();
    strip_additions(&mut result);
    assert_same("the lossy result", &result, profile);
    assert!(
        answer["missing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().unwrap().contains("joins and decompositions")),
        "narrowing to a label does not say what it left out: {}",
        answer["missing"]
    );
}

#[test]
fn identity_and_temporal_never_read_alike() {
    let dir = scratch("evidence");
    run_example("join-between-residency", &dir.join("temporal"));
    let server = repo().join("target/omn-vertical/server");
    let vertical = server.is_file();
    if vertical {
        run_example("vertical-one-request", &dir.join("identity"));
    } else {
        eprintln!(
            "SKIP (half): {} is not built, so no IDENTITY row can be produced here; `make` \
             builds it. The TEMPORAL half still runs.",
            server.display()
        );
    }

    let mut client = Client::over(&dir);
    let mut seen = std::collections::BTreeMap::new();
    for run in ["temporal", "identity"]
        .iter()
        .take(if vertical { 2 } else { 1 })
    {
        let answer = client.call("read_run", json!({"run": run, "max_events": 0}));
        assert_run_unchanged(&answer, &dir.join(run));
        let mut stack = vec![answer.clone()];
        while let Some(value) = stack.pop() {
            match value {
                Value::Object(object) => {
                    if let Some(Value::String(level)) = object.get("evidence") {
                        let means = &object["evidence_means"];
                        assert_eq!(&means["level"], level, "{means}");
                        seen.insert(level.clone(), means.clone());
                    }
                    stack.extend(object.into_iter().map(|(_, child)| child));
                }
                Value::Array(items) => stack.extend(items),
                _ => {}
            }
        }
        assert_eq!(answer["evidence_ladder"].as_array().unwrap().len(), 4);
    }
    let temporal = seen.get("TEMPORAL").expect("the between join is TEMPORAL");
    assert!(
        temporal["establishes"]
            .as_str()
            .unwrap()
            .contains("hypothesis"),
        "{temporal}"
    );
    assert_eq!(temporal["rank"], "1 of 4, weakest first");
    if vertical {
        let identity = seen
            .get("IDENTITY")
            .expect("the vertical case has IDENTITY rows");
        for field in ["rank", "establishes", "does_not_establish"] {
            assert_ne!(temporal[field], identity[field], "{field}");
        }
        assert_eq!(identity["rank"], "4 of 4, weakest first");
    }
}

#[test]
fn a_timestamp_from_another_clock_keeps_its_error() {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("SKIP: no `node` here, and runtime.jit's map needs a runtime that writes one");
        return;
    }
    let dir = scratch("clock");
    let run = dir.join("jit");
    run_example("runtime-jit", &run);
    let written = read_json(&run.join("results.json"));
    let worst = written["results"][0]["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|output| output["events"].as_array().into_iter().flatten())
        .filter_map(|event| event["ts_err_ns"].as_u64())
        .max();
    let Some(worst) = worst else {
        eprintln!(
            "SKIP: node compiled nothing this run's map recorded, so no event carries ts_err_ns"
        );
        return;
    };

    let mut client = Client::over(&dir);
    let answer = client.call("read_run", json!({"run": "jit", "max_events": 0}));
    assert_run_unchanged(&answer, &run);
    let error = &answer["requests"][0]["what_it_is_missing"]["timestamp_error"];
    assert_eq!(error["worst_ts_err_ns"], worst, "{error}");
    assert!(worst > 0);
}

#[test]
fn the_sweep_is_the_one_the_binary_that_runs_gives() {
    let dir = scratch("sweep");
    let listing = Command::new(omn())
        .args(["list", "--color", "never"])
        .output()
        .unwrap();
    let text = String::from_utf8(listing.stdout).unwrap();
    let footer = text
        .lines()
        .find(|line| line.contains(" sources: "))
        .unwrap();
    let said: Vec<u64> = footer
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|part| part.parse().ok())
        .collect();

    let mut client = Client::over(&dir);
    let sweep = client.call("capabilities", json!({}));
    let totals = &sweep["totals"];
    assert_eq!(
        [
            &totals["sources"],
            &totals["available"],
            &totals["unverified"],
            &totals["unavailable"]
        ],
        [
            &json!(said[0]),
            &json!(said[1]),
            &json!(said[2]),
            &json!(said[3])
        ],
        "{footer}"
    );

    // The capabilities it reports are the ones `getcap` reads off the file.
    let getcap = Command::new("getcap").arg(omn()).output();
    let carries = sweep["answered_by"]["file_capabilities"]["cap_bpf"] == true;
    match getcap {
        Ok(output) => assert_eq!(
            String::from_utf8_lossy(&output.stdout).contains("cap_bpf"),
            carries,
            "{}",
            sweep["answered_by"]
        ),
        Err(error) => {
            eprintln!("note: no getcap here ({error}); the capability bits are unchecked")
        }
    }

    // One source in full carries the probe's reasons.
    let one = client.call("capabilities", json!({"source": "sched.offcpu"}));
    assert!(one["listing"].as_str().unwrap().contains("needs"), "{one}");

    // The same binary without its capabilities answers a different sweep,
    // and the answer says why rather than reading like a different machine.
    if !carries {
        eprintln!(
            "SKIP (half): {} carries no CAP_BPF, so there is no privileged sweep to separate \
             from an unprivileged one; `make` grants it",
            omn().display()
        );
        return;
    }
    let bare = dir.join("omn");
    std::fs::copy(omn(), &bare).unwrap();
    let mut unprivileged = Client::start(&[
        "--dir",
        dir.to_str().unwrap(),
        "--omn",
        bare.to_str().unwrap(),
    ]);
    let lesser = unprivileged.call("capabilities", json!({}));
    let (with, without) = (
        totals["available"].as_u64().unwrap(),
        lesser["totals"]["available"].as_u64().unwrap(),
    );
    assert!(
        without < with,
        "{without} available without capabilities, {with} with"
    );
    assert!(
        lesser["missing"][0]
            .as_str()
            .unwrap()
            .contains("neither CAP_BPF nor CAP_PERFMON"),
        "{}",
        lesser["missing"]
    );
    assert_eq!(sweep["missing"], json!([]));
}

#[test]
fn a_plan_says_what_it_would_serve_and_degrade_and_runs_nothing() {
    let dir = scratch("plan");
    let marker = dir.join("the-program-ran");
    // A session whose program would leave a mark if anything launched it.
    let session = format!(
        "requests:\n  - label: profile\n    source: cpu.profile\n    target: {{launch: [/bin/sh, -c, 'touch {}']}}\n    mode: sample@997hz\n    emit: [topk]\n",
        marker.display()
    );
    let mut client = Client::over(&dir);
    let plan = client.call("plan_session", json!({"session": session}));
    assert!(!marker.exists(), "planning a session ran its program");
    assert_eq!(plan["validation"]["valid"], true, "{plan}");

    // Against the binary's own resolution of the same session.
    let session_file = dir.join("plan.yml");
    std::fs::write(&session_file, &session).unwrap();
    let dumped = Command::new(omn())
        .args([
            "-f",
            session_file.to_str().unwrap(),
            "--dump-resolved",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(!marker.exists(), "--dump-resolved ran the program");
    let resolved: Value = serde_json::from_slice(&dumped.stdout).unwrap();
    let planned = &plan["resolution"]["requests"][0];
    assert_eq!(planned["backend"], resolved[0]["backend"]["id"]);
    assert_eq!(planned["rank_in_chain"], resolved[0]["backend"]["rank"]);
    assert_eq!(planned["cost"]["band"], resolved[0]["backend"]["cost"]);
    assert_eq!(planned["degradations"], resolved[0]["degradations"]);
    assert_eq!(planned["chain"], resolved[0]["chain"]);
    assert!(planned["cost"]["not"]
        .as_str()
        .unwrap()
        .contains("finding 47"));
    // And it says ahead of time that this one would not go through run_session.
    assert_eq!(plan["through_run_session"]["allowed"], false, "{plan}");

    // A session that cannot mean anything is refused with the reason, and the
    // machine is not asked.
    let invalid = "requests:\n  - label: wrong\n    source: cpu.profile\n    target: system\n    mode: count\n    emit: [topk]\n";
    let refused = client.call("plan_session", json!({"session": invalid}));
    assert_eq!(refused["validation"]["valid"], false, "{refused}");
    let rejection = &refused["validation"]["rejections"][0];
    assert_eq!(rejection["request"], "wrong", "{rejection}");
    assert!(
        rejection["message"].as_str().unwrap().contains("count"),
        "{rejection}"
    );
    assert!(refused["resolution"]
        .as_str()
        .unwrap()
        .starts_with("not attempted"));
}

#[test]
fn a_call_that_would_change_the_machine_is_refused() {
    let dir = scratch("refused");
    let marker = dir.join("the-program-ran");
    let launching = format!(
        "requests:\n  - source: proc.fd\n    target: {{launch: [/bin/sh, -c, 'touch {}']}}\n    mode: poll@100ms\n    emit: [summary]\n",
        marker.display()
    );

    // Without --allow-run, nothing runs at all.
    let mut reader = Client::over(&dir);
    let reason = reader.refused(
        "run_session",
        json!({"session": launching, "duration": "1s", "budget": "5%"}),
    );
    assert!(reason.contains("--allow-run"), "{reason}");

    // With it, a launch is still refused: it would run a program the caller
    // named.
    let mut runner = Client::start(&["--dir", dir.to_str().unwrap(), "--allow-run", "3s"]);
    let reason = runner.refused(
        "run_session",
        json!({"session": launching, "duration": "1s", "budget": "5%"}),
    );
    assert!(reason.contains("launch"), "{reason}");

    // And the bounds are the server's, not the caller's.
    let existing = format!(
        "requests:\n  - source: proc.fd\n    target: {{pid: [{}]}}\n    mode: poll@100ms\n    emit: [summary]\n",
        std::process::id()
    );
    let reason = runner.refused(
        "run_session",
        json!({"session": existing, "duration": "1h", "budget": "5%"}),
    );
    assert!(reason.contains("at most 3s"), "{reason}");
    let reason = runner.refused(
        "run_session",
        json!({"session": existing, "duration": "1s"}),
    );
    assert!(reason.contains("budget"), "{reason}");

    // Reading stays under the directory the server was pointed at.
    for name in ["..", "/etc", "a/../../.."] {
        let reason = runner.refused("read_run", json!({"run": name}));
        assert!(
            reason.contains("not a run") || reason.contains("no run"),
            "{reason}"
        );
    }
    let reply = runner.request("resources/read", json!({"uri": "omn://runs/../etc/passwd"}));
    assert!(reply["error"].is_object(), "{reply}");

    assert!(!marker.exists(), "a refused call ran the program");
    assert!(
        !dir.join("mcp-runs").exists(),
        "a refused call wrote a run directory"
    );
}

#[test]
fn a_bounded_run_over_an_existing_process_reads_back_what_the_process_reports() {
    let dir = scratch("run");
    // The workload opens a known number of descriptors, says how many it
    // holds by its own count, and keeps a cpu busy so the budget has a
    // denominator.
    let mut workload = Command::new("python3")
        .args([
            "-c",
            "import os, sys, time\n\
             held = [open('/dev/null') for _ in range(17)]\n\
             sys.stdout.write('FDS %d\\n' % (len(os.listdir('/proc/self/fd')) - 1))\n\
             sys.stdout.flush()\n\
             end = time.time() + 20\n\
             while time.time() < end: pass\n",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(workload.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let held: f64 = line.trim().strip_prefix("FDS ").unwrap().parse().unwrap();
    let session = format!(
        "requests:\n  - label: descriptors\n    source: proc.fd\n    target: {{pid: [{}]}}\n    mode: poll@100ms\n    emit: [summary]\n",
        workload.id()
    );

    let mut client = Client::start(&["--dir", dir.to_str().unwrap(), "--allow-run", "5s"]);
    let answer = client.call(
        "run_session",
        json!({"session": session, "duration": "1s", "budget": "5%"}),
    );
    let _ = workload.kill();
    let _ = workload.wait();

    assert_eq!(answer["exit"], 0, "{answer}");
    let request = &answer["read_back"]["requests"][0];
    let series = &request["result"]["outputs"][0]["series"][0];
    assert_eq!(series["min"], held, "{series}");
    assert_eq!(series["max"], held, "{series}");
    assert_eq!(
        request["what_it_is_missing"]["stopped_because"],
        "the requested duration elapsed"
    );
    assert_eq!(request["cost"]["label"], "descriptors");
    assert!(
        request["cost"]["measured_percent"].is_number(),
        "{}",
        request["cost"]
    );

    // What was read back is what is on disk, and reading it again gives the
    // same thing.
    let name = answer["run"].as_str().unwrap();
    let again = client.call("read_run", json!({"run": name, "max_events": 0}));
    assert_run_unchanged(&again, &dir.join(name));
}
