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
//!   threads are listed, not averaged in. When the main thread's largest row
//!   is a wait other tasks ran in, the answer follows the work: a wait for a
//!   child (`wait4`, `waitid`, or a block its child ran through) is followed
//!   into the child that ran during it, whose decomposition takes the wait's
//!   place; a block other threads of the process ran in names the busiest of
//!   them beside it, with its own decomposition. Each followed task is read
//!   the same way, so a driver waiting for a compiler it started is followed
//!   to the compiler, and the verdict says which task each row is from.
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
//! **The mutex source is asked only when a sleep needs it.** `sched.mutex`
//! runs inside the target, and on a program that takes a mutex millions of
//! times a second it doubles what it is asked to explain. So the first pass
//! runs without it, and only when the row the verdict reads is a futex sleep
//! -- which a pthread mutex would explain more precisely -- is the question
//! asked again with it, and the answer read from that second pass. A source
//! whose measured cost is over [`COST_CRITERION_PERCENT`] is said beside the
//! verdict.
//!
//! The objective decides which run is decomposed. `latency` reads the median
//! of `--runs`; `tail` reads the slowest *against* the median, and names the
//! state the slowest run spent more time in. One run has no tail, and an
//! attached process runs once, so both are refused rather than answered with
//! the one latency there is.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::capabilities::Capabilities;
use crate::catalog::Catalog;
use crate::emit::{Results, Shortfall, STACK_ID_ATTR};
use crate::event::Event;
use crate::join::{self, Breakdown, Decomposition, Evidence, Kind, Part, Row, Span, Whole};
use crate::request::{KeyKind, Request, Session, SourceId, StackMode, Target};
use crate::scalar::Duration;
use crate::stack::unwind::SymbolSource;

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

/// How far a wait is followed: a driver, the compiler it started, an
/// assembler the compiler started, and some room. A chain longer than this is
/// cut, and the last reading says so by being the one decomposed.
pub const MAX_FOLLOWED: usize = 8;

/// The share of a block another task has to account for before it is said to
/// have run in it, when nothing else ties the two -- no wait for a child on
/// the blocking stack. A quarter: a thread that ran through a quarter of the
/// main thread's sleep did the program's work for at least that long, and a
/// helper that woke for a millisecond did not.
pub const RAN_IN_IT: f64 = 0.25;

/// A source whose measured cost is over this share of the target's cpu is
/// said beside the verdict: past it, a source dilates what it measures (the
/// catalog's `extreme` band starts here).
pub const COST_CRITERION_PERCENT: f64 = 20.0;

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
    /// Where stack names come from, set on every request that reads stacks:
    /// the on-cpu profile and the blocking stacks. `None` is the default,
    /// `local`.
    pub symbols: Option<SymbolSource>,
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

    /// The session this mode asks the engine for first: one run of it,
    /// without `sched.mutex`, which is asked only when a sleep needs splitting
    /// ([`Ask::split_session`]).
    pub fn session(&self) -> Session {
        self.session_with(false)
    }

    /// The session of the second pass: the first, and `sched.mutex`.
    pub fn split_session(&self) -> Session {
        self.session_with(true)
    }

    fn session_with(&self, mutex: bool) -> Session {
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
            symbols: None,
            backend: None,
            label: Some(label.to_owned()),
            window: None,
        };
        let mut on_cpu = request(
            ON_CPU,
            "cpu.profile",
            &format!("sample@{}hz", self.hz),
            "stack",
        );
        on_cpu.stack = Some(StackMode::Fp);
        on_cpu.symbols = self.symbols;
        let mut blocked = request(BLOCKED, "sched.offcpu", "trace", "stack");
        blocked.stack = Some(StackMode::Fp);
        blocked.symbols = self.symbols;
        let mut requests = vec![
            on_cpu,
            request(RUNNABLE, "sched.runqlat", "trace", "summary"),
            blocked,
            request(FUTEX, "sched.futex", "trace", "summary"),
        ];
        if mutex {
            requests.push(request(MUTEX, "sched.mutex", "trace", "summary"));
        }
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
            via: None,
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
            escalate: None,
            record: None,
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
    /// Asleep on something that is not a lock seen here: a timer, a pipe, a
    /// socket, a lock polled with a sleep between tries, a wait for a child.
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
            Limit::Sleep => {
                "asleep on something that is not a lock seen here: a timer, a pipe, a socket, a \
                 lock polled with a sleep between tries, or a wait for a child process"
            }
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
    /// The sources whose measured cost was over [`COST_CRITERION_PERCENT`] of
    /// the target's cpu, one line each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub costly: Vec<String>,
    /// The task that ran in this one's largest row, when that row is a wait
    /// other tasks ran in, read the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub followed: Option<Box<Followed>>,
}

/// How a followed task stands to the one whose block it ran in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// A process the program started, which ran while it waited: its
    /// decomposition takes the wait's place, and its whole is the wait.
    Child,
    /// Another thread of the same process, which ran while it was blocked:
    /// its decomposition is named beside the block, and its whole is what
    /// its own rows account for.
    Thread,
}

