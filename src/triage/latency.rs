//! `omn latency`: *why is this slow*, answered by thread state analysis.
//!
//! TSA is what `examples/join-decompose-compute.yml` already produces: one
//! task's wall clock split across on a cpu, runnable and blocked, against the
//! program's own life, with a residue that is signed and never rounded away.
//! This mode is the question that asks for it and says what dominates. It adds
//! three things the table alone does not say, and nothing that the table does
//! not carry:
//!
//! - **Which task.** A program's latency is its main thread's life; the other
//!   threads are listed, not averaged in.
//! - **What the blocked time was blocked on.** The row is split by the task
//!   state `sched.offcpu` records at every switch -- `uninterruptible` is
//!   usually a disk, `sleeping` is waiting for something to happen -- and the
//!   sleeping part is named by the lock sources when they cover it: a pthread
//!   mutex (`sched.mutex`), a futex of any kind (`sched.futex`), or neither.
//! - **Whether the largest row is decisive.** A row that is largest but could
//!   be overtaken by what no row read -- a row the run did not supply, the
//!   residue, the rows' own resolution -- is not a culprit, and the answer
//!   says undecided and why rather than naming it.
//!
//! The objective decides which run is decomposed. `latency` reads the median
//! of `--runs`; `tail` reads the slowest *against* the median, and names the
//! state the slowest run spent more time in. One run has no tail, and an
//! attached process runs once, so both are refused rather than answered with
//! the one latency there is.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::capabilities::Capabilities;
use crate::catalog::Catalog;
use crate::emit::{Results, Shortfall, STACK_ID_ATTR};
use crate::event::Event;
use crate::join::{self, Breakdown, Decomposition, Evidence, Kind, Part, Row, Span, Whole};
use crate::request::{KeyKind, Request, Session, SourceId, StackMode, Target};
use crate::scalar::Duration;

use super::engine::{self, Dropped, Run};
use super::objective::{Method, Objective, Refusal};
use super::{span, TriageError};

/// The labels of the requests this mode writes, which are also the rows'.
pub const ON_CPU: &str = "on-cpu";
pub const RUNNABLE: &str = "runnable";
pub const BLOCKED: &str = "blocked";
pub const FUTEX: &str = "futex";
pub const MUTEX: &str = "mutex";

/// The requests this mode can do without: they name a wait, and a table
/// without them is still a table.
pub const OPTIONAL: [&str; 2] = [FUTEX, MUTEX];

/// The decomposition's id in the session this mode writes.
pub const DECOMPOSITION: &str = "where-the-time-went";

/// The most runs one request may ask for: each is a launch with five sources
/// attached, and a request for a thousand is a mistake rather than a tail.
pub const MAX_RUNS: u32 = 200;

/// The fewest runs a tail can be read from: the slowest and the median have to
/// be two different runs, with at least one other beside them.
pub const TAIL_RUNS: u32 = 3;

/// What `omn latency` was asked.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    pub objective: Objective,
    /// A launched program, or one process already running.
    pub target: Target,
    /// How many times to run a launched program.
    pub runs: u32,
    /// How long to observe. Required for a process already running, which has
    /// no exit to end the run on.
    pub duration: Option<Duration>,
    /// The profile's rate, whose period is the on-cpu row's resolution.
    pub hz: u32,
}

impl Ask {
    /// Checks what the request asks against what this method can answer, and
    /// refuses what cannot mean anything, by name.
    pub fn check(&self) -> Result<(), Refusal> {
        Method::Latency.require(Some(self.objective))?;
        let launched = matches!(self.target, Target::Launch(_));
        match &self.target {
            Target::Launch(argv) if argv.is_empty() => {
                return Err(Refusal(
                    "`omn latency --` names no program to launch".to_owned(),
                ))
            }
            Target::Launch(_) => {}
            Target::Pid(pids) if pids.len() == 1 => {}
            Target::Pid(_) => {
                return Err(Refusal(
                    "`omn latency` answers for one program, and was given several pids: a \
                     latency is one task's wall clock, and two programs' do not add up to one"
                        .to_owned(),
                ))
            }
            other => {
                return Err(Refusal(format!(
                    "`omn latency` answers for a program -- `-- PROGRAM` or `--pid PID` -- and \
                     was given {other}, which has no wall clock of its own to decompose"
                )))
            }
        }
        if !launched && self.duration.is_none() {
            return Err(Refusal(
                "`omn latency --pid` needs --duration: a process already running has no exit \
                 to end the run on, and its latency is decomposed over the window observed"
                    .to_owned(),
            ));
        }
        if self.runs == 0 || self.runs > MAX_RUNS {
            return Err(Refusal(format!(
                "--runs {} is out of range: between 1 and {MAX_RUNS}",
                self.runs
            )));
        }
        if !launched && self.runs > 1 {
            return Err(Refusal(
                "--runs repeats a launched program, and a process already running runs once"
                    .to_owned(),
            ));
        }
        if self.objective == Objective::Tail {
            if !launched {
                return Err(Refusal(
                    "`--objective tail` compares the slowest of several runs with the median \
                     one, and a process already running runs once: its tail is not observable \
                     here. Launch it with `-- PROGRAM` and --runs"
                        .to_owned(),
                ));
            }
            if self.runs < TAIL_RUNS {
                return Err(Refusal(format!(
                    "`--objective tail` needs --runs {TAIL_RUNS} or more, and was given {}: one \
                     run is one latency, and its tail is itself. The slowest run is compared \
                     with the median, which takes at least three",
                    self.runs
                )));
            }
        }
        Ok(())
    }

