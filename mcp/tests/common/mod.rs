//! What every test of `omn-mcp` shares: a small MCP client that speaks the
//! protocol to the server, the example runner, and the key-by-key comparison
//! of an answer against the files `omn` wrote.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

pub fn omn() -> PathBuf {
    let path = repo().join("target/debug/omn");
    assert!(
        path.is_file(),
        "{} does not exist: run `make` first, which builds it and grants its capabilities",
        path.display()
    );
    path
}

/// A directory of its own for each test, emptied first.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omn-mcp-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs an example session through the real binary, writing JSON to `out`.
pub fn run_example(example: &str, out: &Path) -> std::process::Output {
    let output = run_example_as_is(example, out);
    assert!(
        output.status.success(),
        "omn -f examples/{example}.yml exited {:?}:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// The machine's refusal of a valid session, when that is what `omn` said:
/// exit 1, a source with no viable backend here or one the kernel refused to
/// open, and the reason as the binary wrote it. Anything else is not the
/// machine's answer and is left to fail.
pub fn withheld(output: &std::process::Output) -> Option<String> {
    let said = String::from_utf8_lossy(&output.stderr);
    let machine = output.status.code() == Some(1)
        && (said.contains("no viable backend on this machine")
            || (said.contains("cannot start collecting") && said.contains("perf_event_paranoid")));
    machine.then(|| {
        said.lines()
            .filter(|line| line.starts_with("error:") || line.trim_start().starts_with("pmu"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// Runs an example, or says out loud that this machine withholds what it
/// needs -- with the machine's own reason -- and what would have been checked.
/// `cpu.profile` at `perf_event_paranoid` 2 or 4 is the case this exists for
/// (the CI's first and second runs): the session is valid and the machine
/// refuses it, which is an answer about the machine, not a failure of the
/// server.
pub fn run_example_or_skip(example: &str, out: &Path, would_have_checked: &str) -> bool {
    let output = run_example_as_is(example, out);
    if output.status.success() {
        return true;
    }
    if let Some(reason) = withheld(&output) {
        eprintln!(
            "SKIP: examples/{example}.yml is valid and this machine cannot serve it, so it \
             was not checked {would_have_checked}. The machine's reason:\n{reason}"
        );
        return false;
    }
    panic!(
        "omn -f examples/{example}.yml exited {:?}:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn run_example_as_is(example: &str, out: &Path) -> std::process::Output {
    Command::new(omn())
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
        .unwrap()
}

pub fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Client {
    pub fn start(args: &[&str]) -> Self {
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

    pub fn over(dir: &Path) -> Self {
        Self::start(&["--dir", dir.to_str().unwrap()])
    }

    pub fn send(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    pub fn notify(&mut self, method: &str) {
        self.send(&json!({"jsonrpc": "2.0", "method": method}));
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
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
    pub fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let reply = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        let result = &reply["result"];
        assert_eq!(result["isError"], false, "{tool} refused: {reply}");
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["structuredContent"], "the two contents differ");
        result["structuredContent"].clone()
    }

    /// A tool call that must be refused; returns the reason.
    pub fn refused(&mut self, tool: &str, arguments: Value) -> String {
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
pub fn strip_additions(value: &mut Value) {
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
pub fn first_difference(path: &str, left: &Value, right: &Value) -> Option<String> {
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

pub fn assert_same(what: &str, served: &Value, written: &Value) {
    if let Some(difference) = first_difference(what, served, written) {
        panic!("the server changed what omn wrote: {difference}");
    }
}

/// Every leaf of a document that is a number, with its path: the count the
/// comparison covered, so a vacuous equality cannot pass.
pub fn numbers(value: &Value) -> usize {
    match value {
        Value::Number(_) => 1,
        Value::Object(object) => object.values().map(numbers).sum(),
        Value::Array(items) => items.iter().map(numbers).sum(),
        _ => 0,
    }
}

/// Checks a whole `read_run` answer against the three files, key by key.
pub fn assert_run_unchanged(answer: &Value, dir: &Path) -> usize {
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
