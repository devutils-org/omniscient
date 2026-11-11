//! `omn advise`: from a reading of this run to the setting behind it.
//!
//! The rules are data (`catalog/advice.yaml`). Each names the measurements
//! and settings that must hold, the one setting the advice is about, the
//! reading changing it is expected to move, and the command a person would
//! type. This layer measures, reads the settings beside the measurement
//! (`conf.*`), and says which rules hold. **It changes nothing**: the command
//! is printed, never run (`docs/DECISIONS.md`, boundary 5). What closes the
//! loop is `--against`: the same question asked after the person changed the
//! setting, compared with the answer before it, rule by rule, on the reading
//! each rule said would move.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use super::engine::{self, Dropped, Run};
use super::{Method, Objective, Refusal, TriageError};
use crate::capabilities::Capabilities;
use crate::catalog::Catalog;
use crate::collect::conf::{self, settings, Reading, State};
use crate::emit::{Output, Shortfall};
use crate::event::{AttrValue, Event};
use crate::request::{Request, Session, SourceId, Target};
use crate::scalar::Duration;

const TABLE: &str = include_str!("../../catalog/advice.yaml");

// --- the rules ---------------------------------------------------------------

/// A number or a word, as a rule compares with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    Number(f64),
    Text(String),
}

impl Scalar {
    fn matches(&self, number: Option<f64>, text: Option<&str>) -> bool {
        match self {
            Scalar::Number(want) => number.is_some_and(|have| (have - want).abs() < 1e-9),
            Scalar::Text(want) => text.is_some_and(|have| have.trim() == want),
        }
    }
}

impl std::fmt::Display for Scalar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scalar::Number(number) => write!(f, "{}", trim_number(*number)),
            Scalar::Text(text) => f.write_str(text),
        }
    }
}

/// What a measure reduces a source's values to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stat {
    Total,
    Mean,
    Max,
    Min,
    Count,
    First,
    Last,
}

/// Which way a reading is expected to move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Goes {
    Down,
    Up,
    Changes,
}

/// One item of a rule: a measure or a setting, with a comparison or a
/// direction. The loader holds each to the shape its place allows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stat: Option<Stat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setting: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is: Option<Scalar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_not: Option<Scalar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below_setting: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goes: Option<Goes>,
    /// For a setting with several instances: every instance must hold, not
    /// one of them (the default).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub every: bool,
    /// A measure this one is divided by: block requests per page-cache miss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per: Option<Box<Item>>,
}

impl Item {
    /// Whether this item is a measurement (or the run's wall clock) rather
    /// than a setting.
    fn is_measure(&self) -> bool {
        self.source.is_some() || self.run.is_some()
    }

    /// The name a measure is known by, in a session and in an answer.
    pub fn label(&self) -> String {
        if let Some(run) = &self.run {
            return format!("run {run}");
        }
        let mut label = format!(
            "{} {}",
            self.source.as_deref().unwrap_or(""),
            self.mode.as_deref().unwrap_or("")
        );
        if self.target.as_deref() == Some("system") {
            label.push_str(" system");
        }
        label
    }

    /// The name of one reading: the measure and what it is reduced to.
    pub fn reading(&self) -> String {
        if self.run.is_some() {
            return self.label();
        }
        let stat = self
            .stat
            .map(|stat| format!("{stat:?}").to_lowercase())
            .unwrap_or_default();
        let own = match &self.attr {
            Some(attr) => format!("{} {stat}({attr})", self.label()),
            None => format!("{} {stat}", self.label()),
        };
        match &self.per {
            Some(per) => format!("{own} per {}", per.reading()),
            None => own,
        }
    }

    fn comparison(&self) -> String {
        let mut parts = Vec::new();
        if let Some(above) = self.above {
            parts.push(format!("above {}", trim_number(above)));
        }
        if let Some(below) = self.below {
            parts.push(format!("below {}", trim_number(below)));
        }
        if let Some(is) = &self.is {
            parts.push(format!("is {is}"));
        }
        if let Some(is_not) = &self.is_not {
            parts.push(format!("is not {is_not}"));
        }
        if let Some(other) = &self.below_setting {
            parts.push(format!("below {other}"));
        }
        parts.join(", ")
    }
}

/// One rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub summary: String,
    pub objectives: Vec<Objective>,
    pub evidence: Vec<Item>,
    pub setting: String,
    pub mechanism: String,
    pub docs: String,
    pub expect: Item,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also: Vec<Item>,
    pub scope: String,
    pub change: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
    pub truth: String,
}

impl Rule {
    /// Every measure the rule reads: its evidence, its expectation, its side
    /// effects.
    fn measures(&self) -> impl Iterator<Item = &Item> {
        self.evidence
            .iter()
            .chain(std::iter::once(&self.expect))
            .chain(&self.also)
            .filter(|item| item.is_measure())
    }

    /// Every measure a session must ask for: the rule's, and what each is
    /// divided by.
    fn requested(&self) -> impl Iterator<Item = &Item> {
        self.measures()
            .flat_map(|item| std::iter::once(item).chain(item.per.as_deref()))
    }

    /// Every setting the rule reads.
    fn settings(&self) -> BTreeSet<&str> {
        let mut found: BTreeSet<&str> = self
            .evidence
            .iter()
            .filter_map(|item| item.setting.as_deref())
            .collect();
        for item in &self.evidence {
            if let Some(other) = &item.below_setting {
                found.insert(other);
            }
        }
        found.insert(&self.setting);
        found
    }
}

/// The rules, as the binary carries them.
pub fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| load(TABLE).expect("the compiled-in advice table loads"))
}

/// Parses a table and holds it to its rules, except the catalog's: those are
/// [`check_against`].
pub fn load(text: &str) -> Result<Vec<Rule>, String> {
    let rules: Vec<Rule> = serde_norway::from_str(text).map_err(|error| error.to_string())?;
    let mut ids = BTreeSet::new();
    for rule in &rules {
        let fail = |why: String| -> Result<Vec<Rule>, String> {
            Err(format!("advice rule `{}`: {why}", rule.id))
        };
        let named = |why: String| format!("advice rule `{}`: {why}", rule.id);
        if !ids.insert(rule.id.as_str()) {
            return fail("declared twice".to_owned());
        }
        if rule.objectives.is_empty() {
            return fail("serves no objective".to_owned());
        }
        if rule.evidence.iter().all(|item| !item.is_measure()) {
            return fail(
                "has no measured evidence: advice fires on a reading of the run, never on a \
                 setting alone"
                    .to_owned(),
            );
        }
        if rule.truth.trim().is_empty() {
            return fail("names no ground-truth test".to_owned());
        }
        if rule.change.trim().is_empty() {
            return fail("names no change".to_owned());
        }
        if !matches!(rule.scope.as_str(), "process" | "cgroup" | "machine") {
            return fail(format!(
                "scope `{}` is not process, cgroup or machine",
                rule.scope
            ));
        }
        for item in &rule.evidence {
            check_item(item, Place::Evidence).map_err(named)?;
        }
        check_item(&rule.expect, Place::Expect).map_err(named)?;
        if !rule.expect.is_measure() {
            return fail(
                "expects a setting to move, and a setting is what the person changes".to_owned(),
            );
        }
        for item in &rule.also {
            check_item(item, Place::Expect).map_err(named)?;
        }
    }
    Ok(rules)
}

