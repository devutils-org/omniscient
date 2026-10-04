//! The runner: a pair from `catalog/references/<family>.yaml` read, `omn`
//! and the tool run on the same workload, the tool's output parsed, and each
//! compared value held to the pair's tolerance. The schema is documented in
//! `catalog/references/README.md`; this file is its only reader.

use crate::common::{installed, root, Parser, Values};
use serde::Deserialize;
use serde_json::Value as Json;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// One pair: an `omn` source against one common use of one tool.
#[derive(Debug, Clone, Deserialize)]
pub struct Pair {
    /// Optional: a name the family's table can cite when `source` and
    /// `tool` are not enough to tell two pairs apart.
    #[serde(default)]
    pub id: Option<String>,
    pub source: String,
    pub tool: String,
    #[serde(rename = "use")]
    pub use_: String,
    #[serde(default)]
    pub workload: Vec<String>,
    #[serde(default)]
    pub omn: Vec<String>,
    pub reference: Reference,
    pub compare: Compares,
    #[serde(default)]
    pub run: RunMode,
    /// Runs per side; the median of each value is compared.
    #[serde(default = "one")]
    pub runs: usize,
    /// `attach`: how long both observers get to attach before the workload
    /// is let go.
    #[serde(default)]
    pub settle_ms: Option<u64>,
    /// Per command; 120 s when absent.
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Written, but `omn` cannot serve it yet: what it lacks. Not run.
    #[serde(default)]
    pub gap: Option<String>,
}

