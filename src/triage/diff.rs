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
//!   frame it is. Its callers' inclusive cost moves with it, which is true and
//!   is not the difference: the verdict names the self cost that moved, and the
//!   inclusive table says what carried it.
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

use serde::{Deserialize, Serialize};

use crate::emit::{Output, Shortfall};
use crate::encode::{self, RunOutput};
use crate::format::OutputFormat;
use crate::request::Mode;
use crate::resolve::ResolvedRequest;

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
            Self::from_run(origin, &run, None)
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
        Self::from_run(dir.display().to_string(), &run, period_in(dir))
    }

    /// A profile out of a run's results, through the same `folded` encoder a
    /// run writes with, so the two inputs are one format by construction.
    pub fn from_run(
        origin: String,
        run: &RunOutput,
        period_ns: Option<f64>,
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
        let single = RunOutput {
            results: vec![chosen.clone()],
            joins: Vec::new(),
            decompositions: Vec::new(),
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
        })
    }
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
    /// By leaf frame, largest movement first.
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
                let leaf = chain.rsplit(';').next().unwrap_or(chain);
                *by.entry(leaf.to_owned()).or_default() += weight * scale;
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
            ("own (leaf frame)", &self.own),
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

    #[test]
    fn a_malformed_line_is_refused_with_where_it_is() {
        let error = Profile::from_folded("f".to_owned(), "main;a 1\nnope\n").unwrap_err();
        assert!(error.contains("f:2"), "{error}");
    }
}
