//! The runner: a pair from `catalog/references/<family>.yaml` read, `omn`
//! and the tool run on the same workload, both outputs parsed, and each
//! compared value held to the pair's tolerance. The schema is documented in
//! `catalog/references/README.md`; this file is its only reader, and a field
//! it does not read is refused rather than ignored.

use crate::common::{installed, root, Parser, Values};
use serde::Deserialize;
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Fields a pair, a reference or a compare carries that the runner does not
/// read: kept only to be refused by name (`Pair::problems`).
type Unknown = BTreeMap<String, serde_norway::Value>;

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
    /// `attach` and `alongside`: how long both observers get to start before
    /// the workload is let go.
    #[serde(default)]
    pub settle_ms: Option<u64>,
    /// Per command; 120 s when absent.
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// An argv run before each side (before the one run, in the modes with
    /// one): it must exit 0, or the pair fails.
    #[serde(default)]
    pub setup: Argv,
    /// Written, but `omn` cannot serve it yet: what it lacks. Not run.
    #[serde(default)]
    pub gap: Option<String>,
    /// `omn` is known to be wrong here: the finding. The pair runs; outside
    /// its tolerance it passes saying so, inside it fails, because the
    /// marker would then outlive the defect.
    #[serde(default)]
    pub known_defect: Option<String>,
    #[serde(flatten)]
    pub unknown: Unknown,
}

/// An argv, or what stood where one should: kept so that a pair file
/// written before the field existed still loads, and its pair is refused.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Argv {
    List(Vec<String>),
    Invalid(serde_norway::Value),
}

impl Default for Argv {
    fn default() -> Self {
        Argv::List(Vec::new())
    }
}

impl Argv {
    pub fn argv(&self) -> &[String] {
        match self {
            Argv::List(v) => v,
            Argv::Invalid(_) => &[],
        }
    }
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
    /// Files whose integer value must meet a bound, or the pair skips
    /// naming the file, its value and the bound.
    #[serde(default)]
    pub requires_setting: Vec<Setting>,
    #[serde(default)]
    pub privileged: bool,
    /// Text in the tool's stderr that means this machine refused it: the run
    /// then skips quoting it rather than failing.
    #[serde(default)]
    pub refused_when: Vec<String>,
    #[serde(flatten)]
    pub unknown: Unknown,
}

/// A machine setting a pair needs: `/proc/sys/kernel/perf_event_paranoid`
/// at most 0, say.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    pub file: String,
    #[serde(default)]
    pub at_most: Option<i64>,
    #[serde(default)]
    pub at_least: Option<i64>,
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
    #[serde(default)]
    pub omn: Option<String>,
    /// Or: a parser of the family applied to `omn`'s output, whose value
    /// under the same key is compared. Exclusive with `omn`.
    #[serde(default)]
    pub omn_parse: Option<String>,
    pub tolerance: Tolerance,
    pub why: String,
    #[serde(default = "unit")]
    pub omn_scale: f64,
    #[serde(default = "unit")]
    pub reference_scale: f64,
    #[serde(flatten)]
    pub unknown: Unknown,
}

impl Compare {
    /// How the compare names `omn`'s side, for its lines and its problems.
    pub fn omn_label(&self) -> String {
        match (&self.omn, &self.omn_parse) {
            (Some(path), _) => path.clone(),
            (None, Some(parser)) => format!("omn_parse {parser}"),
            (None, None) => "(no omn side)".into(),
        }
    }
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
    /// One-sided: `omn` may be on this side of the tool by the band, and on
    /// the other side by nothing.
    #[serde(default)]
    pub side: Option<Direction>,
    /// Equal when the two values' log2 bucket indices are at most this far
    /// apart: a median read off a log2 histogram is only known to a bucket.
    #[serde(default)]
    pub log2_buckets: Option<u32>,
    #[serde(flatten)]
    pub unknown: Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// `omn` may exceed the tool, never fall short of it.
    Above,
    /// `omn` may fall short of the tool, never exceed it.
    Below,
}

impl Tolerance {
    /// The difference allowed around `reference`: the larger of the two.
    pub fn allowed(&self, reference: f64) -> f64 {
        let rel = self.rel.map(|r| r * reference.abs()).unwrap_or(0.0);
        rel.max(self.abs.unwrap_or(0.0))
    }

    /// Whether `omn` against `tool` is within the tolerance, and the line
    /// that says by how much.
    pub fn holds(&self, omn: f64, tool: f64) -> (bool, String) {
        if let Some(n) = self.log2_buckets {
            let (b, t) = (log2_bucket(omn), log2_bucket(tool));
            let ok = match (b, t) {
                (Some(b), Some(t)) => b.abs_diff(t) <= u64::from(n),
                (None, None) => true,
                _ => false,
            };
            let show = |x: Option<i64>| x.map_or("none (not above 0)".into(), |x| x.to_string());
            return (
                ok,
                format!("log2 buckets {} vs {}, {n} apart allowed", show(b), show(t)),
            );
        }
        let allowed = self.allowed(tool);
        let diff = omn - tool;
        let (ok, band) = match self.side {
            None => (diff.abs() <= allowed, format!("{allowed:.4} is allowed")),
            Some(Direction::Above) => (
                (0.0..=allowed).contains(&diff),
                format!("0 to +{allowed:.4} is allowed (omn above only)"),
            ),
            Some(Direction::Below) => (
                (-allowed..=0.0).contains(&diff),
                format!("-{allowed:.4} to 0 is allowed (omn below only)"),
            ),
        };
        (ok, format!("off by {diff:+.4} where {band}"))
    }
}

