//! The `timebar` view: where the wall-clock went, one bar per thread or task,
//! by state -- with the first suspect named on it.
//!
//! **What a bar is.** One table of a decomposition, drawn: a task's (or a
//! request's) whole -- the program's life, a request's span -- split into the
//! states its rows read: on cpu, waiting in the run queue, blocked on a disk,
//! asleep on a lock, waiting for a child, on the network, asleep. The bar is
//! the table `omn latency` reads, laid along time, so the share each state
//! takes is seen before it is read, and the residue -- what no row accounts
//! for, signed -- is said beside it rather than rounded away.
//!
//! **Where the bars come from.** Two places, and the screen says which:
//!
//! - **A decomposition in the run** (`omn latency`'s session carries
//!   `where-the-time-went`; a session file may carry any). Each table is a
//!   bar, its rows exactly as written. The `blocked` row is split by the task
//!   state each block recorded when the run holds `sched.offcpu`'s events, the
//!   way `omn latency` splits it; a recording that kept only the stacks cannot
//!   be split, and says so.
//! - **The view's own reading** of the traced sources, per thread over the
//!   run's window, when no decomposition splits the run by task: blocked by
//!   task state from `sched.offcpu`'s events, the run queue from
//!   `sched.runqlat`, the part of a sleep `sched.futex` saw waiting in
//!   futex(2) split out of it as a lock, and on cpu from `cpu.oncpu_time`. That last one is a per-process total on
//!   this machine (`fs:proc`), so a thread's share of it is a **bracket**: at
//!   least the process's total less what its other threads could have run, at
//!   most what its own other states leave of the window -- drawn solid to the
//!   first and shaded to the second, never as a number it is not.
//!
//! **The first suspect** is named by `omn latency`'s rule
//! (`src/triage/latency.rs`, `latency()`): the largest state read, when it is
//! ahead of the next one and of everything no row read (the rows' own
//! resolution, the whole's remainder when a row is missing). Otherwise the bar
//! names no single state, and says why. A view cannot call the triage -- this
//! crate cannot depend on `omn` -- so the rule is restated here, and
//! `tests/view_timebar.rs` runs the triage itself on every fixture carrying
//! its decomposition and holds this restatement to the state it names. One
//! difference is on purpose: the next state counts at the most it can be,
//! where the triage takes the second-largest amount. The two agree on every
//! decomposition, whose rows carry no bracket; only the view's own reading
//! brackets a row.
//!
//! The view reads only `ctx`: it never opens a source.

use std::collections::{BTreeMap, BTreeSet};

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use serde::Deserialize;

use super::{Action, Ctx, Focus, Need, Region, Scope, View};
use crate::model::{Data, Event, Output, Results};
use crate::trust::{palette, Trust};
use crate::units;

/// What this view needs measured.
pub const NEEDS: &[Need] = &[
    Need {
        source: "cpu.profile",
        mode: "sample@997hz",
        emit: &["stack"],
        keys: &[],
        stack: Some("fp"),
        scope: Scope::Target,
        why: "on cpu",
    },
    Need {
        source: "sched.runqlat",
        mode: "trace",
        emit: &["raw", "summary"],
        keys: &["task"],
        stack: None,
        scope: Scope::Target,
        why: "waiting to run",
    },
    Need {
        source: "sched.offcpu",
        mode: "trace",
        emit: &["raw", "stack"],
        keys: &[],
        stack: Some("fp"),
        scope: Scope::Target,
        why: "blocked, by the task state each block recorded, with the stack it blocked in",
    },
    Need {
        source: "cpu.oncpu_time",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &["task"],
        stack: None,
        scope: Scope::Target,
        why: "on cpu per process, which brackets each thread's share",
    },
    Need {
        source: "sched.futex",
        mode: "trace",
        emit: &["summary"],
        keys: &["task"],
        stack: None,
        scope: Scope::Target,
        why: "waiting on a lock",
    },
];

/// The decomposition `omn latency` writes, whose verdict the bars restate.
pub const LATENCY_DECOMPOSITION: &str = "where-the-time-went";

/// A thread whose states account for less than this share of the window is
/// folded into one line: a helper that ran for a millisecond in a three-second
/// run is a sliver no bar can show.
pub const FOLD_BELOW: f64 = 0.01;

/// The kernel's cpu-time tick (`USER_HZ`), the resolution `cpu.oncpu_time`
/// through `/proc` says on its events; used when the run does not carry it.
const TICK_NS: f64 = 10_000_000.0;

/// A state, as the bar draws it: its fill, its colour, its words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    OnCpu,
    RunQueue,
    Lock,
    Io,
    Blocked,
    Child,
    Network,
    Sleep,
    Other,
}

impl Class {
    /// The class of a decomposition's row, by the state it names.
    pub fn of(state: &str) -> Class {
        let state = state.to_ascii_lowercase();
        match state.as_str() {
            "on-cpu" | "cpu_on" | "oncpu" | "on cpu" | "cpu" => Class::OnCpu,
            "runnable" | "sched_wait" | "runq" | "run queue" => Class::RunQueue,
            "lock_wait" | "lock" | "futex" | "mutex" => Class::Lock,
            "block_io" | "device" | "disk" | "io" | "blocked: uninterruptible" => Class::Io,
            "blocked" | "offcpu" | "off-cpu" => Class::Blocked,
            "blocked: sleeping" | "sleep" | "sleeping" => Class::Sleep,
            "child" | "wait_child" => Class::Child,
            _ if state.starts_with("net") => Class::Network,
            _ if state.contains("child") => Class::Child,
            _ => Class::Other,
        }
    }

    /// The cell a state fills its share of the bar with: a different one per
    /// state, so the bar reads in plain text as it does in colour.
    pub fn glyph(self) -> char {
        match self {
            Class::OnCpu => '█',
            Class::RunQueue => '▚',
            Class::Lock => '▞',
            Class::Io => '▓',
            Class::Blocked => '▓',
            Class::Child => '▙',
            Class::Network => '▄',
            Class::Sleep => '░',
            Class::Other => '▀',
        }
    }

    /// Sixteen-colour names only, as the trust palette: the terminal's theme
    /// decides what they look like.
    pub fn colour(self) -> Color {
        match self {
            Class::OnCpu => Color::Green,
            Class::RunQueue => Color::Magenta,
            Class::Lock => Color::Red,
            Class::Io => Color::Blue,
            Class::Blocked => Color::Blue,
            Class::Child => Color::LightMagenta,
            Class::Network => Color::Cyan,
            Class::Sleep => Color::Gray,
            Class::Other => Color::DarkGray,
        }
    }

    /// A word or two, for the legend and the label inside a bar.
    pub fn short(self) -> &'static str {
        match self {
            Class::OnCpu => "on cpu",
            Class::RunQueue => "run queue",
            Class::Lock => "lock",
            Class::Io => "I/O",
            Class::Blocked => "blocked",
            Class::Child => "child",
            Class::Network => "network",
            Class::Sleep => "sleep",
            Class::Other => "other",
        }
    }

    /// What a task limited by this state is doing, in `omn latency`'s words.
    pub fn limit(self) -> &'static str {
        match self {
            Class::OnCpu => "on a cpu, doing its own work: cpu-bound",
            Class::RunQueue => "runnable and waiting for a cpu: the cpus it may use are busy",
            Class::Lock => "asleep on a lock another thread held: lock-contended",
            Class::Io => "blocked on a device, usually a disk: I/O-bound",
            Class::Blocked => {
                "off cpu, blocked -- not split by task state here, so a disk and a sleep are not \
                 told apart"
            }
            Class::Child => "waiting for a child process: the work is in the child",
            Class::Network => "waiting on the network: the peer, or the wire",
            Class::Sleep => {
                "asleep on something that is not a lock seen here: a timer, a pipe, a socket, \
                 a condition"
            }
            Class::Other => "in a state that is neither on cpu, queued, sleeping nor on a disk",
        }
    }
}

/// One state of one bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub class: Class,
    /// As the decomposition writes it (`on-cpu`, `lock_wait`), or as the view
    /// names a split (`blocked: uninterruptible`).
    pub state: String,
    pub source: String,
    /// In nanoseconds; `None` when it could not be read, `missing` says why.
    pub amount: Option<f64>,
    /// Where the amount is only bracketed, its upper end: the amount is then
    /// the lower end.
    pub upper: Option<f64>,
    pub resolution: Option<f64>,
    pub readings: u64,
    /// `traced`, `sampled`, `polled`...
    pub kind: String,
    /// Its source lost records: the amount is a lower bound.
    pub lower_bound: bool,
    pub missing: Option<String>,
    /// What names it: a stack, a lock holder, a function.
    pub named: Vec<String>,
    pub note: Option<String>,
}

impl Part {
    fn new(class: Class, state: impl Into<String>, source: impl Into<String>) -> Part {
        Part {
            class,
            state: state.into(),
            source: source.into(),
            amount: None,
            upper: None,
            resolution: None,
            readings: 0,
            kind: String::new(),
            lower_bound: false,
            missing: None,
            named: Vec::new(),
            note: None,
        }
    }

    /// The most it can be: its upper end, or its amount.
    fn most(&self) -> Option<f64> {
        self.upper.or(self.amount)
    }
}

/// What a bar's numbers name, by `omn latency`'s rule.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// The largest state is decisive.
    Names {
        part: usize,
        amount: f64,
        /// Of the whole, when there is one.
        share: Option<f64>,
        /// The most the next state can be, and what no row read: what the
        /// largest is ahead of.
        next: f64,
        room: f64,
    },
    /// No single state: why.
    Undecided(String),
}

/// One bar: a task's or a request's whole, by state.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    /// As the decomposition keys it: `task:4242`, `req:a001`.
    pub key: String,
    pub comm: Option<String>,
    pub pid: Option<i32>,
    pub tid: Option<i32>,
    pub request: Option<String>,
    pub whole: Option<f64>,
    pub whole_resolution: Option<f64>,
    pub parts: Vec<Part>,
    /// The whole less the parts, signed; `None` when a part is unread.
    pub residue: Option<f64>,
    pub residue_resolution: Option<f64>,
    /// Why the residue is not given, when it is not for want of a row: the
    /// view's reading derives on cpu from the window, so its rows add up to
    /// the whole by construction and their difference measures nothing.
    pub residue_says: Option<String>,
    pub trust: Trust,
    pub verdict: Verdict,
}

impl Bar {
    /// What the parts read add up to, lower ends.
    pub fn read(&self) -> f64 {
        self.parts.iter().filter_map(|part| part.amount).sum()
    }

    /// The most its parts can add up to: each at its upper end.
    pub fn most_read(&self) -> f64 {
        self.parts.iter().filter_map(Part::most).sum()
    }

    /// The longest the bar is drawn: its whole, or its parts' upper ends when
    /// they overshoot it.
    fn extent(&self) -> f64 {
        self.whole.unwrap_or(0.0).max(self.most_read())
    }

    /// The bar's name in a few cells: the thread and its command, or the key.
    fn label(&self) -> String {
        match (self.tid, &self.comm) {
            (Some(tid), Some(comm)) => format!("{tid} {comm}"),
            (Some(tid), None) => format!("task {tid}"),
            _ => self.key.clone(),
        }
    }

    /// The suspect, in a few words, for the bar's line.
    fn suspect_short(&self) -> (String, Style) {
        match &self.verdict {
            Verdict::Names {
                part,
                amount,
                share,
                ..
            } => {
                let part = &self.parts[*part];
                let bound = if part.lower_bound || part.upper.is_some() {
                    "≥"
                } else {
                    ""
                };
                let figure = match share {
                    Some(share) => format!("{bound}{}", units::percent(share * 100.0)),
                    None => format!("{bound}{}", units::duration(*amount)),
                };
                (
                    format!("▶ {} {figure}", part.class.short()),
                    Style::new()
                        .fg(part.class.colour())
                        .add_modifier(Modifier::BOLD),
                )
            }
            Verdict::Undecided(_) => ("≈ no single state".to_owned(), dim()),
        }
    }
}

