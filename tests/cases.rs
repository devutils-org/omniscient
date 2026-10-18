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
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::{json, Value};

const OMN: &str = env!("CARGO_BIN_EXE_omn");

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
    /// Why the case is not asked, with where that was filed: a case held
    /// back is still discovered and held to [`every_case_is_well_formed`].
    #[serde(default)]
    held: Option<String>,
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
    /// The answer names no function as the hottest: a flat interpreter
    /// profile, whose hottest leaf is the interpreter's and not the
    /// program's. Checked on each way, as `leaf` is.
    #[serde(default)]
    names_no_function: bool,
    /// A frame the dominant stack must pass through.
    #[serde(default)]
    through: Option<String>,
    /// What the person's verdict must not say; read against the agent's
    /// dominant stack too, frame by frame.
    #[serde(default)]
    never_says: Vec<String>,
    /// What the person's verdict must say: the cause, in its words.
    #[serde(default)]
    says: Vec<String>,
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
    /// The check fails in some runs and passes in others, for a reason the
    /// filing names (the verdict decided or not by a few percent). Its pass
    /// does not ask for the line's removal, which is the filing's to close
    /// with the runs that show it closed; its failure is reported as the
    /// finding, never as a failure.
    #[serde(default)]
    sometimes: bool,
    /// The one cause the finding is about. Absent, it is the whole case's: a
    /// failure of the check on any cause is the finding. A finding filed
    /// against one cause names it, so the check still fails on the others.
    #[serde(default)]
    cause: Option<String>,
}