/// The index of the power-of-two bucket `v` falls in (`[2^k, 2^(k+1))` is
/// `k`), or `None` for a value not above 0, which no log2 bucket holds.
pub fn log2_bucket(v: f64) -> Option<i64> {
    (v > 0.0 && v.is_finite()).then(|| v.log2().floor() as i64)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// `omn` launches the workload, then the tool launches it: two runs.
    #[default]
    Separate,
    /// The runner launches the workload stopped, starts both observers with
    /// `{pid}` substituted, and lets it go: one run, seen by both.
    Attach,
    /// `attach` with no `{pid}`: both observers watch the machine, started
    /// before the workload, and end on their own duration.
    Alongside,
    /// `<command> omn <omn> -- <workload>`: one run, the tool over `omn`
    /// and the program, `omn` over the program.
    ToolWrapsOmn,
    /// `omn <omn> -- <command> <workload>`: one run, `omn` over the tool
    /// and the program, the tool's output written to files of its own.
    OmnLaunchesTool,
}

/// Every placeholder a pair's argv may hold.
const PLACEHOLDERS: &[&str] = &["{tmp}", "{out}", "{pid}"];

impl Pair {
    pub fn label(&self) -> String {
        match &self.id {
            Some(id) => id.clone(),
            None => format!("{} / {}", self.source, self.tool),
        }
    }

    fn uses_out(&self) -> bool {
        self.omn.iter().any(|a| a.contains("{out}"))
    }