#[derive(Clone, Copy)]
enum Place {
    Evidence,
    Expect,
}

fn check_item(item: &Item, place: Place) -> Result<(), String> {
    let kinds = [
        item.source.is_some(),
        item.run.is_some(),
        item.setting.is_some(),
    ]
    .iter()
    .filter(|kind| **kind)
    .count();
    if kinds != 1 {
        return Err(format!(
            "an item names exactly one of source, run, setting: {item:?}"
        ));
    }
    if item.every && item.setting.is_none() {
        return Err(format!("`every` is for a setting's instances: {item:?}"));
    }
    if let Some(per) = &item.per {
        if per.source.is_none() || per.per.is_some() || per.stat.is_none() || per.mode.is_none() {
            return Err(format!(
                "`per` names one measure, with a mode and a stat, divided by nothing: {item:?}"
            ));
        }
    }
    if let Some(run) = &item.run {
        if run != "wall_ns" {
            return Err(format!("`run: {run}` is not a reading: `wall_ns` is"));
        }
    }
    if item.source.is_some() && (item.mode.is_none() || item.stat.is_none()) {
        return Err(format!(
            "measure `{}` needs a mode and a stat",
            item.label()
        ));
    }
    if let Some(mode) = &item.mode {
        mode.parse::<crate::request::Mode>()
            .map_err(|error| format!("mode `{mode}`: {error}"))?;
    }
    if let Some(target) = &item.target {
        if target != "system" {
            return Err(format!(
                "target `{target}`: only `system` is written, since a run observes one workload \
                 beside any number of machine targets"
            ));
        }
    }
    let compares = item.above.is_some()
        || item.below.is_some()
        || item.is.is_some()
        || item.is_not.is_some()
        || item.below_setting.is_some();
    match place {
        Place::Evidence if !compares || item.goes.is_some() => Err(format!(
            "evidence `{}` needs a comparison and no direction",
            item.reading()
        )),
        Place::Expect if compares || item.goes.is_none() => Err(format!(
            "expectation `{}` needs a direction and no comparison",
            item.reading()
        )),
        _ => Ok(()),
    }
}

/// Holds the rules to the catalog and the settings table: every source
/// exists and serves the mode named, every setting is a row.
pub fn check_against(rules: &[Rule], catalog: &Catalog) -> Result<(), String> {
    for rule in rules {
        for item in rule.requested() {
            let Some(source) = &item.source else {
                continue;
            };
            let id: SourceId = source
                .parse()
                .map_err(|_| format!("advice rule `{}`: `{source}` is no source id", rule.id))?;
            let entry = catalog.get(&id).ok_or_else(|| {
                format!("advice rule `{}`: the catalog has no `{source}`", rule.id)
            })?;
            let mode: crate::request::Mode = item
                .mode
                .as_deref()
                .unwrap_or("")
                .parse()
                .map_err(|error| format!("advice rule `{}`: {error}", rule.id))?;
            if !entry.modes.contains(&mode.kind()) {
                return Err(format!(
                    "advice rule `{}`: `{source}` has no `{}` mode",
                    rule.id,
                    mode.kind()
                ));
            }
        }
        for setting in rule.settings() {
            if settings::find(setting).is_none() {
                return Err(format!(
                    "advice rule `{}`: `{setting}` is not in settings.yaml",
                    rule.id
                ));
            }
        }
    }
    Ok(())
}

// --- the question ---------------------------------------------------------------

/// `omn advise`'s question.
#[derive(Debug, Clone)]
pub struct Ask {
    pub objective: Objective,
    pub target: Target,
    pub duration: Option<Duration>,
}

const CONF_MACHINE: &str = "conf.machine";
const CONF_PROCESS: &str = "conf.process";
const CONF_CHANGED: &str = "conf.changed";

impl Ask {
    pub fn check(&self) -> Result<(), Refusal> {
        Method::Advise.require(Some(self.objective))?;
        match &self.target {
            Target::Launch(argv) if !argv.is_empty() => Ok(()),
            Target::Pid(pids) if pids.len() == 1 => match self.duration {
                Some(_) => Ok(()),
                None => Err(Refusal(
                    "`omn advise --pid` needs --duration: a process already running has no exit \
                     to end the run on"
                        .to_owned(),
                )),
            },
            other => Err(Refusal(format!(
                "`omn advise` answers for one workload -- `-- PROGRAM` or `--pid PID` -- and was \
                 given {other}"
            ))),
        }
    }

    /// The rules this objective is served by.
    pub fn rules(&self) -> Vec<&'static Rule> {
        rules()
            .iter()
            .filter(|rule| rule.objectives.contains(&self.objective))
            .collect()
    }

    /// The session: every measure the objective's rules read, once each,
    /// and the settings beside them.
    pub fn session(&self) -> Session {
        let mut wanted: BTreeMap<String, (&Item, bool)> = BTreeMap::new();
        for rule in self.rules() {
            for item in rule.requested().filter(|item| item.source.is_some()) {
                let entry = wanted.entry(item.label()).or_insert((item, false));
                entry.1 |= item.attr.is_some();
            }
        }
        let mut requests: Vec<Request> = wanted
            .into_iter()
            .map(|(label, (item, raw))| {
                let target = match item.target.as_deref() {
                    Some("system") => Target::System,
                    _ => self.target.clone(),
                };
                let mut emit = vec!["summary".parse().expect("an emit")];
                if raw {
                    emit.push("raw".parse().expect("an emit"));
                }
                self.request(
                    &label,
                    item.source.as_deref().unwrap_or_default(),
                    target,
                    item.mode.as_deref().unwrap_or_default(),
                    emit,
                )
            })
            .collect();
        let raw = || vec!["raw".parse().expect("an emit")];
        requests.push(self.request(
            CONF_MACHINE,
            CONF_MACHINE,
            Target::System,
            "snapshot",
            raw(),
        ));
        requests.push(self.request(
            CONF_PROCESS,
            CONF_PROCESS,
            self.target.clone(),
            "snapshot",
            raw(),
        ));
        requests.push(self.request(
            CONF_CHANGED,
            CONF_CHANGED,
            self.target.clone(),
            "poll@250ms",
            raw(),
        ));
        Session {
            name: Some(format!("omn advise --objective {}", self.objective)),
            duration: self.duration,
            budget: None,
            out: None,
            format: Default::default(),
            requests,
            joins: Vec::new(),
            decompose: Vec::new(),
            escalate: None,
            record: None,
            launch: Vec::new(),
        }
    }

    fn request(
        &self,
        label: &str,
        source: &str,
        target: Target,
        mode: &str,
        emit: Vec<crate::request::Emit>,
    ) -> Request {
        Request {
            source: source.parse().expect("a catalog source"),
            target,
            mode: mode.parse().expect("a mode"),
            emit,
            keys: Vec::new(),
            filter: None,
            duration: self.duration,
            budget: None,
            stack: None,
            symbols: None,
            backend: None,
            label: Some(label.to_owned()),
            window: None,
            value: None,
            sum: Vec::new(),
        }
    }
}