    /// The session this mode asks the engine for: one run of it.
    pub fn session(&self) -> Session {
        let target = self.target.clone();
        let request = |label: &str, source: &str, mode: &str, emit: &str| Request {
            source: source.parse::<SourceId>().expect("a catalog source"),
            target: target.clone(),
            mode: mode.parse().expect("a mode"),
            emit: vec![emit.parse().expect("an emit")],
            keys: vec![KeyKind::Task],
            filter: None,
            duration: self.duration,
            budget: None,
            stack: None,
            backend: None,
            label: Some(label.to_owned()),
        };
        let mut on_cpu = request(
            ON_CPU,
            "cpu.profile",
            &format!("sample@{}hz", self.hz),
            "stack",
        );
        on_cpu.stack = Some(StackMode::Fp);
        let mut blocked = request(BLOCKED, "sched.offcpu", "trace", "stack");
        blocked.stack = Some(StackMode::Fp);
        let requests = vec![
            on_cpu,
            request(RUNNABLE, "sched.runqlat", "trace", "summary"),
            blocked,
            request(FUTEX, "sched.futex", "trace", "summary"),
            request(MUTEX, "sched.mutex", "trace", "summary"),
        ];
        let whole = match self.target {
            Target::Launch(_) => Whole::Span(Span::Lifetime),
            _ => Whole::Span(Span::Window),
        };
        let part = |state: &str, from: &str, value: &str, less: Option<&str>| Part {
            state: state.to_owned(),
            from: from.to_owned(),
            value: value.to_owned(),
            less: less.map(str::to_owned),
            reading: None,
            filter: None,
            names: None,
        };
        Session {
            name: Some(format!("omn latency --objective {}", self.objective)),
            duration: self.duration,
            budget: None,
            out: None,
            format: Default::default(),
            requests,
            joins: Vec::new(),
            decompose: vec![Decomposition {
                id: DECOMPOSITION.to_owned(),
                whole,
                parts: vec![
                    part(ON_CPU, "cpu.profile", join::SAMPLES, None),
                    part(RUNNABLE, "sched.runqlat", "total_ns", None),
                    part(BLOCKED, "sched.offcpu", "total_ns", Some("queued_ns")),
                ],
                by: KeyKind::Task,
                top: 16,
            }],
        }
    }
}

/// What a state's time was spent on, which is what a verdict names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    /// On a cpu, doing its own work.
    Cpu,
    /// Runnable and waiting for a cpu: the cpu is shared, and busy.
    CpuQueue,
    /// Blocked uninterruptibly, which is usually a disk.
    Disk,
    /// Asleep in a pthread mutex another thread held.
    Lock,
    /// Asleep on a futex that is not a pthread mutex this run saw: another
    /// runtime's lock, a condition variable, a join.
    Futex,
    /// Asleep on something that is not a lock: a timer, a pipe, a socket.
    Sleep,
    /// Blocked in a state that is neither: stopped, traced, idle.
    OtherBlock,
}

impl Limit {
    pub fn as_str(self) -> &'static str {
        match self {
            Limit::Cpu => "cpu",
            Limit::CpuQueue => "cpu-queue",
            Limit::Disk => "disk",
            Limit::Lock => "lock",
            Limit::Futex => "futex",
            Limit::Sleep => "sleep",
            Limit::OtherBlock => "other-block",
        }
    }

    /// What it means, as the verdict says it.
    pub fn describe(self) -> &'static str {
        match self {
            Limit::Cpu => "on a cpu, doing its own work: cpu-bound",
            Limit::CpuQueue => "runnable and waiting for a cpu: the cpus it may use are busy",
            Limit::Disk => {
                "blocked uninterruptibly, which is a device and usually a disk: I/O-bound"
            }
            Limit::Lock => "asleep in a pthread mutex another thread held: lock-contended",
            Limit::Futex => {
                "asleep on a futex that is not a pthread mutex seen here: another runtime's \
                 lock, a condition variable or a join"
            }
            Limit::Sleep => "asleep on something that is not a lock: a timer, a pipe or a socket",
            Limit::OtherBlock => "blocked in a state that is neither sleeping nor uninterruptible",
        }
    }
}

/// One state of one task's wall clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// `on-cpu`, `runnable`, `blocked: uninterruptible`.
    pub state: String,
    pub limit: Option<Limit>,
    pub source: String,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
    /// Absent when the row could not be read, and `missing` says why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<f64>,
    /// Of the whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<f64>,
    /// The source's run was lossy, so the amount is a lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
    /// What else the run says about this state: the lock that covers a
    /// sleep, the function the cpu time was spent in, the stack a block began
    /// from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub named: Vec<Naming>,
}

/// Evidence about what a state was spent on, from a source other than the
/// row's own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Naming {
    pub source: String,
    pub evidence: Evidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    pub says: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
}

/// One task's decomposition, read for the answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    /// The task answered for, as the table keys it: `task:4242`.
    pub task: String,
    /// Why this task: the program's main thread, or the process asked for.
    pub chosen_because: String,
    pub whole: State,
    pub states: Vec<State>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residue: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residue_resolution: Option<f64>,
    /// The other tasks the run saw, heaviest first, one line each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<String>,
    /// Losses, degradations and notes of the sources this reading used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl Reading {
    /// The whole's amount, the wall clock this run is sorted by.
    pub fn wall(&self) -> Option<f64> {
        self.whole.amount
    }

    fn amount_of(&self, state: &str) -> f64 {
        self.states
            .iter()
            .filter(|one| one.state == state)
            .filter_map(|one| one.amount)
            .sum()
    }
}

