//! The `ledger` view: what a process costs, across every resource omn measures.
//!
//! One sheet, one line per resource -- cpu time, memory held and faulted in,
//! I/O through the syscalls, the network, the GPU, energy -- each with its
//! amount, its rate over the run, its shape over the run, what knowing it cost
//! `omn`, and how far it can be trusted. A source the run could not serve is
//! said on its own line, in the words that refused it; one the run did not ask
//! for is said as such. The selected line's detail -- its trust in sentences,
//! what the fallback gave up, why reads were late -- is drawn under the sheet.
//!
//! Every figure is read from what the run holds: the aggregated emits once a
//! run has ended, the `raw` events while it goes (`docs/view/README.md`, "What
//! a run holds, finished and live"), so a live sheet and a recorded one are
//! the same lines. With `--against`, the rate column becomes the second run's
//! amount and its ratio, red where the second run cost more and blue where it
//! cost less.

use std::collections::BTreeMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::{Action, Ctx, Focus, Need, Region, Scope, View};
use crate::model::{Data, Key, Output, Refusal, Results};
use crate::trust::{self, palette, Trust};
use crate::units;

/// What this view needs measured.
pub const NEEDS: &[Need] = &[
    Need {
        source: "cpu.oncpu_time",
        mode: "poll@250ms",
        // `raw` for user and system apart, which only the events carry, and
        // so a live run draws the line from its events as they arrive.
        emit: &["raw", "timeline", "summary"],
        keys: &["task"],
        stack: None,
        scope: Scope::Target,
        why: "cpu time, per thread",
    },
    Need {
        source: "sched.offcpu",
        mode: "trace",
        emit: &["summary"],
        keys: &["task"],
        // The stacks the flame, timebar and lanes ask the same request for.
        stack: Some("fp"),
        scope: Scope::Target,
        why: "blocked time, per thread: the line that opens the timebar",
    },
    Need {
        source: "mem.rss",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "memory held",
    },
    Need {
        source: "mem.faults",
        mode: "trace",
        emit: &["summary"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "memory faulted in",
    },
    Need {
        source: "io.syscall",
        mode: "trace",
        emit: &["raw", "summary", "hist"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "bytes read and written, and their latency as the program saw it",
    },
    Need {
        source: "net.socket",
        mode: "trace",
        emit: &["raw", "hist"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "network volume: bytes sent and received through the target's sockets",
    },
    Need {
        source: "net.tcp.retransmit",
        mode: "trace",
        emit: &["summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "retransmissions, the machine's: a retransmission is sent from a timer, so the collector cannot give it to a process",
    },
    Need {
        source: "gpu.mem",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "GPU memory held",
    },
    Need {
        source: "gpu.kernel",
        mode: "trace",
        emit: &["raw", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "GPU time per kernel",
    },
    Need {
        source: "power.energy",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "energy, where it can be attributed",
    },
];

/// The sheet's keys, for the help line.
const KEYS: &[(&str, &str)] = &[
    ("↑/↓", "line"),
    ("t", "next task"),
    ("enter", "the view behind the line"),
];

#[derive(Default)]
pub struct Ledger {
    /// The line whose detail is drawn under the sheet.
    selected: usize,
    /// The sheet's lines as last drawn, each its index in `ITEMS`.
    regions: Vec<Region>,
}

impl View for Ledger {
    fn name(&self) -> &'static str {
        "ledger"
    }

    fn title(&self) -> &'static str {
        "what a process costs, across every resource omn measures"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        let sheet = Sheet::of(ctx.data, ctx.focus);
        let against = ctx.against.map(|other| Sheet::of(other, ctx.focus));
        self.selected = self.selected.min(ITEMS.len() - 1);
        self.regions = draw_sheet(frame, area, &sheet, against.as_ref(), self.selected);
    }

    fn regions(&self) -> &[Region] {
        &self.regions
    }

    fn select(&mut self, id: usize, _ctx: &Ctx<'_>) -> Action {
        self.selected = id.min(ITEMS.len() - 1);
        Action::None
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(ITEMS.len() - 1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Char('t') => {
                let tasks = tasks_of(ctx.data);
                Action::Focus(match next_task(&tasks, ctx.focus) {
                    Some(tid) => Focus {
                        tid: Some(tid),
                        ..Focus::default()
                    },
                    None => Focus::default(),
                })
            }
            KeyCode::Enter => match ITEMS.get(self.selected).and_then(|item| item.goto) {
                Some(view) => Action::Goto {
                    view,
                    focus: ctx.focus.clone(),
                },
                None => Action::None,
            },
            _ => Action::None,
        }
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        KEYS
    }
}

pub fn view() -> Box<dyn View> {
    Box::<Ledger>::default()
}

/// The task after the focused one, in the order the run holds them; `None`
/// past the last, which clears the focus.
fn next_task(tasks: &[(i32, Option<String>)], focus: &Focus) -> Option<i32> {
    let current = focus.tid.or(focus.pid);
    match current.and_then(|tid| tasks.iter().position(|(task, _)| *task == tid)) {
        Some(index) => tasks.get(index + 1).map(|(tid, _)| *tid),
        None => tasks.first().map(|(tid, _)| *tid),
    }
}

// --- What the sheet is made of ---------------------------------------------

/// One line of the sheet as it is laid out: which need serves it, and which
/// view explains it further.
struct ItemSpec {
    resource: &'static str,
    /// The need's label: the source, with `@system` for a machine-wide one.
    label: &'static str,
    source: &'static str,
    /// The view `enter` opens from this line.
    goto: Option<&'static str>,
}

const ITEMS: &[ItemSpec] = &[
    ItemSpec {
        resource: "cpu time",
        label: "cpu.oncpu_time",
        source: "cpu.oncpu_time",
        goto: Some("flame"),
    },
    ItemSpec {
        resource: "blocked time",
        label: "sched.offcpu",
        source: "sched.offcpu",
        goto: Some("timebar"),
    },
    ItemSpec {
        resource: "memory held",
        label: "mem.rss",
        source: "mem.rss",
        goto: Some("system"),
    },
    ItemSpec {
        resource: "memory faulted",
        label: "mem.faults",
        source: "mem.faults",
        goto: None,
    },
    ItemSpec {
        resource: "I/O syscalls",
        label: "io.syscall",
        source: "io.syscall",
        goto: Some("flows"),
    },
    ItemSpec {
        resource: "network",
        label: "net.socket",
        source: "net.socket",
        goto: Some("flows"),
    },
    ItemSpec {
        resource: "retransmits",
        label: "net.tcp.retransmit@system",
        source: "net.tcp.retransmit",
        goto: Some("flows"),
    },
    ItemSpec {
        resource: "GPU memory",
        label: "gpu.mem",
        source: "gpu.mem",
        goto: Some("system"),
    },
    ItemSpec {
        resource: "GPU time",
        label: "gpu.kernel",
        source: "gpu.kernel",
        goto: Some("lanes"),
    },
    ItemSpec {
        resource: "energy",
        label: "power.energy@system",
        source: "power.energy",
        goto: Some("system"),
    },
];

/// The whole sheet of one run.
struct Sheet {
    items: Vec<Item>,
    /// The run's wall-clock, as the frame's header says it.
    wall_ns: u64,
    /// Every task the per-task results name, with its command where an
    /// event said it.
    tasks: Vec<(i32, Option<String>)>,
    /// What the target's own account says it spent, with its descendants.
    target_cpu: Option<u64>,
    /// What measuring the sheet's lines cost: omn's cpu, and whether some of
    /// it is unknown (a lower bound then).
    cost: Option<(u64, bool)>,
    live: bool,
    /// No target: the run watched the machine.
    machine_run: bool,
    /// Nothing yet says whether the run has a target.
    undecided: bool,
    focus: Focus,
}

/// One line.
struct Item {
    spec: &'static ItemSpec,
    state: State,
}

enum State {
    Served(Box<Served>),
    Refused(Refusal),
    /// The run did not ask for it.
    Absent,
}

/// A served line: what it says, and how far to trust it.
struct Served {
    /// The figure itself, short.
    amount: String,
    /// The one number compared between two runs.
    value: f64,
    /// How the comparison prints `value`, where the amount is another
    /// number; `None` when the amount is `value` itself.
    compared_as: Option<String>,
    /// The rate, a share, or what the figure is a part of.
    rate: Vec<Span<'static>>,
    /// The line's shape over the run: a sparkline, a histogram, or a word.
    graph: Graph,
    /// What knowing it cost omn: its collectors' cpu, and whether some of it
    /// is unknown; `None` while live.
    cost: Option<(u64, bool)>,
    trust: Trust,
    /// Nothing of the focused task: the cost and the trust are the whole
    /// result's, not the task's, so they are not drawn beside the `--`.
    blank: bool,
    /// Lines under it: per task, the split, the estimate.
    subs: Vec<Line<'static>>,
    /// The detail pane when the line is selected.
    detail: Vec<Line<'static>>,
}

enum Graph {
    /// One value per cell, `None` where nothing was measured.
    Spark(Vec<Option<f64>>, Scale),
    /// A distribution: counts per log2 bucket, its lowest and highest bound.
    Hist(Vec<u64>, u64, u64),
    Words(&'static str),
}

#[derive(Clone, Copy, PartialEq)]
enum Scale {
    /// From zero: a rate.
    Zero,
    /// From the series' own lowest value: a level that never nears zero
    /// would otherwise draw as a full bar.
    Span,
}

/// How a source's values add up over time.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    /// Each point is what was spent since the previous one (cpu ns, joules).
    Delta,
    /// Each point is a level held until the next (bytes resident).
    Level,
    /// Each event is one occurrence with its own size (a fault, a kernel).
    Occurrence,
}

/// One series' figures.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Stat {
    /// How many measurements, weighted.
    count: f64,
    /// Their exact sum.
    sum: f64,
    min: f64,
    max: f64,
    first: Option<f64>,
    last: f64,
}

/// Whether a line keeps a series: the focused task's, or every one.
fn keeps(key: &str, focus: &Focus) -> bool {
    match focus.tid.or(focus.pid) {
        Some(id) => {
            key == format!("task:{id}")
                || (key.starts_with("proc:") && key.ends_with(&format!("+{id}")))
        }
        None => true,
    }
}

/// The series key an event is counted under, as the aggregated emits spell
/// it: its task where it has one, else its first key, else the machine.
fn event_key(keys: &[Key]) -> String {
    keys.iter()
        .find(|key| matches!(key, Key::Task { .. }))
        .or(keys.first())
        .map(ToString::to_string)
        .unwrap_or_else(|| "system".to_owned())
}

/// What one event adds to a sum of `attr`: a kernel-built row's exact sum
/// where its value is only its bucket's low bound (`emit.rs`, `total_ns`),
/// else its value times its weight.
fn event_sum(event: &crate::model::Event, attr: &str, value: f64) -> f64 {
    match event.number("total_ns") {
        Some(total) if attr.ends_with("_ns") && attr != "total_ns" => total,
        _ => value * event.number("weight").unwrap_or(1.0),
    }
}

/// Each series' figures, per key: from the `summary` a finished run holds,
/// from the `raw` events of a live one.
fn stats(results: &[&Results], attr: &str, scoped: bool, focus: &Focus) -> BTreeMap<String, Stat> {
    let mut by_key: BTreeMap<String, Stat> = BTreeMap::new();
    for one in results {
        let firsts: BTreeMap<&str, f64> = match one.output("timeline") {
            Some(Output::Timeline { series, .. }) => series
                .iter()
                .filter_map(|series| Some((series.key.as_str(), series.points.first()?.value)))
                .collect(),
            _ => BTreeMap::new(),
        };
        if let Some(Output::Summary { series }) = one.output("summary") {
            for series in series {
                if scoped && !keeps(&series.key, focus) {
                    continue;
                }
                merge(
                    by_key.entry(series.key.clone()).or_default(),
                    Stat {
                        count: series.count as f64,
                        sum: series.mean * series.count as f64,
                        min: series.min,
                        max: series.max,
                        first: firsts.get(series.key.as_str()).copied(),
                        last: series.last,
                    },
                );
            }
            continue;
        }
        for event in one.events() {
            let key = event_key(&event.keys);
            if scoped && !keeps(&key, focus) {
                continue;
            }
            let Some(value) = event.number(attr) else {
                continue;
            };
            merge(
                by_key.entry(key).or_default(),
                Stat {
                    count: event.number("weight").unwrap_or(1.0),
                    sum: event_sum(event, attr, value),
                    min: value,
                    max: value,
                    first: Some(value),
                    last: value,
                },
            );
        }
    }
    by_key
}

fn merge(held: &mut Stat, more: Stat) {
    if held.count == 0.0 && held.first.is_none() {
        *held = more;
        return;
    }
    held.count += more.count;
    held.sum += more.sum;
    held.min = held.min.min(more.min);
    held.max = held.max.max(more.max);
    held.first = held.first.or(more.first);
    held.last = more.last;
}

/// Every series' points, offset from the run's start: the `timeline` a
/// finished run holds, the `raw` events of a live one.
fn points(
    data: &Data,
    results: &[&Results],
    attr: &str,
    scoped: bool,
    focus: &Focus,
) -> Vec<Vec<(u64, f64)>> {
    let opened = data.opened_ns();
    let mut all = Vec::new();
    for one in results {
        let shift = one.window.opened_ns.saturating_sub(opened);
        if let Some(Output::Timeline { series, .. }) = one.output("timeline") {
            for series in series {
                if scoped && !keeps(&series.key, focus) {
                    continue;
                }
                all.push(
                    series
                        .points
                        .iter()
                        .map(|point| (point.offset_ns + shift, point.value))
                        .collect(),
                );
            }
            continue;
        }
        let mut by_key: BTreeMap<String, Vec<(u64, f64)>> = BTreeMap::new();
        for event in one.events() {
            let key = event_key(&event.keys);
            if scoped && !keeps(&key, focus) {
                continue;
            }
            if let Some(value) = event.number(attr) {
                by_key.entry(key).or_default().push((
                    event.ts_ns.saturating_sub(opened),
                    event_sum(event, attr, value),
                ));
            }
        }
        all.extend(by_key.into_values());
    }
    all
}

/// The series laid on `width` cells of the run's `span`: per cell, a rate's
/// time-weighted average, a level's highest value, or the occurrences' sum
/// -- added up over the series.
fn bins(series: &[Vec<(u64, f64)>], span: u64, width: usize, shape: Shape) -> Vec<Option<f64>> {
    let mut out: Vec<Option<f64>> = vec![None; width];
    if width == 0 || span == 0 {
        return out;
    }
    let cell = span as f64 / width as f64;
    let index = |t: f64| ((t / cell) as usize).min(width - 1);
    for points in series {
        let mut one: Vec<Option<f64>> = vec![None; width];
        let mut sorted = points.clone();
        sorted.sort_by_key(|(t, _)| *t);
        match shape {
            Shape::Delta => {
                // A delta is what was spent since the previous point: spread
                // over that interval, not dropped on the cell it was read in.
                let mut covered = vec![0.0; width];
                let mut previous = 0u64;
                for &(t, value) in &sorted {
                    if t <= previous {
                        continue;
                    }
                    let rate = value / (t - previous) as f64;
                    let (from, to) = (previous as f64, t as f64);
                    let cells = index(from)..=index(to);
                    for (b, slot) in one.iter_mut().enumerate() {
                        if !cells.contains(&b) {
                            continue;
                        }
                        let low = (b as f64 * cell).max(from);
                        let high = ((b + 1) as f64 * cell).min(to);
                        if high > low {
                            *slot = Some(slot.unwrap_or(0.0) + rate * (high - low));
                            covered[b] += high - low;
                        }
                    }
                    previous = t;
                }
                for (slot, covered) in one.iter_mut().zip(covered) {
                    if covered > 0.0 {
                        *slot = slot.map(|sum| sum / covered);
                    }
                }
            }
            Shape::Level => {
                for (i, &(t, value)) in sorted.iter().enumerate() {
                    let until = sorted.get(i + 1).map(|(next, _)| *next).unwrap_or(span);
                    let cells = index(t as f64)..=index(until as f64);
                    for (b, slot) in one.iter_mut().enumerate() {
                        if cells.contains(&b) {
                            *slot = Some(slot.map_or(value, |held| held.max(value)));
                        }
                    }
                }
            }
            Shape::Occurrence => {
                for &(t, value) in &sorted {
                    let slot = &mut one[index(t as f64)];
                    *slot = Some(slot.unwrap_or(0.0) + value);
                }
            }
        }
        for (total, value) in out.iter_mut().zip(one) {
            if let Some(value) = value {
                *total = Some(total.unwrap_or(0.0) + value);
            }
        }
    }
    out
}

/// Eight heights of a cell.
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// A sparkline `rows` high, its rows from the top: a column of blocks per
/// cell, blank where nothing was measured; the bottom row is
/// `sparkline`'s, the rows above carry what overflows it.
fn sparkline_rows(values: &[Option<f64>], scale: Scale, rows: usize) -> Vec<String> {
    let rows = rows.max(1);
    let measured = values.iter().flatten().copied();
    let high = measured.clone().fold(f64::MIN, f64::max);
    let low = match scale {
        Scale::Zero => 0.0,
        Scale::Span => measured.fold(f64::MAX, f64::min),
    };
    let eighths: Vec<Option<usize>> = values
        .iter()
        .map(|value| match value {
            None => None,
            Some(_) if high <= low => Some(if high > 0.0 { 4 } else { 1 }),
            Some(value) => {
                let top = 8 * rows - 1;
                Some(((value - low) / (high - low) * top as f64).round() as usize + 1)
            }
        })
        .collect();
    (0..rows)
        .rev()
        .map(|row| {
            eighths
                .iter()
                .map(|eighths| match eighths {
                    None => ' ',
                    Some(eighths) => match eighths.saturating_sub(8 * row).min(8) {
                        0 => ' ',
                        cell => BLOCKS[cell - 1],
                    },
                })
                .collect()
        })
        .collect()
}

/// A sparkline: a block per cell, blank where nothing was measured.
fn sparkline(values: &[Option<f64>], scale: Scale) -> String {
    let measured = values.iter().flatten().copied();
    let high = measured.clone().fold(f64::MIN, f64::max);
    let low = match scale {
        Scale::Zero => 0.0,
        Scale::Span => measured.fold(f64::MAX, f64::min),
    };
    values
        .iter()
        .map(|value| match value {
            None => ' ',
            Some(_) if high <= low => {
                if high > 0.0 {
                    BLOCKS[3]
                } else {
                    BLOCKS[0]
                }
            }
            Some(value) => BLOCKS[(((value - low) / (high - low) * 7.0).round() as usize).min(7)],
        })
        .collect()
}

/// The results a line is served by, or the refusal when it was not.
fn served_by<'a>(data: &'a Data, spec: &ItemSpec) -> Result<Vec<&'a Results>, Option<Refusal>> {
    let held = data.for_need(spec.label, spec.source);
    if !held.is_empty() {
        return Ok(held);
    }
    Err(data
        .refused
        .iter()
        .find(|refusal| refusal.label == spec.label)
        .or_else(|| data.refusal(spec.source))
        .cloned())
}