/// One way of splitting the run into bars: a decomposition, or the view's
/// reading of the traced sources.
#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    pub id: String,
    /// `task`, `req`, `thread`.
    pub by: String,
    /// What a bar's whole is, in words.
    pub whole_says: String,
    /// The view's own reading rather than a decomposition the run carries.
    pub reading: bool,
    pub bars: Vec<Bar>,
    /// Bars too short to draw, by label.
    pub folded: Vec<String>,
    /// Which bar is the program's, and how it was chosen.
    pub main: Option<(usize, String)>,
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// The rule
// ---------------------------------------------------------------------------

/// `omn latency`'s verdict on one table: the largest state read, if it is
/// ahead of the next one (at the most that one could be) and of everything
/// no row read; otherwise no single state, and why.
///
/// What no row read: when a part is missing, the whole less what was read
/// plus the residue's resolution; when every part was read, their own
/// resolutions -- the whole's bracket moves the residue, not a row.
pub fn decide(parts: &[Part], whole: Option<f64>, residue_resolution: Option<f64>) -> Verdict {
    let mut known: Vec<(usize, f64)> = parts
        .iter()
        .enumerate()
        .filter_map(|(index, part)| part.amount.map(|amount| (index, amount)))
        .collect();
    known.sort_by(|a, b| b.1.total_cmp(&a.1));
    let Some(&(top, amount)) = known.first() else {
        return Verdict::Undecided("no state could be read".to_owned());
    };
    let next = known
        .iter()
        .skip(1)
        .filter_map(|(index, _)| parts[*index].most())
        .fold(0.0, f64::max);
    let read: f64 = known.iter().map(|(_, amount)| amount).sum();
    let unread: Vec<&str> = parts
        .iter()
        .filter(|part| part.amount.is_none())
        .map(|part| part.state.as_str())
        .collect();
    let room = match whole {
        Some(whole) if !unread.is_empty() => {
            (whole - read).max(0.0) + residue_resolution.unwrap_or(0.0)
        }
        _ => known
            .iter()
            .filter_map(|(index, _)| parts[*index].resolution)
            .sum(),
    };
    let state = &parts[top].state;
    if amount > next && amount > room {
        Verdict::Names {
            part: top,
            amount,
            share: whole
                .filter(|whole| *whole > 0.0)
                .map(|whole| amount / whole),
            next,
            room,
        }
    } else if amount <= next {
        Verdict::Undecided(format!(
            "`{state}` and the next state are {} and {}: neither is decisive",
            units::duration(amount),
            units::duration(next)
        ))
    } else if unread.is_empty() {
        Verdict::Undecided(format!(
            "`{state}` is {}, within the rows' own resolution ({}) of being overtaken",
            units::duration(amount),
            units::duration(room)
        ))
    } else {
        Verdict::Undecided(format!(
            "`{state}` is {}, and {} of the whole is in no row that was read ({}): the largest \
             row could still be overtaken",
            units::duration(amount),
            units::duration(room),
            unread.join(", ")
        ))
    }
}

// ---------------------------------------------------------------------------
// Reading the run
// ---------------------------------------------------------------------------

/// A decomposition as `omn` writes it (`src/join.rs`, `Decomposition`).
#[derive(Debug, Deserialize)]
struct Written {
    id: String,
    #[serde(default)]
    by: String,
    #[serde(default)]
    unit: String,
    #[serde(default)]
    tables: Vec<Table>,
}

#[derive(Debug, Deserialize)]
struct Table {
    key: String,
    whole: Row,
    #[serde(default)]
    rows: Vec<Row>,
    residue: Option<f64>,
    residue_resolution: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct Row {
    #[serde(default)]
    state: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    kind: String,
    amount: Option<f64>,
    resolution: Option<f64>,
    #[serde(default)]
    readings: u64,
    short: Option<serde_json::Value>,
    missing: Option<String>,
    note: Option<String>,
    named: Option<Named>,
}

#[derive(Debug, Clone, Deserialize)]
struct Named {
    attr: String,
    value: String,
    #[serde(default)]
    events: u64,
    #[serde(default)]
    of: u64,
}

/// Every way the run can be drawn as bars, in order: its decompositions, then
/// the view's reading when none of them splits the run by task.
pub fn sheets(data: &Data) -> Vec<Sheet> {
    let threads = Threads::of(data);
    let (written, unreadable) = decompositions(data);
    let mut sheets: Vec<Sheet> = written
        .into_iter()
        .map(|written| from_decomposition(data, &threads, written))
        .collect();
    if !sheets.iter().any(|sheet| sheet.by == "task") {
        if let Some(reading) = reading(data, &threads) {
            sheets.push(reading);
        }
    }
    for sheet in &mut sheets {
        sheet
            .notes
            .extend(unreadable.iter().map(|why| format!("Not drawn: {why}.")));
    }
    sheets
}

/// The run's decompositions the view can draw, and why each other one is not
/// drawn: never dropped without a word.
fn decompositions(data: &Data) -> (Vec<Written>, Vec<String>) {
    let mut drawn = Vec::new();
    let mut not = Vec::new();
    for value in &data.run.decompositions {
        let id = value
            .get("id")
            .and_then(|id| id.as_str())
            .unwrap_or("a decomposition")
            .to_owned();
        match serde_json::from_value::<Written>(value.clone()) {
            Err(error) => not.push(format!("`{id}` could not be read ({error})")),
            Ok(written) if written.unit != "ns" => not.push(format!(
                "`{id}` is in `{}`, not nanoseconds, and a bar is a time",
                written.unit
            )),
            Ok(written) if written.tables.is_empty() => not.push(format!("`{id}` holds no table")),
            Ok(written) => drawn.push(written),
        }
    }
    (drawn, not)
}

/// The sources whose events describe the thread they are keyed on.
const THREAD_SOURCES: [&str; 5] = [
    "sched.offcpu",
    "sched.runqlat",
    "sched.switches",
    "sched.futex",
    "sched.mutex",
];

/// What the run's events say of each thread: its process and its command.
#[derive(Debug, Default)]
struct Threads {
    pid: BTreeMap<i32, i32>,
    comm: BTreeMap<i32, String>,
}

impl Threads {
    fn of(data: &Data) -> Threads {
        let mut threads = Threads::default();
        let mut named_at: BTreeMap<i32, u64> = BTreeMap::new();
        for results in &data.run.results {
            // The sources whose events are the thread's own: a wakeup's
            // `comm` is its waker's, not its wakee's.
            if !THREAD_SOURCES.contains(&results.source.as_str()) {
                continue;
            }
            for event in results.events() {
                let Some(tid) = tid_of(event) else { continue };
                if let Some(pid) = event.number("pid").filter(|pid| *pid > 0.0) {
                    threads.pid.entry(tid).or_insert(pid as i32);
                }
                // The latest name a thread had: a launched program is `omn`
                // until it executes, and a worker renames itself once
                // started.
                if let Some(comm) = event.attr("comm").and_then(|comm| comm.as_str()) {
                    let at = named_at.entry(tid).or_insert(0);
                    if event.ts_ns >= *at {
                        *at = event.ts_ns;
                        threads.comm.insert(tid, comm.trim().to_owned());
                    }
                }
            }
        }
        threads
    }

    /// The program's main thread: one whose id is its process's.
    fn main(&self) -> Option<i32> {
        self.pid
            .iter()
            .find(|(tid, pid)| tid == pid)
            .map(|(tid, _)| *tid)
    }
}

fn tid_of(event: &Event) -> Option<i32> {
    event
        .tid()
        .or_else(|| event.number("tid").map(|tid| tid as i32))
}

fn task_of(key: &str) -> Option<i32> {
    key.strip_prefix("task:")?.parse().ok()
}

fn lossy(results: &Results) -> bool {
    results.losses.missing() > 0
}

fn trust_of(data: &Data, source: &str) -> Trust {
    Trust::joined(data, data.of_source(source))
}

/// The decomposition's tables as bars, their rows as written.
fn from_decomposition(data: &Data, threads: &Threads, written: Written) -> Sheet {
    let first = &written.tables[0].whole;
    let whole_says = if first.kind == "lifetime" {
        format!("{}, {}", first.source, first.value)
    } else {
        format!("`{}` {}", first.source, first.value)
    };
    let offcpu: Vec<&Results> = data.of_source("sched.offcpu").collect();
    let offcpu_events = by_thread(&offcpu);
    let futex = futex_waits(data);
    let several = written.tables.len() > 1;
    let mut notes = Vec::new();
    let mut bars = Vec::new();
    let mut unsplit = false;
    let mut disagree: Vec<String> = Vec::new();
    for table in &written.tables {
        let tid = task_of(&table.key);
        let mut parts = Vec::new();
        let mut trust = Trust::clean();
        // What the split's events add up to beyond the row they replace: the
        // residue moves by as much, so the bar and its residue still add up
        // to the whole.
        let mut shift = 0.0;
        for row in &table.rows {
            trust.merge(trust_of(data, &row.source));
            let class = Class::of(&row.state);
            let mut part = from_row(row, class);
            if class == Class::Blocked && row.amount.is_some() {
                if let Some(events) = tid.and_then(|tid| offcpu_events.get(&tid)) {
                    let lower = offcpu.iter().any(|results| lossy(results));
                    let waits = futex_of(&futex, tid.unwrap_or_default());
                    let split = split_blocked(data, events, waits, lower);
                    let summed: f64 = split.iter().filter_map(|part| part.amount).sum();
                    let row_amount = row.amount.unwrap_or(0.0);
                    if (summed - row_amount).abs() >= 1.0 {
                        shift += row_amount - summed;
                        disagree.push(format!(
                            "{}: {} in the row, {} in the events split",
                            table.key,
                            units::duration(row_amount),
                            units::duration(summed)
                        ));
                    }
                    parts.extend(split);
                    continue;
                }
                unsplit = true;
                part.named.extend(heaviest_block(data, several));
            }
            if class == Class::OnCpu && row.source == "cpu.profile" {
                part.named.extend(hottest_leaf(data, several));
            }
            parts.push(part);
        }
        let whole = table.whole.amount;
        if let Some(note) = &table.whole.note {
            if !several {
                notes.push(format!("The whole: {note}."));
            }
        }
        let verdict = decide(&parts, whole, table.residue_resolution);
        bars.push(Bar {
            key: table.key.clone(),
            comm: tid.and_then(|tid| threads.comm.get(&tid).cloned()),
            pid: tid.and_then(|tid| threads.pid.get(&tid).copied()),
            tid,
            request: table.key.strip_prefix("req:").map(str::to_owned),
            whole,
            whole_resolution: table.whole.resolution.filter(|res| *res > 0.0),
            parts,
            residue: table.residue.map(|residue| residue + shift),
            residue_resolution: table.residue_resolution,
            residue_says: None,
            trust,
            verdict,
        });
    }
    if unsplit {
        notes.push(
            "`blocked` is drawn whole: the run holds `sched.offcpu`'s stacks, not its events, so \
             a block cannot be split by the task state it recorded (`omn latency` splits it from \
             the events it holds while it runs). Its stack is the run's heaviest."
                .to_owned(),
        );
    }
    if !disagree.is_empty() {
        notes.push(format!(
            "`blocked` is split from `sched.offcpu`'s events, whose sum is not the row's ({}): \
             the residue carries the difference, so the bar and its residue still add up to \
             the whole.",
            disagree.join("; ")
        ));
    }
    let main = (written.by == "task")
        .then(|| main_bar(&bars, threads))
        .flatten();
    Sheet {
        id: written.id,
        by: written.by,
        whole_says,
        reading: false,
        bars,
        folded: Vec::new(),
        main,
        notes,
    }
}

fn from_row(row: &Row, class: Class) -> Part {
    let mut part = Part::new(class, row.state.clone(), row.source.clone());
    part.amount = row.amount;
    part.resolution = row.resolution;
    part.readings = row.readings;
    part.kind = row.kind.clone();
    part.lower_bound = row.short.is_some();
    part.missing = row.missing.clone();
    part.note = row.note.clone();
    if let Some(named) = &row.named {
        part.named.push(format!(
            "{} {}, on {} of its {} event(s)",
            named.attr.replace('_', " "),
            named.value,
            named.events,
            named.of
        ));
    }
    part
}

/// Which bar is the program's: its main thread, or the heaviest, said. None
/// when several processes' main threads are on screen: nothing in the run
/// says which of them was launched.
fn main_bar(bars: &[Bar], threads: &Threads) -> Option<(usize, String)> {
    let leaders: Vec<usize> = bars
        .iter()
        .enumerate()
        .filter(|(_, bar)| bar.tid.is_some() && bar.tid == bar.pid)
        .map(|(index, _)| index)
        .collect();
    match leaders.as_slice() {
        [one] => {
            return Some((
                *one,
                "its main thread, the one whose id is its process's".to_owned(),
            ))
        }
        [_, _, ..] => return None,
        [] => {}
    }
    if let Some(main) = threads.main() {
        if let Some(index) = bars.iter().position(|bar| bar.tid == Some(main)) {
            return Some((
                index,
                "its main thread, the one whose id is its process's".to_owned(),
            ));
        }
    }
    if bars.len() == 1 {
        return Some((0, "the only task the run decomposed".to_owned()));
    }
    let heaviest = bars
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.read().total_cmp(&b.1.read()))
        .map_or(0, |(index, _)| index);
    Some((
        heaviest,
        "no event carried both a process and a thread id, so the task whose rows add up to the \
         most"
            .to_owned(),
    ))
}