/// A task followed from the block it ran in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Followed {
    pub relation: Relation,
    /// The state of the reading above that it ran in: `blocked: sleeping`.
    pub during: String,
    /// That state's amount.
    pub amount: f64,
    /// The wait for a child the blocking stack names, `wait4`, when it names
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<String>,
    pub reading: Reading,
}

impl Reading {
    /// The whole's amount, the wall clock this run is sorted by.
    pub fn wall(&self) -> Option<f64> {
        self.whole.amount
    }

    /// The largest state read.
    fn top(&self) -> Option<&State> {
        self.states
            .iter()
            .filter(|state| state.amount.is_some())
            .max_by(|a, b| a.amount.unwrap_or(0.0).total_cmp(&b.amount.unwrap_or(0.0)))
    }

    /// This reading and every one followed from it, each with how it was
    /// reached; the last is the one a verdict decomposes.
    pub fn chain(&self) -> Vec<(Option<&Followed>, &Reading)> {
        let mut chain = vec![(None, self)];
        let mut at = self;
        while let Some(followed) = at.followed.as_deref() {
            chain.push((Some(followed), &followed.reading));
            at = &followed.reading;
        }
        chain
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
    /// The task whose state is named: the program's main thread, or the task
    /// followed from its wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
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
    /// Why the question was asked a second time with `sched.mutex`, and what
    /// the first pass had named. The answer is the second pass's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_pass: Option<String>,
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
        let read = &self.runs[self.slowest.unwrap_or(self.median)];
        for costly in &read.costly {
            let _ = writeln!(out, "  cost: {costly}");
        }
        if let Some(second) = &self.second_pass {
            let _ = writeln!(out, "  second pass: {second}");
        }
        if !read.costly.is_empty() || self.second_pass.is_some() {
            let _ = writeln!(out);
        }
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
            write_reading(&mut out, run, &title, "  ");
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

/// One reading's table, and the readings followed from it below it.
fn write_reading(out: &mut String, run: &Reading, title: &str, indent: &str) {
    use std::fmt::Write as _;
    let _ = writeln!(out, "{indent}{}{title}: {}", run.task, run.chosen_because);
    let _ = writeln!(
        out,
        "{indent}  {:<27} {:<18} {:<8} {:<9} {:>12} {:>11} {:>7}",
        "STATE", "SOURCE", "KIND", "EVIDENCE", "AMOUNT", "RESOLUTION", "SHARE"
    );
    for state in &run.states {
        let _ = writeln!(out, "{indent}  {}", state_line(state));
        for naming in &state.named {
            let _ = writeln!(
                out,
                "{indent}    -> {} [{} {}]",
                naming.says, naming.source, naming.evidence
            );
        }
        if let Some(missing) = &state.missing {
            let _ = writeln!(out, "{indent}    not read: {missing}");
        }
    }
    let _ = writeln!(out, "{indent}  {}", state_line(&run.whole));
    if let Some(residue) = run.residue {
        let _ = writeln!(
            out,
            "{indent}  {:<27} {:<18} {:<8} {:<9} {:>12} {:>11}",
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
            let _ = writeln!(out, "{indent}  {}: {}", state.state, short.note);
        }
    }
    for other in &run.others {
        let _ = writeln!(out, "{indent}  also: {other}");
    }
    for note in &run.notes {
        let _ = writeln!(out, "{indent}  note: {note}");
    }
    let _ = writeln!(out);
    if let Some(followed) = &run.followed {
        let how = match followed.relation {
            Relation::Child => "in its place",
            Relation::Thread => "beside it",
        };
        let _ = writeln!(
            out,
            "{indent}  followed from `{}` ({}), {how}:",
            followed.during,
            span(followed.amount)
        );
        write_reading(out, &followed.reading, "", &format!("{indent}  "));
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
///
/// Twice when the first pass's answer is a futex sleep and `sched.mutex` can
/// be served: once without the mutex source, which on a program that locks
/// often costs more than the program, and once with it, to tell a pthread
/// mutex from the other futex waits. The second pass is the answer, and says
/// why it was run.
pub fn answer(
    catalog: &Catalog,
    capabilities: &Capabilities,
    ask: &Ask,
) -> Result<Answer, TriageError> {
    ask.check()?;
    let mut full = ask.split_session();
    let dropped = engine::prune(catalog, capabilities, &mut full, &OPTIONAL)?;
    let asked = |label: &str| {
        full.requests
            .iter()
            .any(|request| request.label.as_deref() == Some(label))
    };
    let (mutex, futex) = (asked(MUTEX), asked(FUTEX));
    let mut first = full.clone();
    first
        .requests
        .retain(|request| request.label.as_deref() != Some(MUTEX));
    let answer = conclude(
        ask,
        run_all(catalog, capabilities, &first, ask)?,
        dropped.clone(),
    );
    if !mutex {
        return Ok(answer);
    }
    let read = &answer.runs[answer.slowest.unwrap_or(answer.median)];
    let Some(why) = needs_split(read, futex) else {
        return Ok(answer);
    };
    let mut second = conclude(ask, run_all(catalog, capabilities, &full, ask)?, dropped);
    let window = match ask.target {
        Target::Launch(_) => format!("a second pass of {} run(s)", ask.runs),
        _ => "a second window as long as the first".to_owned(),
    };
    second.second_pass = Some(format!(
        "{why}, so `sched.mutex` was asked in {window}, which this answer is read from; the \
         first pass, without it, said: {}",
        answer.verdict.sentence
    ));
    Ok(second)
}

fn run_all(
    catalog: &Catalog,
    capabilities: &Capabilities,
    session: &Session,
    ask: &Ask,
) -> Result<Vec<Reading>, TriageError> {
    let mut runs = Vec::new();
    for _ in 0..ask.runs {
        let run = engine::run(catalog, capabilities, session)?;
        runs.push(read(&run, &ask.target)?);
    }
    Ok(runs)
}

/// Why the run the verdict reads needs `sched.mutex` to split a sleep, if it
/// does: a reading on its way whose largest row is a futex sleep, which a
/// pthread mutex would name more precisely -- or any sleep, when the futex
/// source was not there to say which sleeps were futexes.
pub fn needs_split(run: &Reading, futex_asked: bool) -> Option<String> {
    run.chain().into_iter().find_map(|(_, reading)| {
        let top = reading.top()?;
        let sleeping = top.state == format!("{BLOCKED}: sleeping");
        match top.limit {
            Some(Limit::Futex) if sleeping => Some(format!(
                "{}'s largest row is a futex sleep ({}) that a pthread mutex could explain",
                reading.task,
                span(top.amount.unwrap_or(0.0))
            )),
            Some(Limit::Sleep) if sleeping && !futex_asked => Some(format!(
                "{}'s largest row is a sleep ({}), and without `sched.futex` nothing said whether \
                 it was a lock",
                reading.task,
                span(top.amount.unwrap_or(0.0))
            )),
            _ => None,
        }
    })
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
        second_pass: None,
    }
}

/// How the verdict reached the task it decomposes, one clause a step:
/// empty when it decomposes the program's main thread.
fn path_clause(chain: &[(Option<&Followed>, &Reading)]) -> String {
    let mut clauses = Vec::new();
    for pair in chain.windows(2) {
        let (_, above) = pair[0];
        let (Some(followed), below) = pair[1] else {
            continue;
        };
        let share = above
            .wall()
            .filter(|whole| *whole > 0.0)
            .map_or(String::new(), |whole| {
                format!(
                    " ({:.1} % of its wall clock)",
                    followed.amount / whole * 100.0
                )
            });
        clauses.push(match (followed.relation, &followed.wait) {
            (Relation::Child, Some(wait)) => format!(
                "{} waited {}{share} for a child, in `{wait}`, and {}, the child that ran \
                 during the wait, is decomposed in its place",
                above.task,
                span(followed.amount),
                below.task
            ),
            (Relation::Child, None) => format!(
                "{} was `{}` {}{share} while its child ran, and {}, that child, is decomposed in \
                 its place",
                above.task,
                followed.during,
                span(followed.amount),
                below.task
            ),
            (Relation::Thread, _) => format!(
                "{} was `{}` {}{share} while other threads of the process ran, and beside it {}, \
                 the busiest of them, is decomposed",
                above.task,
                followed.during,
                span(followed.amount),
                below.task
            ),
        });
    }
    clauses.join("; ")
}

/// The largest state, if nothing unread could overtake it.
fn latency(run: &Reading, runs: usize) -> Verdict {
    let objective = Objective::Latency;
    let which = if runs > 1 {
        format!("the median of {runs} runs")
    } else {
        "the run".to_owned()
    };
    let chain = run.chain();
    let path = path_clause(&chain);
    let (reached, read_one) = *chain.last().expect("a reading");
    // Who is answered for, as the sentence opens.
    let (opening, of_what) = match reached.map(|followed| followed.relation) {
        None => (
            format!("for latency, {} of {which}", read_one.task),
            "of its wall clock",
        ),
        Some(Relation::Child) => (
            format!("for latency, in {which}, {path}: it"),
            "of the wait it ran in",
        ),
        Some(Relation::Thread) => (
            format!("for latency, in {which}, {path}: it"),
            "of what its rows account for",
        ),
    };
    let mut known: Vec<&State> = read_one
        .states
        .iter()
        .filter(|s| s.amount.is_some())
        .collect();
    known.sort_by(|a, b| b.amount.unwrap_or(0.0).total_cmp(&a.amount.unwrap_or(0.0)));
    let Some(top) = known.first() else {
        return Verdict {
            objective,
            task: Some(read_one.task.clone()),
            limit: None,
            state: None,
            amount: None,
            of: read_one.wall(),
            lower_bound: false,
            sentence: format!("{opening}: no state could be read"),
            undecided: Some("every row of the decomposition was missing".to_owned()),
        };
    };
    let amount = top.amount.unwrap_or(0.0);
    let second = known.get(1).and_then(|s| s.amount).unwrap_or(0.0);
    let read: f64 = known.iter().filter_map(|s| s.amount).sum();
    let unread: Vec<&str> = read_one
        .states
        .iter()
        .filter(|s| s.amount.is_none())
        .map(|s| s.state.as_str())
        .collect();
    // What no row read and could still belong to one: the whole less what was
    // read, and the residue's resolution -- the whole's included -- when a
    // row is missing. When every row was read, nothing is outside them, and
    // what could still reorder them is their own resolution: the whole's
    // bracket (a loaded machine's exit seen late) moves the residue, not a
    // row.
    let room = match read_one.wall() {
        Some(whole) if !unread.is_empty() => {
            (whole - read).max(0.0) + read_one.residue_resolution.unwrap_or(0.0)
        }
        _ => known
            .iter()
            .filter_map(|state| state.resolution)
            .sum::<f64>(),
    };
    let lower_bound = top.short.is_some();
    let share = read_one.wall().map(|whole| amount / whole * 100.0);
    let share_text = share.map_or(String::new(), |share| format!(" ({share:.1} % {of_what})"));
    let what = top.limit.map_or("", |limit| limit.describe());
    if amount > second && amount > room {
        Verdict {
            objective,
            task: Some(read_one.task.clone()),
            limit: top.limit,
            state: Some(top.state.clone()),
            amount: Some(amount),
            of: read_one.wall(),
            lower_bound,
            sentence: format!(
                "{opening} spent most of its time {what}: {} {}{share_text}{}",
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
        } else if unread.is_empty() {
            format!(
                "`{}` is {}, within the rows' own resolution ({}) of being overtaken",
                top.state,
                span(amount),
                span(room)
            )
        } else {
            format!(
                "`{}` is {}, and {} of the wall clock is in no row that was read ({}): the \
                 largest row could still be overtaken",
                top.state,
                span(amount),
                span(room),
                unread.join(", ")
            )
        };
        let sentence = if reached.is_none() {
            format!("for latency, {which} names no single state: {why}")
        } else {
            format!("{opening} names no single state: {why}")
        };
        Verdict {
            objective,
            task: Some(read_one.task.clone()),
            limit: None,
            state: None,
            amount: None,
            of: read_one.wall(),
            lower_bound,
            sentence,
            undecided: Some(why),
        }
    }
}

/// The state the slowest run spent more time in than the median did.
///
/// Read off the tasks the two runs' verdicts decompose when both runs were
/// followed the same way (a driver to its compiler in both), and off the main
/// threads otherwise: two different tasks' states do not subtract.
fn tail(slowest: &Reading, median: &Reading, runs: usize) -> Verdict {
    let objective = Objective::Tail;
    let excess = match (slowest.wall(), median.wall()) {
        (Some(slow), Some(typical)) => slow - typical,
        _ => f64::NAN,
    };
    let (slow_chain, median_chain) = (slowest.chain(), median.chain());
    let same_shape = slow_chain.len() == median_chain.len()
        && slow_chain
            .iter()
            .zip(&median_chain)
            .all(|((a, _), (b, _))| a.map(|f| f.relation) == b.map(|f| f.relation));
    let (slowest, median) = if same_shape {
        (slow_chain.last().unwrap().1, median_chain.last().unwrap().1)
    } else {
        (slowest, median)
    };
    let in_task = if same_shape && slow_chain.len() > 1 {
        format!(
            " (in {}, followed from the main thread's wait)",
            slowest.task
        )
    } else {
        String::new()
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
            task: Some(slowest.task.clone()),
            limit,
            state: Some(state.to_owned()),
            amount: Some(delta),
            of: Some(excess),
            lower_bound,
            sentence: format!(
                "for the tail, the slowest run took {} more than the median, and {} of it is \
                 more time {what}{in_task}: {state}{}{caveat}",
                span(excess),
                bounded(delta, lower_bound),
                found.map_or(String::new(), naming_suffix),
            ),
            undecided: None,
        }
    } else {
        let why = format!(
            "the slowest run took {} more than the median; the largest state difference{in_task} \
             is `{state}` at {}, against {} for the next and {} the rows cannot speak for",
            span(excess),
            span(delta),
            span(second),
            span(room)
        );
        Verdict {
            objective,
            task: Some(slowest.task.clone()),
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

/// Reads one run's decomposition for the task the answer is about, and
/// follows its largest row into the task that ran in it, when it is a wait
/// another task ran in.
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
    let mut reading = read_table(run, table, &task, chosen_because, None, false);
    reading.others = output
        .tables
        .iter()
        .filter(|other| other.key != task)
        .map(describe_other)
        .collect();
    reading.notes.extend(notes(run));
    reading.costly = costly(run);
    let mut tasks = Tasks {
        run,
        tables: &output.tables,
        pid_of: pids(run),
        seen: BTreeSet::from([task.clone()]),
        processes: BTreeSet::new(),
    };
    let process = tasks
        .pid_of
        .get(&task)
        .copied()
        .or_else(|| task_number(&task));
    if let Some(process) = process {
        tasks.processes.insert(process);
    }
    tasks.follow(&mut reading, process, 0);
    Ok(reading)
}

/// One task's table, read: the blocked row split by state, the on-cpu row
/// named by its hottest function.
///
/// `whole` replaces the table's whole for a followed task, whose table's
/// whole is the program's life rather than its own. The residue is then the
/// difference with that whole, and absent when a row is. `another_process`
/// is a task outside the launched process, whose zeros are not read as
/// zeros ([`unwatched`]).
fn read_table(
    run: &Run,
    table: &Breakdown,
    task: &str,
    chosen_because: String,
    whole: Option<GivenWhole>,
    another_process: bool,
) -> Reading {
    let mut whole_state = from_row(&table.whole, None, table.whole.amount);
    if let Some(given) = &whole {
        whole_state.amount = Some(given.amount);
        whole_state.state = given.state.clone();
        whole_state.source = given.source.clone();
        whole_state.kind = given.kind;
        whole_state.resolution = given.resolution;
        whole_state.short = None;
        whole_state.share = Some(1.0);
    }
    let wall = whole_state.amount;
    let mut states = Vec::new();
    let mut said = Vec::new();
    for original in &table.rows {
        let row = &unwatched(run, original, task, another_process);
        if original.amount.is_none() && row.amount.is_some() {
            said.extend(
                row.note
                    .as_ref()
                    .map(|note| format!("[{}] {note}", row.state)),
            );
        }
        match row.state.as_str() {
            BLOCKED => states.extend(split_blocked(run, row, task, wall)),
            ON_CPU => {
                let mut state = from_row(row, Some(Limit::Cpu), wall);
                state.named.extend(hottest(run, task));
                states.push(state);
            }
            RUNNABLE => states.push(from_row(row, Some(Limit::CpuQueue), wall)),
            _ => states.push(from_row(row, None, wall)),
        }
    }
    let (residue, residue_resolution) = match whole {
        None => (table.residue, table.residue_resolution),
        Some(given) => {
            let complete = states.iter().all(|state| state.amount.is_some());
            let parts: f64 = states.iter().filter_map(|state| state.amount).sum();
            // The rows' resolutions and the given whole's, as the table sums
            // its own with the program's life.
            let resolution = table
                .rows
                .iter()
                .map(|row| row.resolution)
                .chain([given.resolution])
                .sum::<Option<f64>>();
            (complete.then_some(given.amount - parts), resolution)
        }
    };
    Reading {
        task: task.to_owned(),
        chosen_because,
        whole: whole_state,
        states,
        residue,
        residue_resolution,
        others: Vec::new(),
        notes: said,
        costly: Vec::new(),
        followed: None,
    }
}

/// The whole a followed task's rows are parts of, which is not its table's:
/// that one is the program's life.
struct GivenWhole {
    amount: f64,
    state: String,
    source: String,
    kind: Kind,
    resolution: Option<f64>,
}

/// A row its source has no event of, read for whose task it is.
///
/// A source that watches only the launched process reads nothing of a child
/// it started, and the decomposition writes that as a zero; a zero there is
/// what the source did not see, not what the child did, so it is unread.
///
/// The other way round for the program's own threads, which the sched
/// sources watch at every switch: no event of a thread that never left its
/// cpu is the decomposition's "missing", and is a zero -- a spinning thread
/// never blocked -- as long as the source lost no event (a late drain loses
/// none). A lossy source's silence stays unread.
fn unwatched(run: &Run, row: &Row, task: &str, another_process: bool) -> Row {
    let mut row = row.clone();
    if row.readings != 0 {
        return row;
    }
    if another_process && row.missing.is_none() {
        row.missing = Some(format!(
            "`{}` reported no event of {task}: a source that does not follow the program's \
             children reads nothing of them, so this is not read as a zero",
            row.source
        ));
        row.amount = None;
        return row;
    }
    let watches_every_switch = matches!(row.state.as_str(), BLOCKED | RUNNABLE);
    let clean = run
        .output
        .results
        .iter()
        .find(|results| results.source == row.source)
        .is_some_and(|results| {
            // Events lost, not ticks late: a drain that ran late still read
            // every block the kernel counted.
            let losses = &results.losses;
            losses.dropped == 0
                && losses.failed_reads == 0
                && losses.undecoded == 0
                && losses.evicted == 0
        });
    if !another_process && watches_every_switch && row.amount.is_none() && clean {
        row.amount = Some(0.0);
        row.missing = None;
        row.note = Some(format!(
            "`{}` watches every switch of the program's threads and lost no event, and saw \
             none of {task}'s: zero",
            row.source
        ));
    }
    row
}

/// What a task's rows add up to, read ones only.
fn accounted(table: &Breakdown) -> f64 {
    table.rows.iter().filter_map(|row| row.amount).sum()
}

/// A table's amount for one row.
fn row_amount(table: &Breakdown, state: &str) -> f64 {
    table
        .rows
        .iter()
        .filter(|row| row.state == state)
        .filter_map(|row| row.amount)
        .sum()
}

/// The thread id a task key names: `task:4242`.
fn task_number(task: &str) -> Option<u64> {
    task.strip_prefix("task:")?.parse().ok()
}

/// The process of every task an event tied to one: the sched sources carry
/// `pid` beside the task, the profile does not.
fn pids(run: &Run) -> BTreeMap<String, u64> {
    let mut pids = BTreeMap::new();
    for event in &run.events {
        let (Some(key), Some(pid)) = (event.key(KeyKind::Task), attr(event, "pid")) else {
            continue;
        };
        if pid > 0.0 {
            pids.entry(key.to_string()).or_insert(pid as u64);
        }
    }
    pids
}

/// The wait for a child the task's heaviest blocking stack in `state` goes
/// through, by the function's name: `wait4`, `waitpid`, `waitid`, `wait3`,
/// `wait`, whatever the C library calls its own copy.
fn waits_for_child(run: &Run, task: &str, state: &str) -> Option<String> {
    let results = run.result(BLOCKED)?;
    let mut by_stack: BTreeMap<u64, f64> = BTreeMap::new();
    for event in run
        .events_of("sched.offcpu")
        .filter(|event| is_task(event, task))
        .filter(|event| event.attr("state").map(ToString::to_string).as_deref() == Some(state))
    {
        if let Some(id) = attr(event, STACK_ID_ATTR) {
            *by_stack.entry(id as u64).or_default() +=
                attr(event, "total_ns").unwrap_or(0.0) - attr(event, "queued_ns").unwrap_or(0.0);
        }
    }
    let (id, _) = by_stack.into_iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    results
        .stacks
        .get(&id)?
        .iter()
        .rev()
        .find_map(|frame| is_wait_for_child(frame).then(|| frame.clone()))
}

/// Whether a frame is a wait for a child, by its function's name.
pub fn is_wait_for_child(frame: &str) -> bool {
    let name = frame
        .split(['+', '@', ' ', '('])
        .next()
        .unwrap_or("")
        .trim_start_matches('_');
    let name = name
        .strip_prefix("GI_")
        .unwrap_or(name)
        .trim_start_matches('_');
    matches!(
        name,
        "wait4" | "waitpid" | "waitid" | "wait3" | "wait" | "libc_wait"
    ) || name.starts_with("wait4")
        || name.starts_with("waitpid")
        || name.starts_with("waitid")
}

/// What the run can tell about which task is which, while a reading is
/// followed from task to task.
struct Tasks<'a> {
    run: &'a Run,
    tables: &'a [Breakdown],
    pid_of: BTreeMap<String, u64>,
    /// Tasks already read, which are never followed into again.
    seen: BTreeSet<String>,
    /// Processes already on the way, which a child is never one of.
    processes: BTreeSet<u64>,
}

impl Tasks<'_> {
    /// Follows `reading`'s largest row into the task that ran in it, if it is
    /// a wait another task ran in, and that task's the same way.
    ///
    /// `process` is the reading's own process, when the run said which.
    fn follow(&mut self, reading: &mut Reading, process: Option<u64>, depth: usize) {
        if depth >= MAX_FOLLOWED {
            return;
        }
        let Some(top) = reading.top() else {
            return;
        };
        if !top.state.starts_with(BLOCKED)
            || !matches!(top.limit, Some(Limit::Sleep | Limit::Futex))
        {
            return;
        }
        let during = top.state.clone();
        let block = top.amount.unwrap_or(0.0);
        let (block_kind, block_resolution) = (top.kind, top.resolution);
        if block <= 0.0 {
            return;
        }
        let state = during
            .strip_prefix(&format!("{BLOCKED}: "))
            .unwrap_or("sleeping")
            .to_owned();
        let wait = waits_for_child(self.run, &reading.task, &state);
        let child = self.child(process);
        let thread = self.busiest_thread(process, &reading.task);
        let enough = |table: &Breakdown| accounted(table) >= block * RAN_IN_IT;
        let chosen = match (&wait, child, thread) {
            (Some(_), Some(child), _) => Some((Relation::Child, child)),
            (_, _, Some(thread)) if enough(thread) => Some((Relation::Thread, thread)),
            (_, Some(child), _) if enough(child) => Some((Relation::Child, child)),
            _ => None,
        };
        let Some((relation, table)) = chosen else {
            return;
        };
        let task = table.key.clone();
        let (whole, because) = match relation {
            Relation::Child => (
                GivenWhole {
                    amount: block,
                    state: "whole: the wait".to_owned(),
                    source: reading.task.clone(),
                    kind: block_kind,
                    resolution: block_resolution,
                },
                match &wait {
                    Some(wait) => format!(
                        "the child process that ran longest while {} waited in `{wait}`; its \
                         whole is that wait",
                        reading.task
                    ),
                    None => format!(
                        "the child process that ran longest while {} was `{during}`; its whole \
                         is that block",
                        reading.task
                    ),
                },
            ),
            Relation::Thread => (
                GivenWhole {
                    amount: accounted(table),
                    state: "whole: its rows".to_owned(),
                    source: "the rows, added up".to_owned(),
                    kind: block_kind,
                    resolution: table
                        .rows
                        .iter()
                        .map(|row| row.resolution)
                        .sum::<Option<f64>>(),
                },
                format!(
                    "the busiest other thread of the process while {} was `{during}`; its whole \
                     is what its own rows account for, a thread's life being no source's row",
                    reading.task
                ),
            ),
        };
        let mut followed = read_table(
            self.run,
            table,
            &task,
            because,
            Some(whole),
            relation == Relation::Child,
        );
        if relation == Relation::Thread {
            // A thread of the program is watched by every source: its zeros
            // stand, and its residue is zero by construction, so none is said.
            followed.residue = None;
        }
        self.seen.insert(task.clone());
        let below = match relation {
            Relation::Child => self.pid_of.get(&task).copied(),
            Relation::Thread => process,
        };
        if let Some(below) = below {
            self.processes.insert(below);
        }
        self.follow(&mut followed, below, depth + 1);
        reading.followed = Some(Box::new(Followed {
            relation,
            during,
            amount: block,
            wait: if relation == Relation::Child {
                wait
            } else {
                None
            },
            reading: followed,
        }));
    }

    /// The child that ran longest: the task, among those of processes not on
    /// the way, whose rows account for the most. A process's main thread
    /// stands for it when the run saw it. A task no source tied to a process
    /// is one the sched sources, which watch every thread of the program,
    /// never saw: by elimination another process.
    fn child(&self, process: Option<u64>) -> Option<&Breakdown> {
        let mut best: Option<(&Breakdown, f64)> = None;
        for table in self.tables {
            if self.seen.contains(&table.key) {
                continue;
            }
            let representative = match self.pid_of.get(&table.key) {
                Some(pid) if Some(*pid) == process || self.processes.contains(pid) => continue,
                Some(pid) => {
                    let main = format!("task:{pid}");
                    if table.key != main
                        && self.tables.iter().any(|other| other.key == main)
                        && !self.seen.contains(&main)
                    {
                        // Its process's main thread stands for it.
                        continue;
                    }
                    table
                }
                None => table,
            };
            let weight = accounted(representative);
            if best.is_none_or(|(_, most)| weight > most) {
                best = Some((representative, weight));
            }
        }
        best.map(|(table, _)| table)
    }

    /// The busiest other thread of `process`: the most time on a cpu or
    /// waiting for one.
    fn busiest_thread(&self, process: Option<u64>, task: &str) -> Option<&Breakdown> {
        let process = process?;
        self.tables
            .iter()
            .filter(|table| table.key != task && !self.seen.contains(&table.key))
            .filter(|table| self.pid_of.get(&table.key) == Some(&process))
            .map(|table| {
                (
                    table,
                    row_amount(table, ON_CPU) + row_amount(table, RUNNABLE),
                )
            })
            .filter(|(_, busy)| *busy > 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(table, _)| table)
    }
}

/// The sources that cost the target more than [`COST_CRITERION_PERCENT`] of
/// its cpu, said with the band the catalog declares for them.
fn costly(run: &Run) -> Vec<String> {
    run.costs
        .iter()
        .filter_map(|cost| {
            let percent = cost
                .percent
                .filter(|percent| *percent > COST_CRITERION_PERCENT)?;
            Some(format!(
                "[{}] {} through `{}` cost {percent:.1} % of the target's cpu ({}), over the \
                 {COST_CRITERION_PERCENT:.0} % past which a source dilates what it measures; \
                 declared `{}`",
                cost.label,
                cost.source,
                cost.backend,
                cost.observed_from,
                cost.declared.as_str()
            ))
        })
        .collect()
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
            symbols: None,
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
            costly: Vec::new(),
            followed: None,
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

    fn followed(
        relation: Relation,
        wait: Option<&str>,
        above: &Reading,
        below: Reading,
    ) -> Reading {
        let mut above = above.clone();
        let top = above.top().unwrap();
        above.followed = Some(Box::new(Followed {
            relation,
            during: top.state.clone(),
            amount: top.amount.unwrap(),
            wait: wait.map(str::to_owned),
            reading: below,
        }));
        above
    }

    fn of_task(mut reading: Reading, task: &str) -> Reading {
        reading.task = task.to_owned();
        reading
    }

    /// A driver that waited its whole life for a compiler: the compiler's
    /// decomposition takes the wait's place, and the verdict names the
    /// compiler's state and its task, never the driver's sleep.
    #[test]
    fn a_wait_for_a_child_is_answered_by_the_child_that_ran_during_it() {
        let driver = reading(
            100e6,
            vec![
                state("on-cpu", Limit::Cpu, Some(1e6)),
                state("blocked: sleeping", Limit::Sleep, Some(99e6)),
            ],
            0.0,
        );
        let compiler = of_task(
            reading(
                99e6,
                vec![
                    state("on-cpu", Limit::Cpu, Some(30e6)),
                    state("runnable", Limit::CpuQueue, Some(65e6)),
                    state("blocked: sleeping", Limit::Sleep, Some(1e6)),
                ],
                0.0,
            ),
            "task:2",
        );
        let run = followed(Relation::Child, Some("wait4"), &driver, compiler);
        let verdict = latency(&run, 1);
        assert_eq!(verdict.limit, Some(Limit::CpuQueue), "{}", verdict.sentence);
        assert_eq!(verdict.task.as_deref(), Some("task:2"));
        assert!(
            verdict.sentence.contains("`wait4`") && verdict.sentence.contains("task:2"),
            "{}",
            verdict.sentence
        );
        // Unfollowed, the same driver is named asleep.
        assert_eq!(latency(&driver, 1).limit, Some(Limit::Sleep));
    }

    /// A child no sched source watched has its on-cpu row and nothing else:
    /// the wait it stands in is mostly unread, and the verdict says so rather
    /// than naming the one row it has.
    #[test]
    fn a_child_whose_rows_were_not_watched_is_undecided_rather_than_cpu_bound() {
        let driver = reading(
            100e6,
            vec![state("blocked: sleeping", Limit::Sleep, Some(99e6))],
            0.0,
        );
        let compiler = of_task(
            reading(
                99e6,
                vec![
                    state("on-cpu", Limit::Cpu, Some(30e6)),
                    state("runnable", Limit::CpuQueue, None),
                    state("blocked", Limit::Sleep, None),
                ],
                1e6,
            ),
            "task:2",
        );
        let verdict = latency(&followed(Relation::Child, None, &driver, compiler), 1);
        assert_eq!(verdict.limit, None, "{}", verdict.sentence);
        assert!(
            verdict.sentence.contains("runnable"),
            "the unread rows are named: {}",
            verdict.sentence
        );
    }

    /// A server's main thread asleep in its accept loop while a connection's
    /// thread did the work: the thread is named beside the block.
    #[test]
    fn a_block_another_thread_ran_in_names_that_thread_beside_it() {
        let main = reading(
            100e6,
            vec![
                state("on-cpu", Limit::Cpu, Some(2e6)),
                state("blocked: sleeping", Limit::Sleep, Some(98e6)),
            ],
            0.0,
        );
        let worker = of_task(
            reading(
                90e6,
                vec![
                    state("on-cpu", Limit::Cpu, Some(88e6)),
                    state("runnable", Limit::CpuQueue, Some(2e6)),
                ],
                0.0,
            ),
            "task:7",
        );
        let run = followed(Relation::Thread, None, &main, worker);
        let verdict = latency(&run, 1);
        assert_eq!(verdict.limit, Some(Limit::Cpu), "{}", verdict.sentence);
        assert_eq!(verdict.task.as_deref(), Some("task:7"));
        assert!(
            verdict.sentence.contains("beside it"),
            "{}",
            verdict.sentence
        );
    }

    #[test]
    fn a_tail_followed_the_same_way_in_both_runs_subtracts_the_followed_tasks() {
        let driver = |wall: f64| {
            reading(
                wall,
                vec![state("blocked: sleeping", Limit::Sleep, Some(wall))],
                0.0,
            )
        };
        let compiler = |queued: f64| {
            of_task(
                reading(
                    100e6 + queued,
                    vec![
                        state("on-cpu", Limit::Cpu, Some(100e6)),
                        state("runnable", Limit::CpuQueue, Some(queued)),
                    ],
                    0.0,
                ),
                "task:2",
            )
        };
        let typical = followed(
            Relation::Child,
            Some("wait4"),
            &driver(101e6),
            compiler(1e6),
        );
        let slow = followed(
            Relation::Child,
            Some("wait4"),
            &driver(200e6),
            compiler(100e6),
        );
        let verdict = tail(&slow, &typical, 5);
        assert_eq!(verdict.limit, Some(Limit::CpuQueue), "{}", verdict.sentence);
    }

    #[test]
    fn only_a_futex_sleep_asks_the_mutex_source_for_a_second_pass() {
        let run = |limit: Limit| {
            reading(
                100e6,
                vec![
                    state("on-cpu", Limit::Cpu, Some(10e6)),
                    state("blocked: sleeping", limit, Some(90e6)),
                ],
                0.0,
            )
        };
        assert!(needs_split(&run(Limit::Futex), true).is_some());
        assert!(needs_split(&run(Limit::Sleep), true).is_none());
        assert!(needs_split(&run(Limit::Lock), true).is_none());
        // Without the futex source, a sleep could be a mutex as well as not.
        assert!(needs_split(&run(Limit::Sleep), false).is_some());
        let cpu = reading(100e6, vec![state("on-cpu", Limit::Cpu, Some(90e6))], 0.0);
        assert!(needs_split(&cpu, false).is_none());
    }

    #[test]
    fn the_first_pass_asks_no_mutex_and_both_stacks_carry_the_symbols_asked() {
        let mut asked = ask(Objective::Latency, 1);
        asked.symbols = Some(SymbolSource::Debuginfod);
        let first = asked.session();
        assert!(first
            .requests
            .iter()
            .all(|request| request.label.as_deref() != Some(MUTEX)));
        let second = asked.split_session();
        assert!(second
            .requests
            .iter()
            .any(|request| request.label.as_deref() == Some(MUTEX)));
        for request in &first.requests {
            let reads_stacks = request.stack.is_some();
            assert_eq!(
                request.symbols,
                reads_stacks.then_some(SymbolSource::Debuginfod),
                "{:?}",
                request.label
            );
        }
        assert_eq!(
            first
                .requests
                .iter()
                .filter(|r| r.symbols.is_some())
                .count(),
            2
        );
    }

    #[test]
    fn a_wait_for_a_child_is_known_by_its_function_s_name() {
        for frame in [
            "wait4",
            "__wait4",
            "__GI___wait4",
            "waitpid",
            "__waitid",
            "wait",
        ] {
            assert!(is_wait_for_child(frame), "{frame}");
        }
        for frame in [
            "epoll_wait",
            "pthread_cond_wait",
            "libc.so.6+0xa08f2",
            "select",
            "__futex_abstimed_wait_common",
        ] {
            assert!(!is_wait_for_child(frame), "{frame}");
        }
    }

    #[test]
    fn the_sleep_sentence_names_a_polled_lock_and_a_wait_for_a_child() {
        let said = Limit::Sleep.describe();
        assert!(said.contains("polled") && said.contains("child"), "{said}");
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
