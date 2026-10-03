//! Programs nobody wrote for Omniscient, asked why they are slow.
//!
//! ROADMAP phase 12. Every other ground truth in this suite is a workload
//! written for the test. A case is a program that was not -- the distribution's
//! `sqlite3`, CPython, `gcc` -- with a cause put in front of it from outside
//! (an input that defeats an index, a second process holding a lock) and an
//! account of that cause the tool never reads: the program's own (`.timer`,
//! `EXPLAIN QUERY PLAN`, `cProfile`), or the injecting process's clock.
//!
//! **A case is data.** `cases/<name>/case.yml` says what the program is and
//! how it was obtained, what to fetch and where, what the machine must have,
//! the causes, and for each cause the question, the reference and the verdict
//! expected. This file discovers every directory and runs every cause **both
//! ways**: `omn latency` as a person would ask it, and `omn-mcp` as an agent
//! would -- the question's own session, approved by the person who started the
//! server, run through `run_session` and read back from what the run wrote.
//! `docs/cases.md` describes the format and how to add a case.
//!
//! **The references are read by extractors**, a small registry below keyed by
//! name ([`extractor`]): a case names one, and adding a program with an
//! account of a new shape means adding one function here and nothing under
//! `src/`.
//!
//! **What a check is.** Each cause is held to named checks: the first suspect
//! is the known cause, both ways name the same one, every number the verdict
//! cites agrees with the reference within the reference's resolution, and the
//! verdict says how it knows. A check a case lists under `open_findings` has
//! been filed against the file that got it wrong; while it fails, the test
//! says so and passes, and once it passes the test fails until the line is
//! removed. A case never carries a workaround.
//!
//! **What is not asserted is what the machine was doing.** Six agents share
//! the development machine. A cause the reference says was not there to be
//! found -- the lock released before the target ever asked for it -- is a
//! skip with the numbers, not a pass.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

const OMN: &str = env!("CARGO_BIN_EXE_omn");

/// The budget the agent's way passes to `run_session`, which requires one.
/// The person's way asks with none, and a budget is a share of the target's
/// own cpu: a program that waits 1.5 s on a lock burns a millisecond, the
/// collectors several, and any ceiling a model would pick stops the run
/// before the wait is over -- the question answered would be another one.
/// Filed in `docs/fixes/w17-cases.md`, "a waiting program exceeds every
/// budget".
const UNBOUNDED_BUDGET: &str = "100000%";

/// How long a cause's run may last, either way, before it is a failure.
const RUN_CEILING: Duration = Duration::from_secs(180);

// ------------------------------------------------------------------- format

/// `cases/<name>/case.yml`. Every field is described in `docs/cases.md`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    about: String,
    program: Program,
    #[serde(default)]
    needs: Vec<Need>,
    /// `local` (the default) or `debuginfod`.
    #[serde(default)]
    symbols: Option<String>,
    #[serde(default)]
    fetch: Vec<Fetch>,
    #[serde(default)]
    setup: Vec<Vec<String>>,
    causes: Vec<Cause>,
    #[serde(default)]
    open_findings: Vec<OpenFinding>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Program {
    command: String,
    obtained: String,
    #[serde(default)]
    version: Option<Vec<String>>,
    /// The file carries no `.symtab`; checked, since the case is about it.
    #[serde(default)]
    stripped: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Need {
    run: Vec<String>,
    #[serde(rename = "for")]
    purpose: String,
}

/// A file not on the machine, fetched once into the cache, never the tree.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fetch {
    url: String,
    sha256: String,
    /// The file's name under `{cache}`.
    file: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cause {
    name: String,
    on: On,
    question: String,
    objective: String,
    injection: Injection,
    target: Vec<String>,
    /// Where the program writes its own account: `stdout` is handed to its
    /// stderr, which both ways read back; `stderr` is read as it is.
    #[serde(default)]
    own_account: Option<String>,
    #[serde(default)]
    reference: Vec<Reference>,
    #[serde(default)]
    holds: Vec<Hold>,
    expect: Expect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
enum On {
    #[serde(rename = "cpu")]
    Cpu,
    #[serde(rename = "off-cpu")]
    OffCpu,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Injection {
    /// `input`, `process`, `build` or `cache`: what kind of thing was put in
    /// front of the program. Only `process` runs anything.
    kind: String,
    about: String,
    #[serde(default)]
    run: Option<Vec<String>>,
    /// The line the injecting process prints once the cause is in place.
    #[serde(default)]
    ready: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    name: String,
    extractor: String,
    /// `target` (the measured run's own account) or `injection` (the
    /// injecting process's stdout). Absent, `run` is.
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    run: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hold {
    value: String,
    #[serde(default)]
    at_least: Option<f64>,
    #[serde(default)]
    at_most: Option<f64>,
    why: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expect {
    /// The verdict's `limit`, as `omn latency` names it.
    limit: String,
    /// The state that dominates, before any `:` qualifier (`blocked`, not
    /// `blocked: sleeping`): the decomposition's row.
    state: String,
    /// The dominant stack's leaf: the hottest function on a cpu, the frame a
    /// dominant block began in off it.
    #[serde(default)]
    leaf: Option<String>,
    /// A frame the dominant stack must pass through.
    #[serde(default)]
    through: Option<String>,
    #[serde(default)]
    never_says: Vec<String>,
    #[serde(default)]
    agrees: Vec<Agree>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Agree {
    row: String,
    with: String,
    /// What the reference's account covers of the program's life: the rest
    /// of the life is slack.
    #[serde(default)]
    outside: Option<String>,
    #[serde(default)]
    relative: Option<f64>,
    #[serde(default)]
    slack_ns: Option<f64>,
    /// A reference value added to the slack.
    #[serde(default)]
    plus: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenFinding {
    check: String,
    filed: String,
}

// --------------------------------------------------------------- extractors

/// One number of a reference's account, with the resolution it was given at.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Reading {
    value: f64,
    resolution: f64,
}

type Account = BTreeMap<String, Reading>;
type Extractor = fn(&str) -> Result<Account, String>;

/// The registry: a case names its extractor, and a new kind of account is a
/// new entry here.
fn extractor(name: &str) -> Option<Extractor> {
    let registry: [(&str, Extractor); 4] = [
        ("sqlite3-timer", sqlite3_timer),
        ("sqlite3-stats", sqlite3_stats),
        ("sqlite3-plan", sqlite3_plan),
        ("key-value", key_value),
    ];
    registry
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, function)| *function)
}

/// `.timer on`: one `Run Time: real R user U sys S` line per statement, in
/// seconds to the microsecond. Summed over the statements: `real_ns`,
/// `cpu_ns` (user and sys, which getrusage adds up exactly however it splits
/// them) and `off_cpu_ns`, the statements' wall clock not on a cpu.
fn sqlite3_timer(text: &str) -> Result<Account, String> {
    let (mut real, mut cpu, mut statements) = (0.0, 0.0, 0usize);
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("Run Time:") else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let field = |name: &str| -> Result<f64, String> {
            fields
                .iter()
                .position(|word| *word == name)
                .and_then(|at| fields.get(at + 1))
                .and_then(|value| value.parse::<f64>().ok())
                .ok_or_else(|| format!("no `{name}` in the `.timer` line {line:?}"))
        };
        real += field("real")? * 1e9;
        cpu += (field("user")? + field("sys")?) * 1e9;
        statements += 1;
    }
    if statements == 0 {
        return Err("no `Run Time:` line: the program wrote no `.timer` account".to_owned());
    }
    // Each figure is printed to the microsecond.
    let printed = 1e3 * statements as f64;
    Ok(BTreeMap::from([
        (
            "real_ns".to_owned(),
            Reading {
                value: real,
                resolution: printed,
            },
        ),
        (
            "cpu_ns".to_owned(),
            Reading {
                value: cpu,
                resolution: 2.0 * printed,
            },
        ),
        (
            "off_cpu_ns".to_owned(),
            Reading {
                value: (real - cpu).max(0.0),
                resolution: 3.0 * printed,
            },
        ),
    ]))
}

/// `.stats on`: the counters of the last statement that ran, exact.
fn sqlite3_stats(text: &str) -> Result<Account, String> {
    let mut account = Account::new();
    for (label, key) in [
        ("Fullscan Steps:", "fullscan_steps"),
        ("Virtual Machine Steps:", "vm_steps"),
        ("Sort Operations:", "sorts"),
        ("Autoindex Inserts:", "autoindex_inserts"),
    ] {
        // The last block is the statement's; an earlier one is a
        // dot-command's or an EXPLAIN's.
        if let Some(value) = text
            .lines()
            .filter_map(|line| line.trim().strip_prefix(label))
            .filter_map(|value| value.trim().parse::<f64>().ok())
            .next_back()
        {
            account.insert(
                key.to_owned(),
                Reading {
                    value,
                    resolution: 0.0,
                },
            );
        }
    }
    if account.is_empty() {
        return Err("no `.stats` block: the program wrote no statement counters".to_owned());
    }
    Ok(account)
}

/// `EXPLAIN QUERY PLAN`: how many of the plan's loops scan a table and how
/// many search an index or a key.
fn sqlite3_plan(text: &str) -> Result<Account, String> {
    let steps: Vec<&str> = text
        .lines()
        .map(|line| line.trim_start_matches(['|', '`', '-', ' ']))
        .collect();
    if !text.contains("QUERY PLAN") {
        return Err(format!("not a query plan: {text:?}"));
    }
    let count = |word: &str| steps.iter().filter(|step| step.starts_with(word)).count() as f64;
    let exact = |value| Reading {
        value,
        resolution: 0.0,
    };
    Ok(BTreeMap::from([
        ("scans".to_owned(), exact(count("SCAN "))),
        ("searches".to_owned(), exact(count("SEARCH "))),
    ]))
}

/// `key=value` words, numbers only, the last of each key: an injecting
/// process's own clock. The resolution is the case's to state.
fn key_value(text: &str) -> Result<Account, String> {
    let mut account = Account::new();
    for word in text.split_whitespace() {
        if let Some((key, value)) = word.split_once('=') {
            if let Ok(value) = value.parse::<f64>() {
                account.insert(
                    key.to_owned(),
                    Reading {
                        value,
                        resolution: 0.0,
                    },
                );
            }
        }
    }
    if account.is_empty() {
        return Err(format!("no `key=value` number in {text:?}"));
    }
    Ok(account)
}

// ---------------------------------------------------------------- discovery

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `cases/*/case.yml`, parsed, in name order.
fn cases() -> Vec<(PathBuf, Case)> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(repo().join("cases"))
        .expect("cases/ exists")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("case.yml").is_file())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|dir| {
            let text = std::fs::read_to_string(dir.join("case.yml")).unwrap();
            let case: Case = serde_norway::from_str(&text)
                .unwrap_or_else(|error| panic!("{}/case.yml: {error}", dir.display()));
            (dir, case)
        })
        .collect()
}