/// What measuring these results cost omn: their collectors' cpu, plus what
/// they spent inside the target where that is known. `None` while live: the
/// runner says it at the end.
fn cost_of(data: &Data, results: &[&Results]) -> Option<(u64, bool)> {
    if data.live.is_some() {
        return None;
    }
    let mut cpu = 0;
    let mut unknown = false;
    for one in results {
        cpu += one.overhead.attributed_cpu_ns;
        match one.overhead.in_target_cpu_ns {
            Some(inside) => cpu += inside,
            None => unknown = true,
        }
    }
    Some((cpu, unknown))
}

/// The need a line is served by.
fn need_of(label: &str) -> Option<&'static Need> {
    NEEDS.iter().find(|need| need.label() == label)
}

/// The detail every served line starts with: what serves it, and its trust,
/// fallback and losses in sentences.
fn detail_of(spec: &ItemSpec, results: &[&Results], trust: &Trust) -> Vec<Line<'static>> {
    let why = need_of(spec.label).map(|need| need.why).unwrap_or_default();
    let backends: Vec<&str> = results.iter().map(|one| one.backend.as_str()).collect();
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{} via {}", spec.label, backends.join(", ")),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" -- {why}")),
    ])];
    for sentence in trust.sentences() {
        lines.push(Line::from(format!("· {sentence}")));
    }
    for one in results {
        for degradation in &one.degradations {
            lines.push(Line::from(Span::styled(
                format!("· the fallback says: {degradation}"),
                Style::new().fg(palette::LOSS),
            )));
        }
        for (reason, count) in &one.losses.reasons {
            lines.push(Line::from(Span::styled(
                format!("· {count}× {reason}"),
                Style::new().fg(palette::LOSS),
            )));
        }
        for note in &one.notes {
            lines.push(Line::from(format!("· {note}")));
        }
        let overhead = &one.overhead;
        if overhead.wall_ns > 0 {
            let inside = match overhead.in_target_cpu_ns {
                Some(ns) => units::duration(ns as f64),
                None => "unknown".to_owned(),
            };
            let mut cost = format!(
                "· to know it: {} of omn's cpu, and {inside} inside the target",
                units::duration(overhead.attributed_cpu_ns as f64)
            );
            if !overhead.in_target_from.is_empty() {
                cost.push_str(&format!(" -- {}", overhead.in_target_from));
            }
            lines.push(Line::from(Span::styled(
                cost,
                Style::new().fg(palette::COST),
            )));
        }
    }
    lines
}

/// A task's name, for a line under the sheet.
fn task_name(key: &str, tasks: &[(i32, Option<String>)]) -> String {
    let Some(tid) = key
        .strip_prefix("task:")
        .and_then(|tid| tid.parse::<i32>().ok())
    else {
        return key.to_owned();
    };
    match tasks.iter().find(|(task, _)| *task == tid) {
        Some((_, Some(comm))) => format!("{tid} {comm}"),
        _ => format!("task {tid}"),
    }
}

/// The indent of a line under a resource, and its label's width.
const SUB_LABEL: usize = 16;

/// A line under a resource: a label, a figure, and a bar of its share.
fn share_line(label: String, figure: String, share: f64, style: Style) -> Line<'static> {
    let bar = trust::error_bar(share, share, share, 1.0, 12);
    let mut spans = vec![
        Span::raw(format!(
            "    {:<w$}",
            clip(&label, SUB_LABEL - 1),
            w = SUB_LABEL
        )),
        Span::styled(format!("{figure:>9}  "), style),
    ];
    spans.extend(bar.spans.into_iter().map(|span| {
        let patched = span.style.patch(style);
        span.style(patched)
    }));
    spans.push(Span::styled(
        format!(" {:>5}", units::percent(share * 100.0)),
        style,
    ));
    Line::from(spans)
}