    /// What is wrong with the pair as data, before anything runs. A field
    /// the runner does not read is one: a pair that says more than the
    /// runner does would claim a check nobody makes.
    pub fn problems(&self, parsers: &[Parser]) -> Vec<String> {
        let mut out = Vec::new();
        let unknown = |at: &str, fields: &Unknown, out: &mut Vec<String>| {
            for name in fields.keys() {
                out.push(format!(
                    "`{at}{name}` is not a field the runner reads (README, \"A pair\")"
                ));
            }
        };
        unknown("", &self.unknown, &mut out);
        unknown("reference.", &self.reference.unknown, &mut out);
        for c in self.compare.all() {
            unknown("compare.", &c.unknown, &mut out);
            unknown("compare.tolerance.", &c.tolerance.unknown, &mut out);
        }
        if self.source.trim().is_empty() || self.tool.trim().is_empty() {
            out.push("`source` and `tool` are both required".into());
        }
        if self.gap.is_some() {
            if self.known_defect.is_some() {
                out.push("`gap` and `known_defect` together: a gap does not run".into());
            }
            return out;
        }
        if self
            .known_defect
            .as_deref()
            .is_some_and(|d| d.trim().is_empty())
        {
            out.push("`known_defect` is empty: name the finding".into());
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
        for s in &self.reference.requires_setting {
            if s.file.trim().is_empty() {
                out.push("`requires_setting`: `file` is empty".into());
            }
            if s.at_most.is_none() && s.at_least.is_none() {
                out.push(format!(
                    "`requires_setting` `{}`: no bound (`at_most` or `at_least`)",
                    s.file
                ));
            }
        }
        let uses_pid = self
            .omn
            .iter()
            .chain(&self.reference.command)
            .chain(&self.reference.report)
            .chain(self.setup.argv())
            .any(|a| a.contains("{pid}"));
        if uses_pid && self.run != RunMode::Attach {
            out.push("`{pid}` only means something with `run: attach`".into());
        }
        if self.run == RunMode::Attach && self.workload.is_empty() {
            out.push("`run: attach` needs a workload to launch".into());
        }
        if self.run == RunMode::ToolWrapsOmn && !self.uses_out() {
            out.push(
                "`run: tool_wraps_omn` needs `-o {out}` in `omn`: the tool's output \
                 and omn's share one stdout"
                    .into(),
            );
        }
        if let Argv::Invalid(v) = &self.setup {
            out.push(format!(
                "`setup` is an argv run before each side, not `{}`",
                serde_norway::to_string(v).unwrap_or_default().trim()
            ));
        }
        if self
            .setup
            .argv()
            .first()
            .is_some_and(|a| a.trim().is_empty())
        {
            out.push("`setup` starts with an empty program".into());
        }
        for arg in &self.workload {
            if PLACEHOLDERS.iter().any(|p| arg.contains(p)) {
                out.push(format!(
                    "workload argument `{arg}`: placeholders are not substituted in a workload"
                ));
            }
        }
        let compares = self.compare.all();
        if compares.is_empty() {
            out.push("`compare` is empty".into());
        }
        for c in compares {
            let at = c.omn_label();
            let t = &c.tolerance;
            match (t.log2_buckets, t.rel, t.abs) {
                (None, None, None) => {
                    out.push(format!(
                        "compare `{at}`: no tolerance (`rel`, `abs` or `log2_buckets`)"
                    ));
                }
                (Some(_), rel, abs) if rel.is_some() || abs.is_some() || t.side.is_some() => {
                    out.push(format!(
                        "compare `{at}`: `log2_buckets` stands alone, without `rel`, `abs` or `side`"
                    ));
                }
                _ => {}
            }
            if c.why.trim().is_empty() {
                out.push(format!("compare `{at}`: `why` is empty"));
            }
            match (&c.omn, &c.omn_parse) {
                (Some(_), Some(_)) => out.push(format!(
                    "compare `{at}`: `omn` and `omn_parse` are exclusive"
                )),
                (None, None) => out.push(
                    "a compare names no omn side: `omn` (a path) or `omn_parse` (a parser)".into(),
                ),
                (Some(path), None) => {
                    if c.key == "*" && !path.contains("{key}") {
                        out.push(format!(
                            "compare `{path}`: key `*` needs `{{key}}` in the path"
                        ));
                    }
                    if let Err(e) = parse_path(&path.replace("{key}", "k")) {
                        out.push(format!("compare `{path}`: {e}"));
                    }
                }
                (None, Some(name)) => {
                    if !parsers.iter().any(|p| p.name == name) {
                        out.push(format!(
                            "compare `omn_parse: {name}` names no parser of this family"
                        ));
                    }
                }
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
    /// The finding the pair names, and the lines that show it still holds.
    KnownDefect(String, Vec<String>),
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

/// How a family's pairs came out, counted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    pub held: usize,
    pub skipped: usize,
    pub gaps: usize,
    pub known_defects: usize,
    pub failed: usize,
}

impl Tally {
    pub fn line(&self, family: &str) -> String {
        format!(
            "tally: reference {family}: {} held, {} skipped, {} gaps, {} known defects, {} failed",
            self.held, self.skipped, self.gaps, self.known_defects, self.failed
        )
    }
}

/// Where `pairs_<family>` leaves its tally line for `reference_tally`.
pub fn tally_file(family: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("reference-tally")
        .join(format!("{family}.txt"))
}

/// Runs every pair of a family (or those whose label contains
/// `$OMN_REFERENCE_PAIR`), prints one line per outcome and the family's
/// tally, and fails naming every pair that did not hold.
pub fn run_family(family: &str, parsers: &[Parser]) {
    let pairs = load(family).unwrap_or_else(|e| panic!("{e}"));
    let only = std::env::var("OMN_REFERENCE_PAIR").ok();
    let omn = PathBuf::from(env!("CARGO_BIN_EXE_omn"));
    let mut failures = Vec::new();
    let mut tally = Tally::default();
    for pair in &pairs {
        let label = pair.label();
        if only.as_deref().is_some_and(|o| !label.contains(o)) {
            continue;
        }
        let problems = pair.problems(parsers);
        if !problems.is_empty() {
            tally.failed += 1;
            failures.push(format!("{label}: {}", problems.join("; ")));
            continue;
        }
        match run_pair(pair, parsers, &omn) {
            Outcome::Pass(lines) => {
                tally.held += 1;
                for line in lines {
                    eprintln!("reference {family} {label}: {line}");
                }
            }
            Outcome::Skip(reason, quoted) => {
                tally.skipped += 1;
                eprintln!("skipped: reference {family} {label}: {reason}");
                for line in quoted.iter().take(8) {
                    eprintln!("    {line}");
                }
            }
            Outcome::Gap(what) => {
                tally.gaps += 1;
                eprintln!("gap: reference {family} {label}: {what}");
            }
            Outcome::KnownDefect(finding, lines) => {
                tally.known_defects += 1;
                eprintln!("known defect: reference {family} {label}: {finding}");
                for line in lines {
                    eprintln!("    {line}");
                }
            }
            Outcome::Fail(why) => {
                tally.failed += 1;
                failures.push(format!("{label} ({}): {why}", pair.use_));
            }
        }
    }
    let line = tally.line(family);
    eprintln!("{line}");
    if only.is_none() {
        let file = tally_file(family);
        let _ = std::fs::create_dir_all(file.parent().unwrap());
        let _ = std::fs::write(&file, format!("{line}\n"));
    }
    assert!(
        failures.is_empty(),
        "{} reference pair(s) of `{family}` did not hold:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

/// What one side gave in one run.
#[derive(Debug, Clone)]
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
    /// `run: attach`: how the workload ended when it ended on its own, `None`
    /// when the runner had to kill it or `omn` launched it.
    workload: Option<ExitStatus>,
}

/// `omn`'s output in one run: the text (stdout, or `{out}/results.json`)
/// and its stderr, for an `omn_parse` parser; the JSON, for a path.
struct OmnOutput {
    text: String,
    stderr: String,
    doc: Option<Json>,
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
    for setting in &pair.reference.requires_setting {
        if let Some(outcome) = check_setting(setting) {
            return outcome;
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
    let needs_json = pair.compare.all().iter().any(|c| c.omn.is_some());

    let mut omn_outputs = Vec::new();
    let mut tool_values: Vec<Values> = Vec::new();
    for i in 0..pair.runs {
        let tmp = match scratch(i) {
            Ok(t) => t,
            Err(e) => return Outcome::Fail(e),
        };
        let result = match pair.run {
            RunMode::Separate => run_separate(pair, omn, &tmp, i % 2 == 1),
            RunMode::Attach | RunMode::Alongside => run_attached(pair, omn, &tmp),
            RunMode::ToolWrapsOmn => run_wrapped(pair, omn, &tmp),
            RunMode::OmnLaunchesTool => run_launched(pair, omn, &tmp),
        };
        let result = match result {
            Ok(r) => r,
            Err(e) => return Outcome::Fail(e),
        };
        if let Some(status) = result.workload.filter(|s| !s.success()) {
            return Outcome::Fail(format!(
                "the workload exited {status} on its own, under both observers: \
                 neither side measured what the pair meant"
            ));
        }
        // Wrapped, the tool's refusal is also omn's exit: the tool speaks
        // first, since it is the one that ran.
        if pair.run == RunMode::ToolWrapsOmn {
            if let Some(outcome) = judge_tool(pair, &result.tool) {
                return outcome;
            }
        }
        if let Some(outcome) = judge_omn(pair, &result.omn, result.omn_results.as_deref()) {
            return outcome;
        }
        if let Some(outcome) = judge_tool(pair, &result.tool) {
            return outcome;
        }
        let text = result.omn_results.unwrap_or(result.omn.stdout);
        let doc = if needs_json {
            match parse_json(&text) {
                Ok(doc) => Some(doc),
                Err(e) => return Outcome::Fail(format!("omn's output: {e}")),
            }
        } else {
            None
        };
        omn_outputs.push(OmnOutput {
            text,
            stderr: result.omn.stderr,
            doc,
        });
        match (parser.parse)(&result.tool.stdout, &result.tool.stderr) {
            Ok(values) => tool_values.push(values),
            Err(e) => {
                // Launched by omn, the tool's exit status is not seen: a
                // refusal is read from its stderr when nothing parses.
                if pair.run == RunMode::OmnLaunchesTool {
                    if let Some(outcome) = refusal(pair, &result.tool) {
                        return outcome;
                    }
                }
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
    compare(pair, parsers, &omn_outputs, &tool_values)
}

/// A setting the pair needs, read: `None` when it holds, a skip naming the
/// file, its value and the bound when it does not, a failure when the file
/// does not hold an integer.
fn check_setting(s: &Setting) -> Option<Outcome> {
    let text = match std::fs::read_to_string(&s.file) {
        Ok(t) => t,
        Err(e) => {
            return Some(Outcome::Skip(
                format!(
                    "`{}` cannot be read here ({e}), and this pair needs it",
                    s.file
                ),
                vec![],
            ))
        }
    };
    let value: i64 = match text.trim().parse() {
        Ok(v) => v,
        Err(_) => {
            return Some(Outcome::Fail(format!(
                "`{}` holds `{}`, not an integer",
                s.file,
                text.trim()
            )))
        }
    };
    let bound = match (s.at_most, s.at_least) {
        (Some(most), _) if value > most => format!("at most {most}"),
        (_, Some(least)) if value < least => format!("at least {least}"),
        _ => return None,
    };
    Some(Outcome::Skip(
        format!(
            "`{}` is {value} here, and this pair needs it {bound}",
            s.file
        ),
        vec![],
    ))
}

/// What the compares came to: lines within tolerance, values outside it,
/// and errors (a path that finds nothing, a key a side lacks) that no
/// `known_defect` excuses.
fn compare(pair: &Pair, parsers: &[Parser], omn: &[OmnOutput], tool_values: &[Values]) -> Outcome {
    let mut lines = Vec::new();
    let mut misses = Vec::new();
    let mut errors = Vec::new();
    for c in pair.compare.all() {
        let keys: Vec<String> = if c.key == "*" {
            tool_values[0].iter().map(|(k, _)| k.clone()).collect()
        } else {
            vec![c.key.clone()]
        };
        if keys.is_empty() {
            errors.push(format!("`{}`: the tool gave no keys", c.omn_label()));
        }
        // One parse of omn's output per run, shared by every key.
        let parsed: Vec<Result<Values, String>> = match &c.omn_parse {
            Some(name) => {
                let Some(p) = parsers.iter().find(|p| &p.name == name) else {
                    errors.push(format!("no parser `{name}`"));
                    continue;
                };
                omn.iter().map(|o| (p.parse)(&o.text, &o.stderr)).collect()
            }
            None => Vec::new(),
        };
        if let Some(Err(e)) = parsed.iter().find(|r| r.is_err()) {
            errors.push(format!("omn_parse `{}`: {e}", c.omn_label()));
            continue;
        }
        for key in keys {
            let (mut ours, shown) = match &c.omn {
                Some(path) => {
                    let path = path.replace("{key}", &key);
                    let mut ours = Vec::new();
                    for o in omn {
                        match eval_path(o.doc.as_ref().expect("parsed for a path"), &path) {
                            Ok(v) => ours.push(v * c.omn_scale),
                            Err(e) => {
                                errors.push(format!("omn `{path}`: {e}"));
                                break;
                            }
                        }
                    }
                    (ours, path)
                }
                None => {
                    let values: Vec<f64> = parsed
                        .iter()
                        .filter_map(|r| r.as_ref().ok())
                        .filter_map(|vs| crate::common::value(vs, &key))
                        .map(|v| v * c.omn_scale)
                        .collect();
                    if values.len() != omn.len() {
                        let gave = parsed
                            .first()
                            .and_then(|r| r.as_ref().ok())
                            .map(keys_of)
                            .unwrap_or_else(|| "none".into());
                        errors.push(format!(
                            "omn's output gave no value under key `{key}` (it gave {gave})"
                        ));
                        continue;
                    }
                    (values, c.omn_label())
                }
            };
            let theirs: Vec<f64> = tool_values
                .iter()
                .filter_map(|vs| crate::common::value(vs, &key))
                .map(|v| v * c.reference_scale)
                .collect();
            if theirs.len() != tool_values.len() {
                errors.push(format!(
                    "the tool gave no value under key `{key}` (it gave {})",
                    keys_of(&tool_values[0])
                ));
                continue;
            }
            if ours.len() != omn.len() {
                continue;
            }
            let (o, t) = (median(&mut ours), median(&mut theirs.clone()));
            let (ok, by) = c.tolerance.holds(o, t);
            let shown = if key.is_empty() {
                shown
            } else {
                format!("{key} ({shown})")
            };
            let line = format!("{shown}: omn {o} vs {} {t}, {by}", pair.tool);
            if ok {
                lines.push(format!("same numbers: {line}"));
            } else {
                misses.push(format!("{line} -- {}", c.why.trim()));
            }
        }
    }
    if !errors.is_empty() {
        errors.extend(misses);
        return Outcome::Fail(errors.join("; "));
    }
    match &pair.known_defect {
        Some(_) if misses.is_empty() => Outcome::Fail(format!(
            "the defect no longer reproduces: remove `known_defect` ({})",
            lines.join("; ")
        )),
        Some(finding) => {
            misses.extend(lines);
            Outcome::KnownDefect(finding.clone(), misses)
        }
        None if misses.is_empty() => Outcome::Pass(lines),
        None => Outcome::Fail(misses.join("; ")),
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

/// What `omn`'s exit says about the pair, when it says more than "go on":
/// 1 is this machine, 2 a request that cannot mean anything, 3 nothing in
/// this build collects it.
///
/// Exit 1 is two answers in `man omn.1`: this machine cannot serve the
/// request, or something failed while running. Only the first is a skip, so
/// the second is told apart by what it leaves behind -- results written
/// before the failure, or a workload that fails without any observer -- and
/// is a failure.
fn judge_omn(pair: &Pair, side: &Side, results: Option<&str>) -> Option<Outcome> {
    if side.timed_out {
        return Some(Outcome::Fail("omn did not finish in time".into()));
    }
    match side.status.code() {
        Some(0) => None,
        Some(1) if results.is_some_and(|r| !r.trim().is_empty()) => Some(Outcome::Fail(format!(
            "omn wrote its results and exited 1, so something failed while running: {}",
            tail(&side.stderr, 3).join(" | ")
        ))),
        Some(1) => Some(workload_fails_alone(pair).unwrap_or_else(|| {
            Outcome::Skip("omn cannot run this here".into(), tail(&side.stderr, 8))
        })),
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

/// The workload run once with no observer, asked before a skip is granted.
///
/// Under `run: separate` the workload's stderr is part of each observer's, so
/// a workload that fails with "Permission denied" reads exactly like a tool
/// the machine refused, and a workload that dies at once can turn into an
/// `omn` exit 1. A workload that fails alone is the pair's defect, never the
/// machine's: this is a failure naming it. Asked only on the way to a skip,
/// so a pair that runs costs nothing more.
fn workload_fails_alone(pair: &Pair) -> Option<Outcome> {
    if pair.workload.is_empty() {
        return None;
    }
    let mut cmd = Command::new(&pair.workload[0]);
    cmd.args(&pair.workload[1..]);
    let side = match spawn(&mut cmd).and_then(|child| finish(child, timeout(pair))) {
        Ok(side) => side,
        Err(e) => return Some(Outcome::Fail(format!("the workload does not start: {e}"))),
    };
    if side.timed_out || side.status.success() {
        return None;
    }
    Some(Outcome::Fail(format!(
        "the workload fails with no observer ({}): {}",
        side.status,
        tail(&side.stderr, 3).join(" | ")
    )))
}

/// Wordings of a tool refused by the machine rather than broken.
const REFUSALS: &[&str] = &[
    "perf_event_paranoid",
    "Permission denied",
    "Operation not permitted",
    "must be run as root",
    "requires root",
];

/// The tool's stderr lines that say the machine refused it.
fn refused_lines(pair: &Pair, side: &Side) -> Vec<String> {
    side.stderr
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
        .collect()
}

/// A skip quoting the refusal, unless the workload fails on its own.
fn refusal(pair: &Pair, side: &Side) -> Option<Outcome> {
    let refused = refused_lines(pair, side);
    if refused.is_empty() {
        return None;
    }
    Some(workload_fails_alone(pair).unwrap_or_else(|| {
        Outcome::Skip(
            format!("`{}` was refused on this machine", pair.tool),
            refused,
        )
    }))
}

fn judge_tool(pair: &Pair, side: &Side) -> Option<Outcome> {
    if side.timed_out {
        return Some(Outcome::Fail(format!(
            "`{}` did not finish in time",
            pair.tool
        )));
    }
    if side.status.success() {
        return None;
    }
    refusal(pair, side)
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

/// `omn <omn>`, then `-- <program>` when there is one to launch.
fn omn_command(
    pair: &Pair,
    omn: &Path,
    tmp: &Path,
    program: &[String],
    pid: Option<u32>,
) -> Command {
    let mut cmd = Command::new(omn);
    cmd.args(substitute(&pair.omn, tmp, pid));
    if !program.is_empty() {
        cmd.arg("--").args(program);
    }
    cmd
}

/// The tool's argv, through `sudo -n` when it is privileged.
fn tool_argv(pair: &Pair, argv: &[String], tmp: &Path, pid: Option<u32>) -> Vec<String> {
    let mut out = Vec::new();
    if pair.reference.privileged {
        out.extend(["sudo".to_string(), "-n".to_string()]);
    }
    out.extend(substitute(argv, tmp, pid));
    out
}

fn command_of(argv: &[String]) -> Command {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

fn tool_command(
    pair: &Pair,
    argv: &[String],
    tmp: &Path,
    pid: Option<u32>,
    workload: bool,
) -> Command {
    let mut cmd = command_of(&tool_argv(pair, argv, tmp, pid));
    if workload {
        cmd.args(&pair.workload);
    }
    cmd
}

fn omn_results(pair: &Pair, tmp: &Path) -> Option<String> {
    pair.uses_out()
        .then(|| std::fs::read_to_string(tmp.join("omn/results.json")).unwrap_or_default())
}

fn timeout(pair: &Pair) -> Duration {
    Duration::from_secs(pair.timeout_s.unwrap_or(120))
}

/// `setup`, when the pair has one: it must exit 0.
fn run_setup(pair: &Pair, tmp: &Path) -> Result<(), String> {
    let setup = pair.setup.argv();
    if setup.is_empty() {
        return Ok(());
    }
    let side = finish(
        spawn(&mut command_of(&substitute(setup, tmp, None)))?,
        timeout(pair),
    )?;
    if side.timed_out || !side.status.success() {
        return Err(format!(
            "`setup` ({}) {}: {}",
            setup.join(" "),
            if side.timed_out {
                "did not finish in time".to_string()
            } else {
                format!("exited {}", side.status)
            },
            tail(&side.stderr, 3).join(" | ")
        ));
    }
    Ok(())
}

fn run_separate(
    pair: &Pair,
    omn: &Path,
    tmp: &Path,
    tool_first: bool,
) -> Result<RunResult, String> {
    let limit = timeout(pair);
    let run_omn = || {
        run_setup(pair, tmp)?;
        let mut cmd = omn_command(pair, omn, tmp, &pair.workload, None);
        finish(spawn(&mut cmd)?, limit)
    };
    let run_tool = || -> Result<Side, String> {
        run_setup(pair, tmp)?;
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
        workload: None,
    })
}

/// `tool_wraps_omn`: `<command> omn <omn> -- <workload>`, one process tree.
/// Both sides are that one command; `omn`'s results are read from `{out}`.
fn run_wrapped(pair: &Pair, omn: &Path, tmp: &Path) -> Result<RunResult, String> {
    let limit = timeout(pair);
    run_setup(pair, tmp)?;
    let mut argv = tool_argv(pair, &pair.reference.command, tmp, None);
    argv.push(omn.display().to_string());
    argv.extend(substitute(&pair.omn, tmp, None));
    if !pair.workload.is_empty() {
        argv.push("--".into());
        argv.extend(pair.workload.iter().cloned());
    }
    let side = finish(spawn(&mut command_of(&argv))?, limit)?;
    let tool = report(pair, tmp, side.clone(), limit)?;
    Ok(RunResult {
        omn_results: omn_results(pair, tmp),
        omn: side,
        tool,
        workload: None,
    })
}

/// `omn_launches_tool`: `omn <omn> -- <command> <workload>`. The program
/// `omn` launches is a shell that becomes the tool (`exec`, so the pid `omn`
/// watches is the tool's), its stdout and stderr sent to files of the run;
/// those files are the tool's output. The tool's own exit status is not
/// seen: `omn` reports it, and a refusal is read from its stderr instead.
fn run_launched(pair: &Pair, omn: &Path, tmp: &Path) -> Result<RunResult, String> {
    let limit = timeout(pair);
    run_setup(pair, tmp)?;
    let (out, err) = (tmp.join("tool.stdout"), tmp.join("tool.stderr"));
    let mut program: Vec<String> = vec![
        "sh".into(),
        "-c".into(),
        "o=$1; e=$2; shift 2; exec \"$@\" >\"$o\" 2>\"$e\"".into(),
        "sh".into(),
        out.display().to_string(),
        err.display().to_string(),
    ];
    program.extend(tool_argv(pair, &pair.reference.command, tmp, None));
    program.extend(pair.workload.iter().cloned());
    let omn_side = finish(
        spawn(&mut omn_command(pair, omn, tmp, &program, None))?,
        limit,
    )?;
    let tool = Side {
        status: ExitStatus::from_raw(0),
        stdout: std::fs::read_to_string(&out).unwrap_or_default(),
        stderr: std::fs::read_to_string(&err).unwrap_or_default(),
        timed_out: false,
    };
    let tool = report(pair, tmp, tool, limit)?;
    Ok(RunResult {
        omn_results: omn_results(pair, tmp),
        omn: omn_side,
        tool,
        workload: None,
    })
}

/// `attach` and `alongside`: one run both observers see. The workload, when
/// there is one, is started stopped, both observers are started (with its
/// pid under `attach`), and after `settle_ms` it is let go.
fn run_attached(pair: &Pair, omn: &Path, tmp: &Path) -> Result<RunResult, String> {
    let limit = timeout(pair);
    run_setup(pair, tmp)?;
    // `std` waits for its child to exec, so the stop comes from a shell that
    // stops itself and then becomes the workload under the same pid: neither
    // observer misses its start.
    let mut child = if pair.workload.is_empty() {
        None
    } else {
        let mut workload = Command::new("sh");
        workload
            .args(["-c", "kill -STOP $$; exec \"$@\"", "sh"])
            .args(&pair.workload)
            .current_dir(root())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = workload
            .spawn()
            .map_err(|e| format!("`{}`: {e}", pair.workload[0]))?;
        let mut status = 0;
        unsafe { libc::waitpid(child.id() as i32, &mut status, libc::WUNTRACED) };
        Some(child)
    };
    let pid = match pair.run {
        RunMode::Attach => child.as_ref().map(Child::id),
        _ => None,
    };
    let observers = (|| -> Result<(Child, Child), String> {
        let o = spawn(&mut omn_command(pair, omn, tmp, &[], pid))?;
        let t = spawn(&mut tool_command(
            pair,
            &pair.reference.command,
            tmp,
            pid,
            false,
        ))?;
        Ok((o, t))
    })();
    let (o, t) = match observers {
        Ok(pair) => pair,
        Err(e) => {
            if let Some(child) = child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err(e);
        }
    };
    std::thread::sleep(Duration::from_millis(pair.settle_ms.unwrap_or(500)));
    let ended = match child.as_mut() {
        None => None,
        Some(child) => {
            unsafe { libc::kill(child.id() as i32, libc::SIGCONT) };
            let started = Instant::now();
            loop {
                if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                    break Some(status);
                }
                if started.elapsed() > limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    let omn_side = finish(o, limit)?;
    let tool_side = finish(t, limit)?;
    let tool_side = report(pair, tmp, tool_side, limit)?;
    Ok(RunResult {
        omn_results: omn_results(pair, tmp),
        omn: omn_side,
        tool: tool_side,
        workload: ended,
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

    /// Exit 1 and a "Permission denied" are the machine's only when nothing
    /// else explains them: a workload that fails alone, a workload that dies
    /// under attach, and results written before the failure are the pair's.
    #[test]
    fn a_failure_that_is_not_the_machines_is_not_a_skip() {
        let base = r#"
source: test.notskip
tool: sh
use: "sh"
workload: [sh, -c, "echo 'cat: /nope: Permission denied' >&2; exit 1"]
omn: ["-c", "echo 'has no viable backend' >&2; exit 1", "sh"]
reference:
  command: [sh, -c, "echo 1", sh]
  parse: scalar
compare: {omn: a, tolerance: {abs: 0}, why: "test"}
"#;
        let p = pair(base);
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Fail(why) => assert!(why.contains("no observer"), "{why}"),
            other => panic!("{other:?}"),
        }

        let mut p = pair(base);
        p.omn[1] = "echo '{\"a\":1}'".into();
        p.reference.command[2] = "\"$@\"; echo 'Permission denied' >&2; exit 1".into();
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Fail(why) => assert!(why.contains("no observer"), "{why}"),
            other => panic!("{other:?}"),
        }

        let mut p = pair(base);
        p.workload = vec!["true".into()];
        p.omn = vec![
            "-c".into(),
            "mkdir -p {out}; echo '{\"a\":1}' > {out}/results.json; echo 'error: the collector broke it' >&2; exit 1".into(),
            "sh".into(),
        ];
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Fail(why) => assert!(why.contains("wrote its results"), "{why}"),
            other => panic!("{other:?}"),
        }

        let mut p = pair(base);
        p.run = RunMode::Attach;
        p.settle_ms = Some(50);
        p.workload = vec!["sh".into(), "-c".into(), "exit 127".into()];
        p.omn = vec!["-c".into(), "exit 1".into(), "sh".into()];
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Fail(why) => assert!(why.contains("on its own"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    const BASE: &str = r#"
source: test.new
tool: sh
use: "sh"
omn: ["-c", "echo '{\"a\":5}'", "sh"]
reference:
  command: [sh, -c, "echo 5", sh]
  parse: scalar
compare: {omn: a, tolerance: {abs: 0}, why: "test"}
"#;

    fn expect_pass(p: &Pair) -> Vec<String> {
        assert!(
            p.problems(&parsers()).is_empty(),
            "{:?}",
            p.problems(&parsers())
        );
        match run_pair(p, &parsers(), Path::new(SH)) {
            Outcome::Pass(lines) => lines,
            other => panic!("{other:?}"),
        }
    }

    /// `omn_parse` reads `omn`'s output with a parser of the family, key by
    /// key, and is exclusive with the path.
    #[test]
    fn omn_parse_reads_omns_output_with_a_parser() {
        let mut p = pair(BASE);
        p.omn[1] = "echo 5.02".into();
        let c = match &mut p.compare {
            Compares::One(c) => c,
            Compares::Many(_) => unreachable!(),
        };
        c.omn = None;
        c.omn_parse = Some("scalar".into());
        c.tolerance = Tolerance {
            rel: Some(0.01),
            ..Default::default()
        };
        let lines = expect_pass(&p);
        assert!(lines[0].contains("omn_parse scalar"), "{lines:?}");

        let p = pair(
            r#"
source: test.keyed
tool: sh
use: "sh"
omn: ["-c", "printf 'x 1\\ny 2\\n'", "sh"]
reference:
  command: [sh, -c, "printf 'x 1\\ny 2\\n'", sh]
  parse: keyed
compare: {key: "*", omn_parse: keyed, tolerance: {abs: 0}, why: "test"}
"#,
        );
        assert_eq!(expect_pass(&p).len(), 2);

        let mut p = pair(BASE);
        if let Compares::One(c) = &mut p.compare {
            c.omn_parse = Some("nope".into());
        }
        let problems = p.problems(&parsers()).join("\n");
        assert!(problems.contains("exclusive"), "{problems}");
        if let Compares::One(c) = &mut p.compare {
            c.omn = None;
        }
        let problems = p.problems(&parsers()).join("\n");
        assert!(problems.contains("`omn_parse: nope`"), "{problems}");
    }

    /// A field the runner does not read is refused by name, wherever it is.
    #[test]
    fn a_field_the_runner_does_not_read_is_refused() {
        let p = pair(
            r#"
source: test.unknown
tool: sh
use: "sh"
same_run: alternate
omn: [a]
reference: {command: [sh], parse: scalar, notes: x}
compare: {omn: a, omn_jq: ".a", tolerance: {abs: 0, side: above, band: 2}, why: "w"}
"#,
        );
        let problems = p.problems(&parsers()).join("\n");
        let old: Pair =
            serde_norway::from_str(&BASE.replace("tool: sh", "tool: sh\nsetup: cgroup_spin"))
                .unwrap();
        let old = old.problems(&parsers()).join("\n");
        assert!(old.contains("not `cgroup_spin`"), "{old}");
        for field in [
            "`same_run`",
            "`reference.notes`",
            "`compare.omn_jq`",
            "`compare.tolerance.band`",
        ] {
            assert!(problems.contains(field), "{field} in {problems}");
        }
    }

    /// `tool_wraps_omn`: one command, the tool over `omn` over the workload;
    /// `omn`'s side read from `{out}`, which the mode requires.
    #[test]
    fn the_tool_wraps_omn_in_one_run() {
        let mut p = pair(BASE);
        p.run = RunMode::ToolWrapsOmn;
        p.workload = vec!["true".into()];
        p.omn[1] = "mkdir -p {out}; echo '{\"a\":5}' > {out}/results.json; \
                    [ \"$1\" = -- ] && [ \"$2\" = true ]"
            .into();
        // The tool runs what it wraps, then prints its number.
        p.reference.command = vec![
            "sh".into(),
            "-c".into(),
            "\"$@\" && echo 5".into(),
            "sh".into(),
        ];
        expect_pass(&p);

        p.omn[1] = "echo '{\"a\":5}'".into();
        let problems = p.problems(&parsers()).join("\n");
        assert!(problems.contains("needs `-o {out}`"), "{problems}");
    }

    /// `omn_launches_tool`: `omn -- <command> <workload>`, the tool's output
    /// in files of its own; a refusal on its stderr skips.
    #[test]
    fn omn_launches_the_tool_in_one_run() {
        let mut p = pair(BASE);
        p.run = RunMode::OmnLaunchesTool;
        p.workload = vec!["ignored".into()];
        // The stub omn runs its program, whose stdout must not reach omn's.
        p.omn[1] = "shift; \"$@\"; echo '{\"a\":5}'".into();
        p.reference.command[2] = "echo 5; echo noise >&2".into();
        expect_pass(&p);

        p.reference.command[2] = "echo 'cannot open: Permission denied' >&2; exit 1".into();
        p.workload.clear();
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Skip(reason, quoted) => {
                assert!(reason.contains("was refused"), "{reason}");
                assert_eq!(quoted, ["cannot open: Permission denied"]);
            }
            other => panic!("{other:?}"),
        }
    }

    /// `alongside`: `attach` with no `{pid}`, the workload optional.
    #[test]
    fn alongside_starts_both_then_the_workload() {
        let mut p = pair(BASE);
        p.run = RunMode::Alongside;
        p.settle_ms = Some(20);
        p.workload = vec!["sleep".into(), "0.1".into()];
        expect_pass(&p);
        p.workload.clear();
        expect_pass(&p);

        p.reference.command[2] = "echo {pid}".into();
        let problems = p.problems(&parsers()).join("\n");
        assert!(problems.contains("`{pid}` only means"), "{problems}");
    }

    /// `setup` runs before each side, and a failing setup fails the pair.
    #[test]
    fn setup_runs_before_each_side() {
        let mut p = pair(BASE);
        p.setup = Argv::List(vec![
            "sh".into(),
            "-c".into(),
            "echo x >> {tmp}/setup".into(),
        ]);
        // omn goes first in run 0: it sees one setup, the tool two.
        p.omn[1] = "printf '{\"n\":%d}' $(wc -l < {tmp}/setup)".into();
        p.reference.command[2] = "wc -l < {tmp}/setup".into();
        if let Compares::One(c) = &mut p.compare {
            c.omn = Some("n".into());
            c.omn_scale = 2.0;
        }
        expect_pass(&p);

        p.setup = Argv::List(vec![
            "sh".into(),
            "-c".into(),
            "echo broken >&2; exit 4".into(),
        ]);
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Fail(why) => {
                assert!(why.contains("`setup`") && why.contains("broken"), "{why}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// `requires_setting`: a value past its bound skips naming the file, the
    /// value and the bound.
    #[test]
    fn a_setting_past_its_bound_skips() {
        let dir = scratch(0).unwrap();
        let file = dir.join("paranoid");
        std::fs::write(&file, "2\n").unwrap();
        let mut p = pair(BASE);
        p.reference.requires_setting = vec![Setting {
            file: file.display().to_string(),
            at_most: Some(0),
            at_least: None,
        }];
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Skip(reason, _) => {
                assert!(
                    reason.contains("is 2 here") && reason.contains("at most 0"),
                    "{reason}"
                )
            }
            other => panic!("{other:?}"),
        }
        p.reference.requires_setting[0].at_most = Some(2);
        expect_pass(&p);
        p.reference.requires_setting[0].at_least = Some(3);
        assert!(matches!(
            run_pair(&p, &parsers(), Path::new(SH)),
            Outcome::Skip(..)
        ));

        std::fs::write(&file, "two\n").unwrap();
        assert!(matches!(
            run_pair(&p, &parsers(), Path::new(SH)),
            Outcome::Fail(_)
        ));
        p.reference.requires_setting[0].file = dir.join("absent").display().to_string();
        assert!(matches!(
            run_pair(&p, &parsers(), Path::new(SH)),
            Outcome::Skip(..)
        ));
        p.reference.requires_setting[0].at_most = None;
        p.reference.requires_setting[0].at_least = None;
        assert!(!p.problems(&parsers()).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn one_sided_tolerances_allow_one_side_only() {
        let above = Tolerance {
            rel: Some(0.05),
            side: Some(Direction::Above),
            ..Default::default()
        };
        assert!(above.holds(104.0, 100.0).0);
        assert!(above.holds(100.0, 100.0).0);
        assert!(!above.holds(99.9, 100.0).0);
        assert!(!above.holds(106.0, 100.0).0);
        let below = Tolerance {
            side: Some(Direction::Below),
            ..above
        };
        assert!(below.holds(96.0, 100.0).0);
        assert!(!below.holds(100.1, 100.0).0);
        assert!(!below.holds(94.0, 100.0).0);
        let p = pair(&BASE.replace("{abs: 0}", "{rel: 0.05, side: below}"));
        assert!(p.problems(&parsers()).is_empty());
    }

    #[test]
    fn log2_buckets_count_one_bucket_apart_as_equal() {
        let t = Tolerance {
            log2_buckets: Some(1),
            ..Default::default()
        };
        // 1000 is in [512, 1024), 1500 and 2047 in [1024, 2048).
        assert!(t.holds(1000.0, 1500.0).0);
        assert!(t.holds(2047.0, 1024.0).0);
        assert!(!t.holds(1000.0, 2048.0).0);
        assert!(!t.holds(0.0, 5.0).0, "0 is in no bucket: a lost median");
        assert!(t.holds(0.0, 0.0).0);
        assert_eq!(log2_bucket(1.0), Some(0));
        assert_eq!(log2_bucket(0.5), Some(-1));
        let p = pair(&BASE.replace("{abs: 0}", "{log2_buckets: 1, rel: 0.1}"));
        assert!(!p.problems(&parsers()).is_empty());
        let p = pair(&BASE.replace("{abs: 0}", "{log2_buckets: 1}"));
        assert!(p.problems(&parsers()).is_empty());
    }

    /// `known_defect`: outside the tolerance it passes saying so; inside it,
    /// it fails, so the marker cannot outlive the defect; a broken path is
    /// still a failure.
    #[test]
    fn a_known_defect_holds_until_the_numbers_agree() {
        let mut p = pair(BASE);
        p.known_defect = Some("finding 42: omn reads one too many".into());
        p.reference.command[2] = "echo 4".into();
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::KnownDefect(finding, lines) => {
                assert!(finding.contains("finding 42"));
                assert!(lines[0].contains("omn 5 vs sh 4"), "{lines:?}");
            }
            other => panic!("{other:?}"),
        }
        p.reference.command[2] = "echo 5".into();
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Fail(why) => assert!(why.contains("no longer reproduces"), "{why}"),
            other => panic!("{other:?}"),
        }
        if let Compares::One(c) = &mut p.compare {
            c.omn = Some("b".into());
        }
        match run_pair(&p, &parsers(), Path::new(SH)) {
            Outcome::Fail(why) => assert!(why.contains("omn `b`"), "{why}"),
            other => panic!("{other:?}"),
        }
        p.gap = Some("x".into());
        assert!(!p.problems(&parsers()).is_empty());
    }

    #[test]
    fn the_tally_reads_the_same_in_every_family() {
        let t = Tally {
            held: 3,
            skipped: 2,
            gaps: 1,
            known_defects: 1,
            failed: 0,
        };
        assert_eq!(
            t.line("io"),
            "tally: reference io: 3 held, 2 skipped, 1 gaps, 1 known defects, 0 failed"
        );
    }
}