/// What a case's words stand for: `{dir}` its scratch directory, `{case}`
/// its directory in the tree, `{cache}` where fetched files are kept.
struct Places {
    dir: PathBuf,
    case: PathBuf,
    cache: PathBuf,
}

impl Places {
    fn of(case_dir: &Path, name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("omn-case-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .unwrap_or_else(std::env::temp_dir)
            .join("omniscient/cases")
            .join(name);
        Self {
            dir,
            case: case_dir.to_path_buf(),
            cache,
        }
    }

    fn word(&self, word: &str) -> String {
        word.replace("{dir}", &self.dir.to_string_lossy())
            .replace("{case}", &self.case.to_string_lossy())
            .replace("{cache}", &self.cache.to_string_lossy())
    }

    fn argv(&self, argv: &[String]) -> Vec<String> {
        argv.iter().map(|word| self.word(word)).collect()
    }
}

impl Drop for Places {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ------------------------------------------------------------------ running

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Runs a command to its end, or kills it at `ceiling`.
fn run(argv: &[String], cwd: &Path, ceiling: Duration) -> Result<Output, String> {
    let (program, args) = argv.split_first().ok_or("an empty command")?;
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("`{program}` could not be started: {error}"))?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = out.read_to_end(&mut bytes);
        bytes
    });
    let err_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = err.read_to_end(&mut bytes);
        bytes
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if start.elapsed() > ceiling {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "`{}` ran past {ceiling:?} and was killed",
                argv.join(" ")
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(Output {
        status,
        stdout: reader.join().unwrap(),
        stderr: err_reader.join().unwrap(),
    })
}

/// A process put in front of the target, running beside it.
struct Injected {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Injected {
    /// Starts it and waits for its `ready` line.
    fn start(argv: &[String], ready: Option<&str>, cwd: &Path) -> Result<Self, String> {
        let (program, args) = argv.split_first().ok_or("an empty injection")?;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("the injection `{program}` could not be started: {error}"))?;
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let injected = Self { child, lines };
        if let Some(ready) = ready {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match injected.lines.recv_timeout(left) {
                    Ok(line) if line.contains(ready) => break,
                    Ok(_) => {}
                    Err(_) => {
                        return Err(format!(
                            "the injection `{}` never said `{ready}`",
                            argv.join(" ")
                        ))
                    }
                }
            }
        }
        Ok(injected)
    }

    /// Waits for it to finish and returns what it said after `ready`.
    fn finish(mut self) -> Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if self.child.try_wait().map_err(|e| e.to_string())?.is_some() {
                break;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err("the injection did not finish within 90 s of the target".to_owned());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut said = String::new();
        while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(500)) {
            said.push_str(&line);
            said.push('\n');
        }
        Ok(said)
    }
}