/// A line under a resource, label and text.
fn note_line(label: &str, figure: Span<'static>, text: Span<'static>) -> Line<'static> {
    Line::from(vec![
        Span::raw(format!("    {label:<w$}", w = SUB_LABEL)),
        figure,
        Span::raw("  "),
        text,
    ])
}

/// An aside under a resource, in a quieter voice.
fn aside(text: String) -> Line<'static> {
    Line::from(Span::styled(
        format!("    {text}"),
        Style::new().add_modifier(Modifier::ITALIC | Modifier::DIM),
    ))
}

/// The per-task lines of a line several tasks served: the largest first, at
/// most `most`, then what the rest add up to.
fn per_task(
    stats: &BTreeMap<String, Stat>,
    figure: impl Fn(&Stat) -> f64,
    format: impl Fn(f64) -> String,
    tasks: &[(i32, Option<String>)],
    style: Style,
    most: usize,
) -> Vec<Line<'static>> {
    if stats.len() < 2 {
        return Vec::new();
    }
    let total: f64 = stats.values().map(&figure).sum();
    let mut ranked: Vec<(&String, f64)> = stats
        .iter()
        .map(|(key, stat)| (key, figure(stat)))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut lines: Vec<Line<'static>> = ranked
        .iter()
        .take(most)
        .map(|(key, value)| {
            let share = if total > 0.0 { value / total } else { 0.0 };
            share_line(task_name(key, tasks), format(*value), share, style)
        })
        .collect();
    if ranked.len() > most {
        let rest: f64 = ranked[most..].iter().map(|(_, value)| value).sum();
        lines.push(aside(format!(
            "and {} more tasks: {}",
            ranked.len() - most,
            format(rest)
        )));
    }
    lines
}

impl Sheet {
    fn of(data: &Data, focus: &Focus) -> Sheet {
        let wall_ns = data.closed_ns().saturating_sub(data.opened_ns());
        let tasks = tasks_of(data);
        let observed = |system: bool| {
            data.run
                .results
                .iter()
                .filter(|one| crate::app::metered_on_the_machine(one) == system)
                .filter_map(|one| one.overhead.observed_cpu_ns)
                .filter(|cpu| *cpu > 0)
                .max()
        };
        // A run on the machine has no target: every account is the
        // machine's, even a request keyed per task (an eBPF `cpu.oncpu_time`
        // sees every task on the machine). Neither account known -- a live
        // run before its end, no task-keyed event yet -- says nothing either
        // way.
        let undecided = tasks.is_empty() && observed(false).is_none() && observed(true).is_none();
        let machine_run = observed(true).is_some()
            && (observed(false).is_none() || observed(false) == observed(true));
        let context = Context {
            data,
            focus,
            wall_ns,
            tasks: &tasks,
            // A run on the machine has no target: its own account is the
            // machine's.
            target_cpu: observed(false).filter(|_| !machine_run),
            machine_cpu: observed(true),
        };
        let items: Vec<Item> = ITEMS
            .iter()
            .map(|spec| Item {
                spec,
                state: match served_by(data, spec) {
                    Ok(results) => State::Served(Box::new(context.line(spec, &results))),
                    Err(Some(refusal)) => State::Refused(refusal),
                    Err(None) => State::Absent,
                },
            })
            .collect();
        let used: Vec<&Results> = ITEMS
            .iter()
            .filter_map(|spec| served_by(data, spec).ok())
            .flatten()
            .collect();
        Sheet {
            items,
            wall_ns,
            target_cpu: context.target_cpu,
            cost: if used.is_empty() {
                None
            } else {
                cost_of(data, &used)
            },
            live: data.live.is_some(),
            machine_run,
            undecided,
            focus: focus.clone(),
            tasks,
        }
    }
}

/// One line of the sheet as another view reads it: what `process` says of a
/// resource is then the number the ledger draws, never a second reckoning.
#[derive(Debug, Clone, PartialEq)]
pub struct CostLine {
    /// The line's name on the sheet (`cpu time`, `blocked time`, ...).
    pub resource: &'static str,
    /// The source it reads.
    pub source: &'static str,
    /// The view `Enter` on the line opens.
    pub goto: Option<&'static str>,
    pub reading: Reading,
}

/// What a line holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    Served {
        /// The figure as the sheet prints it.
        amount: String,
        /// The one number behind it, in the source's unit (ns, bytes, ...).
        value: f64,
        /// Nothing of the focus in the result: `value` is not the focus's.
        blank: bool,
        trust: Trust,
    },
    Refused(Refusal),
    /// The run did not ask for it.
    Absent,
}

/// The ledger's lines for `focus` (empty: the whole target), in its order.
pub fn reading(data: &Data, focus: &Focus) -> Vec<CostLine> {
    Sheet::of(data, focus)
        .items
        .into_iter()
        .map(|item| CostLine {
            resource: item.spec.resource,
            source: item.spec.source,
            goto: item.spec.goto,
            reading: match item.state {
                State::Served(served) => Reading::Served {
                    amount: served.amount,
                    value: served.value,
                    blank: served.blank,
                    trust: served.trust,
                },
                State::Refused(refusal) => Reading::Refused(refusal),
                State::Absent => Reading::Absent,
            },
        })
        .collect()
}

/// Every task a per-task result of the sheet names, in the order first met,
/// with its command where an event said it.
fn tasks_of(data: &Data) -> Vec<(i32, Option<String>)> {
    let mut tasks: Vec<(i32, Option<String>)> = Vec::new();
    let mut add =
        |tid: i32, comm: Option<String>| match tasks.iter_mut().find(|(task, _)| *task == tid) {
            Some((_, held)) => {
                if held.is_none() {
                    *held = comm;
                }
            }
            None => tasks.push((tid, comm)),
        };
    for spec in ITEMS.iter().filter(|spec| !spec.label.ends_with("@system")) {
        let Ok(results) = served_by(data, spec) else {
            continue;
        };
        for one in results {
            for output in &one.outputs {
                let keys: Vec<&str> = match output {
                    Output::Summary { series } => series.iter().map(|s| s.key.as_str()).collect(),
                    Output::Timeline { series, .. } => {
                        series.iter().map(|s| s.key.as_str()).collect()
                    }
                    _ => Vec::new(),
                };
                for key in keys {
                    if let Some(tid) = key.strip_prefix("task:").and_then(|t| t.parse().ok()) {
                        add(tid, None);
                    }
                }
            }
            for event in one.events() {
                if let Some(tid) = event.tid() {
                    let comm = event
                        .attr("comm")
                        .and_then(|comm| comm.as_str())
                        .map(str::to_owned);
                    add(tid, comm);
                }
            }
        }
    }
    tasks
}

/// What every line is computed against.
struct Context<'a> {
    data: &'a Data,
    focus: &'a Focus,
    wall_ns: u64,
    tasks: &'a [(i32, Option<String>)],
    target_cpu: Option<u64>,
    machine_cpu: Option<u64>,
}

/// How many cells a line's sparkline is computed on: the graph column at its
/// widest, on a terminal 240 cells wide; it is resampled to the column when
/// drawn narrower, so the graph grows with the terminal.
const GRAPH_CELLS: usize = 150;

