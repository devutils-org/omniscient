//! `omn diff`: two profiles and a subtraction.
//!
//! A differential flame graph needs nothing new from the engine: a profile is
//! already written as collapsed stacks by the `folded` encoder, and two of them
//! subtracted stack by stack are the three-column file `flamegraph.pl
//! --difffolded`-style tools draw. What this adds is the part a picture leaves
//! to the eye -- *which function is the difference* -- and the arithmetic that
//! makes the answer honest:
//!
//! - **Absolute, not shares.** When one function doubles, every other
//!   function's *share* of the profile falls although none of them changed.
//!   Comparing shares would name all of them. Two profiles taken at the same
//!   rate are compared in samples, each worth one period, so only the function
//!   whose own cost moved moves; two at different rates are compared in time,
//!   and two whose rate is unknown are compared in samples and say so.
//! - **Self, then inclusive.** A function's own samples are the ones whose leaf
//!   frame it is -- or, in a stack a perf map named a frame of, whose innermost
//!   such frame it is ([`charged_to`]): under a perf map the leaf is the
//!   runtime's and the program's function is never one. Its callers' inclusive
//!   cost moves with it, which is true and is not the difference: the verdict
//!   names the self cost that moved, and the inclusive table says what carried
//!   it.
//! - **Against the noise a count has.** A sample count is a Poisson count, and
//!   two runs of the same program differ by about the square root of their
//!   counts for no reason at all. A difference inside three standard
//!   deviations of that is not reported as one.
//! - **With the losses of both runs.** A profile read from a run's results
//!   carries its `Shortfall`, and a difference between two profiles one of
//!   which lost a tenth of its samples is a difference in the losses as much as
//!   in the program. A folded file carries none, and the answer says it could
//!   not check.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::emit::{Output, Results, Shortfall};
use crate::encode::{self, RunOutput};
use crate::format::OutputFormat;
use crate::request::{Mode, Request};
use crate::resolve::ResolvedRequest;
use crate::stack::unwind::SymbolSource;

/// How many standard deviations of Poisson noise a difference has to exceed.
pub const SIGMAS: f64 = 3.0;

/// One profile, as collapsed stacks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Where it was read from.
    pub origin: String,
    /// `root;...;leaf` to its weight in samples.
    pub stacks: BTreeMap<String, f64>,
    /// The rate it was sampled at, when the input says: a run's directory
    /// carries it in `resolved.json`, a folded file does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_ns: Option<f64>,
    /// What the run lost, when the input carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
    /// Whether the input could have said what it lost: a folded file cannot.
    pub losses_known: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The frames the run's perf maps named, when its profile cites any: a
    /// sample's own function is then the innermost of them, not its leaf.
    #[serde(default, skip_serializing_if = "is_unread")]
    pub perf_map: PerfMapNames,
}

fn is_unread(names: &PerfMapNames) -> bool {
    names == &PerfMapNames::default()
}

impl Profile {
    pub fn total(&self) -> f64 {
        self.stacks.values().sum()
    }

    /// Reads a run's output directory (`omn ... -o DIR`), a run's JSON
    /// results, or a folded file.
    pub fn load(path: &Path) -> Result<Self, String> {
        if path.is_dir() {
            return Self::load_dir(path);
        }
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let origin = path.display().to_string();
        if text.trim_start().starts_with('{') {
            let run: RunOutput = serde_json::from_str(&text)
                .map_err(|error| format!("{origin} is JSON and not a run's results: {error}"))?;
            Self::from_run(origin, &run, None, modified(path))
        } else {
            Self::from_folded(origin, &text)
        }
    }

    fn load_dir(dir: &Path) -> Result<Self, String> {
        let results: PathBuf = dir.join("results.json");
        if !results.exists() {
            let folded = dir.join("results.folded");
            if folded.exists() {
                let mut profile = Self::load(&folded)?;
                profile.period_ns = period_in(dir);
                return Ok(profile);
            }
            return Err(format!(
                "{} holds neither results.json nor results.folded: a profile to compare is a run \
                 written with `-o DIR` and `--format json` or `--format folded`",
                dir.display()
            ));
        }
        let text = std::fs::read_to_string(&results)
            .map_err(|error| format!("cannot read {}: {error}", results.display()))?;
        let run: RunOutput = serde_json::from_str(&text)
            .map_err(|error| format!("{} is not a run's results: {error}", results.display()))?;
        Self::from_run(
            dir.display().to_string(),
            &run,
            period_in(dir),
            modified(&results),
        )
    }