/// What the answer names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub objective: Objective,
    /// Absent when no state is decisive, and `undecided` says why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<Limit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The state's time: in the median run for `latency`, the slowest run's
    /// excess over the median for `tail`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    /// What `amount` is a share of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<f64>,
    /// Whether the amount is a lower bound, because its source was lossy.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lower_bound: bool,
    pub sentence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undecided: Option<String>,
}

/// Everything `omn latency` answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub method: Method,
    pub objective: Objective,
    pub target: String,
    pub verdict: Verdict,
    /// Every run, in the order they ran.
    pub runs: Vec<Reading>,
    /// The run the verdict reads: the median for `latency`.
    pub median: usize,
    /// The slowest run, which `tail` reads against the median.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slowest: Option<usize>,
    /// Inputs this method would have liked and did without.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<Dropped>,
}

impl Answer {
    /// The report a person reads: the verdict first, then the table it was
    /// read from, with every row's kind, evidence, resolution and shortfall.
    pub fn text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "omn latency  --objective {}  {}",
            self.objective, self.target
        );
        let _ = writeln!(out, "\n  {}\n", self.verdict.sentence);
        if self.runs.len() > 1 {
            let walls: Vec<String> = self
                .runs
                .iter()
                .enumerate()
                .map(|(index, run)| {
                    let wall = run.wall().map_or("?".to_owned(), span);
                    let mark = if index == self.median {
                        " (median)"
                    } else if Some(index) == self.slowest {
                        " (slowest)"
                    } else {
                        ""
                    };
                    format!("{wall}{mark}")
                })
                .collect();
            let _ = writeln!(out, "  runs: {}\n", walls.join(", "));
        }
        let mut shown = vec![self.median];
        if let Some(slowest) = self.slowest {
            if slowest != self.median {
                shown.push(slowest);
            }
        }
        for index in shown {
            let run = &self.runs[index];
            let title = if self.runs.len() == 1 {
                String::new()
            } else if Some(index) == self.slowest {
                format!(", the slowest run (#{})", index + 1)
            } else {
                format!(", the median run (#{})", index + 1)
            };
            let _ = writeln!(out, "  {}{title}: {}", run.task, run.chosen_because);
            let _ = writeln!(
                out,
                "    {:<27} {:<18} {:<8} {:<9} {:>12} {:>11} {:>7}",
                "STATE", "SOURCE", "KIND", "EVIDENCE", "AMOUNT", "RESOLUTION", "SHARE"
            );
            for state in &run.states {
                let _ = writeln!(out, "    {}", state_line(state));
                for naming in &state.named {
                    let _ = writeln!(
                        out,
                        "      -> {} [{} {}]",
                        naming.says, naming.source, naming.evidence
                    );
                }
                if let Some(missing) = &state.missing {
                    let _ = writeln!(out, "      not read: {missing}");
                }
            }
            let _ = writeln!(out, "    {}", state_line(&run.whole));
            if let Some(residue) = run.residue {
                let _ = writeln!(
                    out,
                    "    {:<27} {:<18} {:<8} {:<9} {:>12} {:>11}",
                    "RESIDUE",
                    "whole - parts",
                    "",
                    "",
                    span(residue),
                    run.residue_resolution
                        .map_or("-".to_owned(), |resolution| format!(
                            "+-{}",
                            span(resolution)
                        ))
                );
            }
            for state in run.states.iter().chain([&run.whole]) {
                if let Some(short) = &state.short {
                    let _ = writeln!(out, "    {}: {}", state.state, short.note);
                }
            }
            for other in &run.others {
                let _ = writeln!(out, "    also: {other}");
            }
            for note in &run.notes {
                let _ = writeln!(out, "    note: {note}");
            }
            let _ = writeln!(out);
        }
        for dropped in &self.dropped {
            let _ = writeln!(
                out,
                "  without `{}` ({}): {}",
                dropped.label, dropped.source, dropped.reason
            );
        }
        out
    }
}

fn state_line(state: &State) -> String {
    let amount = match (state.amount, &state.short) {
        (Some(amount), Some(_)) => format!("{}{}", Shortfall::MARK, span(amount)),
        (Some(amount), None) => span(amount),
        (None, _) => "not read".to_owned(),
    };
    let resolution = match state.resolution {
        Some(0.0) => "exact".to_owned(),
        Some(resolution) => format!("+-{}", span(resolution)),
        None => "-".to_owned(),
    };
    format!(
        "{:<27} {:<18} {:<8} {:<9} {:>12} {:>11} {:>7}",
        state.state,
        state.source,
        state.kind.as_str(),
        state.evidence.map_or("-", Evidence::as_str),
        amount,
        resolution,
        state
            .share
            .map_or(String::new(), |share| format!("{:.1} %", share * 100.0)),
    )
}

/// Runs what `ask` needs and reads the answer.
pub fn answer(
    catalog: &Catalog,
    capabilities: &Capabilities,
    ask: &Ask,
) -> Result<Answer, TriageError> {
    ask.check()?;
    let mut session = ask.session();
    let dropped = engine::prune(catalog, capabilities, &mut session, &OPTIONAL)?;
    let mut runs = Vec::new();
    for _ in 0..ask.runs {
        let run = engine::run(catalog, capabilities, &session)?;
        runs.push(read(&run, &ask.target)?);
    }
    Ok(conclude(ask, runs, dropped))
}