impl Context<'_> {
    fn line(&self, spec: &'static ItemSpec, results: &[&Results]) -> Served {
        let trust = Trust::joined(self.data, results.iter().copied());
        let style = trust.style();
        let mut served = match spec.label {
            "cpu.oncpu_time" => self.cpu(results, style),
            "mem.rss" => self.held(results, style),
            "sched.offcpu" => self.blocked(results, style),
            "mem.faults" => self.faults(results, style),
            "io.syscall" => self.syscalls(results, style),
            "net.socket" => self.network(results, style),
            "net.tcp.retransmit@system" => self.retransmits(results),
            "gpu.mem" => self.gpu_memory(results, style),
            "gpu.kernel" => self.gpu_time(results, style),
            "power.energy@system" => self.energy(results, style),
            _ => unreachable!("every line of ITEMS has its figures"),
        };
        let scoped = !spec.label.ends_with("@system");
        if scoped && !self.focus.is_empty() && !holds_focus(results, self.focus) {
            // Nothing of the focus here: said rather than drawn as a zero.
            served.amount = "--".to_owned();
            served.rate.clear();
            served.graph = Graph::Words("");
            served.blank = true;
            served.subs = vec![aside(format!(
                "nothing of {} in this result: its series are keyed {}",
                self.focus.describe(),
                series_keys(results).join(", ")
            ))];
        }
        let mut detail = detail_of(spec, results, &trust);
        detail.append(&mut served.detail);
        served.detail = detail;
        served.cost = cost_of(self.data, results);
        served.trust = trust;
        served
    }

    fn spark(
        &self,
        results: &[&Results],
        attr: &str,
        shape: Shape,
        scoped: bool,
        scale: Scale,
    ) -> Graph {
        let series = points(self.data, results, attr, scoped, self.focus);
        if series.iter().all(Vec::is_empty) {
            return Graph::Words("no series kept");
        }
        Graph::Spark(bins(&series, self.wall_ns, GRAPH_CELLS, shape), scale)
    }

    /// A total per second of the run's wall-clock.
    fn per_second(&self, total: f64) -> Option<f64> {
        (self.wall_ns > 0).then(|| total / (self.wall_ns as f64 / 1e9))
    }

    fn cpu(&self, results: &[&Results], style: Style) -> Served {
        let stats = stats(results, "ns", true, self.focus);
        let total: f64 = stats.values().map(|stat| stat.sum).fold(0.0, |a, b| a + b);
        let mut subs = per_task(
            &stats,
            |stat| stat.sum,
            units::duration,
            self.tasks,
            style,
            4,
        );
        let mut detail = Vec::new();
        // The split is carried on the events only: the aggregated emits add
        // up the one value a poll is.
        let (mut user, mut system, mut split) = (0.0, 0.0, false);
        let kept_events = results.iter().any(|one| !one.events().is_empty());
        for one in results {
            for event in one.events() {
                if !keeps(&event_key(&event.keys), self.focus) {
                    continue;
                }
                if let (Some(u), Some(s)) = (event.number("user_ns"), event.number("system_ns")) {
                    user += u;
                    system += s;
                    split = true;
                }
            }
        }
        if split && user + system > 0.0 {
            let whole = user + system;
            subs.push(share_line(
                "user".into(),
                units::duration(user),
                user / whole,
                style,
            ));
            subs.push(share_line(
                "system".into(),
                units::duration(system),
                system / whole,
                style,
            ));
        } else if kept_events {
            let backend = results
                .first()
                .map(|one| one.backend.as_str())
                .unwrap_or("its backend");
            subs.push(aside(format!(
                "user and system apart: {backend} does not split them; fs:proc does"
            )));
        } else {
            subs.push(aside(
                "user and system apart: on the raw events only, which this run did not keep".into(),
            ));
        }
        if self.focus.is_empty() {
            if let Some(account) = self.target_cpu {
                subs.push(note_line(
                    "own account",
                    Span::styled(
                        format!("{:>9}", units::duration(account as f64)),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "with its descendants, as the scheduler counts them",
                        Style::new().add_modifier(Modifier::DIM),
                    ),
                ));
                detail.push(Line::from(format!(
                    "· own account: {}",
                    observed_from(self.data)
                )));
            }
            if let (Some(account), Some(machine)) = (self.target_cpu, self.machine_cpu) {
                subs.push(share_line(
                    "machine busy".into(),
                    units::duration(machine as f64),
                    (account as f64 / machine as f64).min(1.0),
                    Style::new(),
                ));
                detail.push(Line::from(
                    "· machine busy: every cpu's time not idle (/proc/stat) over the same run, whoever \
                     spent it; the share beside it is the own account's",
                ));
            }
        }
        Served {
            amount: format!("{} on cpu", units::duration(total)),
            value: total,
            rate: self
                .per_second(total)
                .map(|ns| {
                    vec![Span::styled(
                        format!("{} of one cpu", units::percent(ns / 1e9 * 100.0)),
                        style,
                    )]
                })
                .unwrap_or_default(),
            graph: self.spark(results, "ns", Shape::Delta, true, Scale::Zero),
            cost: None,
            compared_as: None,
            blank: false,
            trust: Trust::clean(),
            subs,
            detail,
        }
    }

    /// A level: memory held, on the host or on the device.
    fn held(&self, results: &[&Results], style: Style) -> Served {
        let stats = stats(results, "bytes", true, self.focus);
        // Several series peak at different times: their sum's peak is read
        // over time, never the sum of each one's own peak.
        let series = points(self.data, results, "bytes", true, self.focus);
        let (peak, summed_maxima) = if stats.len() > 1 && series.iter().any(|s| !s.is_empty()) {
            (peak_of_levels(&series), false)
        } else {
            (
                stats.values().map(|stat| stat.max).fold(0.0, |a, b| a + b),
                stats.len() > 1,
            )
        };
        let first: f64 = stats
            .values()
            .map(|stat| stat.first.unwrap_or(stat.min))
            .sum();
        let last: f64 = stats.values().map(|stat| stat.last).fold(0.0, |a, b| a + b);
        let grew = last - first;
        let rate = if stats.is_empty() || (peak == 0.0 && last == 0.0) {
            Vec::new()
        } else {
            vec![Span::styled(
                format!(
                    "{}{} from {}",
                    if grew >= 0.0 { "+" } else { "-" },
                    units::bytes(grew.abs()),
                    units::bytes(first)
                ),
                style,
            )]
        };
        Served {
            amount: format!("{} peak", units::bytes(peak)),
            value: peak,
            rate,
            graph: self.spark(results, "bytes", Shape::Level, true, Scale::Span),
            cost: None,
            compared_as: None,
            blank: false,
            trust: Trust::clean(),
            subs: per_task(&stats, |stat| stat.max, units::bytes, self.tasks, style, 4),
            detail: vec![Line::from(format!(
                "· the peak is {}; it went from {} at the first read to {} at the last",
                if summed_maxima {
                    "at most the sum of each series' own highest level, which no series over \
                     time was kept to narrow"
                } else {
                    "the highest level read"
                },
                units::bytes(first),
                units::bytes(last)
            ))],
        }
    }

    /// Time off cpu: what the threads spent not running, blocked or waiting
    /// to be run once woken, per thread. The timebar splits it by state.
    fn blocked(&self, results: &[&Results], style: Style) -> Served {
        let attr = results
            .iter()
            .find_map(|one| one.value_attr.clone())
            .unwrap_or_else(|| "block_ns".to_owned());
        let stats = stats(results, &attr, true, self.focus);
        let total: f64 = stats.values().map(|stat| stat.sum).fold(0.0, |a, b| a + b);
        let series = results.iter().any(|one| !one.events().is_empty());
        let blocks: f64 = stats.values().map(|stat| stat.count).sum();
        Served {
            amount: units::duration(total),
            value: total,
            rate: self
                .per_second(total)
                .map(|rate| {
                    vec![Span::styled(
                        format!("{} threads", approx(rate / 1e9)),
                        style,
                    )]
                })
                .unwrap_or_default(),
            graph: if total <= 0.0 {
                Graph::Words("no block seen")
            } else if series {
                self.spark(results, &attr, Shape::Occurrence, true, Scale::Zero)
            } else {
                Graph::Words("a total only")
            },
            cost: None,
            compared_as: None,
            blank: false,
            trust: Trust::clean(),
            subs: per_task(
                &stats,
                |stat| stat.sum,
                units::duration,
                self.tasks,
                style,
                4,
            ),
            detail: vec![
                Line::from(format!(
                    "· {} blocks; off cpu from the switch out to the switch back in, the wait \
                     to run once woken included",
                    units::count(blocks.round())
                )),
                Line::from(
                    "· the rate is the threads blocked at once on average; enter opens the \
                     timebar, which splits it by state",
                ),
            ],
        }
    }

    fn faults(&self, results: &[&Results], style: Style) -> Served {
        let stats = stats(results, "count", true, self.focus);
        let total: f64 = stats.values().map(|stat| stat.sum).fold(0.0, |a, b| a + b);
        let series = results
            .iter()
            .any(|one| one.output("timeline").is_some() || !one.events().is_empty());
        Served {
            amount: format!("{} faults", units::count(total)),
            value: total,
            rate: self
                .per_second(total)
                .map(|rate| {
                    vec![Span::styled(
                        format!("{}/s", units::count(rate.round())),
                        style,
                    )]
                })
                .unwrap_or_default(),
            graph: if series {
                self.spark(results, "count", Shape::Occurrence, true, Scale::Zero)
            } else {
                Graph::Words("a total only")
            },
            cost: None,
            compared_as: None,
            blank: false,
            trust: Trust::clean(),
            subs: per_task(&stats, |stat| stat.sum, units::count, self.tasks, style, 4),
            detail: vec![Line::from(
                "· a fault is a page the kernel had to map in; how many bytes that was depends \
                 on each mapping's page size, which the count does not carry",
            )],
        }
    }

    fn syscalls(&self, results: &[&Results], style: Style) -> Served {
        let stats = stats(results, "lat_ns", true, self.focus);
        let calls: f64 = stats
            .values()
            .map(|stat| stat.count)
            .fold(0.0, |a, b| a + b);
        let inside: f64 = stats.values().map(|stat| stat.sum).fold(0.0, |a, b| a + b);
        // The distribution, from the histogram a finished run holds.
        let mut buckets: BTreeMap<u64, u64> = BTreeMap::new();
        let mut high = 0;
        for one in results {
            if let Some(Output::Hist { series, .. }) = one.output("hist") {
                for series in series
                    .iter()
                    .filter(|series| keeps(&series.key, self.focus))
                {
                    for bucket in &series.buckets {
                        *buckets.entry(bucket.low).or_default() += bucket.count;
                        high = high.max(bucket.high.unwrap_or(bucket.low * 2));
                    }
                }
            }
        }
        let graph = match buckets.keys().next() {
            Some(&low) => {
                // Every log2 bucket from the lowest to the highest, the empty
                // ones too, so the shape is the distribution's.
                let mut counts = Vec::new();
                let mut at = low.max(1);
                while at < high {
                    counts.push(buckets.get(&at).copied().unwrap_or(0));
                    at *= 2;
                }
                Graph::Hist(counts, low, high)
            }
            None => self.spark(results, "lat_ns", Shape::Occurrence, true, Scale::Zero),
        };
        let mut subs = per_task(
            &stats,
            |stat| stat.sum,
            units::duration,
            self.tasks,
            style,
            3,
        );
        // The bytes are on each row, never on the aggregates.
        let (mut read, mut written, mut neither, mut rows) = (0.0, 0.0, 0.0, 0);
        for one in results {
            for event in one.events() {
                if !keeps(&event_key(&event.keys), self.focus) {
                    continue;
                }
                let bytes = event.number("bytes").unwrap_or(0.0);
                // `vfs.rs` says "unknown" for an op it does not recognise:
                // those bytes went one way or the other, and neither figure
                // may claim them.
                match event.attr("op").and_then(|op| op.as_str()) {
                    Some("write") => written += bytes,
                    Some("read") => read += bytes,
                    _ => neither += bytes,
                }
                rows += 1;
            }
        }
        if rows > 0 {
            let mut text = format!("read, {} written", units::bytes(written));
            if neither > 0.0 {
                text.push_str(&format!(
                    ", {} by calls of no known direction",
                    units::bytes(neither)
                ));
            }
            subs.push(note_line(
                "bytes",
                Span::styled(format!("{:>9}", units::bytes(read)), style),
                Span::styled(text, style),
            ));
        } else {
            subs.push(aside(
                "bytes read and written: on the raw rows only, which this run did not keep".into(),
            ));
        }
        let span = match &graph {
            Graph::Hist(_, low, high) => format!(
                ": {} to {}",
                units::duration(*low as f64),
                units::duration(*high as f64)
            ),
            _ => String::new(),
        };
        Served {
            amount: format!("{} calls", units::count(calls)),
            // Two runs are compared on the time inside the calls, the cost;
            // the comparison says so rather than print it beside the calls.
            value: inside,
            compared_as: Some(format!("{} inside", units::duration(inside))),
            rate: vec![Span::styled(
                format!("{} inside them", units::duration(inside)),
                style,
            )],
            graph,
            cost: None,
            blank: false,
            trust: Trust::clean(),
            subs,
            detail: vec![Line::from(format!(
                "· the time inside them is exact, each kernel row carrying its sum; the \
                 distribution's buckets are log2-wide, from the shortest call to the longest{span}"
            ))],
        }
    }

    /// The bytes through the target's sockets. Each `net.socket` row stands
    /// for every call of one bucket, and its `bytes` is already their sum:
    /// never weighted again.
    fn network(&self, results: &[&Results], style: Style) -> Served {
        let kept = results.iter().any(|one| one.output("raw").is_some());
        let opened = self.data.opened_ns();
        let (mut sent, mut received, mut neither, mut calls) = (0.0, 0.0, 0.0, 0.0);
        let mut by_key: BTreeMap<String, Stat> = BTreeMap::new();
        let mut series: BTreeMap<String, Vec<(u64, f64)>> = BTreeMap::new();
        for one in results {
            for event in one.events() {
                let key = event_key(&event.keys);
                if !keeps(&key, self.focus) {
                    continue;
                }
                let bytes = event.number("bytes").unwrap_or(0.0);
                match event.attr("op").and_then(|op| op.as_str()) {
                    Some("send") => sent += bytes,
                    Some("recv") => received += bytes,
                    _ => neither += bytes,
                }
                calls += event
                    .number("count")
                    .or(event.number("weight"))
                    .unwrap_or(1.0);
                by_key.entry(key.clone()).or_default().sum += bytes;
                series
                    .entry(key)
                    .or_default()
                    .push((event.ts_ns.saturating_sub(opened), bytes));
            }
        }
        let total = sent + received + neither;
        let detail = vec![
            Line::from(
                "· receives are timed at sock_recvmsg for every family; sends for inet, inet6 \
                 and unix sockets only, and neither sendfile, splice nor io_uring is here",
            ),
            Line::from(
                "· the bytes are summed over the calls that returned a count; a receive's \
                 time includes its wait for the peer, so the volume is the line, not the time",
            ),
        ];
        if !kept {
            return Served {
                amount: "--".to_owned(),
                value: 0.0,
                compared_as: None,
                rate: Vec::new(),
                graph: Graph::Words(""),
                cost: None,
                blank: false,
                trust: Trust::clean(),
                subs: vec![aside(
                    "bytes sent and received: on the raw rows only, which this run did not keep"
                        .into(),
                )],
                detail,
            };
        }
        let series: Vec<Vec<(u64, f64)>> = series.into_values().collect();
        Served {
            amount: format!("{} moved", units::bytes(total)),
            value: total,
            compared_as: None,
            rate: vec![Span::styled(
                format!("{} out, {} in", units::bytes(sent), units::bytes(received)),
                style,
            )],
            graph: if series.is_empty() {
                Graph::Words("no socket call")
            } else {
                Graph::Spark(
                    bins(&series, self.wall_ns, GRAPH_CELLS, Shape::Occurrence),
                    Scale::Zero,
                )
            },
            cost: None,
            blank: false,
            trust: Trust::clean(),
            subs: {
                let mut subs =
                    per_task(&by_key, |stat| stat.sum, units::bytes, self.tasks, style, 3);
                if calls > 0.0 {
                    let mut text = format!("{} socket calls", units::count(calls));
                    if neither > 0.0 {
                        text.push_str(&format!(
                            "; {} by calls of no known direction",
                            units::bytes(neither)
                        ));
                    }
                    subs.push(aside(text));
                }
                subs
            },
            detail,
        }
    }

    fn retransmits(&self, results: &[&Results]) -> Served {
        let stats = stats(results, "retransmits", false, self.focus);
        let total: f64 = stats.values().map(|stat| stat.sum).fold(0.0, |a, b| a + b);
        let series = results.iter().any(|one| !one.events().is_empty());
        Served {
            amount: format!("{} sent again", units::count(total)),
            value: total,
            rate: vec![Span::styled(
                "machine-wide",
                Style::new().add_modifier(Modifier::ITALIC),
            )],
            graph: if series {
                self.spark(
                    results,
                    "retransmits",
                    Shape::Occurrence,
                    false,
                    Scale::Zero,
                )
            } else {
                Graph::Words("a total only")
            },
            cost: None,
            compared_as: None,
            blank: false,
            trust: Trust::clean(),
            subs: Vec::new(),
            detail: vec![Line::from(
                "· a retransmission is sent from a timer, in no process's context: the \
                 collector cannot give it to one, so this line is the whole machine's",
            )],
        }
    }

    fn gpu_memory(&self, results: &[&Results], style: Style) -> Served {
        let mut served = self.held(results, style);
        if let Some(device) = device_memory(self.data) {
            served.rate = vec![Span::styled(
                format!("of {} on the device", units::bytes(device)),
                Style::new().add_modifier(Modifier::DIM),
            )];
            served.detail.push(Line::from(format!(
                "· the device held {} at its peak over the run (gpu.mem@system), whoever \
                 held it",
                units::bytes(device)
            )));
        }
        served
    }

    fn gpu_time(&self, results: &[&Results], style: Style) -> Served {
        let stats = stats(results, "duration_ns", true, self.focus);
        let total: f64 = stats.values().map(|stat| stat.sum).fold(0.0, |a, b| a + b);
        let kernels: f64 = stats
            .values()
            .map(|stat| stat.count)
            .fold(0.0, |a, b| a + b);
        let mut by_kernel: BTreeMap<String, f64> = BTreeMap::new();
        for one in results {
            for event in one.events() {
                if !keeps(&event_key(&event.keys), self.focus) {
                    continue;
                }
                if let (Some(name), Some(ns)) = (
                    event.attr("kernel").and_then(|name| name.as_str()),
                    event.number("duration_ns"),
                ) {
                    *by_kernel.entry(name.to_owned()).or_default() += ns;
                }
            }
        }
        let mut ranked: Vec<(String, f64)> = by_kernel.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let subs = ranked
            .iter()
            .take(3)
            .map(|(name, ns)| {
                share_line(
                    name.clone(),
                    units::duration(*ns),
                    if total > 0.0 { ns / total } else { 0.0 },
                    style,
                )
            })
            .collect();
        Served {
            amount: format!("{} on GPU", units::duration(total)),
            value: total,
            rate: vec![Span::styled(
                format!("{} kernels and copies", units::count(kernels)),
                style,
            )],
            graph: self.spark(results, "duration_ns", Shape::Occurrence, true, Scale::Zero),
            cost: None,
            compared_as: None,
            blank: false,
            trust: Trust::clean(),
            subs,
            detail: Vec::new(),
        }
    }

    fn energy(&self, results: &[&Results], style: Style) -> Served {
        let stats = stats(results, "joules", false, self.focus);
        let joules: f64 = stats.values().map(|stat| stat.sum).fold(0.0, |a, b| a + b);
        let mut subs = Vec::new();
        let mut detail = vec![Line::from(
            "· RAPL counts a whole package: nothing measures what one process drew",
        )];
        if !self.focus.is_empty() {
            subs.push(aside(
                "no estimate for one task: the own account it would be charged by is the whole \
                 target's"
                    .into(),
            ));
        } else if let (Some(target), Some(machine)) = (self.target_cpu, self.machine_cpu) {
            let share = (target as f64 / machine as f64).min(1.0);
            subs.push(note_line(
                "≈ the target's",
                Span::styled(
                    format!("{:>9}", format!("{}J", approx(joules * share))),
                    style.add_modifier(Modifier::ITALIC),
                ),
                Span::styled(
                    format!(
                        "an estimate, not a measurement: its share of the machine's busy cpu, {} \
                         of {}",
                        units::duration(target as f64),
                        units::duration(machine as f64)
                    ),
                    Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC),
                ),
            ));
            detail.push(Line::from(
                "· the estimate charges the package's energy by cpu time: idle power, the GPU's \
                 draw and a busier core's higher cost per second are not apportioned",
            ));
        }
        Served {
            amount: format!("{}J machine", approx(joules)),
            value: joules,
            rate: self
                .per_second(joules)
                .map(|watts| {
                    vec![Span::styled(
                        format!("{}W on average", approx(watts)),
                        style,
                    )]
                })
                .unwrap_or_default(),
            graph: self.spark(results, "joules", Shape::Delta, false, Scale::Zero),
            cost: None,
            compared_as: None,
            blank: false,
            trust: Trust::clean(),
            subs,
            detail,
        }
    }
}