    /// A profile out of a run's results, through the same `folded` encoder a
    /// run writes with, so the two inputs are one format by construction.
    /// `written` is when the results were written, when known: a perf map
    /// the run cites that was modified after it is said to be.
    pub fn from_run(
        origin: String,
        run: &RunOutput,
        period_ns: Option<f64>,
        written: Option<SystemTime>,
    ) -> Result<Self, String> {
        let profiles: Vec<_> = run
            .results
            .iter()
            .filter(|results| {
                results
                    .outputs
                    .iter()
                    .any(|output| matches!(output, Output::Stack { .. }))
            })
            .collect();
        let chosen = match profiles.as_slice() {
            [] => {
                return Err(format!(
                    "{origin} holds no profile: no result carries a `stack` emit"
                ))
            }
            [one] => *one,
            several => {
                return Err(format!(
                    "{origin} holds {} profiles ({}), and a difference is between two profiles \
                     of one thing: write each run with one",
                    several.len(),
                    several
                        .iter()
                        .map(|results| results.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
        };
        // Two runs' tasks are different tasks: a row keyed by its series
        // (`--keys task`) is compared by its frames alone, the series' rows
        // of one stack summed back into one.
        let mut whole = chosen.clone();
        for output in &mut whole.outputs {
            if let Output::Stack { stacks, .. } = output {
                for row in stacks {
                    row.series = None;
                }
            }
        }
        let single = RunOutput {
            results: vec![whole],
            joins: Vec::new(),
            decompositions: Vec::new(),
            machine: None,
        };
        let text = encode::run_output(OutputFormat::Folded, &single)
            .map_err(|error| format!("{origin}: {error}"))?;
        let mut profile = Self::from_folded(origin, &text)?;
        profile.period_ns = period_ns;
        profile.losses_known = true;
        profile.short = chosen.outputs.iter().find_map(|output| match output {
            Output::Stack { short, .. } => short.clone(),
            _ => None,
        });
        let unattributed: usize = chosen
            .outputs
            .iter()
            .map(|output| match output {
                Output::Stack { unattributed, .. } => *unattributed,
                _ => 0,
            })
            .sum();
        if unattributed > 0 {
            profile.notes.push(format!(
                "{unattributed} sample(s) carried no stack and are in no row"
            ));
        }
        profile.notes.extend(chosen.degradations.iter().cloned());
        profile.perf_map = PerfMapNames::of_run(std::slice::from_ref(chosen));
        if let Some(written) = written {
            profile.perf_map.written_after(written);
        }
        profile.notes.extend(profile.perf_map.said());
        Ok(profile)
    }

    /// A folded file: `root;...;leaf weight` per line.
    pub fn from_folded(origin: String, text: &str) -> Result<Self, String> {
        let mut stacks: BTreeMap<String, f64> = BTreeMap::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            let (chain, weight) = line
                .rsplit_once(' ')
                .ok_or_else(|| format!("{origin}:{}: not `stack weight`: {line}", number + 1))?;
            let weight: f64 = weight
                .parse()
                .map_err(|_| format!("{origin}:{}: `{weight}` is not a weight", number + 1))?;
            // The same frames under two stack identities -- two return
            // addresses in one function -- are one stack by name.
            *stacks.entry(chain.to_owned()).or_default() += weight;
        }
        if stacks.is_empty() {
            return Err(format!("{origin} holds no stack"));
        }
        Ok(Self {
            origin,
            stacks,
            period_ns: None,
            short: None,
            losses_known: false,
            notes: Vec::new(),
            perf_map: PerfMapNames::default(),
        })
    }
}

/// When a file was last modified, when the file system says.
fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// The sampling period of the profile request in a run's `resolved.json`.
fn period_in(dir: &Path) -> Option<f64> {
    let text = std::fs::read_to_string(dir.join("resolved.json")).ok()?;
    let resolved: Vec<ResolvedRequest> = serde_json::from_str(&text).ok()?;
    resolved.iter().find_map(|one| match one.request.mode {
        Mode::Sample { .. } => one
            .request
            .mode
            .to_string()
            .strip_prefix("sample@")
            .and_then(|rate| rate.strip_suffix("hz"))
            .and_then(|hz| hz.parse::<f64>().ok())
            .map(|hz| 1e9 / hz),
        _ => None,
    })
}

/// One function's cost in both profiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub function: String,
    pub before: f64,
    pub after: f64,
    /// `after - before`, in the unit the comparison is in.
    pub delta: f64,
    /// `after / before`, absent when it was not there before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    /// How far a difference this size is from zero in Poisson standard
    /// deviations.
    pub sigmas: f64,
    pub significant: bool,
}

/// Two profiles, subtracted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    pub before: Profile,
    pub after: Profile,
    /// What the numbers are in: `samples`, or `ns` when the two rates differ.
    pub unit: String,
    /// What the after profile's samples were multiplied by to be comparable.
    pub scale: f64,
    /// By the function each sample is charged to ([`charged_to`]): its leaf,
    /// or the innermost frame a perf map named. Largest movement first.
    pub own: Vec<Change>,
    /// By every frame a stack passes through, largest movement first.
    pub inclusive: Vec<Change>,
    /// The functions whose own cost moved beyond the noise, heaviest first.
    pub difference: Vec<String>,
    pub sentence: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Subtracts `before` from `after`.
pub fn diff(before: Profile, after: Profile) -> Diff {
    let mut notes = Vec::new();
    // Samples are comparable only at one rate; at two, each is worth its own
    // period and the comparison is in time.
    let (unit, scale_before, scale_after) = match (before.period_ns, after.period_ns) {
        (Some(a), Some(b)) if (a - b).abs() / a.max(b) < 1e-9 => ("samples".to_owned(), 1.0, 1.0),
        (Some(a), Some(b)) => {
            notes.push(format!(
                "the two profiles were sampled at different rates ({:.0} Hz and {:.0} Hz), so \
                 they are compared in time: each sample is worth its own period",
                1e9 / a,
                1e9 / b
            ));
            ("ns".to_owned(), a, b)
        }
        _ => {
            notes.push(
                "the sampling rate of at least one profile is unknown -- a folded file does not \
                 carry it -- so both are assumed to have been taken at the same rate and are \
                 compared in samples"
                    .to_owned(),
            );
            ("samples".to_owned(), 1.0, 1.0)
        }
    };
    for (which, profile) in [("before", &before), ("after", &after)] {
        if !profile.losses_known {
            notes.push(format!(
                "{which} ({}) is a folded file, which carries no loss accounting: whether it \
                 lost samples could not be checked",
                profile.origin
            ));
        } else if let Some(short) = &profile.short {
            notes.push(format!(
                "{which} ({}) is short: {}. A difference between it and the other is partly a \
                 difference in what each lost",
                profile.origin, short.note
            ));
        }
    }

    let own = changes(
        &frames_by(&before, scale_before, Frames::Leaf),
        &frames_by(&after, scale_after, Frames::Leaf),
        scale_before,
        scale_after,
    );
    let inclusive = changes(
        &frames_by(&before, scale_before, Frames::Every),
        &frames_by(&after, scale_after, Frames::Every),
        scale_before,
        scale_after,
    );
    let difference: Vec<String> = own
        .iter()
        .filter(|change| change.significant)
        .map(|change| change.function.clone())
        .collect();
    let sentence = describe(&own, &difference, &unit);
    Diff {
        before,
        after,
        unit,
        scale: scale_after / scale_before,
        own,
        inclusive,
        difference,
        sentence,
        notes,
    }
}

enum Frames {
    Leaf,
    Every,
}

fn frames_by(profile: &Profile, scale: f64, frames: Frames) -> BTreeMap<String, f64> {
    let mut by: BTreeMap<String, f64> = BTreeMap::new();
    for (chain, weight) in &profile.stacks {
        match frames {
            Frames::Leaf => {
                let frames: Vec<String> = chain.split(';').map(str::to_owned).collect();
                let (own, _) = charged_to(&frames, &profile.perf_map);
                *by.entry(own.to_owned()).or_default() += weight * scale;
            }
            Frames::Every => {
                // Once per stack: a recursive function is not twice as
                // expensive for appearing twice in its own stack.
                let seen: BTreeSet<&str> = chain.split(';').collect();
                for frame in seen {
                    *by.entry(frame.to_owned()).or_default() += weight * scale;
                }
            }
        }
    }
    by
}

fn changes(
    before: &BTreeMap<String, f64>,
    after: &BTreeMap<String, f64>,
    scale_before: f64,
    scale_after: f64,
) -> Vec<Change> {
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let mut changes: Vec<Change> = names
        .into_iter()
        .map(|name| {
            let a = before.get(name).copied().unwrap_or(0.0);
            let b = after.get(name).copied().unwrap_or(0.0);
            // The variance of a count is the count; of a count times a
            // period, the count times the period squared.
            let variance = a * scale_before + b * scale_after;
            let sigmas = if variance > 0.0 {
                (b - a).abs() / variance.sqrt()
            } else {
                0.0
            };
            Change {
                function: name.clone(),
                before: a,
                after: b,
                delta: b - a,
                ratio: (a > 0.0).then(|| b / a),
                sigmas,
                significant: sigmas > SIGMAS,
            }
        })
        .collect();
    changes.sort_by(|x, y| {
        y.delta
            .abs()
            .total_cmp(&x.delta.abs())
            .then_with(|| x.function.cmp(&y.function))
    });
    changes
}

fn describe(own: &[Change], difference: &[String], unit: &str) -> String {
    let amount = |value: f64| match unit {
        "ns" => super::span(value),
        _ => format!("{value:+.0} samples"),
    };
    match difference {
        [] => format!(
            "no function's own cost moved beyond {SIGMAS} standard deviations of the noise a \
             sample count has: the two profiles are the same program, as far as this can tell"
        ),
        names => {
            let first = own
                .iter()
                .find(|change| change.function == names[0])
                .expect("a named change");
            let ratio = first
                .ratio
                .map_or("new".to_owned(), |ratio| format!("x{ratio:.2}"));
            let rest = match names.len() {
                1 => "; every other function moved within its noise".to_owned(),
                n => format!(
                    "; {} other function(s) moved beyond the noise as well: {}",
                    n - 1,
                    names[1..].join(", ")
                ),
            };
            format!(
                "the difference is `{}`: {} of its own ({ratio}, {:.1} standard deviations){rest}",
                first.function,
                amount(first.delta),
                first.sigmas
            )
        }
    }
}

impl Diff {
    /// The three-column collapsed file differential flame graphs are drawn
    /// from: `root;...;leaf before after`.
    pub fn folded(&self) -> String {
        let names: BTreeSet<&String> = self
            .before
            .stacks
            .keys()
            .chain(self.after.stacks.keys())
            .collect();
        let mut text = String::new();
        for name in names {
            let a = self.before.stacks.get(name).copied().unwrap_or(0.0);
            let b = self.after.stacks.get(name).copied().unwrap_or(0.0);
            let _ = writeln!(text, "{name} {} {}", a.round() as i64, b.round() as i64);
        }
        text
    }

    /// The report a person reads.
    pub fn text(&self, top: usize) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "diff  {}  ->  {}",
            self.before.origin, self.after.origin
        );
        let _ = writeln!(
            out,
            "  {} samples before, {} after, compared in {}",
            self.before.total().round(),
            self.after.total().round(),
            self.unit
        );
        let _ = writeln!(out, "\n  {}\n", self.sentence);
        for (title, changes) in [
            (
                if self.before.perf_map.symbols.is_empty() && self.after.perf_map.symbols.is_empty()
                {
                    "own (leaf frame)"
                } else {
                    "own (innermost perf-map frame, else leaf)"
                },
                &self.own,
            ),
            ("inclusive", &self.inclusive),
        ] {
            let _ = writeln!(out, "  {title}");
            let _ = writeln!(
                out,
                "    {:<40} {:>10} {:>10} {:>10} {:>7} {:>7}",
                "FUNCTION", "BEFORE", "AFTER", "DELTA", "RATIO", "SIGMA"
            );
            for change in changes.iter().take(top) {
                let _ = writeln!(
                    out,
                    "    {:<40} {:>10.0} {:>10.0} {:>+10.0} {:>7} {:>6.1}{}",
                    truncate(&change.function, 40),
                    change.before,
                    change.after,
                    change.delta,
                    change
                        .ratio
                        .map_or("new".to_owned(), |ratio| format!("{ratio:.2}")),
                    change.sigmas,
                    if change.significant { " *" } else { "" }
                );
            }
            if changes.len() > top {
                let _ = writeln!(out, "    ... and {} more", changes.len() - top);
            }
            let _ = writeln!(out);
        }
        let _ = writeln!(
            out,
            "  * beyond {SIGMAS} standard deviations of the noise a sample count has"
        );
        for note in self
            .notes
            .iter()
            .chain(&self.before.notes)
            .chain(&self.after.notes)
        {
            let _ = writeln!(out, "  note: {note}");
        }
        out
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_owned()
    } else {
        let kept: String = text.chars().take(width - 3).collect();
        format!("{kept}...")
    }
}