// --- the answer ---------------------------------------------------------------

/// One reading of the run, as a rule reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The source lost events: a total or a count is a lower bound.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lower_bound: bool,
    /// Why there is no value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
}

impl Measured {
    fn missing(why: impl Into<String>) -> Self {
        Self {
            number: None,
            text: None,
            lower_bound: false,
            missing: Some(why.into()),
        }
    }

    fn shown(&self) -> String {
        let value = match (&self.number, &self.text) {
            (Some(number), _) => trim_number(*number),
            (None, Some(text)) => text.clone(),
            (None, None) => {
                return format!(
                    "not measured ({})",
                    self.missing.as_deref().unwrap_or("no value")
                )
            }
        };
        if self.lower_bound {
            format!("at least {value} (events were lost)")
        } else {
            value
        }
    }
}

/// A setting that differs between two reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub setting: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub before: String,
    pub after: String,
}

/// A rule that held.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Advice {
    pub rule: String,
    pub summary: String,
    /// Each item of evidence, with the reading that satisfied it.
    pub evidence: Vec<String>,
    pub mechanism: String,
    pub docs: String,
    pub setting: String,
    /// The setting as it reads now, per instance.
    pub current: Vec<String>,
    pub expect: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also: Vec<String>,
    pub scope: String,
    pub change: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
}

/// A rule that could not decide, and what it lacked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Undecided {
    pub rule: String,
    pub lacking: Vec<String>,
}

/// One rule's prediction, checked against a run after the change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub rule: String,
    pub reading: String,
    pub predicted: Goes,
    pub before: Measured,
    pub after: Measured,
    pub verdict: Verdict,
    /// An expected side effect rather than the rule's prediction.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub side_effect: bool,
    /// Whether the rule's setting differs between the two runs, as far as
    /// this run reads it. A process setting is read on the target only, and
    /// the process the change was made to may be another one.
    pub setting_changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    AsPredicted,
    NotAsPredicted,
    NotMeasured,
    /// The reading moved, and the rule's setting did not: the move is not
    /// this rule's to claim.
    SettingUnchanged,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::AsPredicted => "moved as predicted",
            Verdict::NotAsPredicted => "did not move as predicted",
            Verdict::NotMeasured => "not measured on both sides",
            Verdict::SettingUnchanged => {
                "its setting is the same in both runs, so whatever moved this reading, this rule \
                 did not"
            }
        }
    }
}

/// The comparison with the answer before the change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verification {
    /// What the person changed: the settings that differ between the two
    /// runs.
    pub settings_changed: Vec<Change>,
    pub checks: Vec<Check>,
}

/// What `omn advise` says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub objective: Objective,
    pub target: String,
    /// Settings that changed while the run was going: the run straddles two
    /// configurations, and no rule that reads one of them fires.
    pub changed_during_run: Vec<Change>,
    pub advice: Vec<Advice>,
    pub undecided: Vec<Undecided>,
    /// The rules evaluated that did not hold.
    pub held_back: Vec<String>,
    /// Every reading a rule asked for, by name: what `--against` compares.
    pub measures: BTreeMap<String, Measured>,
    /// Every setting read.
    pub settings: Vec<Reading>,
    pub dropped: Vec<Dropped>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_failed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
}

/// Asks the question: runs the session, reads the rules off it.
pub fn answer(
    catalog: &Catalog,
    capabilities: &Capabilities,
    ask: &Ask,
) -> Result<Answer, TriageError> {
    ask.check()?;
    let mut session = ask.session();
    let optional: Vec<String> = session
        .requests
        .iter()
        .filter_map(|request| request.label.clone())
        .filter(|label| label != CONF_PROCESS && label != CONF_MACHINE)
        .collect();
    let optional: Vec<&str> = optional.iter().map(String::as_str).collect();
    let dropped = engine::prune(catalog, capabilities, &mut session, &optional)?;
    let ran = engine::run(catalog, capabilities, &session)?;
    let mut answer = read(ask, &ran.run, dropped);
    answer.failures = ran.failures;
    answer.program_failed = engine::program_failure(&ran.run);
    Ok(answer)
}

/// The settings a run read, by setting id.
fn settings_of(run: &Run) -> Vec<Reading> {
    let mut readings = Vec::new();
    for source in [CONF_MACHINE, CONF_PROCESS] {
        if let Some(results) = results_of(run, source) {
            readings.extend(raw_events(run, &results.label).filter_map(conf::reading_of));
        }
    }
    readings
}

fn changed_of(run: &Run) -> Vec<Change> {
    let Some(results) = results_of(run, CONF_CHANGED) else {
        return Vec::new();
    };
    raw_events(run, &results.label)
        .filter(|event| text_attr(event, "setting").as_deref() != Some("watched"))
        .filter_map(|event| {
            Some(Change {
                setting: text_attr(event, "setting")?,
                instance: text_attr(event, "instance"),
                before: text_attr(event, "before")?,
                after: text_attr(event, "after")?,
            })
        })
        .collect()
}

fn raw_events<'a>(run: &'a Run, label: &str) -> impl Iterator<Item = &'a Event> + 'a {
    run.result(label)
        .into_iter()
        .flat_map(|results| results.outputs.iter())
        .filter_map(|output| match output {
            Output::Raw { events } => Some(events.iter()),
            _ => None,
        })
        .flatten()
}

fn text_attr(event: &Event, name: &str) -> Option<String> {
    event.attr(name).map(|value| match value {
        AttrValue::Text(text) => text.clone(),
        other => other.to_string(),
    })
}

/// One measure, read off the run; a ratio when it names what it is per.
fn measure(run: &Run, item: &Item, dropped: &[Dropped]) -> Measured {
    let Some(per) = &item.per else {
        return measure_one(run, item, dropped);
    };
    let mut own = item.clone();
    own.per = None;
    let (top, bottom) = (
        measure_one(run, &own, dropped),
        measure_one(run, per, dropped),
    );
    match (top.number, bottom.number) {
        // A ratio of two counts either of which lost events is no bound
        // either way: neither `above` nor `below` can be decided on it.
        (Some(_), Some(_)) if top.lower_bound || bottom.lower_bound => Measured::missing(
            "events were lost on one side of the ratio, so it bounds nothing".to_owned(),
        ),
        (Some(top_number), Some(bottom_number)) if bottom_number > 0.0 => Measured {
            number: Some(top_number / bottom_number),
            text: None,
            lower_bound: false,
            missing: None,
        },
        (_, Some(_)) => top.missing.map_or_else(
            || {
                Measured::missing(format!(
                    "{} is zero, and nothing is per nothing",
                    per.reading()
                ))
            },
            Measured::missing,
        ),
        _ => Measured::missing(
            bottom
                .missing
                .unwrap_or_else(|| format!("{} has no value", per.reading())),
        ),
    }
}