impl OpenFinding {
    /// Whether a report line -- `case/cause` or `case/cause/way` -- of
    /// `check` is this finding's.
    fn covers(&self, at: &str, check: &str) -> bool {
        self.check == check
            && self
                .cause
                .as_deref()
                .is_none_or(|cause| at.split('/').nth(1) == Some(cause))
    }
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
    /// The verdict's `limit`; only the person's way names one. As the text
    /// and the catalog spell it, so it compares with the case's.
    limit: Option<String>,
    /// The verdict's `limit` as its JSON wrote it, for [`LIMIT_SPELLED`].
    limit_written: Option<String>,
    /// The dominant state, before any qualifier.
    state: String,
    /// The dominant stack's leaf, when there is one.
    leaf: Option<String>,
    /// Every frame of the dominant stack.
    frames: Vec<String>,
    /// Each state's amount and resolution, by state before any qualifier.
    rows: BTreeMap<String, (f64, f64)>,
    /// What the rows are parts of, and its resolution: the program's life,
    /// or the wait a followed task ran in.
    whole: (f64, f64),
    /// The task the rows are, as the decomposition keys it (`task:4242`).
    task: Option<String>,
    /// How the reading got there from the program's main thread, one step
    /// each: `task:1 blocked 5.2 s -> task:9`.
    followed: Vec<String>,
    /// The rest of the job a wait for a child was followed into, by state
    /// (`on-cpu`, `runnable`): the processes the program started other than
    /// the child decomposed in the wait's place. The child's row alone is
    /// part of the job by construction (`docs/fixes/w20-triage.md`), so a
    /// row held to a reference that counts the whole job adds this.
    job: BTreeMap<String, f64>,
    /// The verdict's own sentence, where there is one.
    sentence: String,
    /// The binary that answered and the capabilities it carried.
    privileges: String,
    /// How each state's row was read, by state: its source and its kind
    /// (`traced`, `sampled`, ...). The backend a row was served by is what a
    /// privilege changes, and these are what either answer says of it.
    read_as: BTreeMap<String, (String, String)>,
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
///
/// The rows and the named stacks are the task the verdict names: the median
/// run's reading, and down its `followed` chain to the last reading, which is
/// the one the verdict decomposes when the main thread's largest row was a
/// wait another task ran in (`docs/fixes/w18-triage.md`). The notes are every
/// reading's of the chain: where names came from is said once, on the first.
fn read_cli(stdout: &str) -> Result<Answer, String> {
    let start = stdout
        .find("\n{\n")
        .map(|at| at + 1)
        .or_else(|| stdout.starts_with('{').then_some(0))
        .ok_or_else(|| format!("no JSON document on stdout: {stdout:?}"))?;
    let document: Value = serde_json::from_str(&stdout[start..])
        .map_err(|error| format!("the verdict is not JSON ({error})"))?;
    let verdict = &document["verdict"];
    let median = document["median"].as_u64().unwrap_or(0) as usize;
    let first = document["runs"]
        .as_array()
        .and_then(|runs| runs.get(median).or(runs.first()))
        .ok_or("the verdict has no run")?;
    let mut chain = vec![first];
    let mut run = first;
    // The job of the last wait followed into a child: the one beside the
    // reading the verdict decomposes.
    let mut job = BTreeMap::new();
    while run["followed"]["reading"].is_object() {
        job = BTreeMap::new();
        let rest = &run["followed"]["job"];
        for (state, field) in [("on-cpu", "on_cpu"), ("runnable", "runnable")] {
            if let Some(amount) = rest[field].as_f64() {
                job.insert(state.to_owned(), amount);
            }
        }
        run = &run["followed"]["reading"];
        chain.push(run);
    }
    let mut answer = Answer {
        way: "cli",
        // The JSON spells the limit as the text and the catalog do
        // (`cpu-queue`); `LIMIT_SPELLED` holds it to that.
        limit: verdict["limit"].as_str().map(str::to_owned),
        limit_written: verdict["limit"].as_str().map(str::to_owned),
        state: base_state(verdict["state"].as_str().unwrap_or("")),
        sentence: verdict["sentence"].as_str().unwrap_or("").to_owned(),
        whole: (
            run["whole"]["amount"].as_f64().unwrap_or(0.0),
            run["whole"]["resolution"].as_f64().unwrap_or(0.0),
        ),
        task: verdict["task"]
            .as_str()
            .or(run["task"].as_str())
            .map(str::to_owned),
        job,
        ..Answer::default()
    };
    answer.how.method = document["method"].as_str().map(str::to_owned);
    let top_state = verdict["state"].as_str().unwrap_or("");
    for state in run["states"].as_array().into_iter().flatten() {
        let name = state["state"].as_str().unwrap_or("").to_owned();
        answer.read_as.insert(
            base_state(&name),
            (
                state["source"].as_str().unwrap_or("").to_owned(),
                state["kind"].as_str().unwrap_or("").to_owned(),
            ),
        );
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
                        // "hottest function `X`, N of M samples"; a flat
                        // interpreter profile names none, and says so.
                        if let Some(name) = says
                            .strip_prefix("hottest function `")
                            .and_then(|rest| rest.split('`').next())
                        {
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
    for reading in &chain {
        for note in reading["notes"].as_array().into_iter().flatten() {
            let note = note.as_str().unwrap_or("");
            if let Some(at) = note.find("stacks unwound by") {
                answer.how.unwind.get_or_insert(note[at..].to_owned());
            }
            if let Some(at) = note.find("names came from") {
                answer.how.names.get_or_insert(note[at..].to_owned());
            }
            if note.contains("lost") || note.contains("degraded") {
                answer.how.unseen.push(note.to_owned());
            }
        }
    }
    Ok(answer)
}

/// How far the agent's way follows a wait into the task that ran in it, as
/// the triage does (`MAX_FOLLOWED` in `src/triage/latency.rs`).
const MAX_FOLLOWED: usize = 8;

/// The share of a block another task must have been busy for, on a cpu or
/// queued for one, to be the task that ran in it (`RAN_IN_IT` there).
const RAN_IN_IT: f64 = 0.25;

/// Below this share of the samples, an interpreter's hottest function by its
/// leaf is not a suspect (`FLAT` in `src/triage/diff.rs`).
const FLAT: f64 = 0.5;

/// The runtimes whose flat profile names nothing (`INTERPRETERS` there).
const INTERPRETERS: [&str; 12] = [
    "python", "nodejs", "node", "jruby", "java", "jvm", "ruby", "perl", "php-fpm", "php", "luajit",
    "lua",
];

/// Whether a program's or a module's name is an interpreter's: the runtime's
/// name, then a version or nothing (`python3.14`, `libpython3.14.so.1.0`),
/// not `node_exporter`.
fn is_interpreter(name: &str) -> bool {
    let name = name.rsplit('/').next().unwrap_or(name);
    let name = name.strip_prefix("lib").unwrap_or(name);
    INTERPRETERS.iter().any(|runtime| {
        name.strip_prefix(runtime).is_some_and(|rest| {
            rest.chars()
                .next()
                .is_none_or(|c| c.is_ascii_digit() || c == '.')
        })
    })
}

/// The symbols of the perf maps a names note cites (`perf-4242.map:
/// perf-map, ...`), read from `/tmp`, where every runtime writes them
/// whatever `TMPDIR` says, and where the triage reads them
/// (`PerfMapNames::of` in `src/triage/diff.rs`): a frame whose label is one
/// of them is a frame the map named.
///
/// A map last written before `since`, the time the run was asked for, is an
/// earlier process's that had the same pid: it is not read, and is returned
/// in the second half to be said.
fn perf_map_symbols(
    notes: &[String],
    since: Option<SystemTime>,
) -> (BTreeSet<String>, Vec<String>) {
    let mut symbols = BTreeSet::new();
    let mut stale = Vec::new();
    for note in notes {
        let Some(at) = note.find("names came from -- ") else {
            continue;
        };
        let listed = &note[at + "names came from -- ".len()..];
        // `module: origin` pairs separated by `; `, an origin holding `; `
        // itself: each map is the module just before a `perf-map` origin.
        let mut pieces: Vec<&str> = listed.split(": perf-map, ").collect();
        pieces.pop();
        for piece in pieces {
            let map = piece.rsplit("; ").next().unwrap_or(piece).trim();
            if !map.starts_with("perf-") || !map.ends_with(".map") || map.contains('/') {
                continue;
            }
            let path = Path::new("/tmp").join(map);
            let before = |since: SystemTime| {
                std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .is_ok_and(|modified| modified < since)
            };
            if since.is_some_and(before) {
                stale.push(format!(
                    "{}: last written before the run, an earlier process's: not read",
                    path.display()
                ));
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            // `START SIZE SYMBOL`, the symbol to the end of the line.
            symbols.extend(text.lines().filter_map(|line| {
                let mut fields = line.splitn(3, ' ');
                let (_, _, symbol) = (fields.next()?, fields.next()?, fields.next()?);
                Some(symbol.trim().to_owned())
            }));
        }
    }
    (symbols, stale)
}

/// What the agent reads of each task off the cpu from `sched.offcpu`'s raw
/// events: the process it is in (the events' `pid`), and its blocked time
/// whose stack goes through a send or a receive.
#[derive(Default)]
struct OnConnection {
    pid: BTreeMap<String, u64>,
    blocked: BTreeMap<String, f64>,
}

/// [`OnConnection`], from a run's read-back.
fn blocked_on_connection(read_back: &Value) -> OnConnection {
    let mut found = OnConnection::default();
    for request in read_back["requests"].as_array().into_iter().flatten() {
        let result = &request["result"];
        if result["source"].as_str() != Some("sched.offcpu") {
            continue;
        }
        for output in result["outputs"].as_array().into_iter().flatten() {
            if output["emit"].as_str() != Some("raw") {
                continue;
            }
            for event in output["events"].as_array().into_iter().flatten() {
                let Some(tid) = event["keys"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find_map(|key| key["task"]["tid"].as_u64())
                else {
                    continue;
                };
                let key = format!("task:{tid}");
                let attr = |name: &str| {
                    event["attrs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|attr| attr["name"] == name)
                        .and_then(|attr| attr["value"].as_f64())
                };
                if let Some(pid) = attr("pid").filter(|pid| *pid > 0.0) {
                    found.pid.entry(key.clone()).or_insert(pid as u64);
                }
                let Some(stack) = attr("stack_id") else {
                    continue;
                };
                let through = result["stacks"][(stack as u64).to_string().as_str()]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .any(is_connection_frame);
                if through {
                    *found.blocked.entry(key).or_default() +=
                        attr("total_ns").unwrap_or(0.0) - attr("queued_ns").unwrap_or(0.0);
                }
            }
        }
    }
    found
}

/// Whether a frame is a send or a receive on a connection
/// (`is_connection_frame` in `src/triage/latency.rs`): an `accept`, a
/// `select` or a `poll` waits for a connection to begin, not on one.
fn is_connection_frame(frame: &str) -> bool {
    if frame.starts_with("_socket.") {
        return true;
    }
    let name = frame
        .split(['+', '@', ' ', '('])
        .next()
        .unwrap_or("")
        .trim_start_matches('_');
    let name = name.strip_prefix("libc_").unwrap_or(name);
    matches!(
        name,
        "send" | "sendto" | "sendmsg" | "sendmmsg" | "recv" | "recvfrom" | "recvmsg" | "recvmmsg"
    ) || name.contains("sendall")
}

/// Whether a frame is the tool's own eBPF program, or the kernel code that
/// runs one (`is_ebpf_frame` in `src/triage/diff.rs`).
fn is_ebpf_frame(frame: &str) -> bool {
    let name = frame.split(['+', ' ', '(']).next().unwrap_or("");
    name.starts_with("bpf_prog_")
        || name.starts_with("bpf_trace_run")
        || name.starts_with("__bpf_trace_")
        || name.starts_with("__bpf_prog_")
        || name.starts_with("bpf_trampoline_")
        || name == "trace_call_bpf"
}

/// The labels a perf map named in a result's stacks, from the provenance
/// written beside them (`named_by`: for each stack id, each frame's namer, in
/// the order of its labels); `None` for a result that carries none.
fn provenance_symbols(result: &Value) -> Option<BTreeSet<String>> {
    let named_by = result["named_by"].as_object()?;
    let mut symbols = BTreeSet::new();
    for (id, namers) in named_by {
        let labels = result["stacks"][id.as_str()].as_array();
        for (label, namer) in labels
            .into_iter()
            .flatten()
            .zip(namers.as_array().into_iter().flatten())
        {
            if namer.as_str() == Some("perf-map") {
                if let Some(label) = label.as_str() {
                    symbols.insert(label.trim_end_matches(" [recovered]").to_owned());
                }
            }
        }
    }
    Some(symbols)
}

/// A row's amount, by state before any qualifier.
fn table_amount(table: &Value, state: &str) -> f64 {
    table["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| base_state(row["state"].as_str().unwrap_or("")) == state)
        .filter_map(|row| row["amount"].as_f64())
        .sum()
}

/// A table's largest row: (amount, state, source, resolution).
fn table_top(table: &Value) -> Option<(f64, String, String, f64)> {
    let mut top: Option<(f64, String, String, f64)> = None;
    for row in table["rows"].as_array().into_iter().flatten() {
        if let Some(amount) = row["amount"].as_f64() {
            if top.as_ref().is_none_or(|(best, ..)| amount > *best) {
                top = Some((
                    amount,
                    row["state"].as_str().unwrap_or("").to_owned(),
                    row["source"].as_str().unwrap_or("").to_owned(),
                    row["resolution"].as_f64().unwrap_or(0.0),
                ));
            }
        }
    }
    top
}

/// Which process each task is in and each process's parent, from every raw
/// event of the read-back that carries `pid` (and `ppid`).
#[derive(Debug, Default)]
struct Processes {
    pids: BTreeMap<String, u64>,
    parents: BTreeMap<u64, u64>,
}

impl Processes {
    fn of(read_back: &Value) -> Self {
        let mut found = Self::default();
        for request in read_back["requests"].as_array().into_iter().flatten() {
            for output in request["result"]["outputs"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if output["emit"].as_str() != Some("raw") {
                    continue;
                }
                for event in output["events"].as_array().into_iter().flatten() {
                    let attr = |name: &str| {
                        event["attrs"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|attr| attr["name"] == name)
                            .and_then(|attr| attr["value"].as_f64())
                            .filter(|value| *value > 0.0)
                            .map(|value| value as u64)
                    };
                    let Some(pid) = attr("pid") else { continue };
                    if let Some(tid) = event["keys"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find_map(|key| key["task"]["tid"].as_u64())
                    {
                        found.pids.entry(format!("task:{tid}")).or_insert(pid);
                    }
                    if let Some(ppid) = attr("ppid") {
                        found.parents.entry(pid).or_insert(ppid);
                    }
                }
            }
        }
        found
    }

    fn pid_of(&self, task: &str) -> Option<u64> {
        self.pids.get(task).copied()
    }

    /// Whether `task`'s process runs under `parent`: yes when its parents
    /// reach it, no when they reach the program's own main process first
    /// (or pid 1), unknown when they stop at a process no event gave a
    /// parent -- triage's `descends`.
    fn under(&self, task: &str, parent: u64, main: Option<u64>) -> Option<bool> {
        let mut at = self.pid_of(task)?;
        for _ in 0..64 {
            if at == parent {
                return Some(true);
            }
            if Some(at) == main || at <= 1 {
                return Some(false);
            }
            at = *self.parents.get(&at)?;
        }
        None
    }
}

/// `run_session`'s read-back, read the way an agent would, and the way
/// `omn latency` reads the same run (`docs/cases.md`, "The agent's reading"):
///
/// - the first table, the task whose rows account for the most of the life:
///   the program's main thread;
/// - while that table's largest row is `blocked` and another task not yet
///   read was busy -- on a cpu or queued for one -- for a quarter of that
///   block, that task's table instead, the block as its whole;
/// - the dominant stack of the request the read table's largest row came
///   from, among that task's own events (`raw`, which the approved session
///   asks beside `stack`): by its blocked nanoseconds off the cpu; on it, each
///   sample charged to the innermost frame a perf map named, or to its leaf,
///   and an interpreter's flat profile -- charged by leaves, none on half
///   the samples -- naming no function.
///
/// `programs` are the names the launched command's words go by; `since` is
/// when the run was asked for, before which a perf map is not this run's.
fn read_mcp(
    answer_doc: &Value,
    programs: &[String],
    since: Option<SystemTime>,
) -> Result<Answer, String> {
    let read_back = &answer_doc["read_back"];
    let tables: Vec<&Value> = read_back["decompositions"]
        .as_array()
        .and_then(|all| all.first())
        .and_then(|decomposition| decomposition["tables"].as_array())
        .map(|tables| tables.iter().collect())
        .unwrap_or_default();
    let mut table = *tables.first().ok_or("the run has no decomposition table")?;
    let mut whole = (
        table["whole"]["amount"].as_f64().unwrap_or(0.0),
        table["whole"]["resolution"].as_f64().unwrap_or(0.0),
    );
    let mut seen: BTreeSet<&str> = BTreeSet::from([table["key"].as_str().unwrap_or("")]);
    let mut followed = Vec::new();
    let on_connection = blocked_on_connection(read_back);
    // Whether the last step was to a thread waiting beside the block rather
    // than one that ran in it: no job follows from it.
    let mut beside = false;
    while followed.len() < MAX_FOLLOWED {
        let Some((block, state, _, resolution)) = table_top(table) else {
            break;
        };
        if base_state(&state) != "blocked" || block <= 0.0 {
            break;
        }
        let busy = |table: &Value| table_amount(table, "on-cpu") + table_amount(table, "runnable");
        let ran = tables
            .iter()
            .filter(|other| !seen.contains(other["key"].as_str().unwrap_or("")))
            .max_by(|a, b| busy(a).total_cmp(&busy(b)))
            .filter(|other| busy(other) >= RAN_IN_IT * block);
        // Nothing ran in the block: a thread of the same process blocked on
        // a connection through a quarter of it is what the program waited
        // on, named beside the block, its whole its own rows (the person's
        // way's `waiting`).
        let process = on_connection
            .pid
            .get(table["key"].as_str().unwrap_or(""))
            .copied();
        let waiting = || {
            tables
                .iter()
                .filter(|other| !seen.contains(other["key"].as_str().unwrap_or("")))
                .filter(|other| {
                    process.is_some()
                        && on_connection
                            .pid
                            .get(other["key"].as_str().unwrap_or(""))
                            .copied()
                            == process
                })
                .map(|other| {
                    let key = other["key"].as_str().unwrap_or("");
                    (
                        other,
                        on_connection.blocked.get(key).copied().unwrap_or(0.0),
                    )
                })
                .filter(|(_, amount)| *amount >= RAN_IN_IT * block)
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(other, _)| other)
        };
        let (next, next_whole, how) = match ran {
            Some(next) => (next, (block, resolution), "->"),
            None => {
                let Some(next) = waiting() else {
                    break;
                };
                let rows = next["rows"].as_array();
                let amount: f64 = rows
                    .into_iter()
                    .flatten()
                    .filter_map(|row| row["amount"].as_f64())
                    .sum();
                let resolution: f64 = rows
                    .into_iter()
                    .flatten()
                    .filter_map(|row| row["resolution"].as_f64())
                    .sum();
                (
                    next,
                    (amount, resolution),
                    "beside, blocked on a connection:",
                )
            }
        };
        beside = ran.is_none();
        followed.push(format!(
            "{} {state} {:.1} ms {how} {}",
            table["key"].as_str().unwrap_or(""),
            block / 1e6,
            next["key"].as_str().unwrap_or("")
        ));
        seen.insert(next["key"].as_str().unwrap_or(""));
        table = next;
        whole = next_whole;
    }
    let task = table["key"].as_str().map(str::to_owned);
    // The rest of the job, as the person's way reads it: every busy task off
    // the followed chain that runs under the chosen child's parent, once the
    // chain left the main thread -- triage's `Job::under`. Read off the raw
    // events' `pid` and `ppid`, which the sched sources carry since the
    // twenty-fifth wave. A task whose parents stop short of an answer is
    // left out of the job, as triage keeps it apart (`unplaced`). With no
    // `ppid` in the run at all, every other task: the upper bound triage's
    // own `Job` says ("may be more").
    let mut job = BTreeMap::new();
    if !followed.is_empty() && !beside {
        let tree = Processes::of(read_back);
        let main = tables
            .first()
            .and_then(|first| tree.pid_of(first["key"].as_str().unwrap_or("")));
        let chosen_parent = task
            .as_deref()
            .and_then(|task| tree.pid_of(task))
            .and_then(|pid| tree.parents.get(&pid).copied());
        for state in ["on-cpu", "runnable"] {
            let rest: f64 = tables
                .iter()
                .filter(|other| !seen.contains(other["key"].as_str().unwrap_or("")))
                .filter(|other| match chosen_parent {
                    Some(parent) if !tree.parents.is_empty() => {
                        tree.under(other["key"].as_str().unwrap_or(""), parent, main) == Some(true)
                    }
                    _ => true,
                })
                .map(|other| table_amount(other, state))
                .sum();
            if rest > 0.0 {
                job.insert(state.to_owned(), rest);
            }
        }
    }
    let mut answer = Answer {
        way: "mcp",
        whole,
        task: task.clone(),
        followed,
        job,
        ..Answer::default()
    };
    answer.how.method = read_back["decompositions"][0]["id"]
        .as_str()
        .map(|id| format!("decomposition `{id}`"));
    for row in table["rows"].as_array().into_iter().flatten() {
        let state = row["state"].as_str().unwrap_or("").to_owned();
        answer.read_as.insert(
            base_state(&state),
            (
                row["source"].as_str().unwrap_or("").to_owned(),
                row["kind"].as_str().unwrap_or("").to_owned(),
            ),
        );
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
        }
    }
    let (_, state, source, _) =
        table_top(table).ok_or("no row of the decomposition has an amount")?;
    answer.state = base_state(&state);
    let tid = task
        .as_deref()
        .and_then(|task| task.strip_prefix("task:"))
        .and_then(|tid| tid.parse::<u64>().ok());
    let mut notes: Vec<String> = Vec::new();
    for request in read_back["requests"].as_array().into_iter().flatten() {
        let result = &request["result"];
        for note in result["notes"].as_array().into_iter().flatten() {
            let note = note.as_str().unwrap_or("");
            notes.push(note.to_owned());
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
    }
    let Some(result) = read_back["requests"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|request| &request["result"])
        .find(|result| result["source"].as_str() == Some(source.as_str()))
    else {
        return Ok(answer);
    };
    let stacks = &result["stacks"];
    let frames_of = |id: u64| -> Vec<String> {
        stacks[id.to_string().as_str()]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    // Each of the task's events: its stack and its weight -- a sample's
    // count on a cpu, a block's nanoseconds less its wait for a cpu off it.
    let mut weighed: Vec<(u64, f64)> = Vec::new();
    let mut raw_seen = false;
    for output in result["outputs"].as_array().into_iter().flatten() {
        if output["emit"].as_str() != Some("raw") {
            continue;
        }
        raw_seen = true;
        for event in output["events"].as_array().into_iter().flatten() {
            // A table keyed by something other than a task has no events of
            // its own: none is taken as its, rather than every event that
            // carries no task.
            let of_task = tid.is_some()
                && event["keys"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|key| key["task"]["tid"].as_u64() == tid);
            if !of_task {
                continue;
            }
            let attr = |name: &str| {
                event["attrs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|attr| attr["name"] == name)
                    .and_then(|attr| attr["value"].as_f64())
            };
            let Some(stack) = attr("stack_id") else {
                continue;
            };
            let weight = if source == "cpu.profile" {
                attr("count").unwrap_or(1.0)
            } else {
                attr("total_ns").unwrap_or(0.0) - attr("queued_ns").unwrap_or(0.0)
            };
            weighed.push((stack as u64, weight));
        }
    }
    if !raw_seen {
        // Without the events, the whole run's stacks, every task's.
        for output in result["outputs"].as_array().into_iter().flatten() {
            for row in output["stacks"].as_array().into_iter().flatten() {
                if let Some(stack) = row["stack"].as_u64() {
                    weighed.push((stack, row["total"].as_f64().unwrap_or(0.0)));
                }
            }
        }
        answer.how.unseen.push(format!(
            "{source}: no raw events, so the stacks are every task's"
        ));
    }
    if source == "cpu.profile" {
        // The frames a perf map named, as the result says them beside its
        // stacks (`named_by`); read back from the maps only for a result
        // that does not, as the triage does (`PerfMapNames::of_run`).
        let named = match provenance_symbols(result) {
            Some(named) => named,
            None => {
                let (named, stale) = perf_map_symbols(&notes, since);
                answer.how.unseen.extend(stale);
                named
            }
        };
        let mut charged: BTreeMap<String, (f64, bool)> = BTreeMap::new();
        let (mut total, mut interpreted) = (0.0, 0.0);
        let mut ebpf = 0.0;
        for (stack, weight) in &weighed {
            let frames = frames_of(*stack);
            // A sample that ran the tool's own eBPF programs is the tool's
            // cost on the program's cpu, never its suspect, as the person's
            // way reads it (`diff::hottest`).
            if frames.iter().any(|frame| is_ebpf_frame(frame)) {
                ebpf += weight;
                continue;
            }
            total += weight;
            if frames.iter().any(|frame| {
                frame
                    .rsplit_once("+0x")
                    .is_some_and(|(module, _)| is_interpreter(module))
            }) {
                interpreted += weight;
            }
            let mapped_frame = frames
                .iter()
                .rev()
                .find(|frame| named.contains(frame.trim_end_matches(" [recovered]")));
            let (name, mapped) = match mapped_frame {
                Some(frame) => (frame.clone(), true),
                None => (
                    frames
                        .last()
                        .cloned()
                        .unwrap_or_else(|| "[no stack]".to_owned()),
                    false,
                ),
            };
            let entry = charged.entry(name).or_default();
            entry.0 += weight;
            entry.1 |= mapped;
        }
        let interpreter =
            programs.iter().any(|name| is_interpreter(name)) || interpreted >= FLAT * total;
        let hottest = charged
            .into_iter()
            .max_by(|a, b| a.1 .0.total_cmp(&b.1 .0).then_with(|| b.0.cmp(&a.0)));
        if let Some((name, (weight, mapped))) = hottest {
            if mapped || !interpreter || weight >= FLAT * total {
                answer.leaf = Some(name);
            }
        }
        if ebpf > 0.0 {
            answer.how.unseen.push(format!(
                "{source}: {ebpf} sample(s) ran eBPF programs, the tool's own cost, and are \
                 charged to no function"
            ));
        }
        answer.frames = answer.leaf.iter().cloned().collect();
    } else {
        let mut by_stack: BTreeMap<u64, f64> = BTreeMap::new();
        for (stack, weight) in &weighed {
            *by_stack.entry(*stack).or_default() += weight;
        }
        if let Some((stack, _)) = by_stack.into_iter().max_by(|a, b| a.1.total_cmp(&b.1)) {
            // Leaf first, as the person's way writes it.
            answer.frames = frames_of(stack).into_iter().rev().collect();
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
const SAYS: &str = "the-sentence-says-what-the-case-knows";
const PRIVILEGES: &str = "both-ways-run-with-the-same-privileges";
const LIMIT_SPELLED: &str = "the-verdict-s-json-spells-its-limit-as-the-text-does";

/// Every check this file knows, so a case cannot list one that does not
/// exist.
const CHECKS: [&str; 12] = [
    LIMIT_SPELLED,
    SAME_SUSPECT,
    KNOWN_CAUSE,
    KNOWN_LEAF,
    NUMBERS,
    HOW,
    CLI_NAMES,
    CLI_ORIGINS,
    BLOCK_NAMED,
    NOTHING_FALSE,
    SAYS,
    PRIVILEGES,
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
    /// Whether the case asks for more than the file's own names. Under
    /// `symbols: local` the file's own names (its symbols, a perf map) are
    /// all there will be, so a frame absent from a stack is absent, not
    /// unnamed.
    beyond_local: bool,
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
    if expect.names_no_function {
        report.pass_or(at, KNOWN_LEAF, answer.leaf.is_none(), || {
            format!(
                "the answer names `{}` as the hottest function, where the case knows the \
                 profile is the interpreter's and names none",
                answer.leaf.as_deref().unwrap_or("")
            )
        });
    }

    if let Some(through) = &expect.through {
        let named = answer.frames.iter().filter(|f| is_named(f)).count();
        let outcome = if answer
            .frames
            .iter()
            .any(|f| f.starts_with(through.as_str()))
        {
            Outcome::Pass
        } else if let (Some(why), true, true) = (
            &naming.unavailable,
            naming.beyond_local,
            named < answer.frames.len(),
        ) {
            // A frame no name reaches is not a frame that is missing, where
            // names could have been had and were not.
            Outcome::Unchecked(format!(
                "{why}; {named} of {} frames named: {}",
                answer.frames.len(),
                answer.frames.join(" <- ")
            ))
        } else {
            Outcome::Fail(format!(
                "the dominant stack does not pass through `{through}` ({named} of {} frames named): {}",
                answer.frames.len(),
                answer.frames.join(" <- ")
            ))
        };
        report.add(at, BLOCK_NAMED, outcome);
    }

    // What the answer must not say: the person's sentence, and the frames of
    // the stack the agent names, which is all the agent's way says.
    if !expect.never_says.is_empty() {
        let said = if answer.way == "cli" {
            answer.sentence.clone()
        } else {
            answer.frames.join(" <- ")
        };
        let found: Vec<&String> = expect
            .never_says
            .iter()
            .filter(|claim| said.contains(claim.as_str()))
            .collect();
        report.pass_or(at, NOTHING_FALSE, found.is_empty(), || {
            format!("the answer says {found:?}: {said}")
        });
    }
    if answer.way == "cli" {
        if let Some(written) = &answer.limit_written {
            report.pass_or(at, LIMIT_SPELLED, !written.contains('_'), || {
                format!(
                    "the JSON's `limit` is `{written}`, the text's and the catalog's `{}`",
                    written.replace('_', "-")
                )
            });
        }
        if !expect.says.is_empty() {
            let missing: Vec<&String> = expect
                .says
                .iter()
                .filter(|claim| !answer.sentence.contains(claim.as_str()))
                .collect();
            report.pass_or(at, SAYS, missing.is_empty(), || {
                format!("the sentence does not say {missing:?}: {}", answer.sentence)
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
    // The rest of a job is added only where the case says the verdict follows
    // a wait for a child: the person's way cannot tell a child's tasks from a
    // thread's, and a thread followed out of a server is not a job.
    let for_a_child = expect.says.iter().any(|says| says.contains("for a child"));
    for agree in &expect.agrees {
        let result = (|| -> Result<Option<String>, String> {
            let (amount, row_resolution) = *answer
                .rows
                .get(&agree.row)
                .ok_or_else(|| format!("no `{}` row has an amount", agree.row))?;
            // A row decomposed in place of a wait for a child is that child's;
            // the rest of its job ran beside it, and a reference that counts
            // the job counts both. The job is every busy task off the way
            // (`triage::latency::Job`), so this can only raise the row: a
            // check that passes with it is loosened, not fixed in triage.
            let job = if for_a_child {
                answer.job.get(&agree.row).copied()
            } else {
                None
            };
            let amount = amount + job.unwrap_or(0.0);
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
    report.pass_or(at, HOW, lacking.is_empty(), || {
        format!("the answer does not say {}", lacking.join(", "))
    });
}

// -------------------------------------------------------------- the two ways

/// Keeps an answer as it was given, under `OMN_CASES_KEEP` when it is set:
/// `<case>-<cause>-<way>.json`, for reading a check's outcome back.
fn kept(places: &Places, cause: &Cause, way: &str, document: &str) {
    let Some(dir) = std::env::var_os("OMN_CASES_KEEP") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let case = places
        .case
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join(format!("{case}-{}-{way}.json", cause.name)),
        document,
    );
}

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
    let said = match (injected, &asked) {
        // Its account is read only with an answer: a run the machine refused
        // or a budget stopped never reaches the end it waits for, and
        // dropping it ends it now rather than at its own deadline.
        (Some(injected), Asked::Answer(..)) => injected.finish()?,
        _ => String::new(),
    };
    Ok((asked, said))
}

fn ask_cli(cause: &Cause, places: &Places, naming: &Naming) -> Result<Asked, String> {
    // Read before the run, as `omn-mcp` reads them before it answers.
    let privileges = privileges_of(Some(Path::new(OMN)), &file_capabilities(Path::new(OMN)));
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
    kept(places, cause, "cli", &stdout);
    let mut answer = read_cli(&stdout)?;
    answer.privileges = privileges;
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
        // The reply is the message carrying this request's id; a notification
        // or a log message the server sends first is not it.
        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .map_err(|error| format!("omn-mcp said nothing: {error}"))?;
            if read == 0 {
                return Err(format!(
                    "omn-mcp closed its output before answering `{method}`"
                ));
            }
            let message: Value = serde_json::from_str(&line)
                .map_err(|error| format!("not a JSON-RPC line ({error}): {line}"))?;
            if message["id"] == id {
                return Ok(message);
            }
        }
    }

    /// A tool call: its structured content, or its refusal.
    fn call(&mut self, tool: &str, arguments: Value) -> Result<Value, String> {
        let reply = self.request("tools/call", json!({"name": tool, "arguments": arguments}))?;
        if !reply["error"].is_null() {
            return Err(format!(
                "{tool} was refused by the protocol: {}",
                reply["error"]
            ));
        }
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
    // Each stack's events too, so the agent can read the stacks of the task
    // it follows rather than every task's: a `stack` output is not keyed by
    // task (`docs/fixes/w18-cases.md`, "Needs from another stream").
    for request in session["requests"].as_sequence_mut().into_iter().flatten() {
        if let Some(emit) = request["emit"].as_sequence_mut() {
            if emit.iter().any(|kind| kind.as_str() == Some("stack"))
                && !emit.iter().any(|kind| kind.as_str() == Some("raw"))
            {
                emit.push("raw".into());
            }
        }
    }
    if let Some(symbols) = &naming.symbols {
        // Names on every request whose collector reads them in this build:
        // asked of every request, and taken back from each source the build
        // refuses it for (`omn` names the first such source, exit 3). So the
        // sources that read `--symbols` are `omn`'s to say, and one that
        // starts to -- `sched.offcpu`'s stacks -- is asked without a line
        // changing here.
        let probe = places.dir.join(format!("symbols-{}.yml", cause.name));
        let mut refused: BTreeSet<String> = BTreeSet::new();
        loop {
            for request in session["requests"].as_sequence_mut().into_iter().flatten() {
                let source = request["source"].as_str().unwrap_or("").to_owned();
                if refused.contains(&source) {
                    if let Some(map) = request.as_mapping_mut() {
                        map.remove("symbols");
                    }
                } else if request.get("symbols").is_none() {
                    request["symbols"] = symbols.clone().into();
                }
            }
            let text_of = serde_norway::to_string(&session).map_err(|error| error.to_string())?;
            std::fs::write(&probe, &text_of).map_err(|error| error.to_string())?;
            let output = run(
                &[
                    OMN.to_owned(),
                    "-f".to_owned(),
                    probe.to_string_lossy().into_owned(),
                    "--dump-resolved".to_owned(),
                ],
                &places.dir,
                Duration::from_secs(30),
            )?;
            let said = text(&output.stderr);
            let not_read = (output.status.code() == Some(3))
                .then(|| said.split("is not implemented for `").nth(1))
                .flatten()
                .and_then(|rest| rest.split('`').next())
                .map(str::to_owned);
            match not_read {
                Some(source) if refused.insert(source.clone()) => {}
                Some(source) => {
                    return Err(format!(
                        "`--symbols` is still refused for `{source}` once taken back out: {said}"
                    ))
                }
                None => break,
            }
        }
    }
    serde_norway::to_string(&session).map_err(|error| error.to_string())
}

/// Asks as a model would: `run_session` with no budget, which it takes
/// none of unless asked (`docs/mcp.md`). What the run said is `omn`'s lines
/// and then the program's whole stderr, which `run_session` hands over apart
/// (`program_stderr`), so a reference read `from: target` finds the program's
/// own account where the person's way finds it.
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
    let arguments = json!({
        "approved": file,
        "duration": format!("{}s", RUN_CEILING.as_secs()),
    });
    // A perf map last written before this is an earlier process's.
    let asked_at = SystemTime::now();
    let ran = mcp.call("run_session", arguments)?;
    let mut said = match &ran["said_by_omn"] {
        Value::Array(lines) => lines
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.as_str().unwrap_or("").to_owned(),
    };
    said.push('\n');
    said.push_str(&program_stderr(&ran["program_stderr"]));
    if ran["exit"] != 0 {
        if let Some(reason) = withheld(ran["exit"].as_i64().map(|code| code as i32), &said) {
            return Ok(Asked::Withheld(reason));
        }
        return Err(format!("run_session's omn exited {}:\n{said}", ran["exit"]));
    }
    // Every event of each request that kept them: the read-back cuts them at
    // a hundred.
    let mut ran = ran;
    let run_name = ran["run"].as_str().unwrap_or("").to_owned();
    let labels: Vec<String> = ran["read_back"]["requests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|request| {
            request["result"]["outputs"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|output| output["emit"] == "raw")
        })
        .filter_map(|request| request["label"].as_str().map(str::to_owned))
        .collect();
    for label in labels {
        let whole = mcp.call(
            "read_run",
            json!({"run": run_name, "label": label, "max_events": 0}),
        )?;
        let Some(full) = whole["requests"].as_array().and_then(|all| all.first()) else {
            continue;
        };
        if let Some(requests) = ran["read_back"]["requests"].as_array_mut() {
            for request in requests.iter_mut() {
                if request["label"].as_str() == Some(label.as_str()) {
                    *request = full.clone();
                }
            }
        }
    }
    kept(places, cause, "mcp", &ran.to_string());
    let mut answer = read_mcp(&ran, &program_names(cause, places), Some(asked_at))?;
    answer.privileges = privileges_of(
        ran["answered_by"]["binary"].as_str().map(Path::new),
        &ran["answered_by"]["file_capabilities"],
    );
    Ok(Asked::Answer(Box::new(answer), said))
}

/// The names the launched command's words go by, as the triage reads them
/// (`executable_names` in `src/triage/diff.rs`): each word's base name, and
/// the file it is on `PATH`, symlinks followed (`python3` is `python3.14`).
fn program_names(cause: &Cause, places: &Places) -> Vec<String> {
    let mut names = Vec::new();
    for word in places.argv(&cause.target) {
        let base = |path: &Path| path.file_name().map(|n| n.to_string_lossy().into_owned());
        names.extend(base(Path::new(&word)));
        if let Some(real) = which(&word).and_then(|path| std::fs::canonicalize(path).ok()) {
            names.extend(base(&real));
        }
    }
    names
}

/// Who answered, said so that two answers can be compared: the binary,
/// symlinks followed, and the capabilities on it, as `omn-mcp`'s
/// `answered_by` spells them.
fn privileges_of(binary: Option<&Path>, capabilities: &Value) -> String {
    let binary = binary
        .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
        .map_or("an unnamed binary".to_owned(), |path| {
            path.display().to_string()
        });
    format!("{binary} with {capabilities}")
}

/// The capabilities on a file, read off its `security.capability`
/// attribute the way `omn-mcp` reads them (`mcp/src/binary.rs`,
/// `file_capabilities`): CAP_BPF, CAP_PERFMON and CAP_NET_RAW permitted, and
/// the effective flag.
fn file_capabilities(path: &Path) -> Value {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return json!({"unknown": "the path holds a NUL byte"});
    };
    let mut buffer = [0u8; 64];
    // SAFETY: both strings are NUL-terminated, and the buffer's length is
    // passed with it.
    let length = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            c"security.capability".as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if length < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENODATA) => json!({
                "cap_bpf": false, "cap_net_raw": false, "cap_perfmon": false, "effective": false,
            }),
            _ => json!({"unknown": error.to_string()}),
        };
    }
    let length = length as usize;
    if length < 12 {
        return json!({"unknown": format!("a {length}-byte capability attribute")});
    }
    let word =
        |index: usize| u32::from_le_bytes(buffer[index * 4..index * 4 + 4].try_into().unwrap());
    let (low, high) = (word(1), if length >= 20 { word(3) } else { 0 });
    let has = |cap: u32| {
        if cap < 32 {
            low & (1 << cap) != 0
        } else {
            high & (1 << (cap - 32)) != 0
        }
    };
    json!({
        "cap_bpf": has(39),
        "cap_net_raw": has(13),
        "cap_perfmon": has(38),
        "effective": word(0) & 1 != 0,
    })
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

/// Whether a file carries a `.symtab`, from its section headers' names:
/// `None` for anything that is not a well-formed 64-bit ELF file, of either
/// byte order.
fn has_symtab(path: &Path) -> Option<bool> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.get(..4)? != b"\x7fELF" || *bytes.get(4)? != 2 {
        return None;
    }
    let little = match *bytes.get(5)? {
        1 => true,
        2 => false,
        _ => return None,
    };
    let int = |at: usize, width: usize| -> Option<usize> {
        let field = bytes.get(at..at.checked_add(width)?)?;
        let value = (0..width).fold(0u64, |value, index| {
            let byte = if little {
                field[width - 1 - index]
            } else {
                field[index]
            };
            value << 8 | u64::from(byte)
        });
        usize::try_from(value).ok()
    };
    let (shoff, shentsize, shnum, shstrndx) =
        (int(0x28, 8)?, int(0x3a, 2)?, int(0x3c, 2)?, int(0x3e, 2)?);
    let section = |index: usize| shoff.checked_add(index.checked_mul(shentsize)?);
    let names = int(section(shstrndx)?.checked_add(0x18)?, 8)?;
    let mut found = false;
    for index in 0..shnum {
        let name = names.checked_add(int(section(index)?, 4)?)?;
        found |= bytes.get(name..name.checked_add(8)?) == Some(b".symtab\0".as_slice());
    }
    Some(found)
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

/// The whole of what the launched program wrote on its stderr, from the run's
/// `program_stderr`: the file that holds all of it, else the text the answer
/// carries, else nothing (an `omn` from before the program had a stderr of
/// its own put it among `said_by_omn`, which is read already).
fn program_stderr(answer: &Value) -> String {
    answer["file"]
        .as_str()
        .and_then(|file| std::fs::read(file).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .or_else(|| answer["text"].as_str().map(str::to_owned))
        .unwrap_or_default()
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
        beyond_local: wants.is_some(),
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
                "case {at}: first suspect {} (limit {}) in {}{}, leaf {:?}; rows {}; reference {}",
                answer.state,
                answer.limit.as_deref().unwrap_or("-"),
                answer.task.as_deref().unwrap_or("-"),
                if answer.followed.is_empty() {
                    String::new()
                } else {
                    format!(" (followed: {})", answer.followed.join(", "))
                },
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
            for unseen in &answer.how.unseen {
                eprintln!("    could not see: {unseen}");
            }
            check_answer(report, &at, cause, &answer, &accounts, &naming);
            answers.push(answer);
        }
        if let [cli, mcp] = &answers[..] {
            let at = format!("{}/{}", case.name, cause.name);
            // Two ways run with different privileges answer from two
            // different machines: what differs is the machine's, and is not
            // compared.
            //
            // The file's capabilities are one half: one file, read twice. The
            // other is what the run itself was served by -- a process that
            // could not use them (`no_new_privs`, a `nosuid` mount) falls
            // back to another backend, and its rows say so in their source
            // and kind -- compared for every state both ways read.
            let read_differently: Vec<String> = cli
                .read_as
                .iter()
                .filter_map(|(state, how)| {
                    let other = mcp.read_as.get(state)?;
                    (other != how).then(|| {
                        format!(
                            "`{state}` read by {} ({}) the person's way, by {} ({}) the agent's",
                            how.0, how.1, other.0, other.1
                        )
                    })
                })
                .collect();
            let differ = if cli.privileges != mcp.privileges {
                Some(format!(
                    "the person's way ran {}, the agent's {}",
                    cli.privileges, mcp.privileges
                ))
            } else if !read_differently.is_empty() {
                Some(format!(
                    "one file with the same capabilities, but the runs were served differently: {}",
                    read_differently.join("; ")
                ))
            } else {
                None
            };
            if let Some(differ) = differ {
                report.add(&at, PRIVILEGES, Outcome::Fail(differ));
                report.add(
                    &at,
                    SAME_SUSPECT,
                    Outcome::Unchecked(
                        "not compared: the two ways ran with different privileges".to_owned(),
                    ),
                );
                continue;
            }
            report.add(&at, PRIVILEGES, Outcome::Pass);
            // The same state, and the same leaf where the case knows the
            // frame and both name one. A cause that names no frame is about a
            // state: the hottest leaf of a spread profile -- a kernel
            // function at 5 % of the samples, the next at 3.5 % -- moves
            // between two runs of the same program, and comparing two of
            // them compares two samples of the noise. Whether a leaf is named
            // at all is still compared there: one way naming a function where
            // the other says the profile names none (a flat interpreter
            // profile) is two different answers. A leaf that is an
            // address is not compared with anything -- an unnamed binary's
            // hottest leaf is an instruction, and moves from run to run -- so
            // that half is said to be unchecked, not passed.
            let differ = || {
                format!(
                    "the person's way names {} at {:?}, the agent's {} at {:?}",
                    cli.state, cli.leaf, mcp.state, mcp.leaf
                )
            };
            let outcome = match (&cli.leaf, &mcp.leaf) {
                _ if cli.state != mcp.state => Outcome::Fail(differ()),
                (Some(_), Some(_)) | (None, None) if cause.expect.leaf.is_none() => Outcome::Pass,
                (Some(_), None) | (None, Some(_)) if cause.expect.leaf.is_none() => {
                    Outcome::Fail(differ())
                }
                (Some(a), Some(b)) if is_named(a) && is_named(b) => {
                    if a == b {
                        Outcome::Pass
                    } else {
                        Outcome::Fail(differ())
                    }
                }
                (None, None) => Outcome::Pass,
                (a, b) => Outcome::Unchecked(format!(
                    "both name `{}`; the leaves were not compared, an address is not a name: \
                     the person's way's is {a:?}, the agent's {b:?}",
                    cli.state
                )),
            };
            report.add(&at, SAME_SUSPECT, outcome);
        }
    }
}

// -------------------------------------------------------------------- tests

/// Every case, both ways. One test, so the cases run one after the other:
/// each measures the machine, and two at once would measure each other.
#[test]
fn every_case_names_its_known_cause_both_ways() {
    let mut failures = Vec::new();
    let only = std::env::var("OMN_CASES_ONLY").unwrap_or_default();
    let only: BTreeSet<&str> = only.split(',').filter(|name| !name.is_empty()).collect();
    for (dir, case) in cases() {
        if !only.is_empty() && !only.contains(case.name.as_str()) {
            eprintln!(
                "case {}: not asked, OMN_CASES_ONLY names {only:?}",
                case.name
            );
            continue;
        }
        if let Some(why) = &case.held {
            eprintln!("HELD case {}: {}", case.name, why.trim());
            continue;
        }
        let mut report = Report::default();
        run_case(&dir, &case, &mut report);
        // Which finding a line is, by its index: two findings of one check
        // may name two causes.
        let finding_of = |at: &str, check: &str| {
            case.open_findings
                .iter()
                .position(|finding| finding.covers(at, check))
        };
        let mut failed_open = BTreeSet::new();
        let mut passed_open = BTreeSet::new();
        for (at, check, outcome) in &report.lines {
            match outcome {
                Outcome::Pass => {
                    eprintln!("  ok        {at}: {check}");
                    if let Some(index) = finding_of(at, check) {
                        passed_open.insert(index);
                    }
                }
                Outcome::Unchecked(why) => eprintln!("  unchecked {at}: {check}: {why}"),
                Outcome::Fail(why) => match finding_of(at, check) {
                    Some(index) => {
                        failed_open.insert(index);
                        eprintln!(
                            "  FINDING   {at}: {check}: {why}\n            filed: {}",
                            case.open_findings[index].filed
                        );
                    }
                    None => {
                        eprintln!("  FAILED    {at}: {check}: {why}");
                        failures.push(format!("{at}: {check}: {why}"));
                    }
                },
            }
        }
        for index in passed_open.difference(&failed_open) {
            let finding = &case.open_findings[*index];
            let (check, on) = (
                &finding.check,
                finding
                    .cause
                    .as_deref()
                    .map_or(String::new(), |cause| format!(" on `{cause}`")),
            );
            if finding.sometimes {
                eprintln!(
                    "  note      {}: `{check}`{on}, listed as failing in some runs, passed in \
                     every run here: close it by hand once its filing shows it closed",
                    case.name
                );
                continue;
            }
            failures.push(format!(
                "{}: `{check}`{on} is listed under open_findings and now passes everywhere it \
                 was checked: remove it from cases/{}/case.yml and close the finding",
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
        if let Some(held) = &case.held {
            assert!(
                !held.trim().is_empty(),
                "{}: a case held back says why, and where that was filed",
                case.name
            );
        }
        for finding in &case.open_findings {
            assert!(
                CHECKS.contains(&finding.check.as_str()),
                "{}: no check named `{}`; the checks are {CHECKS:?}",
                case.name,
                finding.check
            );
            assert!(!finding.filed.trim().is_empty());
            if let Some(cause) = &finding.cause {
                assert!(
                    case.causes.iter().any(|c| c.name == *cause),
                    "{}: the open finding on `{}` names no cause `{cause}`",
                    case.name,
                    finding.check
                );
            }
            assert_eq!(
                case.open_findings
                    .iter()
                    .filter(|other| other.check == finding.check
                        && (other.cause.is_none()
                            || finding.cause.is_none()
                            || other.cause == finding.cause))
                    .count(),
                1,
                "{}: `{}` is listed twice for one cause",
                case.name,
                finding.check
            );
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

/// `has_symtab` answers for a well-formed file and says nothing, rather than
/// panicking, for a truncated one.
#[test]
fn a_truncated_elf_file_has_no_answer() {
    let me = std::env::current_exe().unwrap();
    assert_eq!(
        has_symtab(&me),
        Some(true),
        "a test binary keeps its symbols"
    );
    let bytes = std::fs::read(&me).unwrap();
    let dir = std::env::temp_dir().join(format!("omn-cases-elf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for keep in [3, 6, 0x40, 0x41] {
        let cut = dir.join(format!("cut-{keep}"));
        std::fs::write(&cut, &bytes[..keep]).unwrap();
        assert_eq!(has_symtab(&cut), None, "the first {keep} bytes");
    }
    let _ = std::fs::remove_dir_all(&dir);
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

/// The person's way reads the task the verdict names: the median run, down
/// its `followed` chain, with the notes of every reading of it.
#[test]
fn the_person_s_reading_is_the_task_the_wait_was_followed_into() {
    let document = json!({
        "method": "latency",
        "verdict": {"task": "task:9", "limit": "cpu-queue", "state": "runnable",
                    "sentence": "for latency, in the run, task:1 waited 5s for a child, in `wait4`"},
        "median": 1,
        "runs": [
            {"task": "task:7", "whole": {"amount": 1.0}, "states": []},
            {"task": "task:1", "whole": {"amount": 5e9, "resolution": 1e6},
             "notes": ["[on-cpu] names came from -- make: its symbols"],
             "states": [{"state": "blocked: sleeping", "amount": 5e9, "evidence": "exact"}],
             "followed": {"relation": "child", "during": "blocked: sleeping", "amount": 5e9,
                          "job": {"tasks": ["task:12"], "on_cpu": 2e8, "runnable": 3e8},
                          "reading": {"task": "task:9", "whole": {"amount": 5e9, "resolution": 2e6},
                                      "states": [
                                          {"state": "on-cpu", "amount": 1e9, "resolution": 1e6,
                                           "evidence": "sampled",
                                           "named": [{"source": "cpu.profile",
                                                      "says": "hottest function `cc1_main`, 5 of 9 samples"}]},
                                          {"state": "runnable", "amount": 4e9, "evidence": "exact"}]}}}
        ]
    });
    let answer = read_cli(&document.to_string()).unwrap();
    assert_eq!(answer.task.as_deref(), Some("task:9"));
    assert_eq!(answer.limit.as_deref(), Some("cpu-queue"));
    assert_eq!(answer.state, "runnable");
    assert_eq!(answer.rows["runnable"].0, 4e9);
    // The rest of the job is read beside the child's rows, not into them.
    assert_eq!(answer.job.get("on-cpu"), Some(&2e8));
    assert_eq!(answer.job.get("runnable"), Some(&3e8));
    assert!(
        !answer.rows.contains_key("blocked"),
        "the main thread's rows are not read"
    );
    assert_eq!(answer.whole, (5e9, 2e6));
    assert!(
        answer.how.names.is_some(),
        "the first reading's names note is kept"
    );

    // A flat interpreter profile names no function, and is not read as one.
    let mut flat = document.clone();
    flat["runs"][1]["followed"]["reading"]["states"][0]["named"][0]["says"] =
        json!("the program runs in the python interpreter and its time is spread (`x` 4 %)");
    flat["verdict"]["state"] = json!("on-cpu");
    assert_eq!(read_cli(&flat.to_string()).unwrap().leaf, None);
}

/// The agent's way follows a block into the task that was busy during it,
/// reads that task's own events for its stack, and charges a sample to the
/// innermost frame a perf map named.
#[test]
fn the_agent_s_reading_follows_a_block_into_the_busy_task() {
    // Where every runtime writes its map, and where the reading looks.
    let map = Path::new("/tmp").join(format!("perf-{}.map", std::process::id()));
    std::fs::write(&map, "7f00 40 py::score:/x/hot.py\n").unwrap();
    let map_name = map.file_name().unwrap().to_string_lossy().into_owned();
    let event = |tid: u64, stack: u64| {
        json!({"keys": [{"task": {"tid": tid}}],
               "attrs": [{"name": "count", "value": 1}, {"name": "stack_id", "value": stack}]})
    };
    let mut events: Vec<Value> = (0..6).map(|_| event(2, 1)).collect();
    // The main thread's samples, under another leaf, outnumber nothing of
    // the followed task's: they are not its.
    events.extend((0..9).map(|_| event(1, 2)));
    let answer_doc = json!({
        "read_back": {
            "decompositions": [{"id": "where-the-time-went", "tables": [
                {"key": "task:1", "whole": {"amount": 4e9, "resolution": 1e6}, "rows": [
                    {"state": "on-cpu", "amount": 1e7, "source": "cpu.profile", "evidence": "sampled"},
                    {"state": "blocked", "amount": 3.9e9, "resolution": 1e6, "source": "sched.offcpu",
                     "evidence": "exact"}]},
                {"key": "task:2", "whole": {"amount": 4e9}, "rows": [
                    {"state": "on-cpu", "amount": 3.8e9, "source": "cpu.profile", "evidence": "sampled"},
                    {"state": "blocked", "amount": 1e7, "source": "sched.offcpu", "evidence": "exact"}]},
                {"key": "task:3", "whole": {"amount": 4e9}, "rows": [
                    {"state": "on-cpu", "amount": 2e7, "source": "cpu.profile", "evidence": "sampled"}]}
            ]}],
            "requests": [{"label": "on-cpu", "result": {
                "source": "cpu.profile",
                "notes": [format!("names came from -- {map_name}: perf-map, the 1 lines it holds")],
                "stacks": {"1": ["main", "py::score:/x/hot.py", "__tls_get_addr"],
                           "2": ["main", "select"]},
                "outputs": [{"emit": "raw", "events": events}]}}]
        }
    });
    let answer = read_mcp(&answer_doc, &["python3".to_owned()], None).unwrap();
    // Asked for after the map was last written, the map is an earlier
    // process's: not read, and said.
    let later = SystemTime::now() + Duration::from_secs(3600);
    let stale = read_mcp(&answer_doc, &["python3".to_owned()], Some(later)).unwrap();
    let _ = std::fs::remove_file(&map);
    assert_ne!(stale.leaf.as_deref(), Some("py::score:/x/hot.py"));
    assert!(
        stale
            .how
            .unseen
            .iter()
            .any(|said| said.contains("an earlier process's")),
        "{:?}",
        stale.how.unseen
    );
    assert_eq!(answer.task.as_deref(), Some("task:2"));
    assert_eq!(answer.state, "on-cpu");
    assert_eq!(
        answer.whole,
        (3.9e9, 1e6),
        "the block is the followed task's whole"
    );
    assert_eq!(answer.leaf.as_deref(), Some("py::score:/x/hot.py"));
    assert_eq!(answer.followed.len(), 1);

    // A task that was busy for less than a quarter of the block is not
    // followed: the main thread's block is the answer.
    let mut idle = answer_doc.clone();
    idle["read_back"]["decompositions"][0]["tables"][1]["rows"][0]["amount"] = json!(9e8);
    let answer = read_mcp(&idle, &[], None).unwrap();
    assert_eq!(
        (answer.task.as_deref(), answer.state.as_str()),
        (Some("task:1"), "blocked")
    );
}

/// The agent's way's job, by `ppid`: once the chain leaves the main thread
/// for a busy child, the rest of the job is what runs under that child's
/// parent -- its siblings -- and neither a process elsewhere on the machine
/// nor one whose parent the run never saw.
#[test]
fn the_agent_s_job_is_what_runs_under_the_chosen_child_s_parent() {
    let busy = |key: &str, ns: f64| {
        json!({"key": key, "whole": {"amount": ns}, "rows": [
            {"state": "on-cpu", "amount": ns, "source": "cpu.profile", "evidence": "sampled"}]})
    };
    let raw = |tid: u64, pid: u64, ppid: Option<u64>| {
        let mut attrs = vec![json!({"name": "pid", "value": pid})];
        if let Some(ppid) = ppid {
            attrs.push(json!({"name": "ppid", "value": ppid}));
        }
        json!({"keys": [{"task": {"tid": tid}}], "attrs": attrs})
    };
    let answer_doc = json!({
        "read_back": {
            "decompositions": [{"id": "where-the-time-went", "tables": [
                {"key": "task:100", "whole": {"amount": 4e9, "resolution": 1e6}, "rows": [
                    {"state": "on-cpu", "amount": 1e8, "source": "cpu.profile", "evidence": "sampled"},
                    {"state": "blocked: sleeping", "amount": 3.9e9, "resolution": 1e6,
                     "source": "sched.offcpu", "evidence": "exact"}]},
                busy("task:200", 3e9),
                busy("task:300", 1e9),
                busy("task:400", 2e9),
                busy("task:500", 5e8)
            ]}],
            "requests": [{"label": "blocked", "result": {
                "source": "sched.offcpu",
                "outputs": [{"emit": "raw", "events": [
                    raw(100, 100, Some(1)),
                    raw(200, 200, Some(100)),
                    raw(300, 300, Some(100)),
                    raw(400, 400, Some(1)),
                    raw(500, 500, Some(450))]}]}}]
        }
    });
    let answer = read_mcp(&answer_doc, &[], None).unwrap();
    assert_eq!(
        answer.task.as_deref(),
        Some("task:200"),
        "{:?}",
        answer.followed
    );
    // task:300 is the chosen child's sibling; task:400 is the machine's,
    // task:500's parent 450 was never seen.
    assert_eq!(
        answer.job.get("on-cpu").copied(),
        Some(1e9),
        "{:?}",
        answer.job
    );

    // With no `ppid` in the run, every other busy task: the upper bound.
    let mut unparented = answer_doc.clone();
    for event in unparented["read_back"]["requests"][0]["result"]["outputs"][0]["events"]
        .as_array_mut()
        .unwrap()
    {
        event["attrs"].as_array_mut().unwrap().truncate(1);
    }
    let answer = read_mcp(&unparented, &[], None).unwrap();
    assert_eq!(
        answer.job.get("on-cpu").copied(),
        Some(3.5e9),
        "{:?}",
        answer.job
    );
}

/// The slow client, the agent's way: the main thread's `select` block,
/// nothing busy in it, and a thread of the same process blocked in the
/// socket module's send for most of it -- that thread is named beside the
/// block, its whole its own rows, and no job is added.
#[test]
fn the_agent_s_reading_names_a_thread_blocked_on_a_connection_beside_the_block() {
    let block = |tid: u64, stack: u64, ns: f64| {
        json!({"keys": [{"task": {"tid": tid}}],
               "attrs": [{"name": "pid", "value": 10}, {"name": "stack_id", "value": stack},
                         {"name": "total_ns", "value": ns}, {"name": "queued_ns", "value": 0}]})
    };
    let answer_doc = json!({
        "read_back": {
            "decompositions": [{"id": "where-the-time-went", "tables": [
                {"key": "task:10", "whole": {"amount": 2.4e9, "resolution": 1e6}, "rows": [
                    {"state": "on-cpu", "amount": 5e7, "source": "cpu.profile", "evidence": "sampled"},
                    {"state": "blocked: sleeping", "amount": 2.3e9, "resolution": 1e6,
                     "source": "sched.offcpu", "evidence": "exact"}]},
                {"key": "task:11", "whole": {"amount": 1.4e9}, "rows": [
                    {"state": "on-cpu", "amount": 6e7, "source": "cpu.profile", "evidence": "sampled"},
                    {"state": "blocked: sleeping", "amount": 1.3e9, "resolution": 2e5,
                     "source": "sched.offcpu", "evidence": "exact"}]},
                {"key": "task:99", "whole": {"amount": 2.4e9}, "rows": [
                    {"state": "on-cpu", "amount": 1e8, "source": "cpu.profile", "evidence": "sampled"}]}
            ]}],
            "requests": [{"label": "blocked", "result": {
                "source": "sched.offcpu",
                "stacks": {"1": ["main", "select.cpython-314-x86_64-linux-gnu.so+0x1a2b", "do_select"],
                           "2": ["start_thread", "_socket.cpython-314-x86_64-linux-gnu.so+0x5c10",
                                 "__libc_sendto", "sock_sendmsg"]},
                "outputs": [{"emit": "raw", "events": [
                    block(10, 1, 2.3e9), block(11, 2, 1.25e9), block(11, 1, 5e7)]}]}}]
        }
    });
    let answer = read_mcp(&answer_doc, &["python3".to_owned()], None).unwrap();
    assert_eq!(
        answer.task.as_deref(),
        Some("task:11"),
        "{:?}",
        answer.followed
    );
    assert_eq!(answer.state, "blocked");
    assert_eq!(answer.whole, (1.36e9, 2e5), "its whole is its own rows");
    assert!(
        answer.followed[0].contains("beside"),
        "{:?}",
        answer.followed
    );
    assert!(answer.job.is_empty(), "{:?}", answer.job);
    assert_eq!(
        answer.leaf.as_deref(),
        Some("sock_sendmsg"),
        "the block began in the send, not the select"
    );

    // A thread of another process blocked on a connection is not the
    // program's wait.
    let mut elsewhere = answer_doc.clone();
    elsewhere["read_back"]["requests"][0]["result"]["outputs"][0]["events"][1]["attrs"][0]
        ["value"] = json!(20);
    elsewhere["read_back"]["requests"][0]["result"]["outputs"][0]["events"][2]["attrs"][0]
        ["value"] = json!(20);
    let answer = read_mcp(&elsewhere, &[], None).unwrap();
    assert_eq!(answer.task.as_deref(), Some("task:10"));
}