/// The verdict over every run, from the readings alone.
pub fn conclude(ask: &Ask, runs: Vec<Reading>, dropped: Vec<Dropped>) -> Answer {
    let mut order: Vec<usize> = (0..runs.len()).collect();
    order.sort_by(|left, right| {
        let wall = |index: &usize| runs[*index].wall().unwrap_or(f64::NAN);
        wall(left).total_cmp(&wall(right))
    });
    let median = order[(order.len() - 1) / 2];
    let slowest = *order.last().expect("at least one run");
    let verdict = match ask.objective {
        Objective::Tail => tail(&runs[slowest], &runs[median], runs.len()),
        _ => latency(&runs[median], runs.len()),
    };
    Answer {
        method: Method::Latency,
        objective: ask.objective,
        target: ask.target.to_string(),
        verdict,
        median,
        slowest: (ask.objective == Objective::Tail).then_some(slowest),
        runs,
        dropped,
    }
}

/// The largest state, if nothing unread could overtake it.
fn latency(run: &Reading, runs: usize) -> Verdict {
    let objective = Objective::Latency;
    let which = if runs > 1 {
        format!("the median of {runs} runs")
    } else {
        "the run".to_owned()
    };
    let mut known: Vec<&State> = run.states.iter().filter(|s| s.amount.is_some()).collect();
    known.sort_by(|a, b| b.amount.unwrap_or(0.0).total_cmp(&a.amount.unwrap_or(0.0)));
    let Some(top) = known.first() else {
        return Verdict {
            objective,
            limit: None,
            state: None,
            amount: None,
            of: run.wall(),
            lower_bound: false,
            sentence: format!("no state of {} could be read for {}", run.task, which),
            undecided: Some("every row of the decomposition was missing".to_owned()),
        };
    };
    let amount = top.amount.unwrap_or(0.0);
    let second = known.get(1).and_then(|s| s.amount).unwrap_or(0.0);
    let read: f64 = known.iter().filter_map(|s| s.amount).sum();
    let unread = run.states.iter().any(|s| s.amount.is_none());
    // What no row read and could still belong to one: the whole less what was
    // read, when a row is missing, and the rows' resolution either way.
    let room = match run.wall() {
        Some(whole) if unread => (whole - read).max(0.0),
        _ => 0.0,
    } + run.residue_resolution.unwrap_or(0.0);
    let lower_bound = top.short.is_some();
    let share = run.wall().map(|whole| amount / whole * 100.0);
    let share_text = share.map_or(String::new(), |share| {
        format!(" ({share:.1} % of its wall clock)")
    });
    let what = top.limit.map_or("", |limit| limit.describe());
    if amount > second && amount > room {
        Verdict {
            objective,
            limit: top.limit,
            state: Some(top.state.clone()),
            amount: Some(amount),
            of: run.wall(),
            lower_bound,
            sentence: format!(
                "for latency, {} of {which} spent most of its time {what}: {} {}{share_text}{}",
                run.task,
                top.state,
                bounded(amount, lower_bound),
                naming_suffix(top),
            ),
            undecided: None,
        }
    } else {
        let why = if amount <= second {
            format!(
                "`{}` and the next state are {} and {}: neither is decisive",
                top.state,
                span(amount),
                span(second)
            )
        } else {
            format!(
                "`{}` is {}, and {} of the wall clock is in no row that was read: the largest \
                 row could still be overtaken",
                top.state,
                span(amount),
                span(room)
            )
        };
        Verdict {
            objective,
            limit: None,
            state: None,
            amount: None,
            of: run.wall(),
            lower_bound,
            sentence: format!("for latency, {which} names no single state: {why}"),
            undecided: Some(why),
        }
    }
}