fn measure_one(run: &Run, item: &Item, dropped: &[Dropped]) -> Measured {
    if item.run.is_some() {
        let lifetime = run
            .output
            .results
            .iter()
            .find_map(|results| results.lifetime.as_ref());
        return match lifetime {
            Some(life) if life.exited_ns > life.released_ns => Measured {
                number: Some((life.exited_ns - life.released_ns) as f64),
                text: None,
                lower_bound: false,
                missing: None,
            },
            _ => Measured::missing("the run launched no program, so it has no wall clock"),
        };
    }
    let label = item.label();
    let Some(results) = results_for(run, item) else {
        let why = dropped
            .iter()
            .find(|one| one.label == label)
            .map(|one| one.reason.clone())
            .unwrap_or_else(|| "not in the run".to_owned());
        return Measured::missing(why);
    };
    let lower_bound = Shortfall::of(&results.losses, results.events_seen as u64, false).is_some();
    let stat = item.stat.unwrap_or(Stat::Total);
    let mut measured = match &item.attr {
        Some(attr) => {
            let events: Vec<&Event> = raw_events(run, &results.label)
                .filter(|event| event.attr(attr).is_some())
                .collect();
            reduce_events(&events, attr, stat)
        }
        None => reduce_summary(results, stat),
    };
    measured.lower_bound = lower_bound && matches!(stat, Stat::Total | Stat::Count | Stat::Max);
    measured
}

/// The results a measure reads: the request of that label, which is how
/// `omn advise` asks; or, in a session another method wrote, the request of
/// that source, mode and target, whatever its label.
fn results_for<'a>(run: &'a Run, item: &Item) -> Option<&'a crate::emit::Results> {
    if let Some(results) = run.result(&item.label()) {
        return Some(results);
    }
    let system = item.target.as_deref() == Some("system");
    let index = run.served.iter().position(|served| {
        Some(served.request.source.as_str()) == item.source.as_deref()
            && Some(served.request.mode.to_string().as_str()) == item.mode.as_deref()
            && matches!(served.request.target, Target::System) == system
    })?;
    run.output.results.get(index)
}

/// The results of the first request of one source, whatever its label.
fn results_of<'a>(run: &'a Run, source: &str) -> Option<&'a crate::emit::Results> {
    run.output
        .results
        .iter()
        .find(|results| results.source == source)
}

fn reduce_events(events: &[&Event], attr: &str, stat: Stat) -> Measured {
    if stat == Stat::Count {
        return Measured {
            number: Some(events.len() as f64),
            ..Measured::missing("")
        }
        .found();
    }
    if events.is_empty() {
        return Measured::missing(format!("no event carried `{attr}`"));
    }
    if matches!(stat, Stat::First | Stat::Last) {
        let picked = if stat == Stat::First {
            events.first()
        } else {
            events.last()
        };
        let last = picked.and_then(|event| event.attr(attr));
        return match last {
            Some(AttrValue::Text(text)) => Measured {
                text: Some(text.clone()),
                ..Measured::missing("")
            }
            .found(),
            Some(value) => Measured {
                number: value.as_f64(),
                ..Measured::missing("")
            }
            .found(),
            None => Measured::missing(format!("no event carried `{attr}`")),
        };
    }
    let values: Vec<f64> = events
        .iter()
        .filter_map(|event| event.attr(attr).and_then(AttrValue::as_f64))
        .collect();
    if values.is_empty() {
        return Measured::missing(format!("`{attr}` is not a number"));
    }
    let number = match stat {
        Stat::Total => values.iter().sum(),
        Stat::Mean => values.iter().sum::<f64>() / values.len() as f64,
        Stat::Max => values.iter().copied().fold(f64::MIN, f64::max),
        Stat::Min => values.iter().copied().fold(f64::MAX, f64::min),
        Stat::Count | Stat::First | Stat::Last => unreachable!("answered above"),
    };
    Measured {
        number: Some(number),
        ..Measured::missing("")
    }
    .found()
}

impl Measured {
    fn found(mut self) -> Self {
        self.missing = None;
        self
    }
}

/// A summary's series -- or a histogram's, for a source asked for one --
/// merged into one number. A source that ran and saw nothing has a count
/// and a total of zero, and no mean.
fn reduce_summary(results: &crate::emit::Results, stat: Stat) -> Measured {
    // (count, total, min, max, last) of each series.
    let mut series: Vec<(usize, f64, f64, f64, Option<f64>)> = Vec::new();
    for output in &results.outputs {
        match output {
            Output::Summary { series: list } => series.extend(list.iter().map(|one| {
                (
                    one.count,
                    one.total.unwrap_or(one.mean * one.count as f64),
                    one.min,
                    one.max,
                    Some(one.last),
                )
            })),
            Output::Hist { series: list, .. } => series.extend(
                list.iter()
                    .map(|one| (one.count, one.sum, one.min, one.max, None)),
            ),
            _ => {}
        }
    }
    let count: usize = series.iter().map(|one| one.0).sum();
    let total: f64 = series.iter().map(|one| one.1).sum();
    let number = match stat {
        Stat::Count => Some(count as f64),
        Stat::Total => Some(total),
        _ if count == 0 => None,
        Stat::Mean => Some(total / count as f64),
        Stat::Max => series.iter().map(|one| one.3).reduce(f64::max),
        Stat::Min => series.iter().map(|one| one.2).reduce(f64::min),
        Stat::Last => series.last().and_then(|one| one.4),
        // A summary keeps no first value: a rule names an `attr` for it.
        Stat::First => None,
    };
    match number {
        Some(number) => Measured {
            number: Some(number),
            ..Measured::missing("")
        }
        .found(),
        None => Measured::missing("the source saw nothing to reduce"),
    }
}