/// Every task's futex waits, and how many: `sched.futex`'s events when the
/// run holds them (a live run), its summary otherwise. A task the source
/// served and saw no wait of is absent: the caller says what that means.
fn futex_waits(data: &Data) -> Option<BTreeMap<i32, (f64, u64)>> {
    let mut found: Option<BTreeMap<i32, (f64, u64)>> = None;
    for results in data.of_source("sched.futex") {
        let waits = found.get_or_insert_with(BTreeMap::new);
        if results.output("raw").is_some() {
            for event in results.events() {
                let Some(tid) = tid_of(event) else { continue };
                let entry = waits.entry(tid).or_default();
                entry.0 += event
                    .number("total_ns")
                    .or_else(|| event.number("wait_ns"))
                    .unwrap_or(0.0);
                entry.1 += event.number("count").unwrap_or(1.0) as u64;
            }
        } else if let Some(Output::Summary { series }) = results.output("summary") {
            for series in series {
                if let Some(tid) = task_of(&series.key) {
                    let entry = waits.entry(tid).or_default();
                    entry.0 += series.mean * series.count as f64;
                    entry.1 += series.count;
                }
            }
        }
    }
    found
}

/// One task's futex waits, from [`futex_waits`]: zero when the source was
/// there and saw none of it.
fn futex_of(waits: &Option<BTreeMap<i32, (f64, u64)>>, tid: i32) -> Option<(f64, u64)> {
    waits
        .as_ref()
        .map(|waits| waits.get(&tid).copied().unwrap_or((0.0, 0)))
}

/// The run's events of some results, by the thread they are keyed on.
fn by_thread<'a>(results: &[&'a Results]) -> BTreeMap<i32, Vec<&'a Event>> {
    let mut by: BTreeMap<i32, Vec<&'a Event>> = BTreeMap::new();
    for results in results {
        for event in results.events() {
            if let Some(tid) = tid_of(event) {
                by.entry(tid).or_default().push(event);
            }
        }
    }
    by
}

/// What one state's blocks add up to: time, blocks, time by stack.
type Blocks = (f64, u64, BTreeMap<u64, f64>);

/// A task's blocks, split by the task state each recorded -- `total_ns` less
/// `queued_ns`, summed, as `omn latency` splits its `blocked` row -- with a
/// sleep whose stack waits for a child drawn apart, and the part of a sleep
/// `sched.futex` saw waiting in futex(2) split out of it as a lock.
fn split_blocked(
    data: &Data,
    events: &[&Event],
    futex: Option<(f64, u64)>,
    lower_bound: bool,
) -> Vec<Part> {
    let mut by_state: BTreeMap<String, Blocks> = BTreeMap::new();
    let mut child: Blocks = (0.0, 0, BTreeMap::new());
    let mut waits_for_child: BTreeMap<u64, bool> = BTreeMap::new();
    for event in events {
        let blocked =
            event.number("total_ns").unwrap_or(0.0) - event.number("queued_ns").unwrap_or(0.0);
        let state = event
            .attr("state")
            .map(ToString::to_string)
            .unwrap_or_else(|| "unknown".to_owned());
        let stack = event.number("stack_id").map(|id| id as u64);
        let blocks = event.number("blocks").unwrap_or(1.0) as u64;
        let to_child = state == "sleeping"
            && stack.is_some_and(|id| {
                *waits_for_child.entry(id).or_insert_with(|| {
                    stack_frames(data, id)
                        .iter()
                        .any(|frame| is_wait_for_child(frame))
                })
            });
        let entry = if to_child {
            &mut child
        } else {
            by_state.entry(state).or_default()
        };
        entry.0 += blocked;
        entry.1 += blocks;
        if let Some(id) = stack {
            *entry.2.entry(id).or_default() += blocked;
        }
    }
    // The futex waits come out of the sleep: as much of it as `sched.futex`
    // saw waiting in futex(2), never more than the sleep itself (a wait
    // counts its wake-to-run too, which is not sleep). What is left is
    // sleep; a sleep wholly on futexes is a lock and nothing else.
    let sleeping = by_state.get("sleeping").map_or(0.0, |entry| entry.0);
    let lock = futex
        .filter(|(waited, _)| *waited > 0.0 && sleeping > 0.0)
        .map(|(waited, waits)| (waited.min(sleeping), waited, waits));
    let mut parts = Vec::new();
    let mut push =
        |state: String, class: Class, (amount, blocks, stacks): Blocks, says: Option<String>| {
            let mut part = Part::new(class, state, "sched.offcpu");
            part.amount = Some(amount);
            part.resolution = Some(0.0);
            part.readings = blocks;
            part.kind = "traced".to_owned();
            part.lower_bound = lower_bound;
            part.named.extend(says);
            part.named.extend(stack_naming(data, &stacks));
            parts.push(part);
        };
    if child.1 > 0 {
        push(
            "blocked: sleeping, in a wait for a child".to_owned(),
            Class::Child,
            child,
            None,
        );
    }
    for (state, entry) in by_state {
        match state.as_str() {
            "uninterruptible" => push(
                "blocked: uninterruptible".to_owned(),
                Class::Io,
                entry,
                None,
            ),
            "sleeping" => {
                let Some((taken, waited, waits)) = lock else {
                    push("blocked: sleeping".to_owned(), Class::Sleep, entry, None);
                    continue;
                };
                let (amount, blocks, stacks) = entry;
                let says = format!(
                    "{} waiting in futex(2) over {waits} wait(s), from `sched.futex`",
                    units::duration(waited)
                );
                let rest = amount - taken;
                // The sleep's stacks name the lock too: they are the same
                // blocks, and which of them were the futex's is not known.
                push(
                    "blocked: sleeping, on a futex".to_owned(),
                    Class::Lock,
                    (taken, waits.min(blocks), stacks.clone()),
                    Some(says),
                );
                if rest > amount * 1e-6 {
                    push(
                        "blocked: sleeping".to_owned(),
                        Class::Sleep,
                        (rest, blocks.saturating_sub(waits), stacks),
                        Some("the sleep less the futex waits `sched.futex` saw".to_owned()),
                    );
                }
            }
            other => push(format!("blocked: {other}"), Class::Other, entry, None),
        }
    }
    parts
}

/// The frames of a stack id, root first, from whichever result holds it.
fn stack_frames(data: &Data, id: u64) -> &[String] {
    data.of_source("sched.offcpu")
        .map(|results| results.frames(id))
        .find(|frames| !frames.is_empty())
        .unwrap_or(&[])
}

/// Whether a frame is a wait for a child, by its function's name, as
/// `omn latency` reads it.
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

/// The heaviest of some blocking stacks, leaf first.
fn stack_naming(data: &Data, stacks: &BTreeMap<u64, f64>) -> Option<String> {
    let total: f64 = stacks.values().sum();
    let (id, amount) = stacks.iter().max_by(|a, b| a.1.total_cmp(b.1))?;
    let frames = stack_frames(data, *id);
    if frames.is_empty() || total <= 0.0 {
        return None;
    }
    Some(format!(
        "{:.0} % of it from one stack, leaf first: {}",
        amount / total * 100.0,
        leafward(frames)
    ))
}

/// The run's heaviest blocking stack, from `sched.offcpu`'s stack emit: what
/// names a `blocked` row the run holds no events of.
fn heaviest_block(data: &Data, several: bool) -> Option<String> {
    let mut stacks: BTreeMap<u64, f64> = BTreeMap::new();
    for results in data.of_source("sched.offcpu") {
        if let Some(Output::Stack { stacks: counts, .. }) = results.output("stack") {
            for count in counts {
                *stacks.entry(count.stack).or_default() += count.total;
            }
        }
    }
    let named = stack_naming(data, &stacks)?;
    Some(if several {
        format!("{named} (of every task's blocks)")
    } else {
        named
    })
}

/// The leaf the run's cpu profile sampled most.
fn hottest_leaf(data: &Data, several: bool) -> Option<String> {
    let mut leaves: BTreeMap<&str, f64> = BTreeMap::new();
    let mut total = 0.0;
    for results in data.of_source("cpu.profile") {
        if let Some(Output::Stack { stacks, .. }) = results.output("stack") {
            for count in stacks {
                if let Some(leaf) = results.frames(count.stack).last() {
                    *leaves.entry(leaf.as_str()).or_default() += count.events as f64;
                    total += count.events as f64;
                }
            }
        }
    }
    let (leaf, samples) = leaves.into_iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    let spread = if samples < total * 0.2 {
        ": the samples are spread, no one function dominates"
    } else {
        ""
    };
    Some(format!(
        "most-sampled leaf `{leaf}`: {samples:.0} of {total:.0} samples{}{spread}",
        if several { ", every thread's" } else { "" }
    ))
}

/// A stack, leaf first: up to five frames.
fn leafward(frames: &[String]) -> String {
    let shown: Vec<&str> = frames.iter().rev().take(5).map(String::as_str).collect();
    let mut text = shown.join(" <- ");
    if frames.len() > shown.len() {
        text.push_str(" <- ...");
    }
    text
}

/// One process's cpu time over the run, and the resolution it carries.
fn process_cpu(oncpu: &[&Results], key: &str) -> Option<(f64, f64)> {
    oncpu.iter().find_map(|results| {
        let tick = results
            .events()
            .iter()
            .find_map(|event| event.number("resolution_ns"))
            .unwrap_or(TICK_NS);
        let summed = match results.output("summary") {
            Some(Output::Summary { series }) => series
                .iter()
                .find(|series| series.key == key)
                .map(|series| series.mean * series.count as f64),
            _ => None,
        }
        .or_else(|| match results.output("timeline") {
            Some(Output::Timeline { series, .. }) => series
                .iter()
                .find(|series| series.key == key)
                .map(|series| series.points.iter().map(|point| point.value).sum()),
            _ => None,
        })
        .or_else(|| {
            let events: Vec<&Event> = results
                .events()
                .iter()
                .filter(|event| event.keys.iter().any(|k| k.to_string() == key))
                .collect();
            (!events.is_empty()).then(|| events.iter().filter_map(|e| e.number("ns")).sum())
        });
        summed.map(|cpu| (cpu, tick))
    })
}