impl Drop for Injected {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The program's command line as both ways launch it: the case's, with its
/// stdout handed to its stderr when that is where its account is.
fn launched(cause: &Cause, places: &Places) -> Vec<String> {
    let argv = places.argv(&cause.target);
    match cause.own_account.as_deref() {
        Some("stdout") => {
            let mut wrapped: Vec<String> = ["sh", "-c", "exec \"$@\" 1>&2", "sh"]
                .iter()
                .map(|word| word.to_string())
                .collect();
            wrapped.extend(argv);
            wrapped
        }
        _ => argv,
    }
}

/// The machine's refusal of a valid question: exit 1, and the reason as
/// `omn` gave it, the line that says why first and the rest indented under
/// it, which is the layout the CI's skip gate reads.
fn withheld(code: Option<i32>, said: &str) -> Option<String> {
    let machine = code == Some(1)
        && (said.contains("no viable backend")
            || said.contains("perf_event_paranoid")
            || said.contains("cannot start collecting"));
    if !machine {
        return None;
    }
    let lines: Vec<&str> = said
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with("caveat:"))
        .collect();
    let first = lines
        .iter()
        .position(|line| line.starts_with("error:"))
        .unwrap_or(0);
    let mut reason = lines.get(first).map_or("", |line| line.trim()).to_owned();
    for (index, line) in lines.iter().enumerate().take(12) {
        if index != first {
            reason.push_str("\n    ");
            reason.push_str(line.trim_end());
        }
    }
    Some(reason)
}

// -------------------------------------------------------------- the answers

/// The first suspect and what was cited with it, read the same way from
/// either answer.
#[derive(Debug, Default)]
struct Answer {
    /// `cli` or `mcp`.
    way: &'static str,
    /// The verdict's `limit`; only the person's way names one.
    limit: Option<String>,
    /// The dominant state, before any qualifier.
    state: String,
    /// The dominant stack's leaf, when there is one.
    leaf: Option<String>,
    /// Every frame of the dominant stack.
    frames: Vec<String>,
    /// Each state's amount and resolution, by state before any qualifier.
    rows: BTreeMap<String, (f64, f64)>,
    /// The program's life and its resolution.
    whole: (f64, f64),
    /// The verdict's own sentence, where there is one.
    sentence: String,
    /// What the answer says about how it knows.
    how: How,
}

#[derive(Debug, Default)]
struct How {
    method: Option<String>,
    /// Every row's evidence level, or `None` for a row without one.
    evidence: Vec<(String, Option<String>)>,
    /// The unwind mode, as the notes say it.
    unwind: Option<String>,
    /// Where names came from, as the notes say it.
    names: Option<String>,
    /// What it could not see: losses, degradations and missing rows.
    unseen: Vec<String>,
}

fn base_state(state: &str) -> String {
    state.split(':').next().unwrap_or(state).trim().to_owned()
}

fn is_named(frame: &str) -> bool {
    !frame.contains("+0x") && !frame.starts_with("0x") && !frame.is_empty()
}