/// Whether one item holds: `Some(true)`, `Some(false)`, or `None` with why.
fn holds(
    item: &Item,
    measures: &BTreeMap<String, Measured>,
    readings: &BTreeMap<&str, Vec<&Reading>>,
) -> Result<(bool, String), String> {
    if item.is_measure() {
        let measured = measures
            .get(&item.reading())
            .ok_or_else(|| format!("{}: not measured", item.reading()))?;
        if let Some(why) = &measured.missing {
            return Err(format!("{}: {why}", item.reading()));
        }
        let number = measured.number;
        let text = measured.text.as_deref();
        let shown = format!("{} = {}", item.reading(), measured.shown());
        if let Some(above) = item.above {
            let Some(number) = number else {
                return Err(format!("{}: not a number", item.reading()));
            };
            if number <= above {
                return Ok((false, shown));
            }
        }
        if let Some(below) = item.below {
            let Some(number) = number else {
                return Err(format!("{}: not a number", item.reading()));
            };
            if measured.lower_bound {
                return Err(format!(
                    "{}: a lower bound ({}) cannot say it is below {}",
                    item.reading(),
                    measured.shown(),
                    trim_number(below)
                ));
            }
            if number >= below {
                return Ok((false, shown));
            }
        }
        if let Some(is) = &item.is {
            if !is.matches(number, text) {
                return Ok((false, shown));
            }
        }
        if let Some(is_not) = &item.is_not {
            if is_not.matches(number, text) {
                return Ok((false, shown));
            }
        }
        return Ok((true, format!("{shown}, {}", item.comparison())));
    }
    let id = item.setting.as_deref().unwrap_or_default();
    let found = readings.get(id).cloned().unwrap_or_default();
    let set: Vec<&Reading> = found
        .iter()
        .copied()
        .filter(|reading| reading.state == State::Set)
        .collect();
    if set.is_empty() {
        let why = found
            .first()
            .map(|reading| {
                format!(
                    "{} ({})",
                    reading.state.as_str(),
                    reading.reason.as_deref().unwrap_or("no reason given")
                )
            })
            .unwrap_or_else(|| "not read".to_owned());
        return Err(format!("{id}: {why}"));
    }
    let other = match &item.below_setting {
        Some(other_id) => {
            let other = readings
                .get(other_id.as_str())
                .and_then(|list| list.iter().find(|reading| reading.state == State::Set))
                .and_then(|reading| reading.number());
            Some(other.ok_or_else(|| format!("{other_id}: not read as a number"))?)
        }
        None => None,
    };
    let holds_for = |reading: &&Reading| {
        let number = reading.number();
        let text = reading.effective();
        item.above
            .is_none_or(|above| number.is_some_and(|n| n > above))
            && item
                .below
                .is_none_or(|below| number.is_some_and(|n| n < below))
            && item.is.as_ref().is_none_or(|is| is.matches(number, text))
            && item
                .is_not
                .as_ref()
                .is_none_or(|is_not| !is_not.matches(number, text))
            && other.is_none_or(|other| number.is_some_and(|n| n < other))
    };
    if item.every {
        return Ok(match set.iter().find(|reading| !holds_for(reading)) {
            Some(against) => (
                false,
                format!(
                    "{id}{} = {}",
                    against
                        .instance
                        .as_ref()
                        .map(|instance| format!(" [{instance}]"))
                        .unwrap_or_default(),
                    against.effective().unwrap_or("")
                ),
            ),
            None => (
                true,
                format!("{id}, every one of {}: {}", set.len(), item.comparison()),
            ),
        });
    }
    // A setting with several instances holds where any instance does.
    for reading in &set {
        let number = reading.number();
        let text = reading.effective();
        let ok = item
            .above
            .is_none_or(|above| number.is_some_and(|n| n > above))
            && item
                .below
                .is_none_or(|below| number.is_some_and(|n| n < below))
            && item.is.as_ref().is_none_or(|is| is.matches(number, text))
            && item
                .is_not
                .as_ref()
                .is_none_or(|is_not| !is_not.matches(number, text))
            && other.is_none_or(|other| number.is_some_and(|n| n < other));
        if ok {
            let at = reading
                .instance
                .as_ref()
                .map(|instance| format!(" [{instance}]"))
                .unwrap_or_default();
            let bound = match (&item.below_setting, other) {
                (Some(other_id), Some(value)) => format!(" ({other_id} = {})", trim_number(value)),
                _ => String::new(),
            };
            return Ok((
                true,
                format!(
                    "{id}{at} = {}, {}{bound}",
                    text.unwrap_or(""),
                    item.comparison()
                ),
            ));
        }
    }
    Ok((
        false,
        format!("{id} = {}", set[0].effective().unwrap_or("")),
    ))
}

/// Reads the rules off a run.
pub fn read(ask: &Ask, run: &Run, dropped: Vec<Dropped>) -> Answer {
    let rules = ask.rules();
    let settings = settings_of(run);
    let changed_during_run = changed_of(run);
    let mut measures = BTreeMap::new();
    for rule in &rules {
        for item in rule.measures() {
            measures
                .entry(item.reading())
                .or_insert_with(|| measure(run, item, &dropped));
        }
    }
    let (advice, undecided, held_back) =
        evaluate(&rules, &measures, &settings, &changed_during_run);
    Answer {
        objective: ask.objective,
        target: ask.target.to_string(),
        changed_during_run,
        advice,
        undecided,
        held_back,
        measures,
        settings,
        dropped,
        failures: Vec::new(),
        program_failed: None,
        verification: None,
    }
}

/// The labels another method gives the settings it reads beside its own
/// measures.
pub const SETTINGS_LABELS: [&str; 2] = ["settings of the machine", "settings of the workload"];

/// The two requests that read the settings, for a method that asks its own
/// questions and wants the rules that bear on them: free, and optional.
pub fn settings_requests(target: &Target, duration: Option<Duration>) -> Vec<Request> {
    let ask = Ask {
        objective: Objective::Latency,
        target: target.clone(),
        duration,
    };
    let raw = || vec!["raw".parse().expect("an emit")];
    vec![
        ask.request(
            SETTINGS_LABELS[0],
            CONF_MACHINE,
            Target::System,
            "snapshot",
            raw(),
        ),
        ask.request(
            SETTINGS_LABELS[1],
            CONF_PROCESS,
            target.clone(),
            "snapshot",
            raw(),
        ),
    ]
}

/// What the rules say about a run another method asked for: the rules of
/// `objective` whose every measured piece of evidence that run measured. A
/// rule whose evidence it did not ask for is left out, not undecided: the
/// question was not this one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bearing {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<Advice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undecided: Vec<Undecided>,
}

impl Bearing {
    pub fn is_empty(&self) -> bool {
        self.advice.is_empty() && self.undecided.is_empty()
    }

    /// The section a method's answer prints.
    pub fn text(&self) -> String {
        let mut out = String::new();
        if self.is_empty() {
            return out;
        }
        let _ = writeln!(
            out,
            "\nsettings that bear on this (advice: nothing was changed):"
        );
        for advice in &self.advice {
            let _ = writeln!(out, "  {}: {}", advice.rule, advice.summary);
            for line in &advice.evidence {
                let _ = writeln!(out, "    measured: {line}");
            }
            for current in &advice.current {
                let _ = writeln!(out, "    {} now {current}", advice.setting);
            }
            let _ = writeln!(
                out,
                "    change ({} scope): {}",
                advice.scope, advice.change
            );
            let _ = writeln!(out, "    expected: {}", advice.expect);
        }
        for one in &self.undecided {
            let _ = writeln!(
                out,
                "  {} could not decide: {}",
                one.rule,
                one.lacking.join("; ")
            );
        }
        let _ = writeln!(
            out,
            "  `omn advise --objective <same>` asks every rule, with --save and --against to check a change"
        );
        out
    }
}