// ---------------------------------------------------------------------------
// What a sample is charged to, and what a stack is named
//
// One rule for `omn diff`'s own cost and the triage's hottest function and
// blocking stack, so the two never name a sample's function differently. A
// sample's function is its leaf frame -- except where a perf map named a frame
// of the stack. A perf map names code no file holds: a JIT's, or the
// trampolines an interpreter runs each of the program's functions through
// (`python3 -X perf` writes `py::score:/path/hot.py`). The trampoline *calls*
// the interpreter, so under a perf map the leaf of every sample is one of the
// interpreter's C functions and the program's function is never a leaf; the
// innermost frame the map named is the program's function the sample was in.
// ---------------------------------------------------------------------------

/// How many frames a blocking stack prints at least, leaf first. Four was the
/// whole of it before: enough for a C program, where the program's own frame
/// is usually the second or third.
pub const LEAFWARD_FRAMES: usize = 4;

/// How many frames a blocking stack prints at most while reaching for the
/// program's own frame: an interpreter's is the fifth from the leaf
/// (`__read <- libpython x3 <- py::wait_for_input`), a JIT's deeper still,
/// and a stack whose program frame is further than this prints this many.
pub const LEAFWARD_MAX: usize = 16;

/// Below this share of the samples, the hottest function of an interpreter's
/// profile is not presented as a suspect: the time is spread over the
/// interpreter's own work (`__tls_get_addr` 9 %, `_Py_Dealloc` 7 % in a
/// CPython loop), which every Python program spends, and says nothing about
/// this one. Above it, one C function holds most of the time -- a C extension
/// doing the work, say -- and naming it is the answer.
pub const FLAT: f64 = 0.5;

/// The interpreters a profile is recognised as one of, by the program's name
/// or by a frame's module, and what makes each write a perf map. A list of the
/// runtimes, not a heuristic over the frames: a profile is called an
/// interpreter's only when its program or its library is one of these.
///
/// A name matches an entry when it is the entry, or the entry followed by a
/// version (`python3.14`, `ruby3.3`, `php-fpm8.2`): `node_exporter` is not
/// node. Spellings a distribution installs under another name are entries of
/// their own (`nodejs`, `php-fpm`, `jruby`), before the entry they extend.
///
/// The hints carry no backquotes: in a verdict a backquoted name is the
/// suspect, and a command line is not one.
pub const INTERPRETERS: &[(&str, &str)] = &[
    ("python", "python3 -X perf (3.12 and later)"),
    ("nodejs", "nodejs --perf-basic-prof"),
    ("node", "node --perf-basic-prof"),
    ("jruby", "a JVM agent that writes one"),
    ("java", "a JVM agent that writes one"),
    ("jvm", "a JVM agent that writes one"),
    ("ruby", "ruby --yjit-perf (3.3 and later)"),
    ("perl", "none: Perl writes no perf map"),
    ("php-fpm", "none this tool knows of"),
    ("php", "none this tool knows of"),
    ("luajit", "none this tool knows of"),
    ("lua", "none this tool knows of"),
];

/// The [`INTERPRETERS`] entry a program's or a module's name is, if any.
pub fn runtime_of(name: &str) -> Option<(&'static str, &'static str)> {
    let name = name.rsplit('/').next().unwrap_or(name);
    let name = name.strip_prefix("lib").unwrap_or(name);
    // `python3.14`, `libpython3.14.so.1.0`, `node`, `libjvm.so`: the
    // runtime's name, then a version or a suffix -- not `node_exporter`.
    INTERPRETERS
        .iter()
        .find(|(runtime, _)| {
            name.strip_prefix(runtime).is_some_and(|rest| {
                rest.chars()
                    .next()
                    .is_none_or(|c| c.is_ascii_digit() || c == '.')
            })
        })
        .copied()
}

/// The frames a perf map named in one run, read from the maps its names note
/// cites.
///
/// The stacks in a run's results are labels, not frames: they keep the name
/// and not where it came from. The profile's note does keep that
/// (`names came from -- perf-1234.map: perf-map, the 57 lines ...`), and the
/// map is a file the target wrote on this machine, which a perf map's
/// convention leaves in place after the program exits. So the symbols it holds
/// are read back from it, and a label among them is a frame the map named. A
/// map that is gone by the time the answer is read is said, not guessed at:
/// the samples are then charged to their leaves as before.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PerfMapNames {
    /// The maps read, by path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    /// The maps the note cites that could not be read, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<String>,
    /// The maps read that were modified after the run's results were
    /// written: a map is read back when the answer is built, and one
    /// rewritten since -- the program run again, or a recycled pid -- may
    /// hold names this run's stacks never had.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewritten: Vec<String>,
    /// Every symbol the maps hold.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub symbols: BTreeSet<String>,
    /// Whether the maps were read back from their files after the run,
    /// rather than named by the results' provenance: only then can one have
    /// been rewritten since.
    #[serde(skip)]
    pub read_back: bool,
}