fn one() -> usize {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct Reference {
    pub command: Vec<String>,
    /// A second command whose output is the one parsed (`perf report` after
    /// `perf record`); `command`'s own output is then discarded.
    #[serde(default)]
    pub report: Vec<String>,
    pub parse: String,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub privileged: bool,
    /// Text in the tool's stderr that means this machine refused it: the run
    /// then skips quoting it rather than failing.
    #[serde(default)]
    pub refused_when: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Compares {
    One(Compare),
    Many(Vec<Compare>),
}

impl Compares {
    pub fn all(&self) -> Vec<&Compare> {
        match self {
            Compares::One(c) => vec![c],
            Compares::Many(cs) => cs.iter().collect(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Compare {
    /// The parsed value's key; `""` for a scalar, `*` for every key the tool
    /// gave (the `omn` path then says `{key}` where the key goes).
    #[serde(default)]
    pub key: String,
    /// Where in `omn`'s JSON the value is (README: the path syntax).
    pub omn: String,
    pub tolerance: Tolerance,
    pub why: String,
    #[serde(default = "unit")]
    pub omn_scale: f64,
    #[serde(default = "unit")]
    pub reference_scale: f64,
}

fn unit() -> f64 {
    1.0
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Tolerance {
    #[serde(default)]
    pub rel: Option<f64>,
    #[serde(default)]
    pub abs: Option<f64>,
}

impl Tolerance {
    /// The difference allowed around `reference`: the larger of the two.
    pub fn allowed(&self, reference: f64) -> f64 {
        let rel = self.rel.map(|r| r * reference.abs()).unwrap_or(0.0);
        rel.max(self.abs.unwrap_or(0.0))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunMode {
    /// `omn` launches the workload, then the tool launches it: two runs.
    #[default]
    Separate,
    /// The runner launches the workload stopped, starts both observers with
    /// `{pid}` substituted, and lets it go: one run, seen by both.
    Attach,
}

impl Pair {
    pub fn label(&self) -> String {
        match &self.id {
            Some(id) => id.clone(),
            None => format!("{} / {}", self.source, self.tool),
        }
    }

    /// What is wrong with the pair as data, before anything runs.
    pub fn problems(&self, parsers: &[Parser]) -> Vec<String> {
        let mut out = Vec::new();
        if self.source.trim().is_empty() || self.tool.trim().is_empty() {
            out.push("`source` and `tool` are both required".into());
        }
        if self.gap.is_some() {
            return out;
        }
        if self.omn.is_empty() {
            out.push("`omn` is empty".into());
        }
        if self.reference.command.is_empty() {
            out.push("`reference.command` is empty".into());
        }
        if !parsers.iter().any(|p| p.name == self.reference.parse) {
            out.push(format!(
                "`reference.parse: {}` names no parser of this family",
                self.reference.parse
            ));
        }
        if self.runs == 0 {
            out.push("`runs` is 0".into());
        }
        let uses_pid = self
            .omn
            .iter()
            .chain(&self.reference.command)
            .any(|a| a.contains("{pid}"));
        if uses_pid && self.run != RunMode::Attach {
            out.push("`{pid}` only means something with `run: attach`".into());
        }
        if self.run == RunMode::Attach && self.workload.is_empty() {
            out.push("`run: attach` needs a workload to launch".into());
        }
        let compares = self.compare.all();
        if compares.is_empty() {
            out.push("`compare` is empty".into());
        }
        for c in compares {
            if c.tolerance.rel.is_none() && c.tolerance.abs.is_none() {
                out.push(format!(
                    "compare `{}`: no tolerance (`rel` or `abs`)",
                    c.omn
                ));
            }
            if c.why.trim().is_empty() {
                out.push(format!("compare `{}`: `why` is empty", c.omn));
            }
            if c.key == "*" && !c.omn.contains("{key}") {
                out.push(format!(
                    "compare `{}`: key `*` needs `{{key}}` in the path",
                    c.omn
                ));
            }
            if let Err(e) = parse_path(&c.omn.replace("{key}", "k")) {
                out.push(format!("compare `{}`: {e}", c.omn));
            }
        }
        for arg in &self.workload {
            if arg.starts_with("tests/") && !root().join(arg).exists() {
                out.push(format!("workload file `{arg}` does not exist"));
            }
        }
        out
    }
}

/// What one pair came to.
#[derive(Debug)]
pub enum Outcome {
    /// Each line one compared value, within its tolerance.
    Pass(Vec<String>),
    /// The machine's reason, and the lines quoted from whoever refused.
    Skip(String, Vec<String>),
    /// What `omn` lacks for this use; declared in the pair.
    Gap(String),
    Fail(String),
}

/// Reads a family's pairs.
pub fn load(family: &str) -> Result<Vec<Pair>, String> {
    let path = root().join(format!("catalog/references/{family}.yaml"));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_norway::from_str::<Option<Vec<Pair>>>(&text)
        .map(Option::unwrap_or_default)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Runs every pair of a family (or those whose label contains
/// `$OMN_REFERENCE_PAIR`), prints one line per outcome, and fails naming
/// every pair that did not hold.
pub fn run_family(family: &str, parsers: &[Parser]) {
    let pairs = load(family).unwrap_or_else(|e| panic!("{e}"));
    let only = std::env::var("OMN_REFERENCE_PAIR").ok();
    let omn = PathBuf::from(env!("CARGO_BIN_EXE_omn"));
    let mut failures = Vec::new();
    for pair in &pairs {
        let label = pair.label();
        if only.as_deref().is_some_and(|o| !label.contains(o)) {
            continue;
        }
        let problems = pair.problems(parsers);
        if !problems.is_empty() {
            failures.push(format!("{label}: {}", problems.join("; ")));
            continue;
        }
        match run_pair(pair, parsers, &omn) {
            Outcome::Pass(lines) => {
                for line in lines {
                    eprintln!("reference {family} {label}: {line}");
                }
            }
            Outcome::Skip(reason, quoted) => {
                eprintln!("skipped: reference {family} {label}: {reason}");
                for line in quoted.iter().take(8) {
                    eprintln!("    {line}");
                }
            }
            Outcome::Gap(what) => eprintln!("gap: reference {family} {label}: {what}"),
            Outcome::Fail(why) => failures.push(format!("{label} ({}): {why}", pair.use_)),
        }
    }
    assert!(
        failures.is_empty(),
        "{} reference pair(s) of `{family}` did not hold:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

/// What one side gave in one run.
struct Side {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

/// One run's two sides.
struct RunResult {
    omn: Side,
    omn_results: Option<String>,
    tool: Side,
}

pub fn run_pair(pair: &Pair, parsers: &[Parser], omn: &Path) -> Outcome {
    if let Some(gap) = &pair.gap {
        return Outcome::Gap(gap.clone());
    }
    for binary in &pair.reference.requires {
        if !installed(binary) {
            return Outcome::Skip(format!("`{binary}` is not installed here"), vec![]);
        }
    }
    if pair.reference.privileged && !sudo_grants() {
        return Outcome::Skip(
            "`sudo -n` does not grant root here, and this tool needs it".into(),
            vec![],
        );
    }
    let Some(parser) = parsers.iter().find(|p| p.name == pair.reference.parse) else {
        return Outcome::Fail(format!("no parser `{}`", pair.reference.parse));
    };

    let mut omn_docs = Vec::new();
    let mut tool_values: Vec<Values> = Vec::new();
    for i in 0..pair.runs {
        let tmp = match scratch(i) {
            Ok(t) => t,
            Err(e) => return Outcome::Fail(e),
        };
        let result = match pair.run {
            RunMode::Separate => run_separate(pair, omn, &tmp, i % 2 == 1),
            RunMode::Attach => run_attached(pair, omn, &tmp),
        };
        let result = match result {
            Ok(r) => r,
            Err(e) => return Outcome::Fail(e),
        };
        if let Some(outcome) = judge_omn(&result.omn) {
            return outcome;
        }
        if let Some(outcome) = judge_tool(pair, &result.tool) {
            return outcome;
        }
        let text = result.omn_results.unwrap_or(result.omn.stdout);
        match parse_json(&text) {
            Ok(doc) => omn_docs.push(doc),
            Err(e) => return Outcome::Fail(format!("omn's output: {e}")),
        }
        match (parser.parse)(&result.tool.stdout, &result.tool.stderr) {
            Ok(values) => tool_values.push(values),
            Err(e) => {
                let mut why = format!("parser `{}`: {e}", parser.name);
                if !result.tool.status.success() {
                    why.push_str(&format!(
                        " (the tool exited {}: {})",
                        result.tool.status,
                        tail(&result.tool.stderr, 3).join(" | ")
                    ));
                }
                return Outcome::Fail(why);
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
    compare(pair, &omn_docs, &tool_values)
}

fn compare(pair: &Pair, omn_docs: &[Json], tool_values: &[Values]) -> Outcome {
    let mut lines = Vec::new();
    let mut failed = Vec::new();
    for c in pair.compare.all() {
        let keys: Vec<String> = if c.key == "*" {
            tool_values[0].iter().map(|(k, _)| k.clone()).collect()
        } else {
            vec![c.key.clone()]
        };
        if keys.is_empty() {
            failed.push(format!("`{}`: the tool gave no keys", c.omn));
        }
        for key in keys {
            let path = c.omn.replace("{key}", &key);
            let mut ours = Vec::new();
            for doc in omn_docs {
                match eval_path(doc, &path) {
                    Ok(v) => ours.push(v * c.omn_scale),
                    Err(e) => {
                        failed.push(format!("omn `{path}`: {e}"));
                        break;
                    }
                }
            }
            let theirs: Vec<f64> = tool_values
                .iter()
                .filter_map(|vs| crate::common::value(vs, &key))
                .map(|v| v * c.reference_scale)
                .collect();
            if theirs.len() != tool_values.len() {
                failed.push(format!(
                    "the tool gave no value under key `{key}` (it gave {})",
                    keys_of(&tool_values[0])
                ));
                continue;
            }
            if ours.len() != omn_docs.len() {
                continue;
            }
            let (o, t) = (median(&mut ours), median(&mut theirs.clone()));
            let allowed = c.tolerance.allowed(t);
            let diff = (o - t).abs();
            let shown = if key.is_empty() {
                path.clone()
            } else {
                format!("{key} ({path})")
            };
            let line = format!(
                "{shown}: omn {o} vs {} {t}, off by {diff:.4} where {allowed:.4} is allowed",
                pair.tool
            );
            if diff <= allowed {
                lines.push(format!("same numbers: {line}"));
            } else {
                failed.push(format!("{line} -- {}", c.why.trim()));
            }
        }
    }
    if failed.is_empty() {
        Outcome::Pass(lines)
    } else {
        Outcome::Fail(failed.join("; "))
    }
}

fn keys_of(values: &Values) -> String {
    let keys: Vec<String> = values
        .iter()
        .take(6)
        .map(|(k, _)| format!("`{k}`"))
        .collect();
    if keys.is_empty() {
        "none".into()
    } else {
        keys.join(", ")
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// `omn`'s exit status read as the manual words it: 1 is this machine, 2 a
/// request that cannot mean anything, 3 nothing in this build collects it.
fn judge_omn(side: &Side) -> Option<Outcome> {
    if side.timed_out {
        return Some(Outcome::Fail("omn did not finish in time".into()));
    }
    match side.status.code() {
        Some(0) => None,
        Some(1) => Some(Outcome::Skip(
            "omn cannot run this here".into(),
            tail(&side.stderr, 8),
        )),
        Some(2) => Some(Outcome::Fail(format!(
            "omn refused the request: {}",
            tail(&side.stderr, 3).join(" | ")
        ))),
        Some(3) => Some(Outcome::Fail(
            "nothing in this build collects it: declare the pair's `gap:` instead".into(),
        )),
        _ => Some(Outcome::Fail(format!(
            "omn ended with {}: {}",
            side.status,
            tail(&side.stderr, 3).join(" | ")
        ))),
    }
}

/// Wordings of a tool refused by the machine rather than broken.
const REFUSALS: &[&str] = &[
    "perf_event_paranoid",
    "Permission denied",
    "Operation not permitted",
    "must be run as root",
    "requires root",
];

fn judge_tool(pair: &Pair, side: &Side) -> Option<Outcome> {
    if side.timed_out {
        return Some(Outcome::Fail(format!(
            "`{}` did not finish in time",
            pair.tool
        )));
    }
    let refused: Vec<String> = side
        .stderr
        .lines()
        .filter(|l| {
            REFUSALS
                .iter()
                .map(|s| s.to_string())
                .chain(pair.reference.refused_when.iter().cloned())
                .any(|s| l.contains(&s))
        })
        .map(|l| l.trim().to_string())
        .take(8)
        .collect();
    if !side.status.success() && !refused.is_empty() {
        return Some(Outcome::Skip(
            format!("`{}` was refused on this machine", pair.tool),
            refused,
        ));
    }
    None
}

fn tail(text: &str, n: usize) -> Vec<String> {
    let lines: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

fn sudo_grants() -> bool {
    static GRANTS: OnceLock<bool> = OnceLock::new();
    *GRANTS.get_or_init(|| {
        Command::new("sudo")
            .args(["-n", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

fn scratch(run: usize) -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "omn-reference-{}-{}-{run}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// `{tmp}` the run's scratch directory, `{out}` the directory `omn -o`
/// writes to, `{pid}` the workload under `run: attach`.
fn substitute(args: &[String], tmp: &Path, pid: Option<u32>) -> Vec<String> {
    args.iter()
        .map(|a| {
            let mut a = a
                .replace("{tmp}", &tmp.display().to_string())
                .replace("{out}", &tmp.join("omn").display().to_string());
            if let Some(pid) = pid {
                a = a.replace("{pid}", &pid.to_string());
            }
            a
        })
        .collect()
}

fn omn_command(pair: &Pair, omn: &Path, tmp: &Path, pid: Option<u32>) -> Command {
    let mut cmd = Command::new(omn);
    cmd.args(substitute(&pair.omn, tmp, pid));
    if pid.is_none() && !pair.workload.is_empty() {
        cmd.arg("--").args(&pair.workload);
    }
    cmd
}

fn tool_command(
    pair: &Pair,
    argv: &[String],
    tmp: &Path,
    pid: Option<u32>,
    workload: bool,
) -> Command {
    let argv = substitute(argv, tmp, pid);
    let mut cmd = if pair.reference.privileged {
        let mut c = Command::new("sudo");
        c.arg("-n").args(&argv);
        c
    } else {
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    };
    if workload {
        cmd.args(&pair.workload);
    }
    cmd
}

fn omn_results(pair: &Pair, tmp: &Path) -> Option<String> {
    pair.omn
        .iter()
        .any(|a| a.contains("{out}"))
        .then(|| std::fs::read_to_string(tmp.join("omn/results.json")).unwrap_or_default())
}

fn timeout(pair: &Pair) -> Duration {
    Duration::from_secs(pair.timeout_s.unwrap_or(120))
}

fn run_separate(
    pair: &Pair,
    omn: &Path,
    tmp: &Path,
    tool_first: bool,
) -> Result<RunResult, String> {
    let limit = timeout(pair);
    let run_omn = || {
        let mut cmd = omn_command(pair, omn, tmp, None);
        finish(spawn(&mut cmd)?, limit)
    };
    let run_tool = || -> Result<Side, String> {
        let mut cmd = tool_command(pair, &pair.reference.command, tmp, None, true);
        let side = finish(spawn(&mut cmd)?, limit)?;
        report(pair, tmp, side, limit)
    };
    let (omn_side, tool_side) = if tool_first {
        let t = run_tool()?;
        (run_omn()?, t)
    } else {
        let o = run_omn()?;
        (o, run_tool()?)
    };
    Ok(RunResult {
        omn_results: omn_results(pair, tmp),
        omn: omn_side,
        tool: tool_side,
    })
}

fn run_attached(pair: &Pair, omn: &Path, tmp: &Path) -> Result<RunResult, String> {
    let limit = timeout(pair);
    // `std` waits for its child to exec, so the stop comes from a shell that
    // stops itself and then becomes the workload under the same pid: neither
    // observer misses its start.
    let mut workload = Command::new("sh");
    workload
        .args(["-c", "kill -STOP $$; exec \"$@\"", "sh"])
        .args(&pair.workload)
        .current_dir(root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = workload
        .spawn()
        .map_err(|e| format!("`{}`: {e}", pair.workload[0]))?;
    let pid = child.id();
    let mut status = 0;
    unsafe { libc::waitpid(pid as i32, &mut status, libc::WUNTRACED) };
    let observers = (|| -> Result<(Child, Child), String> {
        let o = spawn(&mut omn_command(pair, omn, tmp, Some(pid)))?;
        let t = spawn(&mut tool_command(
            pair,
            &pair.reference.command,
            tmp,
            Some(pid),
            false,
        ))?;
        Ok((o, t))
    })();
    let (o, t) = match observers {
        Ok(pair) => pair,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    std::thread::sleep(Duration::from_millis(pair.settle_ms.unwrap_or(500)));
    unsafe { libc::kill(pid as i32, libc::SIGCONT) };
    let started = Instant::now();
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let omn_side = finish(o, limit)?;
    let tool_side = finish(t, limit)?;
    let tool_side = report(pair, tmp, tool_side, limit)?;
    Ok(RunResult {
        omn_results: omn_results(pair, tmp),
        omn: omn_side,
        tool: tool_side,
    })
}

/// `reference.report`, when the pair has one, replaces the tool's output.
fn report(pair: &Pair, tmp: &Path, side: Side, limit: Duration) -> Result<Side, String> {
    if pair.reference.report.is_empty() || side.timed_out || !side.status.success() {
        return Ok(side);
    }
    let mut cmd = tool_command(pair, &pair.reference.report, tmp, None, false);
    finish(spawn(&mut cmd)?, limit)
}

fn spawn(cmd: &mut Command) -> Result<Child, String> {
    cmd.current_dir(root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("`{:?}`: {e}", cmd.get_program()))
}

fn finish(mut child: Child, limit: Duration) -> Result<Side, String> {
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if started.elapsed() > limit {
            timed_out = true;
            let _ = child.kill();
            break child.wait().map_err(|e| e.to_string())?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(Side {
        status,
        stdout: reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
        timed_out,
    })
}

/// One JSON document, or one per line (`ndjson`) read as an array.
pub fn parse_json(text: &str) -> Result<Json, String> {
    if let Ok(doc) = serde_json::from_str(text) {
        return Ok(doc);
    }
    let lines: Result<Vec<Json>, _> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect();
    match lines {
        Ok(lines) if !lines.is_empty() => Ok(Json::Array(lines)),
        _ => {
            let head: String = text.chars().take(160).collect();
            Err(format!(
                "not JSON (a workload printing to omn's stdout? use `-o {{out}}`): {head:?}"
            ))
        }
    }
}

#[derive(Debug, PartialEq)]
enum Segment {
    Field(String),
    Index(usize),
    Every,
    Where(String, String),
}

fn parse_path(path: &str) -> Result<Vec<Segment>, String> {
    let mut out = Vec::new();
    let mut name = String::new();
    let mut chars = path.chars();
    let flush = |name: &mut String, out: &mut Vec<Segment>| {
        if name.is_empty() {
            return;
        }
        let seg = if name == "*" {
            Segment::Every
        } else if let Ok(i) = name.parse() {
            Segment::Index(i)
        } else {
            Segment::Field(name.clone())
        };
        out.push(seg);
        name.clear();
    };
    while let Some(c) = chars.next() {
        match c {
            '.' => flush(&mut name, &mut out),
            '[' => {
                flush(&mut name, &mut out);
                let inner: String = chars.by_ref().take_while(|&c| c != ']').collect();
                let (field, value) = inner
                    .split_once('=')
                    .ok_or_else(|| format!("`[{inner}]` is not `[field=value]`"))?;
                out.push(Segment::Where(field.trim().into(), value.trim().into()));
            }
            c => name.push(c),
        }
    }
    flush(&mut name, &mut out);
    if out.is_empty() {
        return Err("the path is empty".into());
    }
    Ok(out)
}

/// The number at `path` in `doc`; where a `*` or a `[field=value]` matches
/// several, their sum.
pub fn eval_path(doc: &Json, path: &str) -> Result<f64, String> {
    let segments = parse_path(path)?;
    let mut here: Vec<&Json> = vec![doc];
    for seg in &segments {
        let mut next = Vec::new();
        for node in here {
            match (seg, node) {
                (Segment::Field(f), Json::Object(m)) => next.extend(m.get(f)),
                (Segment::Index(i), Json::Array(a)) => next.extend(a.get(*i)),
                (Segment::Index(i), Json::Object(m)) => next.extend(m.get(&i.to_string())),
                (Segment::Every, Json::Array(a)) => next.extend(a.iter()),
                (Segment::Every, Json::Object(m)) => next.extend(m.values()),
                (Segment::Where(f, v), Json::Array(a)) => {
                    next.extend(a.iter().filter(|e| match e.get(f) {
                        Some(Json::String(s)) => s == v,
                        Some(Json::Number(n)) => v.parse::<f64>().ok() == n.as_f64(),
                        Some(Json::Bool(b)) => v == if *b { "true" } else { "false" },
                        _ => false,
                    }))
                }
                _ => {}
            }
        }
        if next.is_empty() {
            return Err(format!("nothing at {seg:?}"));
        }
        here = next;
    }
    let mut sum = 0.0;
    for node in here {
        sum += node
            .as_f64()
            .ok_or_else(|| format!("`{}` is not a number", short(node)))?;
    }
    Ok(sum)
}

fn short(node: &Json) -> String {
    let s = node.to_string();
    if s.len() > 80 {
        format!("{}...", &s[..80])
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdout_scalar(stdout: &str, _: &str) -> Result<Values, String> {
        Ok(vec![(String::new(), crate::common::number(stdout)?)])
    }

    fn stdout_keyed(stdout: &str, _: &str) -> Result<Values, String> {
        stdout
            .lines()
            .map(|l| {
                let (k, v) = l.split_once(' ').ok_or("no key")?;
                Ok((k.to_string(), crate::common::number(v)?))
            })
            .collect()
    }

    fn parsers() -> Vec<Parser> {
        vec![
            Parser {
                name: "scalar",
                parse: stdout_scalar,
            },
            Parser {
                name: "keyed",
                parse: stdout_keyed,
            },
        ]
    }

    /// `sh` stands in for `omn`: its argv is the pair's `omn` field, then
    /// `--` and the workload, which `sh -c` takes as ignored positionals.
    fn pair(yaml: &str) -> Pair {
        serde_norway::from_str(yaml).unwrap()
    }

    const SH: &str = "/bin/sh";

    #[test]
    fn a_path_reads_fields_indices_filters_and_sums() {
        let doc: Json = serde_json::from_str(
            r#"{"results":[{"source":"cpu.instructions","outputs":[{"emit":"summary",
                "series":[{"key":"task:1","mean":2.0},{"key":"task:2","mean":3.5}]}]}]}"#,
        )
        .unwrap();
        let base = "results[source=cpu.instructions].outputs[emit=summary].series";
        assert_eq!(eval_path(&doc, &format!("{base}.*.mean")).unwrap(), 5.5);
        assert_eq!(eval_path(&doc, &format!("{base}.1.mean")).unwrap(), 3.5);
        assert_eq!(
            eval_path(&doc, &format!("{base}[key=task:1].mean")).unwrap(),
            2.0
        );
        assert!(eval_path(&doc, &format!("{base}[key=task:9].mean")).is_err());
        assert!(eval_path(&doc, "results.0.source").is_err());
        assert!(parse_path("a[b]").is_err());
    }

    #[test]
    fn ndjson_reads_as_an_array() {
        let doc = parse_json("{\"a\":1}\n{\"a\":2}\n").unwrap();
        assert_eq!(eval_path(&doc, "*.a").unwrap(), 3.0);
        assert!(parse_json("hello\n{\"a\":1}").is_err());
    }

    #[test]
    fn a_pair_within_tolerance_passes_and_one_outside_fails() {
        let mut p = pair(
            r#"
source: test.value
tool: sh
use: "sh"
workload: ["true"]
omn: ["-c", "echo '{\"a\":[{\"v\":2},{\"v\":3}]}'", "sh"]
runs: 3
reference:
  command: [sh, -c, "echo 5.04", sh]
  parse: scalar
compare:
  omn: a.*.v
  tolerance: {rel: 0.01}
  why: "test"
"#,
        );
        assert!(
            p.problems(&parsers()).is_empty(),
            "{:?}",
            p.problems(&parsers())
        );
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Pass(lines) => assert!(lines[0].contains("same numbers"), "{lines:?}"),
            other => panic!("{other:?}"),
        }
        p.reference.command[2] = "echo 6".into();
        assert!(matches!(
            run_pair(&p, &parsers(), Path::new(SH)),
            Outcome::Fail(_)
        ));
    }

    #[test]
    fn every_key_the_tool_gave_is_compared() {
        let p = pair(
            r#"
source: test.hist
tool: sh
use: "sh"
omn: ["-c", "echo '{\"b\":{\"1\":10,\"2\":20}}'", "sh"]
reference:
  command: [sh, -c, "printf '1 10\\n2 21\\n'", sh]
  parse: keyed
compare:
  key: "*"
  omn: "b.{key}"
  tolerance: {abs: 1}
  why: "test"
"#,
        );
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Pass(lines) => assert_eq!(lines.len(), 2, "{lines:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn both_observers_see_the_same_attached_pid() {
        let p = pair(
            r#"
source: test.pid
tool: sh
use: "sh"
run: attach
settle_ms: 50
workload: [sleep, "0.2"]
omn: ["-c", "echo '{\"pid\":{pid}}'", "sh"]
reference:
  command: [sh, -c, "echo {pid}", sh]
  parse: scalar
compare:
  omn: pid
  tolerance: {abs: 0}
  why: "test"
"#,
        );
        assert!(p.problems(&parsers()).is_empty());
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Pass(_) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_machine_skips_and_the_data_fails() {
        let base = r#"
source: test.skip
tool: sh
use: "sh"
omn: ["-c", "echo '{\"a\":1}'", "sh"]
reference:
  command: [sh, -c, "echo 1", sh]
  parse: scalar
compare: {omn: a, tolerance: {abs: 0}, why: "test"}
"#;
        let mut p = pair(base);
        p.reference.requires = vec!["no-such-tool-here".into()];
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Skip(reason, _) => assert!(reason.contains("is not installed here")),
            other => panic!("{other:?}"),
        }

        let mut p = pair(base);
        p.omn = vec![
            "-c".into(),
            "echo 'has no viable backend' >&2; exit 1".into(),
            "sh".into(),
        ];
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Skip(_, quoted) => assert_eq!(quoted, ["has no viable backend"]),
            other => panic!("{other:?}"),
        }

        let mut p = pair(base);
        p.reference.command[2] = "echo 'perf_event_paranoid setting is 4' >&2; exit 255".into();
        assert!(matches!(
            run_pair(&p, &parsers(), Path::new(SH)),
            Outcome::Skip(..)
        ));

        let mut p = pair(base);
        p.omn[1] = "exit 3".into();
        assert!(matches!(
            run_pair(&p, &parsers(), Path::new(SH)),
            Outcome::Fail(_)
        ));

        let mut p = pair(base);
        p.reference.parse = "nope".into();
        assert!(!p.problems(&parsers()).is_empty());

        let mut p = pair(base);
        p.gap = Some("omn has no such source".into());
        assert!(matches!(
            run_pair(&p, &parsers(), Path::new(SH)),
            Outcome::Gap(_)
        ));
    }
}