/// The bars too short to draw, set apart: a bar is folded only when the
/// most its states can be -- a bracket at its upper end -- is under
/// [`FOLD_BELOW`] of the window. A bracket's lower end says nothing of how
/// long a thread ran: a busy worker beside a short-lived sibling has a lower
/// end of zero.
pub fn fold(bars: Vec<Bar>, whole: Option<f64>) -> (Vec<Bar>, Vec<String>) {
    let Some(whole) = whole else {
        return (bars, Vec::new());
    };
    let shown = |bar: &Bar| bar.most_read() >= whole * FOLD_BELOW;
    if !bars.iter().any(shown) {
        return (bars, Vec::new());
    }
    let (kept, small): (Vec<Bar>, Vec<Bar>) = bars.into_iter().partition(shown);
    (kept, small.iter().map(Bar::label).collect())
}

/// Why a thread `sched.offcpu` saw no block of may still have blocked, by
/// what the source said beside its losses: a task still off cpu when the run
/// ended (its block has no end, and is in no total), or a process it could
/// not follow.
fn unseen_blocks(offcpu: &[&Results]) -> Option<String> {
    let mut why = Vec::new();
    for results in offcpu {
        why.extend(
            results
                .notes
                .iter()
                .filter(|note| note.contains("still off cpu"))
                .cloned(),
        );
        why.extend(
            results
                .losses
                .reasons
                .keys()
                .filter(|reason| reason.contains("not followed"))
                .cloned(),
        );
    }
    (!why.is_empty()).then(|| why.join("; "))
}

/// The view's own reading, per thread over the run's window, when no
/// decomposition splits the run by task.
fn reading(data: &Data, threads: &Threads) -> Option<Sheet> {
    let offcpu: Vec<&Results> = data.for_need("sched.offcpu", "sched.offcpu");
    let runq: Vec<&Results> = data.for_need("sched.runqlat", "sched.runqlat");
    let oncpu: Vec<&Results> = data.of_source("cpu.oncpu_time").collect();
    if offcpu.is_empty() && runq.is_empty() {
        return None;
    }
    let opened = data.opened_ns();
    let closed = data.closed_ns();
    let whole = (closed > opened).then(|| (closed - opened) as f64);

    let futex = futex_waits(data);
    // Every thread a sched source saw.
    let mut tids: BTreeSet<i32> = BTreeSet::new();
    for results in offcpu.iter().chain(&runq) {
        tids.extend(results.events().iter().filter_map(tid_of));
        if let Some(Output::Summary { series }) = results.output("summary") {
            tids.extend(series.iter().filter_map(|series| task_of(&series.key)));
        }
    }
    if let Some(waits) = &futex {
        tids.extend(waits.keys());
    }
    if tids.is_empty() {
        return None;
    }

    let offcpu_by = by_thread(&offcpu);
    let runq_by = by_thread(&runq);
    let offcpu_raw = offcpu.iter().any(|results| results.output("raw").is_some());
    let offcpu_lossy = offcpu.iter().any(|results| lossy(results));
    let offcpu_unseen = unseen_blocks(&offcpu);
    let runq_raw = runq.iter().any(|results| results.output("raw").is_some());
    let runq_lossy = runq.iter().any(|results| lossy(results));
    let mut notes = vec![
        "No decomposition splits this run by task: these bars are the view's own reading of the \
         traced sources, per thread over the run's window, and the suspect follows `omn \
         latency`'s rule without being its answer. For its answer: `omn latency --objective \
         latency --dump-session -- PROG > s.yml`, then `omn top -f s.yml`."
            .to_owned(),
    ];

    let mut bars: Vec<Bar> = Vec::new();
    for &tid in &tids {
        // On cpu first, filled in below once every thread's other states are
        // known.
        let mut parts = vec![Part::new(Class::OnCpu, "on-cpu", "cpu.oncpu_time")];
        // The run queue.
        let mut queued = Part::new(Class::RunQueue, "runnable", "sched.runqlat");
        queued.kind = "traced".to_owned();
        queued.resolution = Some(0.0);
        queued.lower_bound = runq_lossy;
        if runq.is_empty() {
            queued.missing = Some("`sched.runqlat` is not in this run".to_owned());
        } else if runq_raw {
            let events: &[&Event] = runq_by.get(&tid).map_or(&[], Vec::as_slice);
            queued.amount = Some(
                events
                    .iter()
                    .map(|event| {
                        event
                            .number("total_ns")
                            .or_else(|| event.number("wait_ns"))
                            .unwrap_or(0.0)
                    })
                    .sum(),
            );
            queued.readings = events.len() as u64;
        } else {
            let key = format!("task:{tid}");
            let series = runq
                .iter()
                .find_map(|results| match results.output("summary") {
                    Some(Output::Summary { series }) => {
                        series.iter().find(|series| series.key == key).cloned()
                    }
                    _ => None,
                });
            queued.amount = Some(
                series
                    .as_ref()
                    .map_or(0.0, |series| series.mean * series.count as f64),
            );
            queued.readings = series.map_or(0, |series| series.count);
        }
        parts.push(queued);
        // Blocked, by state.
        let mut blocked = Part::new(Class::Blocked, "blocked", "sched.offcpu");
        blocked.kind = "traced".to_owned();
        if offcpu.is_empty() {
            blocked.missing = Some("`sched.offcpu` is not in this run".to_owned());
            parts.push(blocked);
        } else if offcpu_raw {
            let events: &[&Event] = offcpu_by.get(&tid).map_or(&[], Vec::as_slice);
            let split = split_blocked(data, events, futex_of(&futex, tid), offcpu_lossy);
            if split.is_empty() {
                blocked.resolution = Some(0.0);
                if offcpu_lossy {
                    blocked.missing = Some(
                        "`sched.offcpu` lost records and saw none of this thread's: not read \
                         as a zero"
                            .to_owned(),
                    );
                } else {
                    blocked.amount = Some(0.0);
                    match &offcpu_unseen {
                        Some(unseen) => {
                            blocked.lower_bound = true;
                            blocked.note = Some(format!(
                                "a lower bound: no block of it ended in the window, but \
                                 {unseen}"
                            ));
                        }
                        None => {
                            blocked.note = Some(
                                "`sched.offcpu` lost nothing, missed no process, and left no \
                                 task off cpu at the end: it never blocked"
                                    .to_owned(),
                            );
                        }
                    }
                }
                parts.push(blocked);
            } else {
                parts.extend(split);
            }
        } else {
            blocked.missing = Some(
                "the run holds `sched.offcpu`'s stacks, not its events: a block cannot be given \
                 to a thread"
                    .to_owned(),
            );
            parts.push(blocked);
        }
        let mut trust = Trust::clean();
        for results in offcpu.iter().chain(&runq).chain(&oncpu) {
            trust.merge(Trust::of(data, results));
        }
        bars.push(Bar {
            key: format!("task:{tid}"),
            comm: threads.comm.get(&tid).cloned(),
            pid: threads.pid.get(&tid).copied(),
            tid: Some(tid),
            request: None,
            whole,
            whole_resolution: None,
            parts,
            residue: None,
            residue_resolution: None,
            residue_says: None,
            trust,
            verdict: Verdict::Undecided(String::new()),
        });
    }

    // On cpu, per thread: what each thread's other states leave of the
    // window is the most it can have been on cpu (or not alive).
    let per_thread = oncpu
        .iter()
        .any(|results| results.backend.starts_with("bpf"));
    let left: Vec<Option<f64>> = bars
        .iter()
        .map(|bar| {
            let others = &bar.parts[1..];
            if others.iter().any(|part| part.amount.is_none()) {
                return None;
            }
            let read: f64 = others.iter().filter_map(|part| part.amount).sum();
            whole.map(|whole| (whole - read).max(0.0))
        })
        .collect();
    let mut bracketed = false;
    // Which bars' on cpu is what their other states leave of the window: their
    // rows then add up to the whole by construction.
    let mut derived = vec![false; bars.len()];
    for index in 0..bars.len() {
        let tid = bars[index].tid.unwrap_or_default();
        let pid = bars[index].pid;
        let mut part = Part::new(Class::OnCpu, "on-cpu", "cpu.oncpu_time");
        part.kind = "polled".to_owned();
        part.lower_bound = oncpu.iter().any(|results| lossy(results));
        if oncpu.is_empty() {
            part.missing = Some("`cpu.oncpu_time` is not in this run".to_owned());
        } else if per_thread {
            match process_cpu(&oncpu, &format!("task:{tid}")) {
                Some((cpu, _)) => {
                    part.amount = Some(cpu);
                    part.resolution = Some(0.0);
                }
                None => part.missing = Some("`cpu.oncpu_time` has no series of it".to_owned()),
            }
        } else if let Some((pid, (cpu, tick))) =
            pid.and_then(|pid| process_cpu(&oncpu, &format!("task:{pid}")).map(|cpu| (pid, cpu)))
        {
            let siblings: Vec<usize> = (0..bars.len())
                .filter(|other| *other != index && bars[*other].pid == Some(pid))
                .collect();
            let others: Option<f64> = siblings.iter().map(|other| left[*other]).sum();
            match (left[index], others) {
                (Some(most), Some(others)) => {
                    let least = (cpu - tick - others).max(0.0).min(most);
                    part.amount = Some(least);
                    part.upper = (most > least).then_some(most);
                    part.resolution = Some(tick);
                    part.note = Some(if siblings.is_empty() {
                        format!(
                            "its process's {} on cpu, to a tick ({}); the rest of the window, up \
                             to {}, is on cpu or before the program ran",
                            units::duration(cpu),
                            units::duration(tick),
                            units::duration(most)
                        )
                    } else {
                        format!(
                            "a bracket: its process's {} on cpu less what its {} other \
                             thread(s) could have run, at least; what its own other states \
                             leave, at most",
                            units::duration(cpu),
                            siblings.len()
                        )
                    });
                    bracketed = true;
                    derived[index] = true;
                }
                _ => {
                    part.missing = Some(
                        "its process's cpu time cannot be shared out: a thread of it has a state \
                         not read"
                            .to_owned(),
                    )
                }
            }
        } else if data.live.is_some() {
            part.missing = Some(
                "`cpu.oncpu_time` has said nothing of this thread's process yet: it is read at \
                 the run's end"
                    .to_owned(),
            );
        } else {
            part.missing = Some(
                "`cpu.oncpu_time` measured no process of this thread: it watches the target, not \
                 the processes the target starts"
                    .to_owned(),
            );
        }
        if part.amount.is_some() {
            part.named.extend(hottest_leaf(data, tids.len() > 1));
        }
        bars[index].parts[0] = part;
    }
    if bracketed {
        notes.push(
            "On cpu: `cpu.oncpu_time` is a per-process total here (`fs:proc`, to a tick), so a \
             thread's share is bracketed -- solid to the least it can be, shaded (▒) to the most \
             -- over the threads the sched sources saw. A thread that started or ended inside the \
             window counts its absence in the shaded part."
                .to_owned(),
        );
    }

    for (bar, derived) in bars.iter_mut().zip(derived) {
        let complete = bar.parts.iter().all(|part| part.amount.is_some());
        let drawn = bar.most_read();
        if derived {
            bar.residue_says =
                Some("not measured: on cpu is derived from the other rows".to_owned());
        } else {
            bar.residue = complete.then(|| whole.map(|whole| whole - drawn)).flatten();
        }
        bar.residue_resolution = Some(bar.parts.iter().filter_map(|part| part.resolution).sum());
        bar.verdict = decide(&bar.parts, bar.whole, bar.residue_resolution);
    }

    // Processes in order, each led by its main thread, its other threads by
    // how much they account for; the slivers folded.
    bars.sort_by(|a, b| {
        let process = |bar: &Bar| bar.pid.unwrap_or(i32::MAX);
        let follows = |bar: &Bar| bar.tid != bar.pid;
        process(a)
            .cmp(&process(b))
            .then(follows(a).cmp(&follows(b)))
            .then(b.most_read().total_cmp(&a.most_read()))
    });
    let (bars, folded) = fold(bars, whole);
    let main = main_bar(&bars, threads);
    Some(Sheet {
        id: "the view's reading".to_owned(),
        by: "thread".to_owned(),
        whole_says: "the run's window".to_owned(),
        reading: true,
        bars,
        folded,
        main,
        notes,
    })
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Shares out a bar's cells by largest remainder, so the cells drawn add up
/// to the bar's length exactly and no state is rounded twice.
pub fn allocate(amounts: &[f64], scale: f64, cells: usize) -> Vec<usize> {
    if scale <= 0.0 || amounts.is_empty() {
        return vec![0; amounts.len()];
    }
    let exact: Vec<f64> = amounts
        .iter()
        .map(|amount| amount.max(0.0) / scale * cells as f64)
        .collect();
    let target = (exact.iter().sum::<f64>().round() as usize).min(cells);
    let mut given: Vec<usize> = exact.iter().map(|x| x.floor() as usize).collect();
    let mut short = target.saturating_sub(given.iter().sum());
    let mut order: Vec<usize> = (0..amounts.len()).collect();
    order.sort_by(|a, b| {
        (exact[*b] - exact[*b].floor()).total_cmp(&(exact[*a] - exact[*a].floor()))
    });
    for index in order {
        if short == 0 {
            break;
        }
        if exact[index] > 0.0 {
            given[index] += 1;
            short -= 1;
        }
    }
    given
}

/// A stretch of a bar: its fill and style, its length, the words inside it.
struct Segment {
    glyph: char,
    style: Style,
    amount: f64,
    label: Option<String>,
}

fn segments(bar: &Bar, dims: bool) -> Vec<Segment> {
    let dimmed = |style: Style| {
        if dims {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    };
    let share = |amount: f64| {
        bar.whole
            .filter(|whole| *whole > 0.0)
            .map(|whole| units::percent(amount / whole * 100.0))
    };
    let mut order: Vec<&Part> = bar.parts.iter().collect();
    order.sort_by_key(|part| part.class);
    let mut out = Vec::new();
    for part in order {
        let Some(amount) = part.amount else { continue };
        let words = match share(amount) {
            Some(share) => format!("{} {share}", part.class.short()),
            None => format!("{} {}", part.class.short(), units::duration(amount)),
        };
        out.push(Segment {
            glyph: part.class.glyph(),
            style: dimmed(Style::new().fg(part.class.colour())),
            amount,
            label: Some(words),
        });
        if let Some(upper) = part.upper {
            out.push(Segment {
                glyph: '▒',
                style: dimmed(Style::new().fg(palette::LOSS)),
                amount: upper - amount,
                label: None,
            });
        }
    }
    let drawn: f64 = out.iter().map(|segment| segment.amount).sum();
    if let Some(whole) = bar.whole {
        if whole > drawn {
            let unread = bar.parts.iter().any(|part| part.amount.is_none());
            out.push(Segment {
                glyph: if unread { '·' } else { ' ' },
                style: dim(),
                amount: whole - drawn,
                label: unread.then(|| "not read".to_owned()),
            });
        }
    }
    out
}

/// The bar itself, `width` cells and its end mark, on a scale of `scale`
/// nanoseconds to the width.
fn bar_spans(bar: &Bar, scale: f64, width: usize, labelled: bool) -> Vec<Span<'static>> {
    let segments = segments(bar, bar.trust.dims());
    let amounts: Vec<f64> = segments.iter().map(|segment| segment.amount).collect();
    let cells = allocate(&amounts, scale, width);
    let mut spans = Vec::new();
    let mut used = 0;
    for (segment, cells) in segments.iter().zip(cells) {
        if cells == 0 {
            continue;
        }
        used += cells;
        let fill = |n: usize| segment.glyph.to_string().repeat(n);
        match &segment.label {
            Some(label) if labelled && cells >= label.chars().count() + 4 => {
                let text = format!(" {label} ");
                let inside = text.chars().count();
                let before = (cells - inside) / 2;
                let after = cells - inside - before;
                spans.push(Span::styled(fill(before), segment.style));
                spans.push(Span::styled(
                    text,
                    segment
                        .style
                        .add_modifier(Modifier::REVERSED | Modifier::BOLD),
                ));
                spans.push(Span::styled(fill(after), segment.style));
            }
            _ => spans.push(Span::styled(fill(cells), segment.style)),
        }
    }
    spans.push(Span::styled("┤", dim()));
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    spans
}

/// Text cut or padded to `width` characters, with an ellipsis when cut.
fn fit(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        format!("{text:<width$}")
    } else if width == 0 {
        String::new()
    } else {
        let mut cut: String = text.chars().take(width - 1).collect();
        cut.push('…');
        cut
    }
}