/// The highest sum of several levels, each held from its point until its
/// next: the peak of what they held together.
fn peak_of_levels(series: &[Vec<(u64, f64)>]) -> f64 {
    let mut changes: Vec<(u64, usize, f64)> = series
        .iter()
        .enumerate()
        .flat_map(|(index, points)| points.iter().map(move |(at, value)| (*at, index, *value)))
        .collect();
    changes.sort_by_key(|(at, _, _)| *at);
    let mut held = vec![0.0; series.len()];
    let mut peak: f64 = 0.0;
    let mut at = 0;
    while at < changes.len() {
        // Every change at one instant before the sum is read.
        let now = changes[at].0;
        while at < changes.len() && changes[at].0 == now {
            held[changes[at].1] = changes[at].2;
            at += 1;
        }
        peak = peak.max(held.iter().sum());
    }
    peak
}

/// Whether any series or event of these results is the focus's.
fn holds_focus(results: &[&Results], focus: &Focus) -> bool {
    series_keys(results).iter().any(|key| keeps(key, focus))
}

/// Every series key these results hold, aggregated or on their events.
fn series_keys(results: &[&Results]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut add = |key: String| {
        if !keys.contains(&key) {
            keys.push(key);
        }
    };
    for one in results {
        for output in &one.outputs {
            match output {
                Output::Summary { series } => series.iter().for_each(|s| add(s.key.clone())),
                Output::Timeline { series, .. } => series.iter().for_each(|s| add(s.key.clone())),
                Output::Hist { series, .. } => series.iter().for_each(|s| add(s.key.clone())),
                Output::Raw { events } => events.iter().for_each(|e| add(event_key(&e.keys))),
                _ => {}
            }
        }
    }
    keys
}

/// What the device held at its peak, whoever held it, when the run measured
/// it machine-wide.
fn device_memory(data: &Data) -> Option<f64> {
    let device: Vec<&Results> = data
        .run
        .results
        .iter()
        .filter(|one| one.label == "gpu.mem@system")
        .collect();
    if device.is_empty() {
        return None;
    }
    let stats = stats(&device, "bytes", false, &Focus::default());
    Some(stats.values().map(|stat| stat.max).sum())
}

/// Where the target's own account came from, in the runner's words.
fn observed_from(data: &Data) -> String {
    data.run
        .results
        .iter()
        .filter(|one| !one.label.ends_with("@system"))
        .map(|one| one.overhead.observed_from.clone())
        .find(|from| !from.is_empty())
        .unwrap_or_else(|| "not said".to_owned())
}

// --- Drawing ----------------------------------------------------------------

/// A measured quantity in three significant digits, as the units module
/// writes the others: `208`, `53.6`, `7.19`.
fn approx(value: f64) -> String {
    let abs = value.abs();
    if abs >= 1000.0 {
        units::count(value)
    } else if abs >= 100.0 {
        format!("{value:.0}")
    } else if abs >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

/// `text` in at most `width` characters, its end replaced by `…`.
fn clip(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let kept: String = text.chars().take(width - 1).collect();
    format!("{kept}…")
}

/// Spans cut to `width` characters and padded to it.
fn fit(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let room = width.saturating_sub(used);
        if room == 0 {
            break;
        }
        let count = span.content.chars().count();
        if count <= room {
            used += count;
            out.push(span);
        } else {
            out.push(Span::styled(clip(&span.content, room), span.style));
            used = width;
        }
    }
    if used < width {
        out.push(Span::raw(" ".repeat(width - used)));
    }
    out
}

/// The sheet's columns, for a body of `width` cells.
struct Columns {
    resource: usize,
    amount: usize,
    rate: usize,
    graph: usize,
    cost: usize,
}

impl Columns {
    fn for_width(width: usize) -> Columns {
        let (resource, amount, cost) = (16, 17, 11);
        let rate = if width >= 110 { 26 } else { 20 };
        let rest = width.saturating_sub(2 + resource + amount + rate + cost);
        // The graph takes what the badges leave, up to its cells: on a wide
        // terminal the line's history is drawn wider, not left in a corner.
        let graph = rest.saturating_sub(22).clamp(8.min(rest), GRAPH_CELLS);
        Columns {
            resource,
            amount,
            rate,
            graph,
            cost,
        }
    }
}