/// The state the slowest run spent more time in than the median did.
fn tail(slowest: &Reading, median: &Reading, runs: usize) -> Verdict {
    let objective = Objective::Tail;
    let excess = match (slowest.wall(), median.wall()) {
        (Some(slow), Some(typical)) => slow - typical,
        _ => f64::NAN,
    };
    let mut names: Vec<&str> = slowest
        .states
        .iter()
        .chain(&median.states)
        .map(|one| one.state.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut deltas: Vec<(&str, f64)> = names
        .iter()
        .map(|name| (*name, slowest.amount_of(name) - median.amount_of(name)))
        .collect();
    deltas.sort_by(|a, b| b.1.total_cmp(&a.1));
    let (state, delta) = deltas.first().copied().unwrap_or(("", 0.0));
    let second = deltas.get(1).map_or(0.0, |(_, delta)| *delta);
    // How much of the difference the rows cannot speak for: what each run left
    // in its residue, and the resolution of both.
    let room = slowest.residue.unwrap_or(0.0).abs()
        + median.residue.unwrap_or(0.0).abs()
        + slowest.residue_resolution.unwrap_or(0.0)
        + median.residue_resolution.unwrap_or(0.0);
    let found = slowest.states.iter().find(|one| one.state == state);
    let limit = found.and_then(|one| one.limit);
    let lower_bound = slowest
        .states
        .iter()
        .chain(&median.states)
        .any(|one| one.state == state && one.short.is_some());
    let caveat = if (runs as u32) < 100 {
        format!(
            "; the slowest of {runs} runs is their maximum, not a percentile -- a p99 needs a \
             hundred runs or more"
        )
    } else {
        String::new()
    };
    let what = limit.map_or("", |limit| limit.describe());
    if delta > 0.0 && delta > second && delta > room {
        Verdict {
            objective,
            limit,
            state: Some(state.to_owned()),
            amount: Some(delta),
            of: Some(excess),
            lower_bound,
            sentence: format!(
                "for the tail, the slowest run took {} more than the median, and {} of it is \
                 more time {what}: {state}{}{caveat}",
                span(excess),
                bounded(delta, lower_bound),
                found.map_or(String::new(), naming_suffix),
            ),
            undecided: None,
        }
    } else {
        let why = format!(
            "the slowest run took {} more than the median; the largest state difference is \
             `{state}` at {}, against {} for the next and {} the rows cannot speak for",
            span(excess),
            span(delta),
            span(second),
            span(room)
        );
        Verdict {
            objective,
            limit: None,
            state: None,
            amount: None,
            of: Some(excess),
            lower_bound,
            sentence: format!("for the tail, no single state is decisive: {why}{caveat}"),
            undecided: Some(why),
        }
    }
}

fn bounded(amount: f64, lower_bound: bool) -> String {
    if lower_bound {
        format!("{}{}", Shortfall::MARK, span(amount))
    } else {
        span(amount)
    }
}

fn naming_suffix(state: &State) -> String {
    state
        .named
        .first()
        .map_or(String::new(), |naming| format!("; {}", naming.says))
}

/// Reads one run's decomposition for the task the answer is about.
pub fn read(run: &Run, target: &Target) -> Result<Reading, TriageError> {
    let output = run
        .output
        .decompositions
        .iter()
        .find(|output| output.id == DECOMPOSITION)
        .ok_or_else(|| engine::Failure {
            whose: crate::collect::Whose::Build,
            message: "the run produced no decomposition".to_owned(),
        })?;
    let (task, chosen_because) = main_task(run, target, &output.tables);
    let Some(table) = output.tables.iter().find(|table| table.key == task) else {
        return Err(TriageError::Failed(engine::Failure {
            whose: crate::collect::Whose::Machine,
            message: format!(
                "no source saw {task} do anything: the run named {} task(s) ({}), and none of \
                 them is the one asked about",
                output.keys_seen,
                output
                    .tables
                    .iter()
                    .map(|table| table.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }));
    };
    let whole = from_row(&table.whole, None, table.whole.amount);
    let wall = table.whole.amount;
    let mut states = Vec::new();
    for row in &table.rows {
        match row.state.as_str() {
            BLOCKED => states.extend(split_blocked(run, row, &task, wall)),
            ON_CPU => {
                let mut state = from_row(row, Some(Limit::Cpu), wall);
                state.named.extend(hottest(run, &task));
                states.push(state);
            }
            RUNNABLE => states.push(from_row(row, Some(Limit::CpuQueue), wall)),
            _ => states.push(from_row(row, None, wall)),
        }
    }
    let others = output
        .tables
        .iter()
        .filter(|other| other.key != task)
        .map(describe_other)
        .collect();
    Ok(Reading {
        task,
        chosen_because,
        whole,
        states,
        residue: table.residue,
        residue_resolution: table.residue_resolution,
        others,
        notes: notes(run),
    })
}

/// The task a latency is about.
///
/// For a process already running, the one asked for. For a launched program,
/// its main thread -- the thread whose id is the process id, which lives as
/// long as the program does -- found on any event that carries both ids. The
/// heaviest table is the fallback, and says so.
fn main_task(run: &Run, target: &Target, tables: &[Breakdown]) -> (String, String) {
    if let Target::Pid(pids) = target {
        if let Some(pid) = pids.first() {
            return (
                format!("task:{pid}"),
                "the process asked for, by its main thread".to_owned(),
            );
        }
    }
    let main = run.events.iter().find_map(|event| {
        let pid = event.attr("pid")?.as_f64()?;
        let tid = event.attr("tid")?.as_f64()?;
        (pid == tid && pid > 0.0).then(|| format!("task:{}", pid as u64))
    });
    if let Some(main) = main {
        if tables.iter().any(|table| table.key == main) {
            return (
                main,
                "the launched program's main thread, whose life is the program's".to_owned(),
            );
        }
    }
    let heaviest = tables
        .iter()
        .max_by(|a, b| a.parts.unwrap_or(0.0).total_cmp(&b.parts.unwrap_or(0.0)))
        .map(|table| table.key.clone())
        .unwrap_or_else(|| "task:?".to_owned());
    (
        heaviest,
        "no event carried both a process and a thread id, so the task whose rows add up to the \
         most"
            .to_owned(),
    )
}

fn from_row(row: &Row, limit: Option<Limit>, whole: Option<f64>) -> State {
    State {
        state: row.state.clone(),
        limit,
        source: row.source.clone(),
        kind: row.kind,
        evidence: row.evidence,
        amount: row.amount,
        resolution: row.resolution,
        share: match (row.amount, whole) {
            (Some(amount), Some(whole)) if whole > 0.0 => Some(amount / whole),
            _ => None,
        },
        short: row.short.clone(),
        missing: row.missing.clone(),
        named: Vec::new(),
    }
}

/// The blocked row, split by the task state each block recorded.
///
/// The same arithmetic the row does -- `total_ns` less `queued_ns`, summed --
/// per state instead of over all of them, from the same events. So the states
/// add up to the row, and a split that did not would be a defect of this file.
fn split_blocked(run: &Run, row: &Row, task: &str, whole: Option<f64>) -> Vec<State> {
    if row.amount.is_none() {
        return vec![from_row(row, None, whole)];
    }
    let mut by_state: BTreeMap<String, (f64, u64)> = BTreeMap::new();
    for event in run
        .events_of("sched.offcpu")
        .filter(|event| is_task(event, task))
    {
        let total = attr(event, "total_ns").unwrap_or(0.0);
        let queued = attr(event, "queued_ns").unwrap_or(0.0);
        let state = event
            .attr("state")
            .map(ToString::to_string)
            .unwrap_or_else(|| "unknown".to_owned());
        let entry = by_state.entry(state).or_default();
        entry.0 += total - queued;
        entry.1 += 1;
    }
    if by_state.is_empty() {
        return vec![from_row(row, None, whole)];
    }
    let sleeping = by_state.get("sleeping").map_or(0.0, |(amount, _)| *amount);
    let (mutex, futex) = (mutex_wait(run, task), futex_wait(run, task));
    by_state
        .into_iter()
        .map(|(name, (amount, _))| {
            let mut state = from_row(row, None, whole);
            state.state = format!("{BLOCKED}: {name}");
            state.amount = Some(amount);
            state.share = whole
                .filter(|whole| *whole > 0.0)
                .map(|whole| amount / whole);
            state.limit = Some(match name.as_str() {
                "uninterruptible" => Limit::Disk,
                "sleeping" => sleeping_limit(sleeping, mutex.as_ref(), futex.as_ref()),
                _ => Limit::OtherBlock,
            });
            let stack = blocking_stack(run, task, &name);
            // The naming that decided the limit goes first: it is the one the
            // verdict quotes, and the others are the evidence beside it.
            match state.limit {
                Some(Limit::Lock) => state
                    .named
                    .extend(mutex.clone().into_iter().chain(futex.clone()).chain(stack)),
                Some(Limit::Futex) => state
                    .named
                    .extend(futex.clone().into_iter().chain(mutex.clone()).chain(stack)),
                Some(Limit::Sleep) => state
                    .named
                    .extend(stack.into_iter().chain(mutex.clone()).chain(futex.clone())),
                _ => state.named.extend(stack),
            }
            state
        })
        .collect()
}

/// What a sleep is named, by the lock sources that cover it.
///
/// Half is the line: a wait source that covers more than half of the sleeping
/// time names it, and one that covers less is evidence beside a sleep that was
/// mostly something else. The wait sources time from the call to the return,
/// which includes the run queue after the wakeup, so they are compared with the
/// sleep rather than subtracted from it.
fn sleeping_limit(sleeping: f64, mutex: Option<&Naming>, futex: Option<&Naming>) -> Limit {
    let covers = |naming: Option<&Naming>| {
        naming
            .and_then(|naming| naming.amount)
            .is_some_and(|amount| sleeping > 0.0 && amount >= sleeping / 2.0)
    };
    if covers(mutex) {
        Limit::Lock
    } else if covers(futex) {
        Limit::Futex
    } else {
        Limit::Sleep
    }
}

fn is_task(event: &Event, task: &str) -> bool {
    event
        .key(KeyKind::Task)
        .is_some_and(|key| key.to_string() == task)
}

fn attr(event: &Event, name: &str) -> Option<f64> {
    event.attr(name).and_then(|value| value.as_f64())
}

/// The pthread mutex waits of one task, and the mutex waited on most.
fn mutex_wait(run: &Run, task: &str) -> Option<Naming> {
    let results = run.result(MUTEX)?;
    let mut waited = 0.0;
    let mut acquisitions = 0u64;
    let mut by_addr: BTreeMap<String, f64> = BTreeMap::new();
    for event in run
        .events_of("sched.mutex")
        .filter(|event| is_task(event, task))
    {
        let wait = attr(event, "wait_ns").unwrap_or(0.0);
        waited += wait;
        acquisitions += 1;
        if let Some(addr) = attr(event, "addr") {
            *by_addr.entry(format!("{:#x}", addr as u64)).or_default() += wait;
        }
    }
    let heaviest = by_addr
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(addr, _)| format!(", most of it on the mutex at {addr}"))
        .unwrap_or_default();
    Some(Naming {
        source: results.source.clone(),
        evidence: Evidence::Key,
        amount: Some(waited),
        says: format!(
            "{} waiting in pthread_mutex_lock over {acquisitions} acquisition(s){heaviest}",
            span(waited)
        ),
        short: short_of(results),
    })
}

/// The futex waits of one task.
fn futex_wait(run: &Run, task: &str) -> Option<Naming> {
    let results = run.result(FUTEX)?;
    let (mut waited, mut waits) = (0.0, 0.0);
    for event in run
        .events_of("sched.futex")
        .filter(|event| is_task(event, task))
    {
        waited += attr(event, "total_ns")
            .or_else(|| attr(event, "wait_ns"))
            .unwrap_or(0.0);
        waits += attr(event, "count").unwrap_or(1.0);
    }
    Some(Naming {
        source: results.source.clone(),
        evidence: Evidence::Key,
        amount: Some(waited),
        says: format!(
            "{} waiting in futex(2) over {waits:.0} wait(s)",
            span(waited)
        ),
        short: short_of(results),
    })
}

fn short_of(results: &Results) -> Option<Shortfall> {
    Shortfall::of(&results.losses, results.events_seen as u64, false)
}

/// The function the task's cpu samples landed in most, by leaf frame.
fn hottest(run: &Run, task: &str) -> Option<Naming> {
    let results = run.result(ON_CPU)?;
    let mut by_leaf: BTreeMap<&str, u64> = BTreeMap::new();
    let mut samples = 0u64;
    for event in run
        .events_of("cpu.profile")
        .filter(|event| is_task(event, task))
    {
        samples += 1;
        let leaf = attr(event, STACK_ID_ATTR)
            .and_then(|id| results.stacks.get(&(id as u64)))
            .and_then(|frames| frames.last())
            .map_or("[no stack]", String::as_str);
        *by_leaf.entry(leaf).or_default() += 1;
    }
    let (leaf, count) = by_leaf.into_iter().max_by_key(|(_, count)| *count)?;
    Some(Naming {
        source: results.source.clone(),
        evidence: Evidence::Key,
        amount: None,
        says: format!(
            "hottest function `{leaf}`, {count} of {samples} samples ({:.0} %)",
            count as f64 / samples as f64 * 100.0
        ),
        short: None,
    })
}

/// Where the task's blocks in one state began, by the heaviest stack.
fn blocking_stack(run: &Run, task: &str, state: &str) -> Option<Naming> {
    let results = run.result(BLOCKED)?;
    let mut by_stack: BTreeMap<u64, f64> = BTreeMap::new();
    for event in run
        .events_of("sched.offcpu")
        .filter(|event| is_task(event, task))
    {
        if event.attr("state").map(ToString::to_string).as_deref() != Some(state) {
            continue;
        }
        let Some(id) = attr(event, STACK_ID_ATTR) else {
            continue;
        };
        let blocked =
            attr(event, "total_ns").unwrap_or(0.0) - attr(event, "queued_ns").unwrap_or(0.0);
        *by_stack.entry(id as u64).or_default() += blocked;
    }
    let total: f64 = by_stack.values().sum();
    let (id, amount) = by_stack.into_iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    let frames = results.stacks.get(&id)?;
    let leafward: Vec<&str> = frames.iter().rev().take(4).map(String::as_str).collect();
    Some(Naming {
        source: results.source.clone(),
        evidence: Evidence::Key,
        amount: Some(amount),
        says: format!(
            "{:.0} % of it from one stack, leaf first: {}",
            if total > 0.0 {
                amount / total * 100.0
            } else {
                0.0
            },
            leafward.join(" <- ")
        ),
        short: None,
    })
}

fn describe_other(table: &Breakdown) -> String {
    let largest = table
        .rows
        .iter()
        .filter(|row| row.amount.is_some())
        .max_by(|a, b| a.amount.unwrap_or(0.0).total_cmp(&b.amount.unwrap_or(0.0)));
    let unread: Vec<&str> = table
        .rows
        .iter()
        .filter(|row| row.amount.is_none())
        .map(|row| row.state.as_str())
        .collect();
    let mut line = match largest {
        Some(row) => format!(
            "{}: largest row `{}` at {}",
            table.key,
            row.state,
            span(row.amount.unwrap_or(0.0))
        ),
        None => format!("{}: no row read", table.key),
    };
    if !unread.is_empty() {
        line.push_str(&format!(", not read: {}", unread.join(", ")));
    }
    line
}

/// What every source of the run lost or gave up, one line each.
pub fn notes(run: &Run) -> Vec<String> {
    let mut notes = Vec::new();
    for results in &run.output.results {
        let losses = &results.losses;
        if !losses.is_clean() {
            let mut counts = Vec::new();
            for (name, count) in [
                ("dropped", losses.dropped),
                ("throttled", losses.throttled),
                ("failed reads", losses.failed_reads),
                ("late ticks", losses.late_ticks),
                ("undecoded", losses.undecoded),
            ] {
                if count > 0 {
                    counts.push(format!("{count} {name}"));
                }
            }
            notes.push(format!(
                "[{}] {} lost: {}",
                results.label,
                results.source,
                counts.join(", ")
            ));
        }
        for degradation in &results.degradations {
            notes.push(format!("[{}] degraded: {degradation}", results.label));
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(objective: Objective, runs: u32) -> Ask {
        Ask {
            objective,
            target: Target::Launch(vec!["true".to_owned()]),
            runs,
            duration: None,
            hz: 997,
        }
    }

    fn state(name: &str, limit: Limit, amount: Option<f64>) -> State {
        State {
            state: name.to_owned(),
            limit: Some(limit),
            source: "test".to_owned(),
            kind: Kind::Traced,
            evidence: Some(Evidence::Key),
            amount,
            resolution: Some(0.0),
            share: None,
            short: None,
            missing: amount.is_none().then(|| "not read".to_owned()),
            named: Vec::new(),
        }
    }

    fn reading(wall: f64, states: Vec<State>, residue_resolution: f64) -> Reading {
        let parts: f64 = states.iter().filter_map(|s| s.amount).sum();
        Reading {
            task: "task:1".to_owned(),
            chosen_because: "test".to_owned(),
            whole: state("whole", Limit::Cpu, Some(wall)),
            residue: Some(wall - parts),
            residue_resolution: Some(residue_resolution),
            states,
            others: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn a_tail_of_one_run_is_refused() {
        let refusal = ask(Objective::Tail, 1).check().unwrap_err().to_string();
        assert!(refusal.contains("--runs 3 or more"), "{refusal}");
    }

    #[test]
    fn a_tail_of_an_attached_process_is_refused() {
        let mut ask = ask(Objective::Tail, 5);
        ask.target = Target::Pid(vec![1]);
        ask.duration = Some("1s".parse().unwrap());
        let refusal = ask.check().unwrap_err().to_string();
        assert!(refusal.contains("runs once"), "{refusal}");
    }

    #[test]
    fn an_attached_process_needs_a_duration() {
        let mut ask = ask(Objective::Latency, 1);
        ask.target = Target::Pid(vec![1]);
        let refusal = ask.check().unwrap_err().to_string();
        assert!(refusal.contains("--duration"), "{refusal}");
    }

    #[test]
    fn a_machine_target_is_refused_by_name() {
        let mut ask = ask(Objective::Latency, 1);
        ask.target = Target::System;
        assert!(ask.check().is_err());
    }

    #[test]
    fn the_session_it_writes_validates_against_the_catalog() {
        let catalog = Catalog::embedded().unwrap();
        for objective in [Objective::Latency, Objective::Tail] {
            let session = ask(objective, 3).session();
            crate::validate::validate_session(&catalog, &session)
                .unwrap_or_else(|rejections| panic!("{rejections:?}"));
        }
        let mut attached = ask(Objective::Latency, 1);
        attached.target = Target::Pid(vec![1]);
        attached.duration = Some("1s".parse().unwrap());
        crate::validate::validate_session(&catalog, &attached.session())
            .unwrap_or_else(|rejections| panic!("{rejections:?}"));
    }

    #[test]
    fn the_largest_state_is_named_when_nothing_unread_could_overtake_it() {
        let run = reading(
            100e6,
            vec![
                state("on-cpu", Limit::Cpu, Some(90e6)),
                state("runnable", Limit::CpuQueue, Some(1e6)),
                state("blocked", Limit::Disk, None),
            ],
            1e6,
        );
        let verdict = latency(&run, 1);
        assert_eq!(verdict.limit, Some(Limit::Cpu), "{}", verdict.sentence);
    }

    #[test]
    fn a_largest_state_an_unread_row_could_overtake_is_undecided() {
        let run = reading(
            100e6,
            vec![
                state("on-cpu", Limit::Cpu, Some(40e6)),
                state("runnable", Limit::CpuQueue, Some(1e6)),
                state("blocked", Limit::Disk, None),
            ],
            1e6,
        );
        let verdict = latency(&run, 1);
        assert_eq!(verdict.limit, None, "{}", verdict.sentence);
        assert!(verdict.undecided.is_some());
    }

    #[test]
    fn two_equal_states_are_undecided() {
        let run = reading(
            100e6,
            vec![
                state("on-cpu", Limit::Cpu, Some(50e6)),
                state("blocked: sleeping", Limit::Sleep, Some(50e6)),
            ],
            0.0,
        );
        assert_eq!(latency(&run, 1).limit, None);
    }

    #[test]
    fn the_tail_names_what_the_slowest_run_spent_more_on() {
        let typical = reading(
            100e6,
            vec![
                state("on-cpu", Limit::Cpu, Some(99e6)),
                state("blocked: sleeping", Limit::Sleep, Some(0.5e6)),
            ],
            1e6,
        );
        let slow = reading(
            300e6,
            vec![
                state("on-cpu", Limit::Cpu, Some(99e6)),
                state("blocked: sleeping", Limit::Sleep, Some(200e6)),
            ],
            1e6,
        );
        let verdict = tail(&slow, &typical, 5);
        assert_eq!(verdict.limit, Some(Limit::Sleep), "{}", verdict.sentence);
        assert!(
            verdict.sentence.contains("not a percentile"),
            "{}",
            verdict.sentence
        );
        // And the median run alone names the cpu: the objective changes the answer.
        assert_eq!(latency(&typical, 5).limit, Some(Limit::Cpu));
    }

    #[test]
    fn the_median_run_is_the_one_a_latency_reads() {
        let runs: Vec<Reading> = [300e6, 100e6, 110e6]
            .into_iter()
            .map(|wall| {
                reading(
                    wall,
                    vec![state("on-cpu", Limit::Cpu, Some(wall * 0.9))],
                    0.0,
                )
            })
            .collect();
        let answer = conclude(&ask(Objective::Tail, 3), runs, Vec::new());
        assert_eq!(answer.median, 2);
        assert_eq!(answer.slowest, Some(0));
    }

    #[test]
    fn a_sleep_is_named_by_the_lock_that_covers_half_of_it() {
        let naming = |amount: f64| Naming {
            source: "x".to_owned(),
            evidence: Evidence::Key,
            amount: Some(amount),
            says: String::new(),
            short: None,
        };
        assert_eq!(
            sleeping_limit(100.0, Some(&naming(90.0)), Some(&naming(95.0))),
            Limit::Lock
        );
        assert_eq!(
            sleeping_limit(100.0, Some(&naming(10.0)), Some(&naming(95.0))),
            Limit::Futex
        );
        assert_eq!(
            sleeping_limit(100.0, Some(&naming(10.0)), Some(&naming(20.0))),
            Limit::Sleep
        );
        assert_eq!(sleeping_limit(100.0, None, None), Limit::Sleep);
    }
}