impl PerfMapNames {
    /// The maps cited by `notes` (a run's results' notes), read from `/tmp`,
    /// where every runtime writes them.
    pub fn of<'a>(notes: impl IntoIterator<Item = &'a String>) -> Self {
        Self::of_in(notes, Path::new("/tmp"))
    }

    /// The frames a perf map named in a run, from what its results say.
    ///
    /// A result that carries its stacks' provenance (`named_by`, written
    /// beside `stacks` by `cpu.profile`) says which frame a perf map named,
    /// label by label, and which maps those were (`perf_maps`): nothing is
    /// read back from `/tmp`, so a map rewritten since, or a recycled pid's,
    /// cannot lend this run names it never had. Only when a result holds
    /// stacks without that provenance -- `sched.offcpu`'s, whose frames pass
    /// through the same map -- are the maps read back, from the paths the
    /// results name, else from the ones the names note cites; one whose
    /// device and inode are no longer the ones the run read is said as
    /// rewritten.
    pub fn of_run(results: &[Results]) -> Self {
        Self::of_run_in(results, Path::new("/tmp"))
    }

    /// As [`PerfMapNames::of_run`], a note's maps read from `dir`.
    pub fn of_run_in(results: &[Results], dir: &Path) -> Self {
        let mut names = Self::default();
        let mut files: Vec<&crate::stack::PerfMapFile> = Vec::new();
        let mut uncovered = false;
        for one in results {
            let provenance = &one.provenance;
            if !one.stacks.is_empty() && provenance.named_by.is_empty() {
                uncovered = true;
            }
            for (id, named_by) in &provenance.named_by {
                let Some(labels) = one.stacks.get(id) else {
                    continue;
                };
                for (label, by) in labels.iter().zip(named_by) {
                    if *by == Some(crate::stack::NamedBy::PerfMap) {
                        names
                            .symbols
                            .insert(label.trim_end_matches(RECOVERED).to_owned());
                    }
                }
            }
            for file in &provenance.perf_maps {
                if !files.iter().any(|known| known.path == file.path) {
                    files.push(file);
                }
            }
        }
        let cited = || Self::of_in(results.iter().flat_map(|one| &one.notes), dir);
        if !uncovered {
            // Every stack says what named it: the maps are named, not read.
            names.read = if files.is_empty() {
                let mut cited: BTreeSet<String> = BTreeSet::new();
                for note in results.iter().flat_map(|one| &one.notes) {
                    cited.extend(perf_maps_cited(note));
                }
                cited
                    .into_iter()
                    .map(|map| dir.join(map).display().to_string())
                    .collect()
            } else {
                files.iter().map(|file| file.path.clone()).collect()
            };
            if names.symbols.is_empty() && names.read.is_empty() {
                return Self::default();
            }
            return names;
        }
        if files.is_empty() {
            let mut read_back = cited();
            read_back.symbols.extend(names.symbols);
            return read_back;
        }
        names.read_back = true;
        for file in files {
            match std::fs::read_to_string(&file.path) {
                Ok(text) => {
                    names.symbols.extend(
                        text.lines()
                            .filter_map(crate::stack::PerfMapLine::parse)
                            .map(|line| label_of(&line.symbol)),
                    );
                    names.read.push(file.path.clone());
                    let same = std::fs::metadata(&file.path).is_ok_and(|meta| {
                        use std::os::unix::fs::MetadataExt as _;
                        (file.dev, file.ino) == (0, 0)
                            || (meta.dev(), meta.ino()) == (file.dev, file.ino)
                    });
                    if !same {
                        names.rewritten.push(file.path.clone());
                    }
                }
                Err(error) => names.unreadable.push(format!("{}: {error}", file.path)),
            }
        }
        names
    }

    /// As [`PerfMapNames::of`], with the maps read from `dir`.
    pub fn of_in<'a>(notes: impl IntoIterator<Item = &'a String>, dir: &Path) -> Self {
        let mut names = Self::default();
        let mut cited: BTreeSet<String> = BTreeSet::new();
        for note in notes {
            cited.extend(perf_maps_cited(note));
        }
        for map in cited {
            let path = dir.join(&map);
            names.read_back = true;
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    names.symbols.extend(
                        text.lines()
                            .filter_map(crate::stack::PerfMapLine::parse)
                            .map(|line| label_of(&line.symbol)),
                    );
                    names.read.push(path.display().to_string());
                }
                Err(error) => names
                    .unreadable
                    .push(format!("{}: {error}", path.display())),
            }
        }
        names
    }

    /// Notes every map read that was modified after `written`, the time the
    /// run's results were written. The names are kept: a label of this run
    /// that a newer map also holds is still a frame some map named, and the
    /// sentence says the map is newer than the run.
    pub fn written_after(&mut self, written: SystemTime) {
        if !self.read_back {
            return;
        }
        let newer: Vec<String> = self
            .read
            .iter()
            .filter(|path| {
                std::fs::metadata(path)
                    .and_then(|meta| meta.modified())
                    .is_ok_and(|modified| modified > written)
            })
            .filter(|path| !self.rewritten.contains(path))
            .cloned()
            .collect();
        self.rewritten.extend(newer);
    }

    /// Whether a perf map named the frame this label is.
    pub fn named(&self, label: &str) -> bool {
        !self.symbols.is_empty() && self.symbols.contains(label.trim_end_matches(RECOVERED))
    }

    /// The one sentence that says where the program's function names came
    /// from, or why they could not be read, when either is worth saying.
    pub fn said(&self) -> Option<String> {
        let said = self.said_read()?;
        if self.rewritten.is_empty() {
            return Some(said);
        }
        Some(format!(
            "{said}; {} modified after the run's results were written, so it may hold names \
             this run's program never wrote (the program run again, or its pid reused)",
            self.rewritten.join(", ")
        ))
    }

    fn said_read(&self) -> Option<String> {
        match (self.read.as_slice(), self.unreadable.as_slice()) {
            ([], []) => None,
            (_, []) => Some(format!(
                "each sample is charged to the innermost frame the program's perf map named \
                 ({}), not to its leaf: under a perf map the leaf is the runtime's",
                self.read.join(", ")
            )),
            (_, gone) => Some(format!(
                "the run cites a perf map that could not be read back ({}): samples whose \
                 innermost named frame was in it are charged to their leaf instead",
                gone.join("; ")
            )),
        }
    }
}

const RECOVERED: &str = crate::stack::RECOVERED_MARK;

/// A perf map's symbol as a stack label spells it: demangled when it is a
/// mangled name, as `Frame::label` does.
fn label_of(symbol: &str) -> String {
    crate::stack::demangle(symbol).unwrap_or_else(|| symbol.to_owned())
}

/// The perf maps a names note cites, by file name: every module of the note
/// whose names came from `perf-map`. The note lists `module: origin` pairs
/// separated by `; `, and an origin may itself hold `; `, so the pairs are
/// found by the origin rather than by the separator.
pub fn perf_maps_cited(note: &str) -> Vec<String> {
    const ORIGIN: &str = ": perf-map, ";
    let Some(at) = note.find("names came from -- ") else {
        return Vec::new();
    };
    let listed = &note[at + "names came from -- ".len()..];
    let mut maps = Vec::new();
    let mut from = 0;
    while let Some(found) = listed[from..].find(ORIGIN) {
        let end = from + found;
        let start = listed[..end].rfind("; ").map_or(0, |at| at + 2);
        let module = listed[start..end].trim();
        if !module.is_empty() && !module.contains('/') {
            maps.push(module.to_owned());
        }
        from = end + ORIGIN.len();
    }
    maps
}

/// What one sample is charged to: the innermost frame a perf map named, or
/// its leaf. Root first, as every stack here is stored; `[no stack]` for none.
pub fn charged_to<'a>(frames: &'a [String], perf: &PerfMapNames) -> (&'a str, bool) {
    if let Some(named) = frames.iter().rev().find(|frame| perf.named(frame)) {
        return (named.as_str(), true);
    }
    (frames.last().map_or("[no stack]", String::as_str), false)
}

/// The interpreter whose unnamed frames (`module+0xoffset`) a stack holds,
/// if any. A named frame carries no module, and is not read as one.
fn runtime_in(frames: &[String]) -> Option<(&'static str, &'static str)> {
    frames.iter().find_map(|frame| {
        let (module, offset) = frame.rsplit_once("+0x")?;
        offset
            .chars()
            .all(|c| c.is_ascii_hexdigit())
            .then(|| runtime_of(module))
            .flatten()
    })
}

/// The interpreter a profile is one of: by a name the program goes by, or
/// by the module of unnamed frames in at least [`FLAT`] of its samples --
/// the interpreter is then where the program runs, not a library a C
/// program called once. `None` for any other profile.
pub fn interpreter<'a>(
    programs: &[String],
    samples: impl IntoIterator<Item = &'a [String]>,
) -> Option<(&'static str, &'static str)> {
    if let Some(found) = programs.iter().find_map(|name| runtime_of(name)) {
        return Some(found);
    }
    let mut total = 0u64;
    let mut by: BTreeMap<(&'static str, &'static str), u64> = BTreeMap::new();
    let mut known: BTreeMap<usize, Option<(&'static str, &'static str)>> = BTreeMap::new();
    for frames in samples {
        total += 1;
        // One stack is one allocation, shared by every sample of it.
        let runtime = *known
            .entry(frames.as_ptr() as usize)
            .or_insert_with(|| runtime_in(frames));
        if let Some(runtime) = runtime {
            *by.entry(runtime).or_default() += 1;
        }
    }
    by.into_iter()
        .max_by_key(|(_, count)| *count)
        .filter(|(_, count)| *count as f64 >= FLAT * total as f64)
        .map(|(runtime, _)| runtime)
}

/// The names a launched program's own frames may be spelled with: the file
/// the kernel mapped (`argv[0]` found on `PATH` and its symlinks followed:
/// `python3` is `python3.14`, `cc` is `gcc`), then `argv[0]`'s own base
/// name. A frame's module is the mapped file's name, so a symlinked
/// launcher's would never match `argv[0]`.
pub fn executable_names(argv0: &str) -> Vec<String> {
    let base = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    let path = if argv0.contains('/') {
        Some(PathBuf::from(argv0))
    } else {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(argv0))
                .find(|candidate| candidate.is_file())
        })
    };
    let mut names = Vec::new();
    if let Some(real) = path.and_then(|path| std::fs::canonicalize(path).ok()) {
        names.extend(base(&real));
    }
    if let Some(given) = base(Path::new(argv0)) {
        if !names.contains(&given) {
            names.push(given);
        }
    }
    names
}