fn draw_sheet(
    frame: &mut Frame<'_>,
    area: Rect,
    sheet: &Sheet,
    against: Option<&Sheet>,
    selected: usize,
) -> Vec<Region> {
    let mut regions = Vec::new();
    let block = Block::new().borders(Borders::ALL).title(Line::from(vec![
        Span::raw(" ledger -- "),
        Span::styled(title_of(sheet), Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
    ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 20 || inner.height < 4 {
        return regions;
    }
    let columns = Columns::for_width(usize::from(inner.width));
    // On a tall terminal every line's history is drawn two rows high, when
    // the rows it takes still leave the detail pane what it holds.
    let served = sheet
        .items
        .iter()
        .filter(|item| matches!(item.state, State::Served(_)))
        .count();
    let subs: usize = sheet
        .items
        .iter()
        .map(|item| match &item.state {
            State::Served(served) => served.subs.len(),
            _ => 0,
        })
        .sum();
    let below = 1 + cost_lines(sheet).len() + 2;
    let detail_rows: usize = detail_lines(sheet, selected)
        .iter()
        .map(|line| line.width().max(1).div_ceil(usize::from(inner.width)))
        .sum::<usize>()
        + 1;
    let tall = 1 + sheet.items.len() + served + subs + below + detail_rows
        <= usize::from(inner.height);
    let rows = if tall { 2 } else { 1 };
    let mut lines: Vec<Line<'static>> = vec![header_line(&columns, against.is_some())];
    for (index, item) in sheet.items.iter().enumerate() {
        // Each line is one row, two on a tall terminal: the sheet does not
        // wrap.
        let y = inner.y + lines.len() as u16;
        let served_rows = if tall && matches!(item.state, State::Served(_)) {
            2
        } else {
            1
        };
        if y < inner.y + inner.height {
            let height = served_rows.min(inner.y + inner.height - y);
            regions.push(Region::new(Rect::new(inner.x, y, inner.width, height), index));
        }
        let other = against.and_then(|other| other.items.get(index));
        if served_rows == 2 {
            lines.push(upper_line(item, &columns));
        }
        lines.push(item_line(item, other, &columns, index == selected, rows));
        if let State::Served(served) = &item.state {
            lines.extend(served.subs.iter().cloned());
        }
    }
    lines.push(Line::default());
    lines.extend(cost_lines(sheet));
    if sheet
        .items
        .iter()
        .all(|item| matches!(item.state, State::Absent))
    {
        lines.push(Line::default());
        lines.push(aside(
            "This run asked for none of the sheet's sources: another session made it; `omn \
             top -- prog` asks for all of them."
                .into(),
        ));
    }
    let room = inner.height.saturating_sub(lines.len() as u16);
    let pane = if room >= 3 { room } else { 0 };
    let [table, pane_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(pane)]).areas(inner);
    frame.render_widget(Paragraph::new(lines), table);
    if pane > 0 {
        let spec = &ITEMS[selected.min(ITEMS.len() - 1)];
        let pane_block = Block::new()
            .borders(Borders::TOP)
            .border_style(Style::new().add_modifier(Modifier::DIM))
            .title(Span::styled(
                format!(" {} -- ↑/↓ another line ", spec.resource),
                Style::new().fg(palette::ACCENT),
            ));
        frame.render_widget(
            Paragraph::new(detail_lines(sheet, selected))
                .block(pane_block)
                .wrap(Wrap { trim: true }),
            pane_area,
        );
    }
    // The detail pane may cover the sheet's last lines.
    regions.retain(|region| region.rect.y < pane_area.y || pane == 0);
    regions
}

/// The sheet's title: whose cost, over how long.
fn title_of(sheet: &Sheet) -> String {
    let named = |tid: i32| match sheet.tasks.iter().find(|(task, _)| *task == tid) {
        Some((_, Some(comm))) => format!("{comm} (task {tid})"),
        _ => format!("task {tid}"),
    };
    let who = match (sheet.focus.tid.or(sheet.focus.pid), sheet.tasks.as_slice()) {
        (Some(tid), _) => named(tid),
        (None, _) if sheet.machine_run => "the machine".to_owned(),
        (None, _) if sheet.undecided => "the target or the machine".to_owned(),
        (None, tasks) => match tasks {
            [(tid, Some(comm))] => format!("{comm} (task {tid})"),
            [(tid, None)] => format!("task {tid}"),
            [(tid, _), more @ ..] => format!("task {tid} and {} more", more.len()),
            [] => "the target".to_owned(),
        },
    };
    let mut title = format!(
        "what {who} cost over {} of wall-clock",
        units::duration(sheet.wall_ns as f64)
    );
    if sheet.live {
        title.push_str(", so far");
    }
    title
}

fn header_line(columns: &Columns, against: bool) -> Line<'static> {
    let text = format!(
        "  {:<r$}{:<a$}{:<t$}{:<g$} {:>c$}  {}",
        "RESOURCE",
        "AMOUNT",
        if against { "AGAINST" } else { "RATE" },
        "OVER THE RUN",
        "TO KNOW IT",
        "TRUST",
        r = columns.resource,
        a = columns.amount,
        t = columns.rate,
        g = columns.graph,
        c = columns.cost - 1,
    );
    Line::from(Span::styled(
        text,
        Style::new().add_modifier(Modifier::DIM | Modifier::BOLD),
    ))
}

fn item_line(
    item: &Item,
    other: Option<&Item>,
    columns: &Columns,
    selected: bool,
    rows: usize,
) -> Line<'static> {
    let (marker, resource_style) = if selected {
        (
            Span::styled(
                "▸ ",
                Style::new()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Style::new()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (Span::raw("  "), Style::new().add_modifier(Modifier::BOLD))
    };
    let mut spans = vec![
        marker,
        Span::styled(
            format!(
                "{:<w$}",
                clip(item.spec.resource, columns.resource - 1),
                w = columns.resource
            ),
            resource_style,
        ),
    ];
    match &item.state {
        State::Refused(refusal) => spans.push(Span::styled(
            format!("refused: {}", refusal.reason),
            Style::new().fg(palette::REFUSED),
        )),
        State::Absent => spans.push(Span::styled(
            format!("not in this run: {} was not asked for", item.spec.label),
            Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC),
        )),
        State::Served(served) => {
            let style = served.trust.style();
            spans.extend(fit(
                vec![Span::styled(served.amount.clone(), style)],
                columns.amount,
            ));
            let rate = match other.map(|other| &other.state) {
                Some(State::Served(theirs)) => compared(served, theirs),
                Some(_) => vec![Span::styled(
                    "not in the other run",
                    Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC),
                )],
                None => served.rate.clone(),
            };
            spans.extend(fit(rate, columns.rate));
            spans.extend(fit(
                graph_spans_rows(&served.graph, columns.graph, style, rows),
                columns.graph,
            ));
            // Nothing of the focused task: what the whole result cost and how
            // far it is trusted are not this task's, so they are not drawn.
            if !served.blank {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    format!("{:>w$}", cost_text(served.cost), w = columns.cost - 1),
                    Style::new().fg(palette::COST),
                ));
                spans.push(Span::raw("  "));
                spans.extend(served.trust.badges());
            }
        }
    }
    Line::from(spans)
}

/// The second run's amount, and its ratio to this one's in the colour of a
/// difference: red where the second cost more.
fn compared(this: &Served, theirs: &Served) -> Vec<Span<'static>> {
    let ratio = if this.value > 0.0 {
        theirs.value / this.value
    } else if theirs.value > 0.0 {
        f64::INFINITY
    } else {
        1.0
    };
    let mut style = theirs.trust.style().add_modifier(Modifier::BOLD);
    if let Some(colour) = trust::difference_colour(ratio) {
        style = style.fg(colour);
    }
    vec![
        Span::styled(
            format!("{} ", theirs.compared_as.as_ref().unwrap_or(&theirs.amount)),
            theirs.trust.style(),
        ),
        Span::styled(
            if ratio.is_finite() {
                format!("×{ratio:.2}")
            } else {
                "new".to_owned()
            },
            style,
        ),
    ]
}

/// A line's values resampled to the column where it is narrower than the
/// cells, each cell at its peak.
fn resample(values: &[Option<f64>], width: usize) -> Vec<Option<f64>> {
    if values.len() > width && width > 0 {
        (0..width)
            .map(|cell| {
                let from = cell * values.len() / width;
                let to = ((cell + 1) * values.len() / width).max(from + 1);
                values[from..to].iter().flatten().copied().reduce(f64::max)
            })
            .collect()
    } else {
        values.to_vec()
    }
}

/// The row above a line on a tall terminal: the top half of its sparkline,
/// under the graph column, drawn two rows high; a line with no sparkline
/// has an empty row.
fn upper_line(item: &Item, columns: &Columns) -> Line<'static> {
    let State::Served(served) = &item.state else {
        return Line::default();
    };
    let Graph::Spark(values, scale) = &served.graph else {
        return Line::default();
    };
    if served.blank {
        return Line::default();
    }
    let values = resample(values, columns.graph);
    let rows = sparkline_rows(&values, *scale, 2);
    let offset = 2 + columns.resource + columns.amount + columns.rate;
    Line::from(vec![
        Span::raw(" ".repeat(offset)),
        Span::styled(
            rows[0].clone(),
            served.trust.style().fg(palette::ACCENT),
        ),
    ])
}

/// `graph_spans` for a graph drawn `rows` high: its bottom row.
fn graph_spans_rows(graph: &Graph, width: usize, style: Style, rows: usize) -> Vec<Span<'static>> {
    match graph {
        Graph::Spark(values, scale) => {
            let values = resample(values, width);
            let bottom = sparkline_rows(&values, *scale, rows)
                .pop()
                .unwrap_or_default();
            vec![Span::styled(bottom, style.fg(palette::ACCENT))]
        }
        Graph::Hist(counts, low, high) => {
            let low_text = units::duration(*low as f64);
            let high_text = units::duration(*high as f64);
            let labels = low_text.chars().count() + high_text.chars().count() + 2;
            // Adjacent buckets added together until every one of them fits,
            // so a tail is never cut off; the bounds beside the bars while
            // they leave a few cells for them, else the bars alone (the
            // detail names the bounds).
            let labelled = width >= labels + counts.len().min(6);
            let room = if labelled {
                width - labels
            } else {
                width.max(1)
            };
            let shown: Vec<u64> = if counts.len() <= room {
                counts.clone()
            } else {
                let per = counts.len().div_ceil(room);
                counts.chunks(per).map(|chunk| chunk.iter().sum()).collect()
            };
            let values: Vec<Option<f64>> = shown.iter().map(|count| Some(*count as f64)).collect();
            let bars: String = sparkline(&values, Scale::Zero)
                .chars()
                .zip(&shown)
                .map(|(bar, count)| if *count == 0 { ' ' } else { bar })
                .collect();
            let bars = Span::styled(bars, style.fg(palette::ACCENT));
            if !labelled {
                return vec![bars];
            }
            let quiet = Style::new().add_modifier(Modifier::DIM);
            vec![
                Span::styled(format!("{low_text} "), quiet),
                bars,
                Span::styled(format!(" {high_text}"), quiet),
            ]
        }
        Graph::Words(words) => vec![Span::styled(
            (*words).to_owned(),
            Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC),
        )],
    }
}

fn cost_text(cost: Option<(u64, bool)>) -> String {
    match cost {
        None => "at the end".to_owned(),
        Some((ns, unknown)) => format!(
            "{}{}",
            if unknown { "≥" } else { "" },
            units::duration(ns as f64)
        ),
    }
}

/// The sheet's last line: what knowing all of it cost, beside what the
/// target spent.
fn cost_lines(sheet: &Sheet) -> Vec<Line<'static>> {
    let style = Style::new().fg(palette::COST);
    let mut spans = vec![Span::styled(
        "  to know all of this",
        Style::new().add_modifier(Modifier::BOLD),
    )];
    match sheet.cost {
        None if sheet.live => spans.push(Span::styled(
            ": what the collectors spent is said when the run ends",
            style,
        )),
        None => spans.push(Span::styled(": nothing was measured", style)),
        Some((ns, unknown)) => {
            let at_least = if unknown { "≥" } else { "" };
            spans.push(Span::styled(
                format!(": {at_least}{} of omn's cpu", units::duration(ns as f64)),
                style,
            ));
            if let Some(target) = sheet.target_cpu {
                spans.push(Span::styled(
                    format!(
                        ", {at_least}{} of what the target spent",
                        units::percent(ns as f64 / target as f64 * 100.0)
                    ),
                    style,
                ));
            }
            let mut lines = vec![Line::from(spans)];
            if unknown {
                lines.push(aside(
                    "a lower bound: some collectors' time inside the target is not timed here; each \
                     line's detail says which"
                        .into(),
                ));
            }
            if !sheet.focus.is_empty() {
                lines.push(aside(format!(
                    "the whole run's, not {}'s: a collector's time is not split per task",
                    sheet.focus.describe()
                )));
            }
            return lines;
        }
    }
    vec![Line::from(spans)]
}