pub fn bearing(objective: Objective, run: &Run) -> Bearing {
    if results_of(run, CONF_PROCESS).is_none() && results_of(run, CONF_MACHINE).is_none() {
        return Bearing::default();
    }
    let rules: Vec<&Rule> = rules()
        .iter()
        .filter(|rule| rule.objectives.contains(&objective))
        .filter(|rule| {
            rule.evidence
                .iter()
                .filter(|item| item.source.is_some())
                .all(|item| results_for(run, item).is_some())
        })
        .collect();
    let mut measures = BTreeMap::new();
    for rule in &rules {
        for item in rule.measures() {
            measures
                .entry(item.reading())
                .or_insert_with(|| measure(run, item, &[]));
        }
    }
    let (advice, undecided, _) = evaluate(&rules, &measures, &settings_of(run), &changed_of(run));
    Bearing { advice, undecided }
}

/// Evaluates rules on readings already taken: what `omn advise` does, and
/// what `omn latency` and `omn saturate` do with the readings they have.
pub fn evaluate(
    rules: &[&Rule],
    measures: &BTreeMap<String, Measured>,
    settings: &[Reading],
    changed: &[Change],
) -> (Vec<Advice>, Vec<Undecided>, Vec<String>) {
    let mut readings: BTreeMap<&str, Vec<&Reading>> = BTreeMap::new();
    for reading in settings {
        readings
            .entry(reading.setting.as_str())
            .or_default()
            .push(reading);
    }
    let changed_ids: BTreeSet<&str> = changed
        .iter()
        .map(|change| change.setting.as_str())
        .collect();
    let (mut advice, mut undecided, mut held_back) = (Vec::new(), Vec::new(), Vec::new());
    for rule in rules {
        let moved: Vec<&str> = rule
            .settings()
            .into_iter()
            .filter(|id| changed_ids.contains(id))
            .collect();
        if !moved.is_empty() {
            undecided.push(Undecided {
                rule: rule.id.clone(),
                lacking: moved
                    .iter()
                    .map(|id| {
                        format!("{id} changed during the run, so the run straddles two settings")
                    })
                    .collect(),
            });
            continue;
        }
        let mut lacking = Vec::new();
        let mut lines = Vec::new();
        let mut failed = false;
        for item in &rule.evidence {
            match holds(item, measures, &readings) {
                Ok((true, line)) => lines.push(line),
                Ok((false, _)) => failed = true,
                Err(why) => lacking.push(why),
            }
        }
        if failed {
            held_back.push(rule.id.clone());
            continue;
        }
        if !lacking.is_empty() {
            undecided.push(Undecided {
                rule: rule.id.clone(),
                lacking,
            });
            continue;
        }
        let current = readings
            .get(rule.setting.as_str())
            .map(|list| {
                list.iter()
                    .map(|reading| {
                        let at = reading
                            .instance
                            .as_ref()
                            .map(|instance| format!("[{instance}] "))
                            .unwrap_or_default();
                        let value = match (reading.state, reading.effective()) {
                            (State::Set, Some(value)) => value.to_owned(),
                            (state, _) => format!(
                                "{} ({})",
                                state.as_str(),
                                reading.reason.as_deref().unwrap_or("")
                            ),
                        };
                        let binding = reading
                            .binding
                            .as_ref()
                            .map(|level| format!(", bound by {level}"))
                            .unwrap_or_default();
                        format!("{at}{value}  ({}{binding})", reading.read_from)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let predicted = |item: &Item| {
            let now = measures
                .get(&item.reading())
                .map(Measured::shown)
                .unwrap_or_else(|| "not measured".to_owned());
            let goes = match item.goes {
                Some(Goes::Down) => "go down",
                Some(Goes::Up) => "go up",
                Some(Goes::Changes) => "change",
                None => "move",
            };
            format!("{} should {goes} (now {now})", item.reading())
        };
        advice.push(Advice {
            rule: rule.id.clone(),
            summary: rule.summary.clone(),
            evidence: lines,
            mechanism: rule.mechanism.clone(),
            docs: rule.docs.clone(),
            setting: rule.setting.clone(),
            current,
            expect: predicted(&rule.expect),
            also: rule.also.iter().map(predicted).collect(),
            scope: rule.scope.clone(),
            change: rule.change.clone(),
            caveats: rule.caveats.clone(),
        });
    }
    (advice, undecided, held_back)
}

/// Compares this answer with the one before a change, on what each rule
/// that held before predicted.
pub fn verify(before: &Answer, after: &Answer) -> Verification {
    let mut settings_changed = Vec::new();
    let index = |answer: &Answer| -> BTreeMap<(String, Option<String>), String> {
        answer
            .settings
            .iter()
            .filter(|reading| !settings::is_computed(&reading.setting))
            .map(|reading| {
                (
                    reading.key(),
                    reading
                        .effective()
                        .map(str::to_owned)
                        .unwrap_or_else(|| reading.state.as_str().to_owned()),
                )
            })
            .collect()
    };
    let (old, new) = (index(before), index(after));
    for (key, value) in &old {
        if let Some(now) = new.get(key) {
            // A thread is a new thread in a new run: per-thread instances are
            // tids, and only their values compare.
            if now != value {
                settings_changed.push(Change {
                    setting: key.0.clone(),
                    instance: key.1.clone(),
                    before: value.clone(),
                    after: now.clone(),
                });
            }
        }
    }
    let mut checks = Vec::new();
    for advice in &before.advice {
        let Some(rule) = rules().iter().find(|rule| rule.id == advice.rule) else {
            continue;
        };
        let items =
            std::iter::once((&rule.expect, false)).chain(rule.also.iter().map(|item| (item, true)));
        for (item, side_effect) in items {
            let reading = item.reading();
            let was = before
                .measures
                .get(&reading)
                .cloned()
                .unwrap_or_else(|| Measured::missing("not in the answer before"));
            let now = after
                .measures
                .get(&reading)
                .cloned()
                .unwrap_or_else(|| Measured::missing("not in this answer"));
            let predicted = item.goes.unwrap_or(Goes::Changes);
            let setting_changed = settings_changed
                .iter()
                .any(|change| change.setting == rule.setting);
            let mut verdict = moved(&was, &now, predicted);
            // A machine or cgroup setting is read whole; one that did not
            // change did not move anything. A process setting is the
            // target's, and a change made to another process of the workload
            // (the one to sacrifice) is not seen, so it is said, not refused.
            if !setting_changed && rule.scope != "process" && verdict != Verdict::NotMeasured {
                verdict = Verdict::SettingUnchanged;
            }
            checks.push(Check {
                rule: rule.id.clone(),
                reading,
                predicted,
                before: was,
                after: now,
                verdict,
                side_effect,
                setting_changed,
            });
        }
    }
    Verification {
        settings_changed,
        checks,
    }
}

fn moved(before: &Measured, after: &Measured, goes: Goes) -> Verdict {
    match (goes, before.number, after.number) {
        (Goes::Down, Some(was), Some(now)) => verdict(now < was),
        (Goes::Up, Some(was), Some(now)) => verdict(now > was),
        (Goes::Changes, Some(was), Some(now)) => verdict((now - was).abs() > f64::EPSILON),
        (Goes::Changes, None, None) => match (&before.text, &after.text) {
            (Some(was), Some(now)) => verdict(was != now),
            _ => Verdict::NotMeasured,
        },
        _ => Verdict::NotMeasured,
    }
}

fn verdict(as_predicted: bool) -> Verdict {
    if as_predicted {
        Verdict::AsPredicted
    } else {
        Verdict::NotAsPredicted
    }
}

fn trim_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{}", number as i64)
    } else {
        format!("{number:.3}")
    }
}

impl Answer {
    /// Whether every check of a verification moved as its rule predicted.
    pub fn verified(&self) -> Option<bool> {
        let verification = self.verification.as_ref()?;
        let predictions: Vec<&Check> = verification
            .checks
            .iter()
            .filter(|check| !check.side_effect)
            .collect();
        if predictions.is_empty() {
            return None;
        }
        Some(
            predictions
                .iter()
                .all(|check| check.verdict == Verdict::AsPredicted),
        )
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "omn advise --objective {}  ({})",
            self.objective, self.target
        );
        let _ = writeln!(
            out,
            "nothing here was changed: each setting below is advice, with the command a person \
             would type and how to check it"
        );
        if let Some(failed) = &self.program_failed {
            let _ = writeln!(
                out,
                "\nthe program {failed}: the readings are of a run that failed"
            );
        }
        for failure in &self.failures {
            let _ = writeln!(out, "\nthe measurement broke the program: {failure}");
        }
        if !self.changed_during_run.is_empty() {
            let _ = writeln!(
                out,
                "\nsettings that changed during the run (no rule reading them fires):"
            );
            for change in &self.changed_during_run {
                let _ = writeln!(
                    out,
                    "  {}  {} -> {}",
                    name(change),
                    change.before,
                    change.after
                );
            }
        }
        if let Some(verification) = &self.verification {
            let _ = writeln!(out, "\nagainst the answer before:");
            if verification.settings_changed.is_empty() {
                let _ = writeln!(
                    out,
                    "  no setting differs between the two runs: whatever moved, no setting \
                     this table reads moved it"
                );
            } else {
                let _ = writeln!(out, "  settings that differ:");
                for change in &verification.settings_changed {
                    let _ = writeln!(
                        out,
                        "    {}  {} -> {}",
                        name(change),
                        change.before,
                        change.after
                    );
                }
            }
            for check in &verification.checks {
                let unseen = if !check.setting_changed && check.verdict != Verdict::SettingUnchanged
                {
                    " (the target's own setting reads the same: the change was elsewhere, or not made)"
                } else {
                    ""
                };
                let _ = writeln!(
                    out,
                    "  {}{}: {} {} -> {}: {}{unseen}",
                    check.rule,
                    if check.side_effect {
                        " (side effect)"
                    } else {
                        ""
                    },
                    check.reading,
                    check.before.shown(),
                    check.after.shown(),
                    check.verdict.as_str()
                );
            }
        }
        if self.advice.is_empty() {
            let _ = writeln!(out, "\nno rule for `{}` held on this run", self.objective);
        }
        for advice in &self.advice {
            let _ = writeln!(out, "\n{}: {}", advice.rule, advice.summary);
            let _ = writeln!(out, "  measured:");
            for line in &advice.evidence {
                let _ = writeln!(out, "    {line}");
            }
            let _ = writeln!(out, "  why: {}", advice.mechanism);
            let _ = writeln!(out, "       (Documentation/{})", advice.docs);
            let _ = writeln!(out, "  setting: {}", advice.setting);
            for current in &advice.current {
                let _ = writeln!(out, "    now {current}");
            }
            let _ = writeln!(out, "  expected: {}", advice.expect);
            for also in &advice.also {
                let _ = writeln!(out, "    and {also}");
            }
            let _ = writeln!(
                out,
                "  change ({} scope, not done): {}",
                advice.scope, advice.change
            );
            let _ = writeln!(
                out,
                "  check: save this answer (--save before.json), make the change, and run the \
                 same command with --against before.json"
            );
            for caveat in &advice.caveats {
                let _ = writeln!(out, "  caveat: {caveat}");
            }
        }
        if !self.undecided.is_empty() {
            let _ = writeln!(out, "\nrules that could not decide:");
            for one in &self.undecided {
                let _ = writeln!(out, "  {}: {}", one.rule, one.lacking.join("; "));
            }
        }
        for dropped in &self.dropped {
            let _ = writeln!(
                out,
                "not measured: {} ({}): {}",
                dropped.label, dropped.source, dropped.reason
            );
        }
        let _ = writeln!(
            out,
            "\n{} rule(s) for `{}` evaluated: {} held, {} could not decide, {} did not hold",
            self.advice.len() + self.undecided.len() + self.held_back.len(),
            self.objective,
            self.advice.len(),
            self.undecided.len(),
            self.held_back.len()
        );
        out
    }
}

fn name(change: &Change) -> String {
    match &change.instance {
        Some(instance) => format!("{} [{instance}]", change.setting),
        None => change.setting.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_load_and_name_only_what_the_catalog_and_the_settings_have() {
        let rules = rules();
        assert!(rules.len() >= 4, "{}", rules.len());
        check_against(rules, &Catalog::embedded().expect("the catalog")).expect("the rules");
    }

    #[test]
    fn a_rule_without_a_measurement_a_truth_or_a_direction_is_refused() {
        let base = |evidence: &str, expect: &str, truth: &str| {
            format!(
                "- id: x\n  summary: s\n  objectives: [latency]\n  evidence: [{evidence}]\n  \
                 setting: vm.swappiness\n  mechanism: m\n  docs: d\n  expect: {expect}\n  \
                 scope: machine\n  change: c\n  truth: '{truth}'\n"
            )
        };
        let measure = "{source: mem.oom, mode: count, stat: total, above: 0}";
        let expect = "{source: mem.oom, mode: count, stat: total, goes: down}";
        assert!(load(&base(measure, expect, "t")).is_ok());
        let setting_only = "{setting: vm.swappiness, is: 60}";
        assert!(load(&base(setting_only, expect, "t"))
            .unwrap_err()
            .contains("no measured evidence"));
        assert!(load(&base(measure, expect, ""))
            .unwrap_err()
            .contains("ground-truth"));
        let no_direction = "{source: mem.oom, mode: count, stat: total}";
        assert!(load(&base(measure, no_direction, "t"))
            .unwrap_err()
            .contains("needs a direction"));
        let uncompared = "{source: mem.oom, mode: count, stat: total}";
        assert!(load(&base(uncompared, expect, "t"))
            .unwrap_err()
            .contains("needs a comparison"));
    }

    fn reading(setting: &str, value: &str) -> Reading {
        Reading {
            setting: setting.to_owned(),
            scope: settings::find(setting).unwrap().scope,
            instance: None,
            state: State::Set,
            value: Some(value.to_owned()),
            binding: None,
            binding_value: None,
            hard: None,
            read_from: "test".to_owned(),
            reason: None,
        }
    }

    fn number(value: f64) -> Measured {
        Measured {
            number: Some(value),
            text: None,
            lower_bound: false,
            missing: None,
        }
    }

    fn rule(id: &str) -> &'static Rule {
        rules().iter().find(|rule| rule.id == id).unwrap()
    }

    #[test]
    fn a_rule_holds_on_its_evidence_and_says_which_readings_held_it() {
        let rule = rule("cgroup.memory.swap-denied");
        let mut measures = BTreeMap::new();
        measures.insert(rule.evidence[0].reading(), number(1.0));
        let settings = vec![
            reading("cgroup.memory.swap.max", "0"),
            reading("mm.swap_total", "4194300"),
        ];
        let (advice, undecided, held) = evaluate(&[rule], &measures, &settings, &[]);
        assert_eq!((advice.len(), undecided.len(), held.len()), (1, 0, 0));
        assert!(
            advice[0].evidence[0].contains("= 1"),
            "{:?}",
            advice[0].evidence
        );
        assert!(
            advice[0].current[0].starts_with('0'),
            "{:?}",
            advice[0].current
        );

        // No kill: the rule does not hold.
        measures.insert(rule.evidence[0].reading(), number(0.0));
        let (advice, _, held) = evaluate(&[rule], &measures, &settings, &[]);
        assert!(advice.is_empty());
        assert_eq!(held, vec![rule.id.clone()]);
    }

    #[test]
    fn a_setting_that_cannot_be_read_or_changed_during_the_run_decides_nothing() {
        let rule = rule("cgroup.memory.swap-denied");
        let mut measures = BTreeMap::new();
        measures.insert(rule.evidence[0].reading(), number(2.0));
        let mut unknown = reading("cgroup.memory.swap.max", "0");
        unknown.state = State::Unknown;
        unknown.value = None;
        unknown.reason = Some("not readable".to_owned());
        let settings = vec![reading("mm.swap_total", "4194300"), unknown];
        let (advice, undecided, _) = evaluate(&[rule], &measures, &settings, &[]);
        assert!(advice.is_empty());
        assert!(undecided[0].lacking[0].contains("unknown"), "{undecided:?}");

        let settings = vec![
            reading("cgroup.memory.swap.max", "0"),
            reading("mm.swap_total", "4194300"),
        ];
        let changed = vec![Change {
            setting: "cgroup.memory.swap.max".to_owned(),
            instance: None,
            before: "0".to_owned(),
            after: "max".to_owned(),
        }];
        let (advice, undecided, _) = evaluate(&[rule], &measures, &settings, &changed);
        assert!(advice.is_empty());
        assert!(undecided[0].lacking[0].contains("changed during the run"));
    }

    #[test]
    fn a_lower_bound_cannot_say_a_reading_is_below_anything() {
        let rules = load(
            "- id: x\n  summary: s\n  objectives: [latency]\n  evidence:\n    - {source: mem.oom, \
             mode: count, stat: total, below: 1}\n  setting: vm.swappiness\n  mechanism: m\n  \
             docs: d\n  expect: {source: mem.oom, mode: count, stat: total, goes: down}\n  \
             scope: machine\n  change: c\n  truth: t\n",
        )
        .unwrap();
        let mut measures = BTreeMap::new();
        let mut lost = number(0.0);
        lost.lower_bound = true;
        measures.insert(rules[0].evidence[0].reading(), lost);
        let (advice, undecided, _) = evaluate(&[&rules[0]], &measures, &[], &[]);
        assert!(advice.is_empty());
        assert!(
            undecided[0].lacking[0].contains("lower bound"),
            "{undecided:?}"
        );
    }

    #[test]
    fn a_ratio_reads_requests_per_miss_and_a_rule_holds_on_it() {
        let rule = rule("bdi.readahead-off");
        let ratio = rule
            .evidence
            .iter()
            .find(|item| item.per.is_some())
            .expect("the rule reads a ratio");
        assert!(
            ratio.reading().contains(" per io.pagecache trace total"),
            "{}",
            ratio.reading()
        );
        let mut measures = BTreeMap::new();
        measures.insert(rule.evidence[0].reading(), number(8192.0));
        measures.insert(ratio.reading(), number(1.0));
        let settings = vec![reading("bdi.read_ahead_kb", "0")];
        let (advice, _, held) = evaluate(&[rule], &measures, &settings, &[]);
        assert_eq!(advice.len(), 1, "{held:?}");
        measures.insert(ratio.reading(), number(306.0 / 9728.0));
        let (advice, _, held) = evaluate(&[rule], &measures, &settings, &[]);
        assert!(advice.is_empty());
        assert_eq!(held.len(), 1);
    }

    #[test]
    fn the_check_compares_what_each_rule_predicted() {
        let rule = rule("cgroup.memory.swap-denied");
        let reading_name = rule.expect.reading();
        let mut before = Answer {
            objective: Objective::Latency,
            target: "x".to_owned(),
            changed_during_run: Vec::new(),
            advice: Vec::new(),
            undecided: Vec::new(),
            held_back: Vec::new(),
            measures: BTreeMap::from([(reading_name.clone(), number(1.0))]),
            settings: vec![reading("cgroup.memory.swap.max", "0")],
            dropped: Vec::new(),
            failures: Vec::new(),
            program_failed: None,
            verification: None,
        };
        let mut measures = before.measures.clone();
        measures.insert(rule.evidence[0].reading(), number(1.0));
        let settings = vec![
            reading("cgroup.memory.swap.max", "0"),
            reading("mm.swap_total", "4194300"),
        ];
        before.advice = evaluate(&[rule], &measures, &settings, &[]).0;
        let mut after = before.clone();
        after.advice.clear();
        after.measures = BTreeMap::from([(reading_name, number(0.0))]);
        after.settings = vec![reading("cgroup.memory.swap.max", "max")];
        let mut verification = verify(&before, &after);
        assert_eq!(verification.settings_changed[0].after, "max");
        let check = verification.checks.remove(0);
        assert_eq!(check.verdict, Verdict::AsPredicted);

        // The same move with the setting unchanged is not the rule's.
        after.settings = before.settings.clone();
        let check = verify(&before, &after).checks.remove(0);
        assert_eq!(check.verdict, Verdict::SettingUnchanged);
    }
}