/// `omn latency --format json`: the verdict, the per-state rows and notes.
fn read_cli(stdout: &str) -> Result<Answer, String> {
    let start = stdout
        .find("\n{\n")
        .map(|at| at + 1)
        .or_else(|| stdout.starts_with('{').then_some(0))
        .ok_or_else(|| format!("no JSON document on stdout: {stdout:?}"))?;
    let document: Value = serde_json::from_str(&stdout[start..])
        .map_err(|error| format!("the verdict is not JSON ({error})"))?;
    let verdict = &document["verdict"];
    let run = document["runs"]
        .as_array()
        .and_then(|runs| runs.first())
        .ok_or("the verdict has no run")?;
    let mut answer = Answer {
        way: "cli",
        limit: verdict["limit"].as_str().map(str::to_owned),
        state: base_state(verdict["state"].as_str().unwrap_or("")),
        sentence: verdict["sentence"].as_str().unwrap_or("").to_owned(),
        whole: (
            run["whole"]["amount"].as_f64().unwrap_or(0.0),
            run["whole"]["resolution"].as_f64().unwrap_or(0.0),
        ),
        ..Answer::default()
    };
    answer.how.method = document["method"].as_str().map(str::to_owned);
    let top_state = verdict["state"].as_str().unwrap_or("");
    for state in run["states"].as_array().into_iter().flatten() {
        let name = state["state"].as_str().unwrap_or("").to_owned();
        answer
            .how
            .evidence
            .push((name.clone(), state["evidence"].as_str().map(str::to_owned)));
        if let Some(missing) = state["missing"].as_str() {
            answer.how.unseen.push(format!("{name}: {missing}"));
        }
        if let Some(amount) = state["amount"].as_f64() {
            let row = answer.rows.entry(base_state(&name)).or_insert((0.0, 0.0));
            row.0 += amount;
            row.1 += state["resolution"].as_f64().unwrap_or(0.0);
        }
        if name == top_state {
            for named in state["named"].as_array().into_iter().flatten() {
                let says = named["says"].as_str().unwrap_or("");
                match named["source"].as_str() {
                    Some("cpu.profile") => {
                        // "hottest function `X`, N of M samples"
                        if let Some(name) = says.split('`').nth(1) {
                            answer.leaf = Some(name.to_owned());
                            answer.frames = vec![name.to_owned()];
                        }
                    }
                    Some("sched.offcpu") => {
                        // "... leaf first: A <- B <- C"
                        if let Some((_, stack)) = says.split_once("leaf first:") {
                            answer.frames =
                                stack.split(" <- ").map(|f| f.trim().to_owned()).collect();
                            answer.leaf = answer.frames.first().cloned();
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    for note in run["notes"].as_array().into_iter().flatten() {
        let note = note.as_str().unwrap_or("");
        if let Some(at) = note.find("stacks unwound by") {
            answer.how.unwind = Some(note[at..].to_owned());
        }
        if let Some(at) = note.find("names came from") {
            answer.how.names = Some(note[at..].to_owned());
        }
        if note.contains("lost") || note.contains("degraded") {
            answer.how.unseen.push(note.to_owned());
        }
    }
    Ok(answer)
}

/// `run_session`'s read-back, read the way an agent would: the biggest row of
/// the decomposition, and the dominant stack of the request that row came
/// from.
fn read_mcp(answer_doc: &Value) -> Result<Answer, String> {
    let read_back = &answer_doc["read_back"];
    let table = read_back["decompositions"]
        .as_array()
        .and_then(|all| all.first())
        .and_then(|decomposition| decomposition["tables"].as_array())
        .and_then(|tables| tables.first())
        .ok_or("the run has no decomposition table")?;
    let mut answer = Answer {
        way: "mcp",
        whole: (
            table["whole"]["amount"].as_f64().unwrap_or(0.0),
            table["whole"]["resolution"].as_f64().unwrap_or(0.0),
        ),
        ..Answer::default()
    };
    answer.how.method = read_back["decompositions"][0]["id"]
        .as_str()
        .map(|id| format!("decomposition `{id}`"));
    let mut top: Option<(f64, String, String)> = None;
    for row in table["rows"].as_array().into_iter().flatten() {
        let state = row["state"].as_str().unwrap_or("").to_owned();
        answer.how.evidence.push((
            state.clone(),
            row["evidence_means"]["level"]
                .as_str()
                .or(row["evidence"].as_str())
                .map(str::to_owned),
        ));
        if let Some(missing) = row["missing"].as_str() {
            answer.how.unseen.push(format!("{state}: {missing}"));
        }
        if let Some(amount) = row["amount"].as_f64() {
            let entry = answer.rows.entry(base_state(&state)).or_insert((0.0, 0.0));
            entry.0 += amount;
            entry.1 += row["resolution"].as_f64().unwrap_or(0.0);
            if top.as_ref().is_none_or(|(best, _, _)| amount > *best) {
                top = Some((
                    amount,
                    state.clone(),
                    row["source"].as_str().unwrap_or("").to_owned(),
                ));
            }
        }
    }
    let (_, state, source) = top.ok_or("no row of the decomposition has an amount")?;
    answer.state = base_state(&state);
    for request in read_back["requests"].as_array().into_iter().flatten() {
        let result = &request["result"];
        for note in result["notes"].as_array().into_iter().flatten() {
            let note = note.as_str().unwrap_or("");
            if let Some(at) = note.find("stacks unwound by") {
                answer.how.unwind.get_or_insert(note[at..].to_owned());
            }
            if let Some(at) = note.find("names came from") {
                answer.how.names.get_or_insert(note[at..].to_owned());
            }
        }
        let missing = &request["what_it_is_missing"];
        for (what, value) in missing["losses"].as_object().into_iter().flatten() {
            answer.how.unseen.push(format!(
                "{}: {what} {value}",
                request["label"].as_str().unwrap_or("")
            ));
        }
        for degradation in missing["degradations"].as_array().into_iter().flatten() {
            answer
                .how
                .unseen
                .push(degradation.as_str().unwrap_or("").to_owned());
        }
        if result["source"].as_str() != Some(source.as_str()) {
            continue;
        }
        // The dominant stack: by its total where the source weighs a stack
        // (a block's nanoseconds), by its leaf's own samples where it counts
        // them (the hottest function).
        let stacks = &result["stacks"];
        let frames_of = |id: &Value| -> Vec<String> {
            stacks[id.to_string().as_str()]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        };
        let mut by_leaf: BTreeMap<String, f64> = BTreeMap::new();
        let mut heaviest: Option<(f64, Vec<String>)> = None;
        for output in result["outputs"].as_array().into_iter().flatten() {
            for row in output["stacks"].as_array().into_iter().flatten() {
                let frames = frames_of(&row["stack"]);
                let total = row["total"].as_f64().unwrap_or(0.0);
                if let Some(leaf) = frames.last() {
                    *by_leaf.entry(leaf.clone()).or_default() += total;
                }
                if heaviest.as_ref().is_none_or(|(best, _)| total > *best) {
                    heaviest = Some((total, frames));
                }
            }
        }
        if source == "cpu.profile" {
            answer.leaf = by_leaf
                .into_iter()
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(leaf, _)| leaf);
            answer.frames = answer.leaf.iter().cloned().collect();
        } else if let Some((_, frames)) = heaviest {
            // Leaf first, as the person's way writes it.
            answer.frames = frames.into_iter().rev().collect();
            answer.leaf = answer.frames.first().cloned();
        }
    }
    Ok(answer)
}

// ------------------------------------------------------------------- checks

const SAME_SUSPECT: &str = "both-ways-name-the-same-suspect";
const KNOWN_CAUSE: &str = "the-first-suspect-is-the-known-cause";
const KNOWN_LEAF: &str = "the-dominant-stack-ends-in-the-known-frame";
const NUMBERS: &str = "every-cited-number-agrees-with-the-reference";
const HOW: &str = "the-verdict-says-how-it-knows";
const CLI_NAMES: &str = "the-person-s-way-names-the-hottest-function";
const CLI_ORIGINS: &str = "the-person-s-way-says-where-names-came-from";
const BLOCK_NAMED: &str = "the-block-s-frames-are-named";
const NOTHING_FALSE: &str = "the-sentence-claims-nothing-false";

/// Every check this file knows, so a case cannot list one that does not
/// exist.
const CHECKS: [&str; 9] = [
    SAME_SUSPECT,
    KNOWN_CAUSE,
    KNOWN_LEAF,
    NUMBERS,
    HOW,
    CLI_NAMES,
    CLI_ORIGINS,
    BLOCK_NAMED,
    NOTHING_FALSE,
];

#[derive(Debug)]
enum Outcome {
    Pass,
    Fail(String),
    /// Not made, and why: names that were not asked for, a frame no name
    /// reaches.
    Unchecked(String),
}

#[derive(Default)]
struct Report {
    /// (case/cause/way, check, outcome)
    lines: Vec<(String, &'static str, Outcome)>,
}

impl Report {
    fn add(&mut self, at: &str, check: &'static str, outcome: Outcome) {
        self.lines.push((at.to_owned(), check, outcome));
    }

    fn pass_or(&mut self, at: &str, check: &'static str, ok: bool, why: impl FnOnce() -> String) {
        let outcome = if ok {
            Outcome::Pass
        } else {
            Outcome::Fail(why())
        };
        self.add(at, check, outcome);
    }
}

/// A named reference value, `<reference>.<key>`.
fn lookup(accounts: &BTreeMap<String, Account>, path: &str) -> Result<Reading, String> {
    let (name, key) = path
        .split_once('.')
        .ok_or_else(|| format!("`{path}` is not `<reference>.<key>`"))?;
    accounts
        .get(name)
        .ok_or_else(|| format!("no reference named `{name}`"))?
        .get(key)
        .copied()
        .ok_or_else(|| format!("the reference `{name}` has no `{key}`"))
}

/// Whether names were asked for and can be had: `Err` is why not.
struct Naming {
    /// What the agent's session asks for.
    symbols: Option<String>,
    /// Whether the person's way could ask for it too.
    cli_flag: bool,
    /// Why names from debuginfod were not asked for here, if they were not.
    unavailable: Option<String>,
}

fn check_answer(
    report: &mut Report,
    at: &str,
    cause: &Cause,
    answer: &Answer,
    accounts: &BTreeMap<String, Account>,
    naming: &Naming,
) {
    let expect = &cause.expect;
    let suspect_ok = answer.state == expect.state
        && answer
            .limit
            .as_ref()
            .is_none_or(|limit| *limit == expect.limit);
    report.pass_or(at, KNOWN_CAUSE, suspect_ok, || {
        format!(
            "the first suspect is {} (limit {:?}), the known cause is {} (limit {}): {}",
            answer.state, answer.limit, expect.state, expect.limit, answer.sentence
        )
    });

    match (&expect.leaf, &answer.leaf) {
        (Some(want), Some(leaf)) if is_named(leaf) || !is_named(want) => {
            report.pass_or(at, KNOWN_LEAF, leaf.starts_with(want.as_str()), || {
                format!("the dominant stack ends in `{leaf}`, the known frame is `{want}`")
            })
        }
        (Some(want), Some(leaf)) => report.add(
            at,
            KNOWN_LEAF,
            Outcome::Unchecked(format!(
                "the leaf is `{leaf}`, an address, and `{want}` is a name"
            )),
        ),
        (Some(want), None) => report.add(
            at,
            KNOWN_LEAF,
            Outcome::Fail(format!(
                "no dominant stack was named; the known frame is `{want}`"
            )),
        ),
        _ => {}
    }

    if let Some(through) = &expect.through {
        let named = answer.frames.iter().filter(|f| is_named(f)).count();
        let outcome = if answer
            .frames
            .iter()
            .any(|f| f.starts_with(through.as_str()))
        {
            Outcome::Pass
        } else if cause.on == On::Cpu && naming.unavailable.is_some() {
            Outcome::Unchecked(naming.unavailable.clone().unwrap())
        } else {
            Outcome::Fail(format!(
                "the dominant stack does not pass through `{through}` ({named} of {} frames named): {}",
                answer.frames.len(),
                answer.frames.join(" <- ")
            ))
        };
        report.add(at, BLOCK_NAMED, outcome);
    }

    if answer.way == "cli" {
        let found: Vec<&String> = expect
            .never_says
            .iter()
            .filter(|claim| answer.sentence.contains(claim.as_str()))
            .collect();
        if !expect.never_says.is_empty() {
            report.pass_or(at, NOTHING_FALSE, found.is_empty(), || {
                format!("the sentence says {found:?}: {}", answer.sentence)
            });
        }
        if cause.on == On::Cpu && expect.leaf.is_some() {
            let outcome = match (&naming.unavailable, &answer.leaf) {
                (Some(why), _) => Outcome::Unchecked(why.clone()),
                (None, Some(leaf)) if is_named(leaf) => Outcome::Pass,
                (None, leaf) => Outcome::Fail(format!(
                    "the hottest function is {leaf:?} where the agent's session, asking \
                     debuginfod, names it; `omn latency` {} `--symbols`",
                    if naming.cli_flag { "took" } else { "has no" }
                )),
            };
            report.add(at, CLI_NAMES, outcome);
        }
        report.pass_or(at, CLI_ORIGINS, answer.how.names.is_some(), || {
            "no note of the verdict says where its names came from".to_owned()
        });
    }

    // Every number the answer cites for a state the case knows.
    let mut failures = Vec::new();
    for agree in &expect.agrees {
        let result = (|| -> Result<Option<String>, String> {
            let (amount, row_resolution) = *answer
                .rows
                .get(&agree.row)
                .ok_or_else(|| format!("no `{}` row has an amount", agree.row))?;
            let reference = lookup(accounts, &agree.with)?;
            let mut slack = row_resolution + reference.resolution;
            if let Some(outside) = &agree.outside {
                let covered = lookup(accounts, outside)?;
                slack += (answer.whole.0 + answer.whole.1 - covered.value).max(0.0);
            }
            slack += agree.relative.unwrap_or(0.0) * reference.value;
            slack += agree.slack_ns.unwrap_or(0.0);
            if let Some(plus) = &agree.plus {
                slack += lookup(accounts, plus)?.value;
            }
            let off = (amount - reference.value).abs();
            Ok((off > slack).then(|| {
                format!(
                    "`{}` is {:.3} ms against `{}` {:.3} ms: {:.3} ms apart, beyond the {:.3} ms \
                     the two resolutions allow",
                    agree.row,
                    amount / 1e6,
                    agree.with,
                    reference.value / 1e6,
                    off / 1e6,
                    slack / 1e6
                )
            }))
        })();
        match result {
            Ok(None) => {}
            Ok(Some(apart)) => failures.push(apart),
            Err(why) => failures.push(why),
        }
    }
    if !expect.agrees.is_empty() {
        report.pass_or(at, NUMBERS, failures.is_empty(), || failures.join("; "));
    }

    let how = &answer.how;
    let mut lacking = Vec::new();
    if how.method.is_none() {
        lacking.push("which method answered".to_owned());
    }
    for (state, evidence) in &how.evidence {
        if evidence.is_none() {
            lacking.push(format!("the evidence level of the `{state}` row"));
        }
    }
    if how.evidence.is_empty() {
        lacking.push("any row with its evidence".to_owned());
    }
    if how.unwind.is_none() {
        lacking.push("the unwind mode".to_owned());
    }
    if answer.way == "mcp" && how.names.is_none() {
        lacking.push("where the names came from".to_owned());
    }
    if how.unseen.is_empty() {
        lacking.push("what it could not see (no loss, degradation or missing row)".to_owned());
    }
    report.pass_or(at, HOW, lacking.is_empty(), || {
        format!("the answer does not say {}", lacking.join(", "))
    });
}

// -------------------------------------------------------------- the two ways

fn latency_args(cause: &Cause, naming: &Naming) -> Vec<String> {
    let mut args = vec![
        cause.question.clone(),
        "--objective".to_owned(),
        cause.objective.clone(),
        "--format".to_owned(),
        "json".to_owned(),
    ];
    if naming.cli_flag {
        if let Some(symbols) = &naming.symbols {
            args.extend(["--symbols".to_owned(), symbols.clone()]);
        }
    }
    args
}

/// What a way of asking gave back: the answer, or the machine's refusal.
enum Asked {
    Answer(Box<Answer>, String),
    Withheld(String),
}

/// Runs an injection (if the cause has one) around `ask`, and returns the
/// answer with the injection's own account.
fn around_injection(
    cause: &Cause,
    places: &Places,
    ask: impl FnOnce() -> Result<Asked, String>,
) -> Result<(Asked, String), String> {
    let injected = match &cause.injection.run {
        Some(argv) if cause.injection.kind == "process" => Some(Injected::start(
            &places.argv(argv),
            cause.injection.ready.as_deref(),
            &places.dir,
        )?),
        _ => None,
    };
    let asked = ask()?;
    let said = match injected {
        Some(injected) => injected.finish()?,
        None => String::new(),
    };
    Ok((asked, said))
}

fn ask_cli(cause: &Cause, places: &Places, naming: &Naming) -> Result<Asked, String> {
    let mut argv = vec![OMN.to_owned()];
    argv.extend(latency_args(cause, naming));
    argv.push("--".to_owned());
    argv.extend(launched(cause, places));
    let output = run(&argv, &places.dir, RUN_CEILING)?;
    let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
    if let Some(reason) = withheld(output.status.code(), &stderr) {
        return Ok(Asked::Withheld(reason));
    }
    if !output.status.success() {
        return Err(format!(
            "`omn {}` exited {:?}:\n{stderr}",
            latency_args(cause, naming).join(" "),
            output.status.code()
        ));
    }
    let answer = read_cli(&stdout)?;
    Ok(Asked::Answer(Box::new(answer), stderr))
}

/// A small MCP client over the server's stdio.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(args: &[String]) -> Result<Self, String> {
        let binary = Path::new(OMN).with_file_name("omn-mcp");
        if !binary.is_file() {
            return Err(format!(
                "{} does not exist: run `make`, which builds the whole workspace",
                binary.display()
            ));
        }
        let mut child = Command::new(&binary)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("omn-mcp could not be started: {error}"))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Self {
            child,
            stdin,
            stdout,
            next: 0,
        };
        mcp.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "omn-cases", "version": "0"},
            }),
        )?;
        mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        Ok(mcp)
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|error| format!("omn-mcp stopped reading: {error}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|error| format!("omn-mcp said nothing: {error}"))?;
        let reply: Value = serde_json::from_str(&line)
            .map_err(|error| format!("not a JSON-RPC line ({error}): {line}"))?;
        Ok(reply)
    }

    /// A tool call: its structured content, or its refusal.
    fn call(&mut self, tool: &str, arguments: Value) -> Result<Value, String> {
        let reply = self.request("tools/call", json!({"name": tool, "arguments": arguments}))?;
        let result = &reply["result"];
        if result["isError"] == true {
            return Err(format!(
                "{tool} refused: {}",
                result["content"][0]["text"].as_str().unwrap_or("")
            ));
        }
        Ok(result["structuredContent"].clone())
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The session the person's question is asked through, as the person
/// starting the server approves it: `omn latency --dump-session`, written as
/// JSON so the run can be read back, and asking for the names the case asks
/// for where the question itself could not.
fn approved_session(cause: &Cause, places: &Places, naming: &Naming) -> Result<String, String> {
    let mut argv = vec![OMN.to_owned()];
    argv.extend(latency_args(cause, naming));
    argv.push("--dump-session".to_owned());
    argv.push("--".to_owned());
    argv.extend(launched(cause, places));
    let output = run(&argv, &places.dir, Duration::from_secs(30))?;
    if !output.status.success() {
        return Err(format!(
            "--dump-session exited {:?}: {}",
            output.status.code(),
            text(&output.stderr)
        ));
    }
    let mut session: serde_norway::Value = serde_norway::from_slice(&output.stdout)
        .map_err(|error| format!("the dumped session is not YAML: {error}"))?;
    session["format"] = "json".into();
    if let (Some(symbols), false) = (&naming.symbols, naming.cli_flag) {
        for request in session["requests"].as_sequence_mut().into_iter().flatten() {
            if request["source"].as_str() == Some("cpu.profile") {
                request["symbols"] = symbols.clone().into();
            }
        }
    }
    serde_norway::to_string(&session).map_err(|error| error.to_string())
}

fn ask_mcp(cause: &Cause, places: &Places, naming: &Naming) -> Result<Asked, String> {
    let approved = places.dir.join(format!("approved-{}", cause.name));
    let runs = places.dir.join(format!("runs-{}", cause.name));
    std::fs::create_dir_all(&approved).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&runs).map_err(|e| e.to_string())?;
    let file = format!("{}.yml", cause.name);
    std::fs::write(
        approved.join(&file),
        approved_session(cause, places, naming)?,
    )
    .map_err(|e| e.to_string())?;
    let mut mcp = Mcp::start(&[
        "--dir".to_owned(),
        runs.to_string_lossy().into_owned(),
        "--allow-run".to_owned(),
        format!("{}s", RUN_CEILING.as_secs()),
        "--allow-launch".to_owned(),
        approved.to_string_lossy().into_owned(),
    ])?;
    // What may run, and whether this machine can serve it, before anything
    // does.
    let listed = mcp.call("list_approved", json!({}))?;
    let entry = listed["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|session| session["approved"].as_str() == Some(file.as_str()))
        .ok_or_else(|| format!("list_approved does not list {file}: {listed}"))?;
    let resolution = &entry["plan"]["resolution"];
    if resolution["resolved"] == false {
        let said = match &resolution["said_by_omn"] {
            Value::Array(lines) => lines
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            other => other.as_str().unwrap_or("").to_owned(),
        };
        if let Some(reason) = withheld(Some(1), &said) {
            return Ok(Asked::Withheld(reason));
        }
        return Err(format!("the approved session does not resolve: {entry}"));
    }
    let ran = mcp.call(
        "run_session",
        json!({
            "approved": file,
            "duration": format!("{}s", RUN_CEILING.as_secs()),
            "budget": UNBOUNDED_BUDGET,
        }),
    )?;
    let said = match &ran["said_by_omn"] {
        Value::Array(lines) => lines
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.as_str().unwrap_or("").to_owned(),
    };
    if ran["exit"] != 0 {
        if let Some(reason) = withheld(ran["exit"].as_i64().map(|code| code as i32), &said) {
            return Ok(Asked::Withheld(reason));
        }
        return Err(format!("run_session's omn exited {}:\n{said}", ran["exit"]));
    }
    let answer = read_mcp(&ran)?;
    Ok(Asked::Answer(Box::new(answer), said))
}

// --------------------------------------------------------------- one case

/// Why a case cannot run here, said the way the skip gate reads it.
fn unmet_need(case: &Case, places: &Places) -> Option<String> {
    for need in &case.needs {
        let argv = places.argv(&need.run);
        match run(&argv, &places.dir, Duration::from_secs(30)) {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                return Some(format!(
                    "this machine cannot run `{}` ({}): it exited {:?}: {}",
                    argv.join(" "),
                    need.purpose,
                    output.status.code(),
                    text(&output.stderr).trim()
                ))
            }
            Err(why) => {
                return Some(format!(
                    "this machine cannot run `{}` ({}): {why}",
                    argv.join(" "),
                    need.purpose
                ))
            }
        }
    }
    None
}

/// Fetches what the case needs into the cache, once.
fn fetched(case: &Case, places: &Places) -> Result<(), String> {
    for fetch in &case.fetch {
        let path = places.cache.join(&fetch.file);
        let digest = |path: &Path| -> Option<String> {
            let output = Command::new("sha256sum").arg(path).output().ok()?;
            text(&output.stdout)
                .split_whitespace()
                .next()
                .map(str::to_owned)
        };
        if path.is_file() && digest(&path).as_deref() == Some(fetch.sha256.as_str()) {
            continue;
        }
        std::fs::create_dir_all(&places.cache).map_err(|e| e.to_string())?;
        let partial = places
            .cache
            .join(format!("{}.{}", fetch.file, std::process::id()));
        let output = run(
            &[
                "curl".to_owned(),
                "-fsSL".to_owned(),
                "-o".to_owned(),
                partial.to_string_lossy().into_owned(),
                fetch.url.clone(),
            ],
            &places.dir,
            Duration::from_secs(300),
        )?;
        if !output.status.success() {
            let _ = std::fs::remove_file(&partial);
            return Err(format!(
                "this machine could not fetch {}: {}",
                fetch.url,
                text(&output.stderr).trim()
            ));
        }
        let got = digest(&partial);
        if got.as_deref() != Some(fetch.sha256.as_str()) {
            let _ = std::fs::remove_file(&partial);
            return Err(format!(
                "{} has sha256 {got:?}, the case says {}",
                fetch.url, fetch.sha256
            ));
        }
        std::fs::rename(&partial, &path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Whether a file carries a `.symtab`, from its section headers' names.
fn has_symtab(path: &Path) -> Option<bool> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.get(..4)? != b"\x7fELF" || bytes[4] != 2 {
        return None;
    }
    let u16_at = |at: usize| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize;
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
    let (shoff, shentsize, shnum, shstrndx) =
        (u64_at(0x28), u16_at(0x3a), u16_at(0x3c), u16_at(0x3e));
    let section = |index: usize| shoff + index * shentsize;
    let names = u64_at(section(shstrndx) + 0x18);
    Some((0..shnum).any(|index| {
        let name = names
            + u32::from_le_bytes(
                bytes[section(index)..section(index) + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
        bytes.get(name..name + 8) == Some(b".symtab\0")
    }))
}

fn which(command: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(command))
            .find(|candidate| candidate.is_file())
    })
}

/// The account of each reference, from the run it describes.
fn accounts(
    cause: &Cause,
    places: &Places,
    target_said: &str,
    injection_said: &str,
) -> Result<BTreeMap<String, Account>, String> {
    let mut accounts = BTreeMap::new();
    for reference in &cause.reference {
        let parse = extractor(&reference.extractor)
            .ok_or_else(|| format!("no extractor named `{}`", reference.extractor))?;
        let source = match (reference.from.as_deref(), &reference.run) {
            (Some("target"), _) => target_said.to_owned(),
            (Some("injection"), _) => injection_said.to_owned(),
            (None, Some(argv)) => {
                let output = run(&places.argv(argv), &places.dir, Duration::from_secs(60))?;
                text(&output.stdout) + &text(&output.stderr)
            }
            (from, _) => return Err(format!("a reference from {from:?} with no `run`")),
        };
        let account =
            parse(&source).map_err(|why| format!("the reference `{}`: {why}", reference.name))?;
        accounts.insert(reference.name.clone(), account);
    }
    Ok(accounts)
}

/// Whether the cause was there to be found, by the reference's own figures.
fn cause_held(cause: &Cause, accounts: &BTreeMap<String, Account>) -> Result<(), String> {
    for hold in &cause.holds {
        let value = lookup(accounts, &hold.value)?.value;
        let low = hold.at_least.is_some_and(|least| value < least);
        let high = hold.at_most.is_some_and(|most| value > most);
        if low || high {
            return Err(format!(
                "`{}` is {value}, outside [{:?}, {:?}]: {} did not hold",
                hold.value, hold.at_least, hold.at_most, hold.why
            ));
        }
    }
    Ok(())
}

fn run_case(case_dir: &Path, case: &Case, report: &mut Report) {
    let places = Places::of(case_dir, &case.name);
    if let Some(reason) = unmet_need(case, &places) {
        eprintln!("SKIPPED case {}: {reason}", case.name);
        return;
    }
    if let Err(reason) = fetched(case, &places) {
        eprintln!("SKIPPED case {}: {reason}", case.name);
        return;
    }
    if let Some(version) = &case.program.version {
        if let Ok(output) = run(&places.argv(version), &places.dir, Duration::from_secs(10)) {
            eprintln!(
                "case {}: {} ({}), obtained as {}",
                case.name,
                case.program.command,
                text(&output.stdout).trim(),
                case.program.obtained
            );
        }
    }
    if case.program.stripped {
        let path = which(&places.word(&case.program.command));
        if path.as_deref().and_then(has_symtab) != Some(false) {
            eprintln!(
                "SKIPPED case {}: this machine's `{}` ({path:?}) is not a stripped ELF file, \
                 and the case is about one",
                case.name, case.program.command
            );
            return;
        }
    }
    for step in &case.setup {
        let argv = places.argv(step);
        let output = run(&argv, &places.dir, Duration::from_secs(600))
            .unwrap_or_else(|why| panic!("case {}: setup: {why}", case.name));
        assert!(
            output.status.success(),
            "case {}: setup `{}` exited {:?}: {}",
            case.name,
            argv.join(" "),
            output.status.code(),
            text(&output.stderr)
        );
    }

    let help = Command::new(OMN)
        .args(["latency", "--help"])
        .output()
        .map(|output| text(&output.stdout))
        .unwrap_or_default();
    let wants = case.symbols.clone().filter(|symbols| symbols != "local");
    let unavailable = match wants.as_deref() {
        Some("debuginfod")
            if std::env::var("DEBUGINFOD_URLS").map_or(true, |urls| urls.trim().is_empty()) =>
        {
            Some("names from debuginfod were not asked for: DEBUGINFOD_URLS is unset".to_owned())
        }
        Some(_) => None,
        None => Some("the case asks for the file's own names only".to_owned()),
    };
    let naming = Naming {
        symbols: if unavailable.is_none() { wants } else { None },
        cli_flag: help.contains("--symbols"),
        unavailable,
    };

    for cause in &case.causes {
        let mut answers: Vec<Answer> = Vec::new();
        for way in ["cli", "mcp"] {
            let at = format!("{}/{}/{way}", case.name, cause.name);
            let asked = around_injection(cause, &places, || match way {
                "cli" => ask_cli(cause, &places, &naming),
                _ => ask_mcp(cause, &places, &naming),
            });
            let (answer, said, injection_said) = match asked {
                Ok((Asked::Answer(answer, said), injection_said)) => {
                    (*answer, said, injection_said)
                }
                Ok((Asked::Withheld(reason), _)) => {
                    eprintln!(
                        "SKIPPED case {at}: this machine withholds a source it needs: {reason}"
                    );
                    continue;
                }
                Err(why) => {
                    report.add(&at, KNOWN_CAUSE, Outcome::Fail(why));
                    continue;
                }
            };
            let accounts = match accounts(cause, &places, &said, &injection_said) {
                Ok(accounts) => accounts,
                Err(why) => {
                    let tail: Vec<&str> = said
                        .lines()
                        .filter(|line| !line.starts_with("caveat:") && !line.starts_with("note:"))
                        .collect();
                    let tail = tail[tail.len().saturating_sub(12)..].join("\n    ");
                    report.add(
                        &at,
                        NUMBERS,
                        Outcome::Fail(format!("{why}; the run said, last:\n    {tail}")),
                    );
                    continue;
                }
            };
            if let Err(why) = cause_held(cause, &accounts) {
                eprintln!("SKIPPED case {at}: the cause was not there to be found: {why}");
                continue;
            }
            eprintln!(
                "case {at}: first suspect {} (limit {}), leaf {:?}; rows {}; reference {}",
                answer.state,
                answer.limit.as_deref().unwrap_or("-"),
                answer.leaf,
                answer
                    .rows
                    .iter()
                    .map(|(state, (amount, _))| format!("{state} {:.1} ms", amount / 1e6))
                    .collect::<Vec<_>>()
                    .join(", "),
                accounts
                    .iter()
                    .flat_map(|(name, account)| account
                        .iter()
                        .map(move |(key, reading)| format!("{name}.{key}={}", reading.value)))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            check_answer(report, &at, cause, &answer, &accounts, &naming);
            answers.push(answer);
        }
        if let [cli, mcp] = &answers[..] {
            let at = format!("{}/{}", case.name, cause.name);
            let leaves_comparable =
                matches!((&cli.leaf, &mcp.leaf), (Some(a), Some(b)) if is_named(a) && is_named(b));
            let same = cli.state == mcp.state && (!leaves_comparable || cli.leaf == mcp.leaf);
            report.pass_or(&at, SAME_SUSPECT, same, || {
                format!(
                    "the person's way names {} at {:?}, the agent's {} at {:?}",
                    cli.state, cli.leaf, mcp.state, mcp.leaf
                )
            });
        }
    }
}

// -------------------------------------------------------------------- tests

/// Every case, both ways. One test, so the cases run one after the other:
/// each measures the machine, and two at once would measure each other.
#[test]
fn every_case_names_its_known_cause_both_ways() {
    let mut failures = Vec::new();
    for (dir, case) in cases() {
        let mut report = Report::default();
        run_case(&dir, &case, &mut report);
        let open: BTreeMap<&str, &str> = case
            .open_findings
            .iter()
            .map(|finding| (finding.check.as_str(), finding.filed.as_str()))
            .collect();
        let mut failed_open = BTreeSet::new();
        let mut passed_open = BTreeSet::new();
        for (at, check, outcome) in &report.lines {
            match outcome {
                Outcome::Pass => {
                    eprintln!("  ok        {at}: {check}");
                    if open.contains_key(check) {
                        passed_open.insert(*check);
                    }
                }
                Outcome::Unchecked(why) => eprintln!("  unchecked {at}: {check}: {why}"),
                Outcome::Fail(why) => match open.get(check) {
                    Some(filed) => {
                        failed_open.insert(*check);
                        eprintln!("  FINDING   {at}: {check}: {why}\n            filed: {filed}");
                    }
                    None => {
                        eprintln!("  FAILED    {at}: {check}: {why}");
                        failures.push(format!("{at}: {check}: {why}"));
                    }
                },
            }
        }
        for check in passed_open.difference(&failed_open) {
            failures.push(format!(
                "{}: `{check}` is listed under open_findings and now passes everywhere it was \
                 checked: remove it from cases/{}/case.yml and close the finding",
                case.name, case.name
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Machine-independent: every case parses, names checks and extractors that
/// exist, and refers only to references it declares.
#[test]
fn every_case_is_well_formed() {
    let all = cases();
    assert!(!all.is_empty(), "no case under cases/");
    for (dir, case) in &all {
        assert_eq!(
            dir.file_name().unwrap().to_string_lossy(),
            case.name,
            "a case is named after its directory"
        );
        assert!(!case.about.trim().is_empty() && !case.program.obtained.trim().is_empty());
        for finding in &case.open_findings {
            assert!(
                CHECKS.contains(&finding.check.as_str()),
                "{}: no check named `{}`; the checks are {CHECKS:?}",
                case.name,
                finding.check
            );
            assert!(!finding.filed.trim().is_empty());
        }
        assert!(
            case.causes.iter().any(|cause| cause.on == On::Cpu)
                && case.causes.iter().any(|cause| cause.on == On::OffCpu),
            "{}: a case has a cause on the cpu and one off it",
            case.name
        );
        for cause in &case.causes {
            assert_eq!(
                cause.question, "latency",
                "{}/{}: only `omn latency` is asked both ways yet",
                case.name, cause.name
            );
            assert!(
                !cause.injection.about.trim().is_empty(),
                "{}/{}: an injection says what it is",
                case.name,
                cause.name
            );
            assert!(
                ["input", "process", "build", "cache"].contains(&cause.injection.kind.as_str()),
                "{}/{}: injection kind `{}`",
                case.name,
                cause.name,
                cause.injection.kind
            );
            assert_eq!(
                cause.injection.kind == "process",
                cause.injection.run.is_some(),
                "{}/{}: a `process` injection, and only one, runs something",
                case.name,
                cause.name
            );
            let names: BTreeSet<&str> = cause.reference.iter().map(|r| r.name.as_str()).collect();
            for reference in &cause.reference {
                assert!(
                    extractor(&reference.extractor).is_some(),
                    "{}/{}: no extractor named `{}`",
                    case.name,
                    cause.name,
                    reference.extractor
                );
                assert!(
                    matches!(reference.from.as_deref(), Some("target" | "injection"))
                        || (reference.from.is_none() && reference.run.is_some()),
                    "{}/{}: the reference `{}` comes from nowhere",
                    case.name,
                    cause.name,
                    reference.name
                );
            }
            let mut used: Vec<&str> = cause.holds.iter().map(|h| h.value.as_str()).collect();
            for agree in &cause.expect.agrees {
                used.push(&agree.with);
                used.extend(agree.outside.as_deref());
                used.extend(agree.plus.as_deref());
            }
            for path in used {
                let name = path.split('.').next().unwrap();
                assert!(
                    names.contains(name),
                    "{}/{}: `{path}` names no reference of the cause",
                    case.name,
                    cause.name
                );
            }
        }
    }
}

/// The extractors, on text the programs print.
#[test]
fn the_extractors_read_what_the_programs_print() {
    let timer = sqlite3_timer(
        "200\nRun Time: real 1.379752 user 1.278680 sys 0.090187\n\
         Run Time: real 0.000100 user 0.000050 sys 0.000000\n",
    )
    .unwrap();
    assert!((timer["real_ns"].value - 1_379_852_000.0).abs() < 1.0);
    assert!((timer["cpu_ns"].value - 1_368_917_000.0).abs() < 1.0);
    assert!((timer["off_cpu_ns"].value - 10_935_000.0).abs() < 1.0);
    assert_eq!(timer["real_ns"].resolution, 2_000.0);
    assert!(sqlite3_timer("no account").is_err());

    let stats = sqlite3_stats(
        "Fullscan Steps:                      0\nVirtual Machine Steps:  0\n\
         Fullscan Steps:                      39999999\nVirtual Machine Steps:  200000612\n",
    )
    .unwrap();
    assert_eq!(stats["fullscan_steps"].value, 39_999_999.0);
    assert_eq!(stats["vm_steps"].value, 200_000_612.0);

    let plan = sqlite3_plan("QUERY PLAN\n|--SCAN p\n`--SCAN t USING COVERING INDEX t_k\n").unwrap();
    assert_eq!((plan["scans"].value, plan["searches"].value), (2.0, 0.0));
    let plan = sqlite3_plan("QUERY PLAN\n|--SCAN p\n`--SEARCH t USING COVERING INDEX t_k (k=?)\n")
        .unwrap();
    assert_eq!((plan["scans"].value, plan["searches"].value), (1.0, 1.0));

    let held =
        key_value("held\nheld_after_target_ns=1500123 target_seen=1 poll_ns=2000\n").unwrap();
    assert_eq!(held["held_after_target_ns"].value, 1_500_123.0);
    assert_eq!(held["target_seen"].value, 1.0);
}