/// The hottest function of a set of samples, said as the triage says it.
///
/// `samples` is each sample's stack (root first), `None` for a sample that
/// carried none; `programs` the names the program's executable goes by
/// ([`executable_names`]). The sentence begins `hottest function `NAME``
/// when there is one to name, and says how it was charged: through the
/// program's perf map, or by its leaf. A flat profile of an interpreter
/// whose hottest functions were charged by their leaves names none of them
/// as the suspect, and says why.
///
/// A sample taken while the kernel ran an eBPF program -- a frame of
/// [`is_ebpf_frame`] anywhere on its stack -- is the tool's own cost charged
/// to the program's cpu, not the program's work: it is counted and said, and
/// never ranked, so the tool's programs are never the named suspect.
pub fn hottest<'a>(
    samples: impl IntoIterator<Item = Option<&'a [String]>>,
    perf: &PerfMapNames,
    programs: &[String],
) -> Option<String> {
    // Per function: the samples charged to it, and whether through the map.
    let mut by: BTreeMap<&str, (u64, bool)> = BTreeMap::new();
    let mut total = 0u64;
    let mut through_map = 0u64;
    let mut ebpf = 0u64;
    let mut stacks: Vec<&'a [String]> = Vec::new();
    for frames in samples {
        if frames.is_some_and(|frames| frames.iter().any(|frame| is_ebpf_frame(frame))) {
            ebpf += 1;
            continue;
        }
        total += 1;
        let (charged, mapped) = match frames {
            Some(frames) => {
                stacks.push(frames);
                charged_to(frames, perf)
            }
            None => ("[no stack]", false),
        };
        through_map += u64::from(mapped);
        let entry = by.entry(charged).or_default();
        entry.0 += 1;
        entry.1 |= mapped;
    }
    let mut ranked: Vec<(&str, u64, bool)> = by
        .into_iter()
        .map(|(name, (count, mapped))| (name, count, mapped))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let tool = if ebpf > 0 {
        format!(
            "; {ebpf} more sample(s) ran eBPF programs -- the tool's own cost on the program's \
             cpu -- and are not ranked"
        )
    } else {
        String::new()
    };
    let Some(&(name, count, mapped)) = ranked.first() else {
        return (ebpf > 0).then(|| {
            format!(
                "no function is named: every one of the {ebpf} samples ran eBPF programs, the \
                 tool's own cost on the program's cpu"
            )
        });
    };
    let share = |count: u64| count as f64 / total as f64 * 100.0;
    if mapped {
        return Some(format!(
            "hottest function `{name}`, {count} of {total} samples ({:.0} %), charged to the \
             innermost frame the program's perf map named ({through_map} of the {total} samples \
             had one; the leaf under it is the runtime's){tool}",
            share(count)
        ));
    }
    // Charged by its leaf from here on.
    let partly = if through_map > 0 {
        format!(
            "; only {through_map} of the {total} samples had a frame the program's perf map named"
        )
    } else {
        String::new()
    };
    if (count as f64) < FLAT * total as f64 {
        if let Some((runtime, map)) = interpreter(programs, stacks) {
            let spread: Vec<String> = ranked
                .iter()
                .take(3)
                .map(|(name, count, _)| format!("{name} {:.0} %", share(*count)))
                .collect();
            let unnamed = if through_map > 0 {
                "too few frames name the program's own functions"
            } else {
                "no frame names the program's own functions"
            };
            return Some(format!(
                "the program runs in the {runtime} interpreter and its time is spread, none on \
                 half the samples ({} of {total}, each by its leaf{partly}): {unnamed}, so \
                 which of them these ran for is not known, and none is named as the suspect -- \
                 each may be the interpreter's own work or a library's the program called. A \
                 perf map would name the program's functions: {map}{tool}",
                spread.join(", ")
            ));
        }
    }
    Some(format!(
        "hottest function `{name}`, {count} of {total} samples ({:.0} %){}{tool}",
        share(count),
        if partly.is_empty() {
            String::new()
        } else {
            format!(", charged by its leaf{partly}")
        }
    ))
}

/// Whether a frame is the kernel running an eBPF program: the program
/// itself (`bpf_prog_<tag>_<name>`), or the code that calls one from a
/// tracepoint, a kprobe or a trampoline (`bpf_trace_run2`,
/// `__bpf_trace_sched_switch`, `trace_call_bpf`, `bpf_trampoline_...`). The
/// tool attaches such programs to the program's own tracepoints, and their
/// time lands on its cpu.
pub fn is_ebpf_frame(frame: &str) -> bool {
    let name = frame.split(['+', ' ', '(']).next().unwrap_or("");
    name.starts_with("bpf_prog_")
        || name.starts_with("bpf_trace_run")
        || name.starts_with("__bpf_trace_")
        || name.starts_with("__bpf_prog_")
        || name.starts_with("bpf_trampoline_")
        || name == "trace_call_bpf"
}

/// A blocking stack's frames, leaf first, as far as the program's own frame:
/// at least [`LEAFWARD_FRAMES`], and on to the innermost frame a perf map
/// named -- or, when none was, the innermost frame in the program's
/// executable (`executable+0x...`, under any of `executables`) -- at most
/// [`LEAFWARD_MAX`]. The perf map comes first: a statically linked
/// interpreter's own C frames are in its executable (`python3.12+0x...`),
/// and they are leafward of the program's function. "Leaf first" says the
/// printed frames are the innermost ones; the root is not among them unless
/// the stack is that short.
pub fn leafward<'a>(
    frames: &'a [String],
    perf: &PerfMapNames,
    executables: &[String],
) -> Vec<&'a str> {
    let in_executable = |frame: &str| {
        executables.iter().any(|exe| {
            frame
                .strip_prefix(exe.as_str())
                .is_some_and(|rest| rest.starts_with("+0x"))
        })
    };
    let leaf_first: Vec<&str> = frames.iter().rev().map(String::as_str).collect();
    let reach = leaf_first
        .iter()
        .position(|frame| perf.named(frame))
        .or_else(|| leaf_first.iter().position(|frame| in_executable(frame)))
        .map_or(0, |at| at + 1);
    let kept = reach
        .clamp(LEAFWARD_FRAMES, LEAFWARD_MAX)
        .min(leaf_first.len());
    leaf_first[..kept].to_vec()
}

/// Where the names of each request's stacks came from, as the profile said
/// it (`names came from -- ...`), under the request's label: a verdict that
/// names a function says how it knows the name.
pub fn names_notes(results: &[Results]) -> Vec<String> {
    let mut notes = Vec::new();
    for one in results {
        for note in &one.notes {
            if let Some(at) = note.find("names came from") {
                let line = format!("[{}] {}", one.label, &note[at..]);
                if !notes.contains(&line) {
                    notes.push(line);
                }
            }
        }
    }
    notes
}