/// Words wrapped to `width`, the first line after `first`, the others after
/// `rest`.
fn wrap(text: &str, width: usize, first: &str, rest: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = first.to_owned();
    let mut empty = true;
    for word in text.split_whitespace() {
        let len = line.chars().count() + word.chars().count() + usize::from(!empty);
        if !empty && len > width {
            lines.push(std::mem::replace(&mut line, rest.to_owned()));
            empty = true;
        }
        if !empty {
            line.push(' ');
        }
        line.push_str(word);
        empty = false;
    }
    if !empty {
        lines.push(line);
    }
    lines
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn resolution_text(resolution: Option<f64>) -> String {
    match resolution {
        Some(res) if res > 0.0 => format!("±{}", units::duration(res)),
        Some(_) => "exact".to_owned(),
        None => "?".to_owned(),
    }
}

/// The width of the table's columns: state, source, amount, share, ±, reads.
const COLUMNS: [usize; 6] = [36, 15, 15, 7, 9, 6];

/// The label column before each bar.
const LABEL: usize = 22;

/// The most rows a bar is drawn on, when the terminal is tall enough.
const BAR_ROWS: usize = 5;

/// The view's state: which way of splitting the run, which bar, which scale.
#[derive(Debug, Default)]
pub struct Timebar {
    sheet: usize,
    selected: usize,
    /// Each bar to its own whole, rather than every bar on one time scale.
    own_scale: bool,
    /// The bars as last drawn, each its place among the visible ones.
    regions: Vec<Region>,
}

impl Timebar {
    /// The bars the focus leaves on screen, as indices into the sheet; all of
    /// them, and `false`, when it names none of them.
    fn visible(sheet: &Sheet, focus: &Focus) -> (Vec<usize>, bool) {
        if focus.is_empty() {
            return ((0..sheet.bars.len()).collect(), true);
        }
        let matching: Vec<usize> = sheet
            .bars
            .iter()
            .enumerate()
            .filter(|(_, bar)| {
                focus.tid.is_some_and(|tid| bar.tid == Some(tid))
                    || (focus.tid.is_none() && focus.pid.is_some_and(|pid| bar.pid == Some(pid)))
                    || focus
                        .request
                        .as_ref()
                        .is_some_and(|request| bar.request.as_ref() == Some(request))
            })
            .map(|(index, _)| index)
            .collect();
        if matching.is_empty() {
            ((0..sheet.bars.len()).collect(), false)
        } else {
            (matching, true)
        }
    }

    /// A run with nothing to draw as bars: what the view needs, what the run
    /// holds of it, and how to get a decomposition.
    fn draw_empty(&self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        let width = usize::from(area.width);
        let mut lines = vec![
            Line::from(Span::styled(
                "Nothing to split: this run holds no decomposition, and no per-thread state to \
                 read one from.",
                bold(),
            )),
            Line::default(),
        ];
        for need in NEEDS {
            let label = need.label();
            let held = ctx.data.for_need(&label, need.source);
            let mut line = vec![Span::styled(format!("{:<16}", need.source), bold())];
            if let Some(refusal) = ctx.data.refused.iter().find(|r| r.label == label) {
                line.push(Span::styled(
                    format!("refused: {}", refusal.reason),
                    Style::new().fg(palette::REFUSED),
                ));
            } else if held.is_empty() {
                line.push(Span::styled("not in this run", dim()));
            } else {
                let trust = Trust::joined(ctx.data, held.iter().copied());
                let events: u64 = held.iter().map(|results| results.events_seen).sum();
                line.push(Span::styled(
                    format!("{} events", units::count(events as f64)),
                    trust.style(),
                ));
            }
            line.push(Span::styled(format!("  -- {}", need.why), dim()));
            lines.push(Line::from(line));
        }
        lines.push(Line::default());
        for why in decompositions(ctx.data).1 {
            for text in wrap(&format!("Not drawn: {why}."), width, "", "  ") {
                lines.push(Line::from(Span::styled(
                    text,
                    Style::new().fg(palette::REFUSED),
                )));
            }
        }
        for text in wrap(
            "A bar per thread needs `sched.offcpu` or `sched.runqlat` on a target; a bar per task \
             as `omn latency` reads it needs its session: `omn latency --objective latency \
             --dump-session -- PROG > s.yml`, then `omn top -f s.yml`.",
            width,
            "",
            "",
        ) {
            lines.push(Line::from(Span::styled(text, dim())));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// The suspect of the chosen bar: four lines.
    fn suspect_lines(sheet: &Sheet, index: usize, width: usize) -> Vec<Line<'static>> {
        let bar = &sheet.bars[index];
        let who = match &sheet.main {
            Some((main, _)) if *main == index && sheet.bars.len() > 1 => {
                format!("{} (the program)", bar.label())
            }
            _ => bar.label(),
        };
        let mut lines = Vec::new();
        match &bar.verdict {
            Verdict::Names {
                part,
                amount,
                share,
                next,
                room,
            } => {
                let part = &bar.parts[*part];
                let bound = if part.lower_bound || part.upper.is_some() {
                    "≥"
                } else {
                    ""
                };
                let of = if sheet.by == "req" {
                    "whole"
                } else {
                    "wall clock"
                };
                let share = share.map_or(String::new(), |share| {
                    format!(" ({} of its {of})", units::percent(share * 100.0))
                });
                lines.push(Line::from(vec![
                    Span::styled("▶ first suspect  ", bold()),
                    Span::styled(format!("{who}: "), bold()),
                    Span::styled(
                        format!("{} {bound}{}{share}", part.state, units::duration(*amount)),
                        Style::new()
                            .fg(part.class.colour())
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.push(Line::from(format!(
                    "  {}",
                    fit(part.class.limit(), width.saturating_sub(2))
                )));
                let named = part
                    .named
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "nothing in the run names it further".to_owned());
                lines.push(Line::from(format!(
                    "  {}",
                    fit(&named, width.saturating_sub(2))
                )));
                let rule = if sheet.reading {
                    "omn latency's rule, on the view's reading"
                } else {
                    "omn latency's rule, on the run's decomposition"
                };
                lines.push(Line::from(Span::styled(
                    fit(
                        &format!(
                            "  decisive: ahead of the next state ({}) and of what no row read \
                             ({}) -- {rule}",
                            units::duration(*next),
                            units::duration(*room),
                        ),
                        width,
                    ),
                    dim(),
                )));
            }
            Verdict::Undecided(why) => {
                lines.push(Line::from(vec![
                    Span::styled("≈ no single suspect  ", bold()),
                    Span::styled(who, bold()),
                ]));
                for text in wrap(why, width, "  ", "  ").into_iter().take(3) {
                    lines.push(Line::from(text));
                }
                while lines.len() < 4 {
                    lines.push(Line::default());
                }
            }
        }
        lines
    }

    /// The chosen bar's rows: its whole, each state with its source and
    /// trust, what names it, and the residue.
    ///
    /// `detail` is how many lines may say what names each state: what is
    /// missing, its stack or lock holder, how it was read.
    fn table_lines(
        ctx: &Ctx<'_>,
        sheet: &Sheet,
        bar: &Bar,
        width: usize,
        detail: usize,
    ) -> Vec<Line<'static>> {
        let [state_w, source_w, amount_w, share_w, res_w, reads_w] = COLUMNS;
        // The state's column gives way first on a narrow screen.
        let state_w = width.saturating_sub(84).clamp(18, state_w);
        let mut lines = Vec::new();
        let header = format!(
            "  {} {} {} {} {} {} trust",
            fit("state", state_w),
            fit("source", source_w),
            fit("amount", amount_w),
            fit("share", share_w),
            fit("±", res_w),
            fit("reads", reads_w),
        );
        lines.push(Line::from(Span::styled(fit(&header, width), bold())));
        let share_of = |amount: f64| {
            bar.whole
                .filter(|whole| *whole > 0.0)
                .map_or(String::new(), |whole| {
                    units::percent(amount / whole * 100.0)
                })
        };
        if let Some(whole) = bar.whole {
            lines.push(Line::from(Span::styled(
                fit(
                    &format!(
                        "  {} {} {} {} {}",
                        fit(&format!("whole: {}", sheet.whole_says), state_w),
                        fit("", source_w),
                        fit(&units::duration(whole), amount_w),
                        fit("100%", share_w),
                        fit(&resolution_text(bar.whole_resolution.or(Some(0.0))), res_w),
                    ),
                    width,
                ),
                bar.trust.style(),
            )));
        }
        let mut order: Vec<&Part> = bar.parts.iter().collect();
        order.sort_by_key(|part| part.class);
        for part in order {
            let trust = trust_of(ctx.data, &part.source);
            let (amount, share) = match (part.amount, part.upper) {
                (Some(least), Some(most)) => (
                    format!("{} - {}", units::duration(least), units::duration(most)),
                    share_of(least),
                ),
                (Some(amount), None) => (
                    format!(
                        "{}{}",
                        if part.lower_bound { "≥" } else { "" },
                        units::duration(amount)
                    ),
                    share_of(amount),
                ),
                (None, _) => ("not read".to_owned(), String::new()),
            };
            let mut spans = vec![
                Span::styled(
                    format!("{} ", part.class.glyph()),
                    Style::new().fg(part.class.colour()),
                ),
                Span::styled(
                    format!(
                        "{} {} {} {} {} {} ",
                        fit(&part.state, state_w),
                        fit(&part.source, source_w),
                        fit(&amount, amount_w),
                        fit(&share, share_w),
                        fit(&resolution_text(part.resolution), res_w),
                        fit(&part.readings.to_string(), reads_w),
                    ),
                    trust.style(),
                ),
            ];
            spans.extend(trust.badges());
            lines.push(Line::from(spans));
            let said = part
                .missing
                .iter()
                .chain(&part.named)
                .chain(&part.note)
                .take(detail);
            for said in said {
                lines.push(Line::from(Span::styled(
                    fit(&format!("    ↳ {said}"), width),
                    dim(),
                )));
            }
        }
        let residue = match bar.residue {
            Some(residue) => {
                let res = bar.residue_resolution.unwrap_or(0.0);
                let reads = if residue.abs() <= res {
                    "within its resolution"
                } else if residue > 0.0 {
                    "time no row accounts for"
                } else {
                    "the rows add up to more than the whole"
                };
                format!(
                    "  {} {} {} {} {} {reads}",
                    fit("residue: the whole less the rows", state_w),
                    fit("", source_w),
                    fit(&units::duration(residue), amount_w),
                    fit(&share_of(residue), share_w),
                    fit(&resolution_text(Some(res)), res_w),
                )
            }
            None => format!(
                "  {} {} {}",
                fit("residue: the whole less the rows", state_w),
                fit("", source_w),
                bar.residue_says
                    .as_deref()
                    .unwrap_or("unknown: a row was not read"),
            ),
        };
        lines.push(Line::from(Span::styled(fit(&residue, width), dim())));
        lines
    }
}

impl View for Timebar {
    fn name(&self) -> &'static str {
        "timebar"
    }

    fn title(&self) -> &'static str {
        "where the wall-clock went: one bar per thread, by state"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("↑↓", "bar"),
            ("d", "decomposition"),
            ("s", "scale"),
            ("enter", "open"),
        ]
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        let sheets = sheets(ctx.data);
        if sheets.is_empty() {
            return Action::None;
        }
        self.sheet = self.sheet.min(sheets.len() - 1);
        let sheet = &sheets[self.sheet];
        let (visible, _) = Self::visible(sheet, ctx.focus);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(visible.len().saturating_sub(1))
            }
            KeyCode::Char('d') => {
                self.sheet = (self.sheet + 1) % sheets.len();
                self.selected = 0;
            }
            KeyCode::Char('s') => self.own_scale = !self.own_scale,
            KeyCode::Enter => {
                let chosen = self.selected.min(visible.len().saturating_sub(1));
                let Some(bar) = visible.get(chosen).map(|index| &sheet.bars[*index]) else {
                    return Action::None;
                };
                if let Some(request) = &bar.request {
                    return Action::Goto {
                        view: "request",
                        focus: Focus {
                            request: Some(request.clone()),
                            ..Focus::default()
                        },
                    };
                }
                if let Some(tid) = bar.tid {
                    return Action::Goto {
                        view: "lanes",
                        focus: Focus {
                            pid: bar.pid,
                            tid: Some(tid),
                            request: None,
                        },
                    };
                }
            }
            _ => {}
        }
        Action::None
    }

    fn regions(&self) -> &[Region] {
        &self.regions
    }

    fn select(&mut self, id: usize, _ctx: &Ctx<'_>) -> Action {
        self.selected = id;
        Action::None
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        self.regions.clear();
        let block = Block::new()
            .borders(Borders::ALL)
            .title(format!(" timebar -- {} ", self.title()));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let inner = Rect {
            x: inner.x + 1,
            width: inner.width.saturating_sub(2),
            ..inner
        };
        let sheets = sheets(ctx.data);
        if sheets.is_empty() {
            self.draw_empty(frame, inner, ctx);
            return;
        }
        self.sheet = self.sheet.min(sheets.len() - 1);
        let sheet = &sheets[self.sheet];
        let (visible, focus_found) = Self::visible(sheet, ctx.focus);
        if visible.is_empty() {
            self.draw_empty(frame, inner, ctx);
            return;
        }
        self.selected = self.selected.min(visible.len() - 1);
        let chosen = visible[self.selected];
        let bar = &sheet.bars[chosen];
        let width = usize::from(inner.width);
        let height = usize::from(inner.height);

        // The heading: which split, what a bar's whole is.
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut heading = vec![
            Span::styled(sheet.id.clone(), bold().fg(palette::ACCENT)),
            Span::styled(
                format!(
                    "  one bar per {}, whole: {}",
                    if sheet.by == "req" {
                        "request"
                    } else {
                        sheet.by.as_str()
                    },
                    sheet.whole_says
                ),
                dim(),
            ),
        ];
        if sheets.len() > 1 {
            heading.push(Span::styled(
                format!("  ({} of {}: d for the next)", self.sheet + 1, sheets.len()),
                dim(),
            ));
        }
        if !ctx.focus.is_empty() {
            heading.push(Span::styled(
                if focus_found {
                    format!("  focus: {}", ctx.focus.describe())
                } else {
                    format!("  focus {}: not among these bars", ctx.focus.describe())
                },
                Style::new().fg(palette::ACCENT),
            ));
        }
        lines.push(Line::from(heading));
        lines.push(Line::default());
        lines.extend(Self::suspect_lines(sheet, chosen, width));
        lines.push(Line::default());

        // The bars, as many as leave room for the table under them.
        // Under 100 cells the bars keep the width the suspect's column
        // would take: the first suspect is named above them.
        let suspects = width >= 100;
        let tail = 10 + if suspects { 2 + 18 } else { 0 };
        let bar_w = width.saturating_sub(LABEL + tail + 2).max(10);
        let common = visible
            .iter()
            .map(|index| sheet.bars[*index].extent())
            .fold(0.0, f64::max);
        // Notes, wrapped, and the chosen bar's table: with every line that
        // names its states when the screen has the room.
        let mut notes: Vec<String> = Vec::new();
        if let Some((main, because)) = &sheet.main {
            if *main == chosen && sheet.bars.len() > 1 {
                notes.push(format!("{} is the program's: {because}.", bar.key));
            }
        }
        notes.extend(sheet.notes.iter().cloned());
        let sentences = bar.trust.sentences();
        if !sentences.is_empty() {
            notes.push(format!("Trust: {}.", sentences.join("; ")));
        }
        let mut wrapped = Vec::new();
        for note in &notes {
            wrapped.extend(wrap(note, width, "", "  "));
        }
        let terse = Self::table_lines(ctx, sheet, bar, width, 1);
        let full = Self::table_lines(ctx, sheet, bar, width, 3);
        let fixed = lines.len() + visible.len() + 2 + 2 + 1 + wrapped.len();
        let table = if fixed + full.len() <= height {
            full
        } else {
            terse
        };
        let below = 2 + 1 + table.len() + 2;
        let fold_line = usize::from(!sheet.folded.is_empty());
        // The bars before the table: on a short screen the table's foot is
        // what gives way.
        let room = height
            .saturating_sub(lines.len() + below + fold_line)
            .max(visible.len().min(4))
            .max(1);
        let first = (self.selected + 1).saturating_sub(room);
        // A tall terminal draws each bar thicker, up to five rows, rather
        // than leaving the rows under the table empty: what the screen has
        // over the lines it draws, shared among the bars.
        let shown_bars = visible.len().saturating_sub(first).min(room).max(1);
        let drawn = lines.len() + shown_bars + fold_line + below + 1 + wrapped.len();
        let thickness = (1 + height.saturating_sub(drawn) / shown_bars).min(BAR_ROWS);
        for (shown, &index) in visible.iter().enumerate().skip(first).take(room) {
            let y = inner.y + lines.len() as u16;
            if y < inner.y + inner.height {
                let rows = (thickness as u16).min(inner.y + inner.height - y);
                self.regions
                    .push(Region::new(Rect::new(inner.x, y, inner.width, rows), shown));
            }
            let one = &sheet.bars[index];
            let picked = shown == self.selected;
            let style = if picked {
                bold().fg(palette::ACCENT)
            } else {
                Style::new()
            };
            let mut spans = vec![
                Span::styled(if picked { "▸ " } else { "  " }, style),
                Span::styled(fit(&one.label(), LABEL - 2), style),
                Span::styled("├", dim()),
            ];
            let scale = if self.own_scale { one.extent() } else { common };
            // The rows above the labelled one: the same bar, its words left
            // to the row that carries the thread's name.
            for _ in 1..thickness {
                let mut above = vec![Span::raw(" ".repeat(LABEL)), Span::styled("│", dim())];
                above.extend(bar_spans(one, scale, bar_w, false));
                lines.push(Line::from(above));
            }
            spans.extend(bar_spans(one, scale, bar_w, true));
            let whole = one.whole.map_or("?".to_owned(), units::duration);
            spans.push(Span::styled(format!("{whole:>9} "), one.trust.style()));
            if suspects {
                let (suspect, suspect_style) = one.suspect_short();
                spans.push(Span::styled(
                    format!(" {}", fit(&suspect, 18)),
                    suspect_style,
                ));
            }
            lines.push(Line::from(spans));
        }
        if fold_line > 0 {
            lines.push(Line::from(Span::styled(
                fit(
                    &format!(
                        "  + {} thread(s) under {} of the window each: {}",
                        sheet.folded.len(),
                        units::percent(FOLD_BELOW * 100.0),
                        sheet.folded.join(", ")
                    ),
                    width,
                ),
                dim(),
            )));
        }
        // The axis.
        let axis = if self.own_scale {
            let mid = " each bar to its own whole ";
            let span = bar_w.saturating_sub(mid.len());
            format!(
                "{}0 {}{mid}{}┤ 100%",
                " ".repeat(LABEL - 1),
                "─".repeat(span / 2),
                "─".repeat(span - span / 2),
            )
        } else {
            let half = format!(" {} ", units::duration(common / 2.0));
            let span = bar_w.saturating_sub(half.chars().count() + 1);
            format!(
                "{}0 {}┼{half}{}┤ {}",
                " ".repeat(LABEL - 1),
                "─".repeat(span / 2),
                "─".repeat(span - span / 2),
                units::duration(common)
            )
        };
        lines.push(Line::from(Span::styled(fit(&axis, width), dim())));
        // The legend: the states on screen.
        let mut classes: BTreeSet<Class> = BTreeSet::new();
        let (mut bracket, mut unread) = (false, false);
        for index in &visible {
            for part in &sheet.bars[*index].parts {
                if part.amount.is_some_and(|amount| amount > 0.0) {
                    classes.insert(part.class);
                }
                bracket |= part.upper.is_some();
                unread |= part.amount.is_none();
            }
        }
        let mut legend = vec![Span::raw(" ".repeat(LABEL + 1))];
        for class in classes {
            legend.push(Span::styled(
                class.glyph().to_string(),
                Style::new().fg(class.colour()),
            ));
            legend.push(Span::raw(format!(" {}   ", class.short())));
        }
        if bracket {
            legend.push(Span::styled("▒", Style::new().fg(palette::LOSS)));
            legend.push(Span::raw(" bracketed   "));
        }
        if unread {
            legend.push(Span::styled("·", dim()));
            legend.push(Span::raw(" not read"));
        }
        lines.push(Line::from(legend));
        lines.push(Line::default());
        lines.extend(table);

        let left = height.saturating_sub(lines.len() + 1);
        if left > 0 && !wrapped.is_empty() {
            lines.push(Line::default());
            let cut = wrapped.len() > left;
            for (index, text) in wrapped.into_iter().take(left).enumerate() {
                let text = if cut && index + 1 == left {
                    format!("{}…", text.trim_end())
                } else {
                    text
                };
                lines.push(Line::from(Span::styled(text, dim())));
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Timebar::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::as_live_until;
    use crate::snapshot::{draw, fixture, text, HEIGHT, WIDTH};

    fn part(class: Class, state: &str, amount: Option<f64>, resolution: f64) -> Part {
        let mut part = Part::new(class, state, "test");
        part.amount = amount;
        part.resolution = Some(resolution);
        part
    }

    #[test]
    fn the_largest_state_is_named_only_when_it_is_decisive() {
        let parts = [
            part(Class::OnCpu, "on-cpu", Some(40.0), 1.0),
            part(Class::Blocked, "blocked", Some(240.0), 0.0),
            part(Class::RunQueue, "runnable", Some(2.0), 0.0),
        ];
        match decide(&parts, Some(284.0), Some(9.0)) {
            Verdict::Names { part, share, .. } => {
                assert_eq!(parts[part].state, "blocked");
                assert!((share.unwrap() - 240.0 / 284.0).abs() < 1e-9);
            }
            other => panic!("{other:?}"),
        }
        // A tie names nothing.
        let tie = [
            part(Class::OnCpu, "on-cpu", Some(100.0), 0.0),
            part(Class::Sleep, "blocked: sleeping", Some(100.0), 0.0),
        ];
        assert!(
            matches!(decide(&tie, Some(200.0), None), Verdict::Undecided(why) if why.contains("neither is decisive"))
        );
        // A row not read that could hold more than the largest names nothing.
        let unread = [
            part(Class::OnCpu, "on-cpu", None, 0.0),
            part(Class::RunQueue, "runnable", Some(10.0), 0.0),
        ];
        assert!(
            matches!(decide(&unread, Some(100.0), None), Verdict::Undecided(why) if why.contains("no row that was read"))
        );
        // Nor does one within the rows' own resolution.
        let coarse = [
            part(Class::OnCpu, "on-cpu", Some(5.0), 10.0),
            part(Class::RunQueue, "runnable", Some(1.0), 0.0),
        ];
        assert!(
            matches!(decide(&coarse, Some(6.0), Some(10.0)), Verdict::Undecided(why) if why.contains("resolution"))
        );
        // A bracket counts at its most against the largest.
        let mut bracketed = part(Class::OnCpu, "on-cpu", Some(10.0), 0.0);
        bracketed.upper = Some(90.0);
        let parts = [
            bracketed,
            part(Class::Sleep, "blocked: sleeping", Some(80.0), 0.0),
        ];
        assert!(matches!(
            decide(&parts, Some(170.0), None),
            Verdict::Undecided(_)
        ));
    }

    #[test]
    fn cells_add_up_to_the_bar_and_no_state_is_rounded_twice() {
        let cells = allocate(&[43.1, 1.7, 242.3], 287.1, 60);
        assert_eq!(cells.iter().sum::<usize>(), 60);
        assert_eq!(cells, vec![9, 0, 51]);
        let cells = allocate(&[1.0, 1.0, 1.0], 3.0, 10);
        assert_eq!(cells.iter().sum::<usize>(), 10);
        assert_eq!(allocate(&[50.0], 100.0, 10), vec![5]);
    }

    /// `omn latency`'s own session on the sqlite commits: one task, mostly
    /// blocked, named as the triage names it.
    #[test]
    fn the_latency_session_names_blocked_as_omn_latency_does() {
        let data = fixture("sqlite-latency");
        let sheets = sheets(&data);
        assert_eq!(
            sheets.len(),
            1,
            "the decomposition, and no reading beside it"
        );
        let sheet = &sheets[0];
        assert_eq!(sheet.id, LATENCY_DECOMPOSITION);
        assert!(!sheet.reading);
        assert_eq!(sheet.bars.len(), 1);
        let bar = &sheet.bars[0];
        let Verdict::Names { part, share, .. } = &bar.verdict else {
            panic!("{:?}", bar.verdict)
        };
        // The recording keeps the blocked rows' events, so the part is split
        // by the state the task blocked in: a commit waiting on fdatasync is
        // in the disk's uninterruptible sleep.
        assert_eq!(bar.parts[*part].state, "blocked: uninterruptible");
        assert!(share.unwrap() > 0.5, "{share:?}");
        // Named by the stack it blocked in: the commit's sync.
        assert!(
            bar.parts[*part]
                .named
                .iter()
                .any(|named| named.contains("fdatasync")),
            "{:?}",
            bar.parts[*part].named
        );
        // The residue is the table's, signed, within its resolution.
        let residue = bar.residue.unwrap();
        assert!(residue.abs() <= bar.residue_resolution.unwrap());
    }

    /// The vertical case's requests, each with the cause the server
    /// injected: the bar names it.
    #[test]
    fn each_request_is_named_by_its_injected_cause() {
        let data = fixture("vertical-requests");
        let sheets = sheets(&data);
        let sheet = sheets
            .iter()
            .find(|sheet| sheet.id == "one-request")
            .expect("the request decomposition");
        let suspect = |key: &str| {
            let bar = sheet.bars.iter().find(|bar| bar.key == key).expect(key);
            match &bar.verdict {
                Verdict::Names { part, .. } => bar.parts[*part].class,
                other => panic!("{key}: {other:?}"),
            }
        };
        assert_eq!(suspect("req:a001"), Class::Lock);
        assert_eq!(suspect("req:a002"), Class::Io);
        assert_eq!(suspect("req:a003"), Class::Lock);
        assert_eq!(suspect("req:a004"), Class::Network);
        assert_eq!(suspect("req:a005"), Class::Network);
        let a001 = sheet.bars.iter().find(|bar| bar.key == "req:a001").unwrap();
        assert!(a001
            .parts
            .iter()
            .any(|part| part.named.iter().any(|named| named.contains("holder tid"))));
    }

    /// The http server: two processes, three threads, cpu time measured per
    /// process. The handler thread's share is a bracket that holds most of
    /// the process's cpu; the server's main thread is asleep.
    #[test]
    fn a_process_cpu_time_is_shared_out_as_a_bracket() {
        let data = fixture("http-server");
        let sheets = sheets(&data);
        let sheet = sheets
            .iter()
            .find(|sheet| sheet.reading)
            .expect("the reading");
        let handler = sheet
            .bars
            .iter()
            .find(|bar| bar.pid.is_some() && bar.tid != bar.pid)
            .expect("the server's handler thread");
        let on = &handler.parts[0];
        let least = on.amount.expect("a bracket");
        let most = on.most().unwrap();
        let whole = handler.whole.unwrap();
        assert!(least <= most && most <= whole, "{least} {most} {whole}");
        // Separations, not thresholds: the bracket holds most of the window.
        assert!(least > whole / 2.0, "{least} of {whole}");
        let main = sheet
            .bars
            .iter()
            .find(|bar| bar.tid == handler.pid)
            .expect("the server's main thread");
        let Verdict::Names { part, .. } = &main.verdict else {
            panic!("{:?}", main.verdict)
        };
        assert_eq!(main.parts[*part].class, Class::Sleep);
        // A thread's states never add up to more than its whole.
        for bar in &sheet.bars {
            let drawn: f64 = bar.parts.iter().filter_map(Part::most).sum();
            assert!(drawn <= bar.whole.unwrap() + 1.0, "{}: {drawn}", bar.key);
        }
    }

    #[test]
    fn a_cpu_bound_program_is_named_on_cpu() {
        let data = fixture("cpython-hot");
        let sheets = sheets(&data);
        let bar = &sheets[0].bars[0];
        let Verdict::Names { part, .. } = &bar.verdict else {
            panic!("{:?}", bar.verdict)
        };
        assert_eq!(bar.parts[*part].class, Class::OnCpu);
    }

    /// The split partitions the events it is made of: nothing is counted
    /// twice or left out. (Whether those events add up to a decomposition's
    /// row is the residue's business: see `from_decomposition`.)
    #[test]
    fn blocks_split_by_state_partition_the_events() {
        let data = fixture("sqlite-commits");
        let events: Vec<&Event> = data
            .of_source("sched.offcpu")
            .flat_map(|results| results.events())
            .collect();
        let total: f64 = events
            .iter()
            .map(|event| {
                event.number("total_ns").unwrap_or(0.0) - event.number("queued_ns").unwrap_or(0.0)
            })
            .sum();
        let split = split_blocked(&data, &events, None, false);
        let summed: f64 = split.iter().filter_map(|part| part.amount).sum();
        assert!((summed - total).abs() < 1.0, "{summed} != {total}");
        assert!(split.iter().any(|part| part.class == Class::Io));
    }

    /// `sched.futex`'s waits come out of the sleep as a lock, however small a
    /// part of it they are, and never more than the sleep.
    #[test]
    fn futex_waits_are_split_out_of_the_sleep() {
        let data = fixture("workload-timebar");
        let events: Vec<&Event> = data
            .of_source("sched.offcpu")
            .flat_map(|results| results.events())
            .filter(|event| {
                event.attr("state").map(ToString::to_string).as_deref() == Some("sleeping")
            })
            .collect();
        let blocked = |event: &&Event| {
            event.number("total_ns").unwrap_or(0.0) - event.number("queued_ns").unwrap_or(0.0)
        };
        let sleeping: f64 = events.iter().map(blocked).sum();
        assert!(sleeping > 0.0);
        let amount = |parts: &[Part], class: Class| -> f64 {
            parts
                .iter()
                .filter(|part| part.class == class)
                .filter_map(|part| part.amount)
                .sum()
        };
        let none = split_blocked(&data, &events, None, false);
        let whole = amount(&none, Class::Sleep) + amount(&none, Class::Child);
        assert!((whole - sleeping).abs() < 1.0);
        // A quarter of the sleep on a futex: a quarter is a lock.
        let quarter = split_blocked(&data, &events, Some((whole / 4.0, 3)), false);
        assert!((amount(&quarter, Class::Lock) - whole / 4.0).abs() < 1.0);
        assert!(
            (amount(&quarter, Class::Sleep) - (whole * 0.75 - amount(&none, Class::Child))).abs()
                < 1.0
                || amount(&none, Class::Child) > 0.0
        );
        let summed: f64 = quarter.iter().filter_map(|part| part.amount).sum();
        assert!((summed - sleeping).abs() < 1.0, "{summed} != {sleeping}");
        assert!(quarter
            .iter()
            .any(|part| part.named.iter().any(|named| named.contains("futex(2)"))));
        // More futex wait than sleep: the sleep is a lock, and only that.
        let all = split_blocked(&data, &events, Some((sleeping * 2.0, 9)), false);
        let summed: f64 = all.iter().filter_map(|part| part.amount).sum();
        assert!((summed - sleeping).abs() < 1.0);
        assert_eq!(amount(&all, Class::Sleep), 0.0);
    }

    #[test]
    fn a_run_with_nothing_to_split_says_so_and_how_to_get_it() {
        let screen = text(&draw(fixture("machine"), "timebar", WIDTH, HEIGHT));
        assert!(screen.contains("Nothing to split"), "{screen}");
        assert!(
            screen.contains("omn latency --objective latency"),
            "{screen}"
        );
    }

    #[test]
    fn a_live_run_draws_from_its_events_and_dims() {
        let data = fixture("sqlite-commits");
        let half = data.opened_ns() + (data.closed_ns() - data.opened_ns()) / 2;
        let live = as_live_until(&data, half);
        let sheets = sheets(&live);
        let sheet = sheets
            .iter()
            .find(|sheet| sheet.reading)
            .expect("a reading");
        assert!(sheet.bars[0].trust.dims());
        // The screen says it is the view's reading, and the first bar's
        // verdict is the one drawn.
        let expected = match &sheet.bars[0].verdict {
            Verdict::Names { .. } => format!("first suspect  {}", sheet.bars[0].label()),
            Verdict::Undecided(_) => format!("no single suspect  {}", sheet.bars[0].label()),
        };
        let screen = text(&draw(live, "timebar", WIDTH, HEIGHT));
        assert!(screen.contains("the view's reading"), "{screen}");
        assert!(screen.contains(&expected), "{expected}\n{screen}");
    }

    #[test]
    fn a_wait_for_a_child_is_known_by_its_frame() {
        assert!(is_wait_for_child("wait4"));
        assert!(is_wait_for_child("__GI___wait4+0x12"));
        assert!(is_wait_for_child("waitpid@GLIBC"));
        assert!(!is_wait_for_child("fdatasync"));
        assert!(!is_wait_for_child("wait_for_completion"));
    }

    #[test]
    fn keys_move_between_bars_and_open_another_view() {
        let data = fixture("vertical-requests");
        let focus = Focus::default();
        let ctx = Ctx {
            data: &data,
            against: None,
            focus: &focus,
        };
        let mut view = Timebar::default();
        view.key(KeyEvent::from(KeyCode::Down), &ctx);
        match view.key(KeyEvent::from(KeyCode::Enter), &ctx) {
            Action::Goto { view, focus } => {
                assert_eq!(view, "request");
                assert!(focus.request.is_some());
            }
            other => panic!("{other:?}"),
        }
        view.key(KeyEvent::from(KeyCode::Char('d')), &ctx);
        assert_eq!(view.sheet, 1);
        assert_eq!(view.selected, 0);
    }

    /// The gallery's other states of the view, beside the one drawn on each
    /// fixture: the user judges the view on these too.
    #[test]
    fn the_gallery_holds_the_view_live_chosen_and_rescaled() {
        use crate::app::{App, Cost};
        use crate::snapshot::{assert_snapshot, draw_app};
        let app = |data: Data| {
            let mut app = App::new(data);
            app.cost = Cost::Snapshot;
            app.open("timebar").expect("the view");
            app
        };
        // Half way through the sqlite commits, live: from the events alone.
        let data = fixture("sqlite-commits");
        let half = data.opened_ns() + (data.closed_ns() - data.opened_ns()) / 2;
        let mut live = app(as_live_until(&data, half));
        assert_snapshot(
            "timebar--sqlite-commits-live",
            &draw_app(&mut live, WIDTH, HEIGHT),
        );
        // The http server's handler thread chosen: its bracket in the table.
        let mut chosen = app(fixture("http-server"));
        chosen.key(KeyEvent::from(KeyCode::Down));
        assert_snapshot(
            "timebar--http-server-handler",
            &draw_app(&mut chosen, WIDTH, HEIGHT),
        );
        // Each request to its own span, then the block I/O decomposition.
        let mut requests = app(fixture("vertical-requests"));
        requests.key(KeyEvent::from(KeyCode::Char('s')));
        assert_snapshot(
            "timebar--vertical-requests-own-scale",
            &draw_app(&mut requests, WIDTH, HEIGHT),
        );
        requests.key(KeyEvent::from(KeyCode::Char('s')));
        requests.key(KeyEvent::from(KeyCode::Char('d')));
        assert_snapshot(
            "timebar--vertical-requests-block-io",
            &draw_app(&mut requests, WIDTH, HEIGHT),
        );
        // A small terminal: the bars keep their place, the table gives way.
        let data = fixture("http-server");
        let sheets = sheets(&data);
        let first = &sheets.iter().find(|sheet| sheet.reading).unwrap().bars[0];
        let label = format!("{} {}", first.tid.unwrap(), first.comm.as_deref().unwrap());
        let mut small = app(data);
        let screen = text(&draw_app(&mut small, 80, 24));
        assert!(screen.contains(&label), "no {label:?} in\n{screen}");
    }

    #[test]
    fn a_focus_narrows_the_bars_to_its_thread() {
        let data = fixture("http-server");
        let sheets = sheets(&data);
        let sheet = sheets.iter().find(|sheet| sheet.reading).unwrap();
        let tid = sheet.bars[1].tid;
        let focus = Focus {
            tid,
            ..Focus::default()
        };
        let (visible, found) = Timebar::visible(sheet, &focus);
        assert!(found);
        assert_eq!(visible, vec![1]);
        let elsewhere = Focus {
            tid: Some(1),
            ..Focus::default()
        };
        let (visible, found) = Timebar::visible(sheet, &elsewhere);
        assert!(!found);
        assert_eq!(visible.len(), sheet.bars.len());
    }

    fn on_cpu_bracket(least: f64, most: f64) -> Part {
        let mut on = part(Class::OnCpu, "on-cpu", Some(least), 0.0);
        on.upper = Some(most);
        on
    }

    fn thread(tid: i32, parts: Vec<Part>) -> Bar {
        Bar {
            key: format!("task:{tid}"),
            comm: None,
            pid: Some(1),
            tid: Some(tid),
            request: None,
            whole: Some(1000.0),
            whole_resolution: None,
            parts,
            residue: None,
            residue_resolution: None,
            residue_says: None,
            trust: Trust::clean(),
            verdict: Verdict::Undecided(String::new()),
        }
    }

    /// A busy worker beside a short-lived helper: the worker's lower end is
    /// zero, its upper end the window. It is drawn, not folded as a sliver;
    /// only a thread whose states can be under the threshold even at their
    /// most is folded.
    #[test]
    fn a_thread_is_folded_only_when_its_most_is_a_sliver() {
        let worker = thread(
            2,
            vec![
                on_cpu_bracket(0.0, 995.0),
                part(Class::RunQueue, "runnable", Some(3.0), 0.0),
            ],
        );
        let sliver = thread(
            3,
            vec![
                part(Class::OnCpu, "on-cpu", Some(2.0), 0.0),
                part(Class::RunQueue, "runnable", Some(1.0), 0.0),
            ],
        );
        let main = thread(
            1,
            vec![part(Class::Sleep, "blocked: sleeping", Some(990.0), 0.0)],
        );
        let (kept, folded) = fold(vec![main, worker, sliver], Some(1000.0));
        let kept: Vec<Option<i32>> = kept.iter().map(|bar| bar.tid).collect();
        assert_eq!(kept, vec![Some(1), Some(2)]);
        assert_eq!(folded, vec!["task 3".to_owned()]);
    }

    /// A bar whose on cpu is derived from the window gives no residue -- it
    /// would be zero by construction -- and says why instead. That is the
    /// per-process backend's case (`fs:proc`); the recordings now carry the
    /// eBPF one, per thread, so each is also read with its cpu time put back
    /// in the per-process shape: one total for the process.
    #[test]
    fn a_residue_that_cannot_fail_is_not_printed_as_one() {
        for name in ["http-server", "cpython-hot"] {
            let data = as_per_process(fixture(name));
            let sheets = sheets(&data);
            let sheet = sheets.iter().find(|sheet| sheet.reading).expect(name);
            let bracketed: Vec<&Bar> = sheet
                .bars
                .iter()
                .filter(|bar| bar.parts[0].amount.is_some())
                .collect();
            assert!(!bracketed.is_empty(), "{name}: no on-cpu read");
            for bar in bracketed {
                assert_eq!(bar.residue, None, "{name} {}", bar.key);
                assert!(bar.residue_says.is_some(), "{name} {}", bar.key);
            }
            let screen = text(&draw(data, "timebar", WIDTH, HEIGHT));
            assert!(
                screen.contains("not measured: on cpu is derived"),
                "{name}\n{screen}"
            );
        }
    }

    /// Read per thread, on cpu is measured, not derived: a bar whose every
    /// row was read has a residue, which is a measurement and is printed.
    #[test]
    fn a_residue_measured_per_thread_is_printed() {
        for name in ["http-server", "cpython-hot"] {
            let data = fixture(name);
            let sheets = sheets(&data);
            let sheet = sheets.iter().find(|sheet| sheet.reading).expect(name);
            let complete: Vec<&Bar> = sheet
                .bars
                .iter()
                .filter(|bar| bar.parts.iter().all(|part| part.amount.is_some()))
                .collect();
            assert!(!complete.is_empty(), "{name}: no bar read whole");
            for bar in complete {
                assert!(bar.residue.is_some(), "{name} {}", bar.key);
                assert_eq!(bar.residue_says, None, "{name} {}", bar.key);
            }
            let screen = text(&draw(data, "timebar", WIDTH, HEIGHT));
            assert!(!screen.contains("on cpu is derived"), "{name}\n{screen}");
        }
    }

    /// A recording whose `cpu.oncpu_time` was read per thread, in the shape
    /// the per-process backend gives: one series, the process's, holding
    /// the sum of its threads'.
    fn as_per_process(mut data: Data) -> Data {
        let pid = data
            .first("mem.rss")
            .and_then(|results| match results.output("timeline") {
                Some(Output::Timeline { series, .. }) => series.first().map(|s| s.key.clone()),
                _ => None,
            })
            .expect("the process's RSS");
        for results in &mut data.run.results {
            if results.source != "cpu.oncpu_time" {
                continue;
            }
            let total: f64 = match results.output("summary") {
                Some(Output::Summary { series }) => series
                    .iter()
                    .map(|series| series.mean * series.count as f64)
                    .sum(),
                _ => panic!("a summary"),
            };
            results.backend = "fs:proc".to_owned();
            results.outputs = vec![Output::Summary {
                series: vec![crate::model::SeriesSummary {
                    key: pid.clone(),
                    count: 1,
                    min: total,
                    max: total,
                    mean: total,
                    last: total,
                    unmeasured: 0,
                    short: None,
                }],
            }];
        }
        data
    }

    /// A decomposition the view cannot draw is said, not dropped.
    #[test]
    fn a_decomposition_not_drawn_says_why() {
        let mut data = fixture("vertical-requests");
        let mut odd = data.run.decompositions[0].clone();
        odd["id"] = serde_json::json!("in-bytes");
        odd["unit"] = serde_json::json!("bytes");
        data.run.decompositions.push(odd);
        data.run
            .decompositions
            .push(serde_json::json!({"id": "broken", "tables": "not a list"}));
        let sheets = sheets(&data);
        assert!(sheets.iter().all(|sheet| sheet.id != "in-bytes"));
        for sheet in &sheets {
            assert!(
                sheet
                    .notes
                    .iter()
                    .any(|note| note.contains("`in-bytes` is in `bytes`")),
                "{:?}",
                sheet.notes
            );
            assert!(
                sheet
                    .notes
                    .iter()
                    .any(|note| note.contains("`broken` could not be read")),
                "{:?}",
                sheet.notes
            );
        }
    }
}