/// The selected line's detail.
fn detail_lines(sheet: &Sheet, selected: usize) -> Vec<Line<'static>> {
    let Some(item) = sheet.items.get(selected) else {
        return Vec::new();
    };
    match &item.state {
        State::Served(served) => served.detail.clone(),
        State::Refused(refusal) => trust::refusal_lines(refusal),
        State::Absent => vec![
            Line::from(format!(
                "{} -- {}",
                item.spec.label,
                need_of(item.spec.label)
                    .map(|need| need.why)
                    .unwrap_or_default()
            )),
            Line::from(
                "This run did not ask for it. `omn top` live asks for every line of the sheet; \
                 a recording made by another session holds what that session asked.",
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Cost};
    use crate::model::{Attr, AttrValue, Event, Live, Run};
    use crate::snapshot::{self, fixture};

    fn sheet(name: &str) -> Sheet {
        Sheet::of(&fixture(name), &Focus::default())
    }

    fn line<'a>(sheet: &'a Sheet, resource: &str) -> &'a Served {
        match &sheet
            .items
            .iter()
            .find(|item| item.spec.resource == resource)
            .expect("a line of that name")
            .state
        {
            State::Served(served) => served,
            _ => panic!("{resource} is not served"),
        }
    }

    fn words(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect()
    }

    /// The cpu line adds up every poll's delta: the timeline's points and
    /// the summary's mean times its count are the same number.
    #[test]
    fn cpu_time_is_the_sum_of_the_polls() {
        let data = fixture("cpython-hot");
        let results = data.first("cpu.oncpu_time").expect("served");
        let Some(Output::Timeline { series, .. }) = results.output("timeline") else {
            panic!("a timeline");
        };
        let points: f64 = series.iter().flat_map(|s| &s.points).map(|p| p.value).sum();
        let cpu = line(&sheet("cpython-hot"), "cpu time").value;
        assert!((cpu - points).abs() < 1.0, "{cpu} against {points}");
        assert!(cpu > 3e9, "a cpu-bound run of four seconds: {cpu}");
    }

    /// The blocked line is the sum of what `sched.offcpu` saw off cpu, and
    /// `Enter` on it opens the timebar; the public reading says the same.
    #[test]
    fn the_blocked_line_sums_the_time_off_cpu_and_opens_the_timebar() {
        let data = fixture("workload-timebar");
        let results = data.first("sched.offcpu").expect("served");
        let expected: f64 = results
            .events()
            .iter()
            .filter_map(|event| event.number("total_ns"))
            .sum();
        assert!(expected > 0.0);
        let blocked = line(&sheet("workload-timebar"), "blocked time").value;
        assert!(
            (blocked - expected).abs() < 1.0,
            "{blocked} against {expected}"
        );
        let lines = reading(&data, &Focus::default());
        let read = lines
            .iter()
            .find(|line| line.resource == "blocked time")
            .expect("a blocked line");
        assert_eq!(read.goto, Some("timebar"));
        assert!(matches!(read.reading, Reading::Served { value, .. } if value == blocked));
        // Every line of the sheet is in the reading, in its order.
        let names: Vec<&str> = lines.iter().map(|line| line.resource).collect();
        let sheet_names: Vec<&str> = ITEMS.iter().map(|spec| spec.resource).collect();
        assert_eq!(names, sheet_names);
    }

    /// The heavier run costs well more cpu than the lighter one: the
    /// separation the comparison column is there to show.
    #[test]
    fn the_heavier_run_costs_more_cpu() {
        let light = line(&sheet("cpython-hot"), "cpu time").value;
        let heavy = line(&sheet("cpython-hot-heavier"), "cpu time").value;
        assert!(heavy > light * 1.5, "{heavy} against {light}");
    }

    /// Several tasks fault: each has its line under the total.
    #[test]
    fn faults_are_split_per_task_when_several_tasks_fault() {
        let sheet = sheet("http-server");
        let faults = line(&sheet, "memory faulted");
        assert!(faults.subs.len() >= 2, "one line per task");
        assert!(faults.value > 10_000.0, "{}", faults.value);
        // One task, cpython's, faults alone.
        assert!(line(&self::sheet("cpython-hot"), "memory faulted")
            .subs
            .is_empty());
    }

    /// Narrowed to one task, a per-task line counts that task alone; the
    /// machine's lines stay the machine's.
    #[test]
    fn a_focus_narrows_every_per_task_line() {
        let data = fixture("http-server");
        let whole = Sheet::of(&data, &Focus::default());
        let (tid, _) = whole.tasks[whole.tasks.len() - 1];
        let narrowed = Sheet::of(
            &data,
            &Focus {
                tid: Some(tid),
                ..Focus::default()
            },
        );
        // Every per-task line counts no more of the task than of the whole,
        // or says it holds nothing of it.
        for resource in [
            "cpu time",
            "memory held",
            "memory faulted",
            "I/O syscalls",
            "network",
            "GPU memory",
        ] {
            let all = line(&whole, resource);
            let one = line(&narrowed, resource);
            if one.blank {
                assert_eq!(one.amount, "--", "{resource}");
                assert!(words(&one.subs).contains("nothing of"), "{resource}");
                continue;
            }
            assert!(
                one.value <= all.value,
                "{resource}: {} of {}",
                one.value,
                all.value
            );
        }
        // Where the task did hold some, its share is drawn, not a zero; and
        // where others held some too, it is less than the whole.
        for resource in ["memory faulted", "I/O syscalls", "network", "cpu time"] {
            let one = line(&narrowed, resource);
            assert!(!one.blank && one.value > 0.0, "{resource}: {}", one.value);
        }
        // `cpu time` is the eBPF backend's on this run, per thread: it
        // narrows like the probes do.
        for resource in ["memory faulted", "I/O syscalls", "cpu time"] {
            let (all, one) = (line(&whole, resource), line(&narrowed, resource));
            assert!(
                one.value < all.value,
                "{resource}: {} of {}",
                one.value,
                all.value
            );
        }
        // The process-keyed sources (`fs:proc`, the GPU driver) carry the
        // process alone: nothing of one of its threads, said rather than
        // drawn as a zero.
        for resource in ["memory held", "GPU memory"] {
            assert!(line(&narrowed, resource).blank, "{resource}");
        }
        // The machine's lines stay the machine's.
        for resource in ["retransmits", "energy"] {
            assert_eq!(
                line(&whole, resource).value,
                line(&narrowed, resource).value
            );
        }
    }

    /// A line with nothing of the focused task draws neither the whole
    /// result's cost nor its trust beside its `--`, and the sheet's total
    /// says it is the whole run's.
    #[test]
    fn a_line_without_the_focus_claims_no_cost() {
        let data = fixture("http-server");
        let tasks = tasks_of(&data);
        let focus = Focus {
            tid: Some(tasks[tasks.len() - 1].0),
            ..Focus::default()
        };
        let sheet = Sheet::of(&data, &focus);
        let (index, item) = sheet
            .items
            .iter()
            .enumerate()
            .find(|(_, item)| matches!(&item.state, State::Served(s) if s.blank))
            .expect("a line with nothing of the task");
        let columns = Columns::for_width(118);
        let drawn = words(&[item_line(item, None, &columns, false, 1)]);
        let cost = cost_text(match &item.state {
            State::Served(served) => served.cost,
            _ => unreachable!(),
        });
        assert!(!drawn.contains(&cost), "{index}: {drawn}");
        assert!(words(&cost_lines(&sheet)).contains("not split per task"));
    }

    /// Two runs' I/O lines are compared on the time inside the calls, and
    /// the comparison prints that time, not the calls beside its ratio.
    #[test]
    fn the_comparison_prints_the_figure_its_ratio_is_of() {
        let light = sheet("cpython-hot");
        let heavy = sheet("cpython-hot-heavier");
        let (this, theirs) = (line(&light, "I/O syscalls"), line(&heavy, "I/O syscalls"));
        let text = words(&[Line::from(compared(this, theirs))]);
        let ratio = format!("×{:.2}", theirs.value / this.value);
        assert!(text.contains(&ratio), "{text}");
        assert!(text.contains("inside"), "{text}");
        assert!(!text.contains("calls"), "{text}");
    }

    /// The network line is the bytes the socket rows carry, each row's
    /// already summed over its calls, split by direction.
    #[test]
    fn network_volume_is_the_rows_bytes() {
        let data = fixture("http-server");
        let results = data.first("net.socket").unwrap();
        let (mut sent, mut received) = (0.0, 0.0);
        for event in results.events() {
            let bytes = event.number("bytes").unwrap();
            match event.attr("op").and_then(|op| op.as_str()) {
                Some("send") => sent += bytes,
                _ => received += bytes,
            }
        }
        let sheet = self::sheet("http-server");
        let network = line(&sheet, "network");
        assert_eq!(network.value, sent + received);
        assert!(sent > 0.0 && received > 0.0, "{sent} {received}");
        let rate = words(&[Line::from(network.rate.clone())]);
        assert!(
            rate.contains(&format!("{} out", units::bytes(sent))),
            "{rate}"
        );
        // A program with no socket says so rather than draw a zero shape.
        let quiet = self::sheet("cpython-hot");
        assert!(matches!(
            line(&quiet, "network").graph,
            Graph::Words("no socket call")
        ));
    }

    /// Levels held together peak at the highest sum at any one time, not at
    /// the sum of each one's own peak.
    #[test]
    fn levels_peak_together_not_each() {
        let one = vec![(0, 10.0), (10, 1.0)];
        let other = vec![(0, 1.0), (10, 10.0)];
        assert_eq!(peak_of_levels(&[one, other]), 11.0);
        assert_eq!(peak_of_levels(&[vec![(0, 3.0), (5, 7.0)]]), 7.0);
    }

    /// A narrow column draws every bucket of the distribution, added
    /// together, rather than cutting its tail.
    #[test]
    fn a_narrow_distribution_keeps_its_tail() {
        let mut counts = vec![0; 20];
        counts[0] = 5;
        counts[19] = 1;
        let graph = Graph::Hist(counts, 512, 512 << 20);
        for width in [8, 12, 24] {
            let text = words(&[Line::from(graph_spans_rows(&graph, width, Style::new(), 1))]);
            let bars: String = text.chars().filter(|c| ('▁'..='█').contains(c)).collect();
            assert_eq!(bars.chars().count(), 2, "{width}: {text:?}");
            assert!(!text.contains('…'), "{width}: {text:?}");
            assert!(text.chars().count() <= width, "{width}: {text:?}");
        }
    }

    /// An unrecognised syscall op adds to neither read nor written.
    #[test]
    fn bytes_of_no_known_direction_are_neither_read_nor_written() {
        let row = |op: &str, bytes| {
            event(
                1,
                7,
                vec![
                    ("lat_ns", AttrValue::Uint(1024)),
                    ("count", AttrValue::Uint(1)),
                    ("bytes", AttrValue::Uint(bytes)),
                    ("op", AttrValue::Text(op.into())),
                ],
            )
        };
        let data = Data {
            run: Run {
                results: vec![Results {
                    label: "io.syscall".into(),
                    source: "io.syscall".into(),
                    outputs: vec![Output::Raw {
                        events: vec![row("read", 100), row("unknown", 30)],
                    }],
                    ..Results::default()
                }],
                ..Run::default()
            },
            live: Some(Live {
                opened_ns: 0,
                now_ns: 10,
            }),
            ..Data::default()
        };
        let sheet = Sheet::of(&data, &Focus::default());
        let text = words(&line(&sheet, "I/O syscalls").subs);
        assert!(text.contains("100B"), "{text}");
        assert!(
            text.contains("30B by calls of no known direction"),
            "{text}"
        );
    }

    /// A live run with no task yet and no account says neither "the
    /// machine" nor a task.
    #[test]
    fn a_live_run_without_tasks_does_not_claim_the_machine() {
        let data = Data {
            run: Run {
                results: vec![Results {
                    label: "mem.rss".into(),
                    source: "mem.rss".into(),
                    outputs: vec![Output::Raw { events: Vec::new() }],
                    ..Results::default()
                }],
                ..Run::default()
            },
            live: Some(Live {
                opened_ns: 0,
                now_ns: 10,
            }),
            ..Data::default()
        };
        let sheet = Sheet::of(&data, &Focus::default());
        assert!(!sheet.machine_run);
        let title = title_of(&sheet);
        assert!(title.contains("the target or the machine"), "{title}");
        // A finished run on the machine says so, even where a request on it
        // is keyed per task (`machine`'s eBPF `cpu.oncpu_time`), or none
        // asks for a ledger line (`system-wakeup`).
        for name in ["machine", "system-wakeup"] {
            let title = title_of(&self::sheet(name));
            assert!(title.contains("the machine cost"), "{name}: {title}");
        }
    }

    /// A refused source is said on its own line, in the refusal's words.
    #[test]
    fn a_refused_line_says_why() {
        // No recording refuses a ledger line any more (`gpu.kernel` is
        // served since the twentieth wave), so the refusal is the one an
        // earlier recording carried, put in place of the served result.
        let mut data = fixture("cpython-hot");
        data.run
            .results
            .retain(|results| results.source != "gpu.kernel");
        data.refused.push(Refusal {
            label: "gpu.kernel".into(),
            source: "gpu.kernel".into(),
            reason: "no collector is written for `gpu.kernel` on backend `vendor:cupti` yet".into(),
        });
        let sheet = Sheet::of(&data, &Focus::default());
        let item = sheet
            .items
            .iter()
            .find(|item| item.spec.resource == "GPU time")
            .unwrap();
        assert!(
            matches!(&item.state, State::Refused(refusal) if refusal.reason.contains("gpu.kernel"))
        );
        let text = snapshot::text(&snapshot::draw(data, "ledger", 120, 40));
        assert!(
            text.contains("refused: no collector is written for `gpu.kernel`"),
            "{text}"
        );
    }

    /// The energy estimate is the machine's energy times the target's share
    /// of the machine's busy cpu, and says it is one.
    #[test]
    fn energy_is_estimated_by_cpu_share_and_said_so() {
        let sheet = sheet("sqlite-commits");
        let energy = line(&sheet, "energy");
        let target = sheet.target_cpu.unwrap();
        let machine = fixture("sqlite-commits")
            .run
            .results
            .iter()
            .filter(|one| one.label.ends_with("@system"))
            .filter_map(|one| one.overhead.observed_cpu_ns)
            .max()
            .unwrap();
        assert!(target < machine);
        let expected = approx(energy.value * target as f64 / machine as f64);
        let text = words(&energy.subs);
        assert!(text.contains(&format!("{expected}J")), "{text}");
        assert!(text.contains("not a measurement"), "{text}");
    }

    /// The I/O line's time is the exact sum the kernel rows carry, and its
    /// distribution spans the histogram's buckets.
    #[test]
    fn syscall_time_is_exact_and_its_distribution_drawn() {
        let data = fixture("sqlite-commits");
        let results = data.first("io.syscall").unwrap();
        let Some(Output::Hist { series, .. }) = results.output("hist") else {
            panic!("a histogram");
        };
        let sum: f64 = series.iter().map(|series| series.sum).sum();
        let sheet = self::sheet("sqlite-commits");
        let syscalls = line(&sheet, "I/O syscalls");
        assert!(
            (syscalls.value - sum).abs() < 1.0,
            "{} against {sum}",
            syscalls.value
        );
        // The distribution starts at the bucket of the fastest call, a power
        // of two, and spans the buckets the kernel filled.
        let fastest = series
            .iter()
            .map(|series| series.min)
            .fold(f64::INFINITY, f64::min);
        let Graph::Hist(ref counts, low, _) = syscalls.graph else {
            panic!("a histogram");
        };
        assert!(
            low.is_power_of_two() && low as f64 <= fastest && fastest < 2.0 * low as f64,
            "from {low} for a fastest call of {fastest}"
        );
        assert!(counts.len() >= 5, "{counts:?}");
    }

    fn event(ts_ns: u64, tid: i32, attrs: Vec<(&str, AttrValue)>) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: String::new(),
            keys: vec![Key::Task { tid }],
            attrs: attrs
                .into_iter()
                .map(|(name, value)| Attr {
                    name: name.into(),
                    value,
                })
                .collect(),
            collector_id: String::new(),
            seq: 0,
        }
    }

    /// While a run goes, results hold raw events only: the sheet is drawn
    /// from them -- the same totals, the split the aggregates do not carry,
    /// the bytes of each syscall row -- and the cost waits for the end.
    #[test]
    fn a_live_run_is_drawn_from_its_events() {
        let cpu = |ts, user: u64, system: u64| {
            event(
                ts,
                7,
                vec![
                    ("ns", AttrValue::Uint(user + system)),
                    ("user_ns", AttrValue::Uint(user)),
                    ("system_ns", AttrValue::Uint(system)),
                    ("comm", AttrValue::Text("worker".into())),
                ],
            )
        };
        let row = |ts, op: &str, bytes| {
            event(
                ts,
                7,
                vec![
                    ("lat_ns", AttrValue::Uint(1024)),
                    ("count", AttrValue::Uint(4)),
                    ("weight", AttrValue::Uint(4)),
                    ("total_ns", AttrValue::Uint(6000)),
                    ("bytes", AttrValue::Uint(bytes)),
                    ("op", AttrValue::Text(op.into())),
                ],
            )
        };
        let results = |label: &str, events: Vec<Event>| Results {
            label: label.into(),
            source: label.into(),
            events_seen: events.len() as u64,
            outputs: vec![Output::Raw { events }],
            ..Results::default()
        };
        let data = Data {
            run: Run {
                results: vec![
                    results(
                        "cpu.oncpu_time",
                        vec![
                            cpu(250_000_000, 150_000_000, 50_000_000),
                            cpu(500_000_000, 200_000_000, 0),
                        ],
                    ),
                    results(
                        "io.syscall",
                        vec![row(100, "read", 4096), row(200, "write", 512)],
                    ),
                ],
                ..Run::default()
            },
            live: Some(Live {
                opened_ns: 0,
                now_ns: 500_000_000,
            }),
            ..Data::default()
        };
        let sheet = Sheet::of(&data, &Focus::default());
        assert_eq!(sheet.tasks, vec![(7, Some("worker".to_owned()))]);
        let cpu_line = line(&sheet, "cpu time");
        assert_eq!(cpu_line.value, 400_000_000.0);
        assert!(cpu_line.cost.is_none(), "said at the end");
        let syscalls = line(&sheet, "I/O syscalls");
        assert_eq!(syscalls.value, 12_000.0, "the rows' exact sums");
        assert_eq!(syscalls.amount, "8 calls");
        let mut app = App::new(data);
        app.cost = Cost::Snapshot;
        app.open("ledger").unwrap();
        let text = snapshot::text(&snapshot::draw_app(&mut app, 120, 40));
        for wanted in [
            "what worker (task 7) cost over 500ms of wall-clock, so far",
            "350ms",
            "50.0ms",
            "4.00KiB  read, 512B written",
            "said when the run ends",
            "at the end",
        ] {
            assert!(text.contains(wanted), "{wanted:?} missing from\n{text}");
        }
    }

    /// Events kept from a backend that does not split user and system: the
    /// line says which backend, not that the run kept no events.
    #[test]
    fn a_backend_without_the_split_is_named() {
        let data = Data {
            run: Run {
                results: vec![Results {
                    label: "cpu.oncpu_time".into(),
                    source: "cpu.oncpu_time".into(),
                    backend: "bpf:raw_tp".into(),
                    events_seen: 1,
                    outputs: vec![Output::Raw {
                        events: vec![event(250_000_000, 7, vec![("ns", AttrValue::Uint(9))])],
                    }],
                    ..Results::default()
                }],
                ..Run::default()
            },
            ..Data::default()
        };
        let sheet = Sheet::of(&data, &Focus::default());
        let said = words(&line(&sheet, "cpu time").subs);
        assert!(
            said.contains("bpf:raw_tp does not split them; fs:proc does"),
            "{said}"
        );
    }

    #[test]
    fn rates_spread_over_their_interval_and_levels_hold() {
        // Fifty units over the first half, nothing over the second.
        let delta = bins(
            &[vec![(0, 0.0), (50, 50.0), (100, 0.0)]],
            100,
            4,
            Shape::Delta,
        );
        assert_eq!(delta, vec![Some(1.0), Some(1.0), Some(0.0), Some(0.0)]);
        let level = bins(&[vec![(0, 5.0), (60, 9.0)]], 100, 4, Shape::Level);
        assert_eq!(level, vec![Some(5.0), Some(5.0), Some(9.0), Some(9.0)]);
        assert_eq!(sparkline(&[Some(0.0), Some(1.0), None], Scale::Zero), "▁█ ");
        assert_eq!(sparkline(&[Some(5.0), Some(9.0)], Scale::Span), "▁█");
    }

    #[test]
    fn t_steps_through_the_tasks_then_clears() {
        let tasks = vec![(1, None), (2, None)];
        let on = |tid| Focus {
            tid: Some(tid),
            ..Focus::default()
        };
        assert_eq!(next_task(&tasks, &Focus::default()), Some(1));
        assert_eq!(next_task(&tasks, &on(1)), Some(2));
        assert_eq!(next_task(&tasks, &on(2)), None);
    }

    /// The comparison of two runs, and one task narrowed to: drawn into the
    /// gallery beside the per-fixture snapshots, for the person judging.
    #[test]
    fn gallery_comparison_and_focus() {
        let mut app = App::new(fixture("cpython-hot"));
        app.against = Some(fixture("cpython-hot-heavier"));
        app.cost = Cost::Snapshot;
        app.open("ledger").unwrap();
        let buffer = snapshot::draw_app(&mut app, snapshot::WIDTH, snapshot::HEIGHT);
        assert!(snapshot::text(&buffer).contains("AGAINST"));
        snapshot::assert_snapshot("ledger--against--cpython-hot-heavier", &buffer);

        let data = fixture("http-server");
        let tasks = tasks_of(&data);
        let mut app = App::new(data);
        app.cost = Cost::Snapshot;
        app.open("ledger").unwrap();
        app.focus = Focus {
            tid: Some(tasks[tasks.len() - 1].0),
            ..Focus::default()
        };
        // The I/O line selected, so its detail is the one drawn.
        let io = ITEMS
            .iter()
            .position(|spec| spec.resource == "I/O syscalls")
            .expect("an I/O line");
        for _ in 0..io {
            app.key(KeyEvent::from(KeyCode::Down));
        }
        let buffer = snapshot::draw_app(&mut app, snapshot::WIDTH, snapshot::HEIGHT);
        snapshot::assert_snapshot("ledger--focus--http-server", &buffer);
    }
}