/// Sets `--symbols` on every request whose collector reads it, and on no
/// other: a source that does not name its stacks through it refuses
/// `debuginfod` (`symbols_not_read`), and a triage question asks several of
/// each kind at once. Returns the labels it was set on.
pub fn ask_symbols(requests: &mut [Request], symbols: Option<SymbolSource>) -> Vec<String> {
    let Some(symbols) = symbols else {
        return Vec::new();
    };
    let mut set = Vec::new();
    for request in requests {
        if crate::stack::reads_symbols(request.source.as_str()) {
            request.symbols = Some(symbols);
            set.push(
                request
                    .label
                    .clone()
                    .unwrap_or_else(|| request.source.to_string()),
            );
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(lines: &str) -> Profile {
        Profile::from_folded("test".to_owned(), lines).unwrap()
    }

    #[test]
    fn one_function_doubled_is_the_only_difference() {
        let before = profile("main;alpha 1000\nmain;beta 1000\nmain;gamma 1000\n");
        let after = profile("main;alpha 1010\nmain;beta 2000\nmain;gamma 985\n");
        let diff = diff(before, after);
        assert_eq!(diff.difference, ["beta"], "{}", diff.sentence);
        // Its caller carries it, which is true and is not the difference.
        let main = diff
            .inclusive
            .iter()
            .find(|c| c.function == "main")
            .unwrap();
        assert!(main.significant);
    }

    #[test]
    fn shares_would_have_named_everything() {
        // Every other function's share falls from a third to a quarter, and
        // none of them changed: the comparison has to be absolute.
        let before = profile("a 100\nb 100\nc 100\n");
        let after = profile("a 100\nb 200\nc 100\n");
        assert_eq!(diff(before, after).difference, ["b"]);
    }

    #[test]
    fn noise_is_not_a_difference() {
        let before = profile("a 400\nb 400\n");
        let after = profile("a 420\nb 385\n");
        let diff = diff(before, after);
        assert!(diff.difference.is_empty(), "{}", diff.sentence);
    }

    #[test]
    fn two_rates_are_compared_in_time() {
        let mut before = profile("a 100\nb 100\n");
        let mut after = profile("a 200\nb 400\n");
        before.period_ns = Some(1e6);
        after.period_ns = Some(0.5e6);
        let diff = diff(before, after);
        assert_eq!(diff.unit, "ns");
        assert_eq!(diff.difference, ["b"], "{}", diff.sentence);
    }

    #[test]
    fn a_folded_file_says_it_carries_no_losses() {
        let diff = diff(profile("a 1\n"), profile("a 1\n"));
        assert!(diff
            .notes
            .iter()
            .any(|note| note.contains("no loss accounting")));
    }

    #[test]
    fn the_differential_file_has_both_weights_per_stack() {
        let diff = diff(profile("m;a 3\nm;b 1\n"), profile("m;a 3\nm;c 2\n"));
        assert_eq!(diff.folded(), "m;a 3 3\nm;b 1 0\nm;c 0 2\n");
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn labels(stack: &str) -> Vec<String> {
        stack.split(';').map(str::to_owned).collect()
    }

    /// `sqlite3-durability`'s commits: the leaf of the samples the tool's
    /// programs took is `bpf_trace_run2` or a helper under it, and it
    /// outnumbered the program's own leaf.
    #[test]
    fn the_tool_s_own_ebpf_programs_are_said_and_never_named_as_the_hottest_function() {
        let tool = labels("main;fsync;schedule;__traceiter_sched_switch;bpf_trace_run2");
        let helper = labels(
            "main;futex_wait;bpf_trace_run2;bpf_prog_6deef7357e7b4530_omn_futex_out;\
             _raw_spin_unlock_irqrestore",
        );
        let own = labels("main;sqlite3VdbeExec");
        let mut samples: Vec<Option<&[String]>> = Vec::new();
        samples.extend(std::iter::repeat_n(Some(tool.as_slice()), 57));
        samples.extend(std::iter::repeat_n(Some(helper.as_slice()), 34));
        samples.extend(std::iter::repeat_n(Some(own.as_slice()), 40));
        let says = hottest(samples.iter().copied(), &PerfMapNames::default(), &[]).unwrap();
        assert!(
            says.starts_with("hottest function `sqlite3VdbeExec`, 40 of 40 samples"),
            "{says}"
        );
        assert!(
            says.contains("91 more sample(s) ran eBPF programs"),
            "{says}"
        );
        let only_tool: Vec<Option<&[String]>> = vec![Some(tool.as_slice()); 3];
        let says = hottest(only_tool, &PerfMapNames::default(), &[]).unwrap();
        assert!(says.starts_with("no function is named"), "{says}");
        assert!(!says.contains("hottest function"), "{says}");
        for frame in [
            "bpf_trace_run2",
            "bpf_trace_run4+0x7a",
            "bpf_prog_6deef7357e7b4530_omn_futex_out",
            "__bpf_trace_sched_switch",
            "trace_call_bpf",
        ] {
            assert!(is_ebpf_frame(frame), "{frame}");
        }
        for frame in [
            "schedule",
            "bpf_map_lookup_elem_user",
            "__traceiter_sched_switch",
        ] {
            assert!(!is_ebpf_frame(frame), "{frame}");
        }
    }

    fn mapped(symbols: &[&str]) -> PerfMapNames {
        PerfMapNames {
            read: vec!["/tmp/perf-1.map".to_owned()],
            symbols: symbols.iter().map(|s| (*s).to_owned()).collect(),
            ..PerfMapNames::default()
        }
    }

    /// The names note as `Unwinder::facts` writes it, a perf map among
    /// modules whose own origin holds `; `.
    const NOTE: &str = "names came from -- python3: its dynamic symbols only: its internal \
                        functions are unnamed; libc.so.6: its dynamic symbols only; debuginfod: \
                        no server; perf-4242.map: perf-map, the 57 lines the program wrote for \
                        code no file holds";

    #[test]
    fn the_perf_maps_a_note_cites_are_found_by_their_origin() {
        assert_eq!(perf_maps_cited(NOTE), ["perf-4242.map"]);
        assert!(perf_maps_cited("names came from -- sqlite3: no names: stripped").is_empty());
        assert!(perf_maps_cited("stacks unwound by fp").is_empty());
    }

    #[test]
    fn a_cited_map_is_read_back_and_a_missing_one_is_said() {
        let dir = std::env::temp_dir().join(format!("omn-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("perf-4242.map"),
            "7f0000001000 b py::score:/tmp/hot.py\nnot a line\n7f0000002000 b py::main:/tmp/hot.py\n",
        )
        .unwrap();
        let notes = vec![NOTE.to_owned()];
        let names = PerfMapNames::of_in(&notes, &dir);
        assert!(names.named("py::score:/tmp/hot.py"));
        assert!(names.named("py::main:/tmp/hot.py [recovered]"));
        assert!(!names.named("_PyEval_EvalFrameDefault"));
        assert!(names.said().unwrap().contains("innermost frame"));
        std::fs::remove_file(dir.join("perf-4242.map")).unwrap();
        let gone = PerfMapNames::of_in(&notes, &dir);
        assert!(gone.symbols.is_empty());
        assert!(
            gone.said().unwrap().contains("could not be read back"),
            "{gone:?}"
        );
        let _ = std::fs::remove_dir(&dir);
    }

    /// `python3 -X perf`, as w17-python measured it: the leaf of every
    /// sample is the interpreter's, and `py::score` is under it on 1 043 of
    /// 1 056 samples.
    #[test]
    fn under_a_perf_map_the_hottest_is_the_innermost_mapped_frame() {
        let perf = mapped(&["py::score:/x/hot.py", "py::main:/x/hot.py"]);
        let in_score =
            labels("_start;py::main:/x/hot.py;_PyEval;py::score:/x/hot.py;_PyEval;__tls_get_addr");
        let in_score_dealloc = labels("_start;py::main:/x/hot.py;py::score:/x/hot.py;_Py_Dealloc");
        let elsewhere = labels("_start;py::main:/x/hot.py;_PyEval;__tls_get_addr");
        let mut samples: Vec<Option<&[String]>> = Vec::new();
        samples.extend(std::iter::repeat_n(Some(in_score.as_slice()), 600));
        samples.extend(std::iter::repeat_n(Some(in_score_dealloc.as_slice()), 443));
        samples.extend(std::iter::repeat_n(Some(elsewhere.as_slice()), 13));
        let says = hottest(samples.iter().copied(), &perf, &names(&["python3"])).unwrap();
        assert!(
            says.starts_with("hottest function `py::score:/x/hot.py`, 1043 of 1056 samples"),
            "{says}"
        );
        // Ranked by leaf, `__tls_get_addr` would have been named.
        let by_leaf = hottest(samples.iter().copied(), &PerfMapNames::default(), &[]).unwrap();
        assert!(
            by_leaf.starts_with("hottest function `__tls_get_addr`"),
            "{by_leaf}"
        );
    }

    /// The same program without `-X perf`: a flat profile of the
    /// interpreter's C functions is said as one, and none is the suspect.
    #[test]
    fn a_flat_interpreter_profile_names_no_c_function_as_the_suspect() {
        let stacks: Vec<Vec<String>> = [
            ("libpython3.14.so.1.0+0x2ebff6;__tls_get_addr", 100),
            ("libpython3.14.so.1.0+0x2ebff6;_Py_Dealloc", 75),
            ("libpython3.14.so.1.0+0x2ebff6;PyLong_FromLong", 55),
        ]
        .iter()
        .flat_map(|(stack, n)| std::iter::repeat_n(labels(stack), *n))
        // The interpreter's internal functions are unnamed, and an unnamed
        // leaf is an instruction: spread over hundreds of addresses.
        .chain((0..890).map(|i| {
            labels(&format!(
                "libpython3.14.so.1.0+0x2ebff6;libpython3.14.so.1.0+{:#x}",
                0x1000 + i % 300
            ))
        }))
        .collect();
        let samples = stacks.iter().map(|s| Some(s.as_slice()));
        // Recognised by the library alone: the program may be a script.
        let says = hottest(samples, &PerfMapNames::default(), &names(&["hot.py"])).unwrap();
        assert!(!says.contains("hottest function `"), "{says}");
        assert!(says.contains("python interpreter"), "{says}");
        assert!(says.contains("-X perf"), "{says}");
        // Nothing in backquotes: a verdict's backquoted name is its suspect.
        assert!(!says.contains('`'), "{says}");
        // A C program's flat profile is still ranked: it is its own code.
        let c: Vec<Vec<String>> = (0..10).map(|i| labels(&format!("main;f{i}"))).collect();
        let says = hottest(
            c.iter().map(|s| Some(s.as_slice())),
            &PerfMapNames::default(),
            &names(&["app"]),
        )
        .unwrap();
        assert!(says.starts_with("hottest function `f"), "{says}");
    }

    /// One C function holding most of an interpreter's time -- a C extension
    /// doing the work -- is the answer, and is named.
    #[test]
    fn an_interpreter_s_dominant_c_function_is_still_named() {
        let hash = labels("python3+0x10;sha256_block");
        let other = labels("python3+0x10;_Py_Dealloc");
        let mut samples: Vec<Option<&[String]>> = vec![Some(hash.as_slice()); 80];
        samples.extend(vec![Some(other.as_slice()); 20]);
        let says = hottest(samples, &PerfMapNames::default(), &names(&["python3"])).unwrap();
        assert!(
            says.starts_with("hottest function `sha256_block`"),
            "{says}"
        );
    }

    #[test]
    fn interpreters_are_known_by_name_not_by_prefix_alone() {
        let none: [&[String]; 0] = [];
        let by_name = |name: &str| interpreter(&names(&[name]), none).map(|r| r.0);
        assert_eq!(by_name("/usr/bin/python3.14"), Some("python"));
        assert_eq!(by_name("node"), Some("node"));
        assert_eq!(by_name("nodejs"), Some("nodejs"));
        assert_eq!(by_name("php-fpm8.2"), Some("php-fpm"));
        assert_eq!(by_name("php8.3"), Some("php"));
        assert_eq!(by_name("jruby"), Some("jruby"));
        assert_eq!(by_name("ruby3.3"), Some("ruby"));
        assert_eq!(by_name("luajit"), Some("luajit"));
        assert_eq!(by_name("node_exporter"), None);
        assert_eq!(by_name("./sqlite3"), None);
        let frames = labels("libjvm.so+0x99;libc.so.6+0x1");
        assert_eq!(
            interpreter(&[], [frames.as_slice()]).map(|r| r.0),
            Some("jvm")
        );
        // A named frame carries no module, and is not read as one.
        let named = labels("python_helper");
        assert_eq!(interpreter(&[], [named.as_slice()]), None);
    }

    /// A C program that embeds python, or calls into a JVM now and then, is
    /// not an interpreter's profile: the runtime's frames must be where the
    /// program runs, in at least half the samples.
    #[test]
    fn a_c_program_that_calls_an_interpreter_is_not_one() {
        let embedded = labels("main;run_script;libpython3.12.so.1.0+0x1234;PyEval");
        let own: Vec<Vec<String>> = (0..9).map(|i| labels(&format!("main;work{i}"))).collect();
        let mut samples: Vec<Option<&[String]>> = own.iter().map(|s| Some(s.as_slice())).collect();
        samples.push(Some(embedded.as_slice()));
        let says = hottest(samples, &PerfMapNames::default(), &names(&["app"])).unwrap();
        assert!(says.starts_with("hottest function `"), "{says}");
        assert!(!says.contains("interpreter"), "{says}");
    }

    /// A C extension holding less than half -- `sha256_block` at 45 % -- is
    /// listed, and not said to be the interpreter's own work.
    #[test]
    fn a_flat_interpreter_profile_does_not_say_whose_its_functions_are() {
        let hash = labels("python3+0x10;sha256_block");
        let other: Vec<Vec<String>> = (0..55)
            .map(|i| labels(&format!("python3+0x10;python3+{:#x}", 0x100 + i)))
            .collect();
        let mut samples: Vec<Option<&[String]>> = vec![Some(hash.as_slice()); 45];
        samples.extend(other.iter().map(|s| Some(s.as_slice())));
        let says = hottest(samples, &PerfMapNames::default(), &names(&["python3"])).unwrap();
        assert!(says.contains("sha256_block 45 %"), "{says}");
        assert!(
            says.contains("may be the interpreter's own work or a library's"),
            "{says}"
        );
        assert!(!says.contains("interpreter's own functions"), "{says}");
        assert!(!says.contains('`'), "{says}");
    }

    /// A few samples through the perf map -- a JIT warming up -- do not make
    /// a function charged by its leaf read as charged through the map.
    #[test]
    fn a_leaf_charged_function_is_said_so_when_a_few_samples_were_mapped() {
        let perf = mapped(&["py::warm:/x/a.py"]);
        let warm = labels("_start;py::warm:/x/a.py;_PyEval;leaf");
        let gc = labels("_start;libc.so.6+0x1;gc_collect");
        let mut samples: Vec<Option<&[String]>> = vec![Some(warm.as_slice()); 3];
        samples.extend(vec![Some(gc.as_slice()); 97]);
        let says = hottest(samples, &perf, &names(&["myapp"])).unwrap();
        assert!(
            says.starts_with("hottest function `gc_collect`, 97 of 100"),
            "{says}"
        );
        assert!(
            says.contains("charged by its leaf; only 3 of the 100"),
            "{says}"
        );
        assert!(!says.contains("charged to the innermost"), "{says}");
    }

    /// `docs/fixes/w17-python.md` finding 3: the Python function is the fifth
    /// frame from the leaf, and four printed none of it.
    #[test]
    fn a_block_s_stack_is_printed_to_the_program_s_own_frame() {
        let perf = mapped(&["py::wait_for_input:/x/reader.py"]);
        let frames = labels(
            "_start;py::main:/x/reader.py;py::wait_for_input:/x/reader.py;libpython+0x5dfc1;\
             libpython+0x3335ca;libpython+0x2ebff6;__read",
        );
        let printed = leafward(&frames, &perf, &names(&["python3"]));
        assert_eq!(printed.len(), 5, "{printed:?}");
        assert_eq!(printed[4], "py::wait_for_input:/x/reader.py");
        // A stripped executable's own frame, by its module.
        let sqlite = labels("sqlite3+0x1;sqlite3+0x70880;a;b;c;d;clock_nanosleep");
        let printed = leafward(&sqlite, &PerfMapNames::default(), &names(&["sqlite3"]));
        assert_eq!(printed.last(), Some(&"sqlite3+0x70880"), "{printed:?}");
        // Never fewer than four, never more than the ceiling.
        let short = labels("main;f;read");
        assert_eq!(leafward(&short, &PerfMapNames::default(), &[]).len(), 3);
        let deep: Vec<String> = (0..40).map(|i| format!("f{i}")).collect();
        let mut deep = deep;
        deep.insert(0, "app+0x10".to_owned());
        assert_eq!(
            leafward(&deep, &PerfMapNames::default(), &names(&["app"])).len(),
            LEAFWARD_MAX
        );
        assert_eq!(
            leafward(&deep, &PerfMapNames::default(), &[]).len(),
            LEAFWARD_FRAMES
        );
    }

    /// Debian's and Ubuntu's python3.x is statically linked: its C frames are
    /// in the executable (`python3.12+0x...`), leafward of the Python
    /// function. The perf map's frame is reached, not the first of those.
    #[test]
    fn a_static_interpreter_s_own_frames_do_not_stop_the_reach() {
        let perf = mapped(&["py::wait_for_input:/x/reader.py"]);
        let frames = labels(
            "python3.12+0x10;py::main:/x/reader.py;py::wait_for_input:/x/reader.py;\
             python3.12+0x5dfc1;python3.12+0x3335ca;python3.12+0x2ebff6;__read",
        );
        let printed = leafward(&frames, &perf, &names(&["python3.12", "python3"]));
        assert_eq!(printed.len(), 5, "{printed:?}");
        assert_eq!(printed[4], "py::wait_for_input:/x/reader.py");
    }

    /// `python3` -> `python3.14`: the frames are spelled by the mapped file.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_launcher_is_known_by_the_file_it_points_to() {
        let dir = std::env::temp_dir().join(format!("omn-exe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("python3.14");
        std::fs::write(&real, "").unwrap();
        let link = dir.join("python3");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let found = executable_names(&link.display().to_string());
        assert_eq!(found, ["python3.14", "python3"]);
        let sqlite = labels("main;python3.14+0x70880;a;b;c;d;read");
        assert_eq!(
            leafward(&sqlite, &PerfMapNames::default(), &found).last(),
            Some(&"python3.14+0x70880")
        );
        // A name found nowhere is still its own.
        assert_eq!(
            executable_names("no-such-program-here"),
            ["no-such-program-here"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A result whose stacks carry `named_by`: one stack, the interpreter's
    /// leaf under a frame a perf map named.
    fn profiled(perf_maps: serde_json::Value) -> Results {
        serde_json::from_value(serde_json::json!({
            "label": "on-cpu", "source": "cpu.profile", "backend": "pmu:sw",
            "outputs": [], "events_seen": 0, "observed_ns": 0, "stopped_because": "exit",
            "unit": serde_json::to_value(crate::catalog::Unit::Count).unwrap(),
            "losses": serde_json::to_value(crate::collect::Losses::default()).unwrap(),
            "notes": [NOTE],
            "stacks": {"1": ["main", "py::score:/tmp/hot.py", "_PyEval_EvalFrameDefault"]},
            "named_by": {"1": ["elf", "perf-map", "elf"]},
            "perf_maps": perf_maps,
        }))
        .unwrap_or_else(|error| panic!("a minimal Results: {error}"))
    }

    /// The results say which frame a perf map named: nothing is read back
    /// from the directory the note points at, so a map there that holds
    /// other names lends none of them.
    #[test]
    fn the_results_provenance_names_the_mapped_frames_without_reading_the_map() {
        let dir = std::env::temp_dir().join(format!("omn-provenance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Another process's map under the same name: what a reused pid leaves.
        std::fs::write(
            dir.join("perf-4242.map"),
            "7f0000001000 b py::other:/tmp/x.py\n",
        )
        .unwrap();
        let results = [profiled(serde_json::json!([]))];
        let names = PerfMapNames::of_run_in(&results, &dir);
        assert!(names.named("py::score:/tmp/hot.py"));
        assert!(names.named("py::score:/tmp/hot.py [recovered]"));
        assert!(!names.named("py::other:/tmp/x.py"), "{names:?}");
        assert!(!names.named("_PyEval_EvalFrameDefault"));
        assert!(!names.read_back);
        assert_eq!(
            names.read,
            [dir.join("perf-4242.map").display().to_string()]
        );
        assert!(names.said().unwrap().contains("innermost frame"));
        // The map named by the results' own `perf_maps` is the one said.
        let named = [profiled(serde_json::json!([
            {"path": "/tmp/perf-4242.map", "dev": 1, "ino": 2}
        ]))];
        let names = PerfMapNames::of_run_in(&named, &dir);
        assert_eq!(names.read, ["/tmp/perf-4242.map"]);
        let mut written = names.clone();
        written.written_after(SystemTime::UNIX_EPOCH);
        assert!(written.rewritten.is_empty(), "nothing was read back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A result whose stacks carry no provenance (`sched.offcpu`'s) beside
    /// one that does: the map is read back, from the path the results name,
    /// and a file that is no longer the one the run read is said.
    #[test]
    fn stacks_without_provenance_read_the_named_map_back_and_say_another_file() {
        let dir = std::env::temp_dir().join(format!("omn-provenance-b-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let map = dir.join("perf-4242.map");
        std::fs::write(&map, "7f0000001000 b py::wait:/tmp/hot.py\n").unwrap();
        let meta = std::fs::metadata(&map).unwrap();
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt as _;
            (meta.dev(), meta.ino())
        };
        let offcpu: Results = serde_json::from_value(serde_json::json!({
            "label": "blocked", "source": "sched.offcpu", "backend": "ebpf",
            "outputs": [], "events_seen": 0, "observed_ns": 0, "stopped_because": "exit",
            "unit": serde_json::to_value(crate::catalog::Unit::Ns).unwrap(),
            "losses": serde_json::to_value(crate::collect::Losses::default()).unwrap(),
            "stacks": {"7": ["main", "py::wait:/tmp/hot.py", "select"]},
        }))
        .unwrap_or_else(|error| panic!("a minimal Results: {error}"));
        let path = map.display().to_string();
        let same = [
            profiled(serde_json::json!([{"path": path, "dev": dev, "ino": ino}])),
            offcpu.clone(),
        ];
        let names = PerfMapNames::of_run_in(&same, &dir);
        assert!(names.read_back);
        assert!(names.named("py::wait:/tmp/hot.py"), "{names:?}");
        assert!(names.named("py::score:/tmp/hot.py"));
        assert!(names.rewritten.is_empty(), "{names:?}");
        let other = [
            profiled(serde_json::json!([{"path": path, "dev": dev, "ino": ino + 1}])),
            offcpu,
        ];
        let names = PerfMapNames::of_run_in(&other, &dir);
        assert_eq!(names.rewritten, [path]);
        assert!(
            names.said().unwrap().contains("modified after"),
            "{names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_map_rewritten_after_the_run_is_said() {
        let dir = std::env::temp_dir().join(format!("omn-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("perf-4242.map"),
            "7f0000001000 b py::score:/tmp/hot.py\n",
        )
        .unwrap();
        let notes = vec![NOTE.to_owned()];
        let mut names = PerfMapNames::of_in(&notes, &dir);
        names.written_after(SystemTime::now() + std::time::Duration::from_secs(60));
        assert!(names.rewritten.is_empty());
        names.written_after(SystemTime::UNIX_EPOCH);
        assert_eq!(names.rewritten.len(), 1);
        assert!(
            names.said().unwrap().contains("modified after"),
            "{names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_names_note_is_carried_under_the_request_s_label() {
        let mut on_cpu: Results = serde_json::from_value(serde_json::json!({
            "label": "on-cpu", "source": "cpu.profile", "backend": "pmu:sw",
            "outputs": [], "events_seen": 0, "observed_ns": 0, "stopped_because": "exit",
            "unit": serde_json::to_value(crate::catalog::Unit::Count).unwrap(),
            "losses": serde_json::to_value(crate::collect::Losses::default()).unwrap(),
        }))
        .unwrap_or_else(|error| panic!("a minimal Results: {error}"));
        on_cpu.notes = vec![
            "`fp` walked 10 of 10".to_owned(),
            "names came from -- sqlite3: no names: stripped".to_owned(),
        ];
        assert_eq!(
            names_notes(&[on_cpu]),
            ["[on-cpu] names came from -- sqlite3: no names: stripped"]
        );
    }

    #[test]
    fn symbols_are_asked_only_of_the_requests_that_read_them() {
        let mut requests: Vec<Request> = ["cpu.profile", "sched.runqlat"]
            .iter()
            .map(|source| Request {
                source: source.parse().unwrap(),
                target: crate::request::Target::Launch(vec!["true".to_owned()]),
                mode: "trace".parse().unwrap(),
                emit: vec!["summary".parse().unwrap()],
                keys: Vec::new(),
                filter: None,
                duration: None,
                budget: None,
                stack: None,
                symbols: None,
                backend: None,
                label: Some((*source).to_owned()),
                window: None,
                value: None,
                sum: Vec::new(),
            })
            .collect();
        assert!(ask_symbols(&mut requests, None).is_empty());
        let set = ask_symbols(&mut requests, Some(SymbolSource::Debuginfod));
        assert_eq!(set, ["cpu.profile"]);
        assert_eq!(requests[0].symbols, Some(SymbolSource::Debuginfod));
        assert_eq!(requests[1].symbols, None);
    }

    /// `omn diff` charges by the same rule: under a perf map, the Python
    /// function whose time doubled is the difference, not the C leaf.
    #[test]
    fn a_diff_under_a_perf_map_names_the_mapped_function() {
        let mut before = profile("main;py::a;_PyEval;leaf 1000\nmain;py::b;_PyEval;leaf 1000\n");
        let mut after = profile("main;py::a;_PyEval;leaf 1000\nmain;py::b;_PyEval;leaf 2000\n");
        before.perf_map = mapped(&["py::a", "py::b"]);
        after.perf_map = mapped(&["py::a", "py::b"]);
        let diff = diff(before, after);
        assert_eq!(diff.difference, ["py::b"], "{}", diff.sentence);
    }

    #[test]
    fn a_malformed_line_is_refused_with_where_it_is() {
        let error = Profile::from_folded("f".to_owned(), "main;a 1\nnope\n").unwrap_err();
        assert!(error.contains("f:2"), "{error}");
    }
}
