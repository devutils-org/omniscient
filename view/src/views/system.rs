//! The `system` view: the overview: cpu, memory, disk, network, GPU and power, and the processes.
//!
//! A btop-like front page, drawn from what the run measured and nothing else.
//! Three things set it apart from a system monitor:
//!
//! - **One time axis.** Every chart spans the run, from its first window to
//!   its last (or to now, live), so in a column of panels a disk burst and
//!   the cpu that caused it sit one above the other.
//! - **Colour is trust.** The charts are drawn in the terminal's own colour;
//!   every coloured cell on this screen is the trust style -- a loss, a time
//!   error, a refusal, a cost -- so what is coloured is what to read twice.
//!   `gpu.engine_busy`'s one-second sampling window is drawn as a ruler on
//!   the run's own time axis, not as a footnote.
//! - **What it costs.** A corner lists what each panel's collector cost, as a
//!   share of one cpu.
//!
//! It reads a finished run's `timeline`s and, live (when a result holds its
//! `raw` events only), the events themselves -- which say more: the cpu by
//! state, bytes read apart from written, in apart from out, a process's name.
//! What only the events say is drawn when they are there, and said to be
//! missing when they are not.

use std::collections::{BTreeMap, BTreeSet};

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::{Action, Ctx, Focus, Need, Scope, View};
use crate::model::{Data, Event, Output, Refusal, Results};
use crate::trust::{self, palette, Trust};
use crate::units;

/// What this view needs measured.
pub const NEEDS: &[Need] = &[
    Need {
        source: "cpu.util",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the cpu panel: utilization by state, and its sparkline",
    },
    Need {
        source: "mem.pressure",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the memory panel: stall time under memory pressure",
    },
    Need {
        source: "io.bytes",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the disk panel: bytes read and written per device",
    },
    Need {
        source: "net.bytes",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the network panel: bytes in and out per interface",
    },
    Need {
        source: "gpu.engine_busy",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the GPU panel: occupancy per engine",
    },
    Need {
        source: "gpu.mem",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the GPU panel: memory held",
    },
    Need {
        source: "power.energy",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the power panel: watts per RAPL domain",
    },
    Need {
        source: "cpu.oncpu_time",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &["task"],
        stack: None,
        scope: Scope::Target,
        why: "the process list: cpu time per thread",
    },
];

/// The view's keys, for the help line.
const KEYS: &[(&str, &str)] = &[
    ("↑/↓", "process"),
    ("enter", "its ledger"),
    ("s", "sort"),
    ("i", "idle devices"),
];

/// What the process list can be sorted by, in the order `s` steps through.
const SORTS: &[Sort] = &[
    Sort::Cpu,
    Sort::Memory,
    Sort::Faults,
    Sort::Io,
    Sort::RunQueue,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sort {
    Cpu,
    Memory,
    Faults,
    Io,
    RunQueue,
}

impl Sort {
    fn column(self) -> Col {
        match self {
            Sort::Cpu => Col::CpuTime,
            Sort::Memory => Col::Rss,
            Sort::Faults => Col::Faults,
            Sort::Io => Col::InIo,
            Sort::RunQueue => Col::RunQueue,
        }
    }

    fn words(self) -> &'static str {
        match self {
            Sort::Cpu => "cpu",
            Sort::Memory => "memory held",
            Sort::Faults => "faults",
            Sort::Io => "time in I/O",
            Sort::RunQueue => "time waiting to run",
        }
    }
}

pub struct System {
    /// The selected row of the process list, by thread id, so a re-sort or
    /// a live update keeps the same process selected.
    selected: Option<i32>,
    sort: usize,
    /// Whether devices and interfaces that moved nothing are listed one by
    /// one, rather than folded into one line.
    idle: bool,
}

impl View for System {
    fn name(&self) -> &'static str {
        "system"
    }

    fn title(&self) -> &'static str {
        "the overview: cpu, memory, disk, network, GPU and power, and the processes"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        let axis = Axis::of(ctx.data);
        let procs = processes(ctx.data, SORTS[self.sort]);
        let held_machine = MACHINE
            .iter()
            .any(|(label, source)| served(ctx.data, label, source).is_some());
        let list_height = (procs.rows.len() as u16 + 4).clamp(7, 14);
        let top_height = if held_machine {
            let width = usize::from(area.width);
            let lines = |cpu_rows| Strips::new(ctx.data, width, cpu_rows, self.idle).build();
            let mut machine = lines(CPU_ROWS);
            let spare =
                usize::from(area.height).saturating_sub(machine.len() + usize::from(list_height));
            if spare > 0 {
                machine = lines(CPU_ROWS + spare.min(4));
            }
            let height = (machine.len() as u16).min(area.height.saturating_sub(5));
            frame.render_widget(Paragraph::new(machine), Rect { height, ..area });
            height
        } else {
            let height = 5.min(area.height);
            draw_no_machine(frame, Rect { height, ..area }, ctx.data);
            height
        };
        // Below the machine, the rest; without it, what the list needs, so
        // a short run is not drawn as a tall empty frame.
        let rest = area.height - top_height;
        let bottom = Rect {
            y: area.y + top_height,
            height: if held_machine {
                rest
            } else {
                rest.min(list_height + 2)
            },
            ..area
        };
        let [list, corner] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(34.min(area.width / 3)),
        ])
        .areas(bottom);
        self.draw_processes(frame, list, ctx, &axis, &procs);
        draw_cost(frame, corner, ctx.data);
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        let procs = processes(ctx.data, SORTS[self.sort]);
        let index = self
            .selected
            .and_then(|tid| procs.rows.iter().position(|row| row.tid == tid));
        let last = procs.rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                let next = index.map_or(0, |index| (index + 1).min(last));
                self.selected = procs.rows.get(next).map(|row| row.tid);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let next = index.map_or(0, |index| index.saturating_sub(1));
                self.selected = procs.rows.get(next).map(|row| row.tid);
                Action::None
            }
            KeyCode::Enter => match procs.rows.get(index.unwrap_or(0)) {
                Some(row) => Action::Goto {
                    view: "ledger",
                    focus: row.focus(),
                },
                None => Action::None,
            },
            KeyCode::Char('s') => {
                self.sort = (self.sort + 1) % SORTS.len();
                Action::None
            }
            KeyCode::Char('i') => {
                self.idle = !self.idle;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        KEYS
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(System {
        selected: None,
        sort: 0,
        idle: false,
    })
}

/// The machine's panels, by the label `omn top` gives each and its source.
const MACHINE: &[(&str, &str)] = &[
    ("cpu.util@system", "cpu.util"),
    ("mem.pressure@system", "mem.pressure"),
    ("io.bytes@system", "io.bytes"),
    ("net.bytes@system", "net.bytes"),
    ("gpu.engine_busy@system", "gpu.engine_busy"),
    ("gpu.mem@system", "gpu.mem"),
    ("power.energy@system", "power.energy"),
];

// ---------------------------------------------------------------------------
// The data: a source's series, on the run's time axis.
// ---------------------------------------------------------------------------

/// How one point's value is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Per {
    /// A share of something, between 0 and 1: drawn as it is.
    Share,
    /// An amount since the last read (bytes, joules, nanoseconds of cpu):
    /// drawn per second, summed for a total.
    Tick,
    /// An amount held (bytes of memory): drawn as it is, the last is now.
    Level,
}

/// How a source's events are read into a series: the attribute `omn`'s
/// collector names as its value, and the parts only its events carry.
struct Reading {
    attr: &'static str,
    per: Per,
    /// Named parts of the value, each the sum of these attributes.
    parts: &'static [(&'static str, &'static [&'static str])],
    /// An attribute that says the ceiling of a level (the device's memory).
    ceiling: Option<&'static str>,
}

fn reading(source: &str) -> Reading {
    let plain = |attr, per| Reading {
        attr,
        per,
        parts: &[],
        ceiling: None,
    };
    match source {
        "cpu.util" => Reading {
            attr: "busy",
            per: Per::Share,
            parts: &[
                ("user", &["user", "nice"]),
                ("sys", &["system", "irq", "softirq"]),
                ("iowait", &["iowait"]),
                ("steal", &["steal"]),
            ],
            ceiling: None,
        },
        "mem.pressure" => plain("stall_ratio", Per::Share),
        "io.bytes" => Reading {
            attr: "bytes",
            per: Per::Tick,
            parts: &[("read", &["read_bytes"]), ("written", &["write_bytes"])],
            ceiling: None,
        },
        "net.bytes" => Reading {
            attr: "bytes",
            per: Per::Tick,
            parts: &[("in", &["rx"]), ("out", &["tx"])],
            ceiling: None,
        },
        "gpu.engine_busy" => plain("ratio", Per::Share),
        "gpu.mem" => Reading {
            attr: "bytes",
            per: Per::Level,
            parts: &[],
            ceiling: Some("device_total_bytes"),
        },
        "power.energy" => plain("joules", Per::Tick),
        "cpu.oncpu_time" => plain("ns", Per::Tick),
        "mem.rss" => plain("bytes", Per::Level),
        "mem.faults" => plain("count", Per::Tick),
        "io.syscall" => plain("lat_ns", Per::Tick),
        "sched.runqlat" | "sched.futex" => plain("wait_ns", Per::Tick),
        _ => plain("value", Per::Level),
    }
}

/// One point: when, on the run's axis, and what.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Point {
    at_ns: u64,
    /// What was read: an amount since the last read, a share or a level.
    read: f64,
    /// What is drawn: per second for an amount, as read otherwise.
    shown: f64,
}

/// One series of one source.
#[derive(Debug, Clone, Default)]
struct Track {
    key: String,
    points: Vec<Point>,
    /// The value's parts, point for point, when the events carried them.
    parts: Vec<(&'static str, Vec<f64>)>,
    /// A text the events name the series with (a process's `comm`).
    name: Option<String>,
    /// The ceiling a level is held under, when the events say it.
    ceiling: Option<f64>,
}

impl Track {
    fn total(&self) -> f64 {
        self.points.iter().map(|point| point.read).sum()
    }

    fn peak(&self) -> f64 {
        self.points
            .iter()
            .map(|point| point.shown)
            .filter(|shown| shown.is_finite())
            .fold(0.0, f64::max)
    }

    fn mean(&self) -> f64 {
        let finite: Vec<f64> = self
            .points
            .iter()
            .map(|point| point.shown)
            .filter(|shown| shown.is_finite())
            .collect();
        if finite.is_empty() {
            return 0.0;
        }
        finite.iter().sum::<f64>() / finite.len() as f64
    }

    fn last(&self) -> Option<f64> {
        self.points.last().map(|point| point.shown)
    }

    /// Each part's sum over the whole series, by name.
    fn part_totals(&self) -> Vec<(&'static str, f64)> {
        self.parts
            .iter()
            .map(|(name, values)| (*name, values.iter().sum::<f64>()))
            .collect()
    }
}

/// The series of a result: from its `raw` events when it kept them -- a live
/// run, or one that asked for them, and the events say more -- and from its
/// `timeline` otherwise.
fn tracks(data: &Data, results: &Results) -> Vec<Track> {
    let reading = reading(&results.source);
    let timeline = results.outputs.iter().find_map(|output| match output {
        Output::Timeline { series } => Some(series),
        _ => None,
    });
    let raw = results.events();
    if !raw.is_empty() || timeline.is_none() {
        return from_events(data, raw, &reading);
    }
    timeline
        .into_iter()
        .flatten()
        .map(|series| {
            let mut points: Vec<Point> = series
                .points
                .iter()
                .map(|point| Point {
                    at_ns: point.offset_ns,
                    read: point.value,
                    shown: point.value,
                })
                .collect();
            per_second(&mut points, reading.per, &[]);
            Track {
                key: series.key.clone(),
                points,
                ..Track::default()
            }
        })
        .collect()
}

/// The series of a run's events, grouped as `omn` groups a timeline: by the
/// event's keys, or -- for a source keyed on nothing that names a domain --
/// by that name.
fn from_events(data: &Data, events: &[Event], reading: &Reading) -> Vec<Track> {
    let opened = data.opened_ns();
    let mut grouped: BTreeMap<String, (Vec<Option<f64>>, Track)> = BTreeMap::new();
    for event in events {
        let Some(value) = event.number(reading.attr) else {
            continue;
        };
        let key = series_key(event);
        let (intervals, track) = grouped.entry(key.clone()).or_insert_with(|| {
            (
                Vec::new(),
                Track {
                    key,
                    parts: reading
                        .parts
                        .iter()
                        .map(|(name, _)| (*name, Vec::new()))
                        .collect(),
                    ..Track::default()
                },
            )
        });
        intervals.push(event.number("interval_ns"));
        track.points.push(Point {
            at_ns: event.ts_ns.saturating_sub(opened),
            read: value,
            shown: value,
        });
        for ((_, attrs), (_, values)) in reading.parts.iter().zip(track.parts.iter_mut()) {
            let carried: Vec<f64> = attrs.iter().filter_map(|attr| event.number(attr)).collect();
            if carried.is_empty() {
                // Kept apart from a zero, to be dropped below if no event
                // carries the part at all.
                values.push(f64::NAN);
            } else {
                values.push(carried.iter().sum());
            }
        }
        if let Some(name) = event.attr("comm").and_then(|value| value.as_str()) {
            track.name = Some(name.to_owned());
        }
        if let Some(ceiling) = reading.ceiling.and_then(|attr| event.number(attr)) {
            track.ceiling = Some(ceiling);
        }
    }
    grouped
        .into_values()
        .map(|(intervals, mut track)| {
            // A part no event carried is not a part of zero: it is not there.
            track
                .parts
                .retain(|(_, values)| values.iter().any(|value| value.is_finite()));
            for (_, values) in &mut track.parts {
                for value in values.iter_mut().filter(|value| !value.is_finite()) {
                    *value = 0.0;
                }
            }
            per_second(&mut track.points, reading.per, &intervals);
            track
        })
        .collect()
}

/// The key a timeline would file this event under.
fn series_key(event: &Event) -> String {
    if event.keys.is_empty() {
        return event
            .attr("domain")
            .and_then(|value| value.as_str())
            .map_or_else(|| "system".to_owned(), str::to_owned);
    }
    event
        .keys
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Turns amounts since the last read into amounts per second: by the
/// interval the event says it covers, or the spacing of the reads, or --
/// for the first, which has no read before it -- the reads' usual spacing.
fn per_second(points: &mut [Point], per: Per, intervals: &[Option<f64>]) {
    if per != Per::Tick {
        return;
    }
    let mut spacing: Vec<u64> = points
        .windows(2)
        .map(|pair| pair[1].at_ns.saturating_sub(pair[0].at_ns))
        .filter(|gap| *gap > 0)
        .collect();
    spacing.sort_unstable();
    let typical = spacing.get(spacing.len() / 2).copied();
    for index in 0..points.len() {
        let interval = intervals
            .get(index)
            .copied()
            .flatten()
            .filter(|ns| *ns > 0.0)
            .or_else(|| {
                (index > 0)
                    .then(|| points[index].at_ns.saturating_sub(points[index - 1].at_ns))
                    .filter(|gap| *gap > 0)
                    .map(|gap| gap as f64)
            })
            .or(typical.map(|gap| gap as f64));
        points[index].shown = match interval {
            Some(ns) => points[index].read * 1e9 / ns,
            None => f64::NAN,
        };
    }
}

/// What the run holds for one of the view's needs: its results and their
/// trust, or its refusal.
enum Held<'a> {
    Served(Vec<&'a Results>, Trust),
    Refused(&'a Refusal),
}

fn served<'a>(data: &'a Data, label: &str, source: &str) -> Option<Held<'a>> {
    let held: Vec<&Results> = data.for_need(label, source);
    if held.is_empty() {
        // A source refused in one scope may be served in another (`gpu.mem`
        // on the target and on the machine): only a source with no result
        // is drawn as its refusal.
        return data
            .refused
            .iter()
            .find(|refusal| refusal.label == label || refusal.source == source)
            .map(Held::Refused);
    }
    let trust = Trust::joined(data, held.iter().copied());
    Some(Held::Served(held, trust))
}

/// The run's time axis: how long it is.
#[derive(Debug, Clone, Copy)]
struct Axis {
    span_ns: u64,
}

impl Axis {
    fn of(data: &Data) -> Axis {
        Axis {
            span_ns: data.closed_ns().saturating_sub(data.opened_ns()).max(1),
        }
    }

    /// One value per cell: the largest drawn value whose point falls in the
    /// cell's slice of the run -- a spike is never averaged away -- or, for
    /// a cell no point falls in, the value of the point before it, held
    /// until the next. Nothing after the last point: a live run stops where
    /// it is.
    fn cells(&self, points: &[Point], width: usize) -> Vec<Option<f64>> {
        let mut cells = vec![None; width];
        if width == 0 || points.is_empty() {
            return cells;
        }
        let cell_of = |at_ns: u64| {
            ((at_ns as f64 / self.span_ns as f64) * width as f64).clamp(0.0, width as f64 - 1.0)
                as usize
        };
        for point in points.iter().filter(|point| point.shown.is_finite()) {
            let cell = cell_of(point.at_ns);
            cells[cell] = Some(cells[cell].map_or(point.shown, |held: f64| held.max(point.shown)));
        }
        let first = cell_of(points[0].at_ns);
        let last = cell_of(points[points.len() - 1].at_ns);
        let mut held = None;
        for cell in cells.iter_mut().take(last + 1).skip(first) {
            match cell {
                Some(value) => held = Some(*value),
                None => *cell = held,
            }
        }
        cells
    }
}

// ---------------------------------------------------------------------------
// Drawing: charts.
// ---------------------------------------------------------------------------

const EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// An area chart of `rows` lines, each cell filled in eighths of a row up to
/// its value against `max`. A cell with something in it is never drawn
/// empty; a cell measured at zero is a dot on the baseline, and a cell with
/// nothing measured is blank.
fn chart(cells: &[Option<f64>], max: f64, rows: usize, style: Style) -> Vec<Line<'static>> {
    let eighths: Vec<Option<usize>> = cells
        .iter()
        .map(|cell| {
            cell.map(|value| {
                if max <= 0.0 || value <= 0.0 {
                    0
                } else {
                    (((value / max).min(1.0) * (rows * 8) as f64).round() as usize).max(1)
                }
            })
        })
        .collect();
    (0..rows)
        .map(|row| {
            // Row 0 is the top line.
            let floor = (rows - 1 - row) * 8;
            let text: String = eighths
                .iter()
                .map(|filled| match filled {
                    None => ' ',
                    Some(0) if row == rows - 1 => '·',
                    Some(filled) => EIGHTHS[filled.saturating_sub(floor).min(8)],
                })
                .collect();
            Line::from(Span::styled(text, style))
        })
        .collect()
}

/// The smallest round ceiling at or above `peak`: 1, 2 or 5 times a power of
/// ten, so an axis reads at a glance.
fn ceiling(peak: f64) -> f64 {
    if peak.is_nan() || peak <= 0.0 || peak.is_infinite() {
        return 1.0;
    }
    let power = 10f64.powf(peak.log10().floor());
    for step in [1.0, 2.0, 5.0] {
        if step * power >= peak * 0.999_999 {
            return step * power;
        }
    }
    10.0 * power
}

/// A panel's frame: its name, a note, and its trust's badges on the right.
fn panel<'a>(name: &str, note: &str, trust: Option<&Trust>) -> Block<'a> {
    let mut title = vec![Span::styled(
        format!(" {name} "),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if !note.is_empty() {
        title.push(Span::styled(format!("{note} "), dim()));
    }
    let mut block = Block::new().borders(Borders::ALL).title(Line::from(title));
    if let Some(trust) = trust {
        let badges = trust.badges();
        if !badges.is_empty() {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(badges);
            spans.push(Span::raw(" "));
            block = block.title_top(Line::from(spans).right_aligned());
        }
    }
    block
}

/// The width of a row's label, before its chart.
const LABEL: usize = 10;
/// The most cells a row's figure is given, after its chart: a third of the
/// box on a narrow terminal.
const FIGURE: usize = 36;
/// The fewest rows the cpu chart is drawn on.
const CPU_ROWS: usize = 5;

/// A label of exactly `LABEL` cells.
fn label(text: &str) -> Span<'static> {
    let text: String = text.chars().take(LABEL - 1).collect();
    Span::raw(format!("{text:<LABEL$}"))
}

/// A label in the dim style, for a row that continues the one above.
fn dim_label(text: &str) -> Span<'static> {
    let text: String = text.chars().take(LABEL - 1).collect();
    Span::styled(format!("{text:<LABEL$}"), dim())
}

/// Spans cut to exactly `width` cells: padded, or cut with `…`.
fn fit(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let span_width = span.width();
        if used + span_width <= width {
            used += span_width;
            out.push(span);
            continue;
        }
        let room = width - used;
        let mut text = String::new();
        let mut taken = 0;
        for ch in span.content.chars() {
            let cell = Span::raw(ch.to_string()).width();
            if taken + cell + 1 > room {
                break;
            }
            text.push(ch);
            taken += cell;
        }
        if room > 0 {
            text.push('…');
            taken += 1;
        }
        out.push(Span::styled(text, span.style));
        used += taken;
        break;
    }
    if used < width {
        out.push(Span::raw(" ".repeat(width - used)));
    }
    out
}

/// The time axis under the charts: the run's start, its middle and its end.
fn time_axis(axis: &Axis, width: usize, live: bool) -> Vec<Span<'static>> {
    let end = if live {
        format!("now {}", units::duration(axis.span_ns as f64))
    } else {
        units::duration(axis.span_ns as f64)
    };
    let middle = units::duration(axis.span_ns as f64 / 2.0);
    let mut text = vec!['─'; width];
    let mut put = |at: usize, word: &str| {
        for (offset, ch) in word.chars().enumerate() {
            if let Some(cell) = text.get_mut(at + offset) {
                *cell = ch;
            }
        }
    };
    put(0, "└0s");
    let end_at = width.saturating_sub(end.chars().count() + 1);
    let middle_at = (width / 2).saturating_sub(middle.chars().count() / 2);
    if middle_at > 5 && middle_at + middle.chars().count() + 2 < end_at {
        put(middle_at, &middle);
    }
    put(end_at, &format!("{end}┘"));
    vec![Span::styled(text.into_iter().collect::<String>(), dim())]
}

/// A share as a person reads it, a tiny one included: `<0.01%`, not `0.00%`.
fn share(value: f64) -> String {
    if value > 0.0 && value < 0.0001 {
        "<0.01%".to_owned()
    } else {
        units::percent(value * 100.0)
    }
}

/// Watts, to three digits.
fn watts(value: f64) -> String {
    if !value.is_finite() {
        "?W".to_owned()
    } else if value >= 100.0 {
        format!("{value:.0}W")
    } else if value >= 10.0 {
        format!("{value:.1}W")
    } else {
        format!("{value:.2}W")
    }
}

// ---------------------------------------------------------------------------
// Drawing: the machine, one section per resource, on one time axis.
// ---------------------------------------------------------------------------

/// The machine's box, line by line: one section per resource, each chart in
/// the same columns, so one moment is one column from top to bottom.
struct Strips<'a> {
    data: &'a Data,
    axis: Axis,
    /// The box's width inside its borders.
    inner: usize,
    /// Every chart's width.
    chart: usize,
    cpu_rows: usize,
    idle: bool,
    lines: Vec<Line<'static>>,
}

impl<'a> Strips<'a> {
    fn new(data: &'a Data, width: usize, cpu_rows: usize, idle: bool) -> Strips<'a> {
        let inner = width.saturating_sub(2);
        Strips {
            data,
            axis: Axis::of(data),
            inner,
            chart: inner
                .saturating_sub(LABEL + 1 + (inner / 3).clamp(16, FIGURE))
                .max(8),
            cpu_rows,
            idle,
            lines: Vec::new(),
        }
    }

    /// Everything, in order.
    fn build(mut self) -> Vec<Line<'static>> {
        self.cpu();
        self.gpu();
        self.memory();
        self.power();
        self.devices(&DISK);
        self.devices(&NETWORK);
        let mut axis = vec![Span::raw(" ".repeat(LABEL))];
        axis.extend(time_axis(&self.axis, self.chart, self.data.live.is_some()));
        axis.push(Span::styled(
            format!(
                " a cell is {}, drawn at its peak",
                units::duration(self.axis.span_ns as f64 / self.chart as f64)
            ),
            dim(),
        ));
        self.lines.push(boxed(axis, self.inner));
        self.lines
            .push(Line::from(format!("└{}┘", "─".repeat(self.inner))));
        self.lines
    }

    /// A rule across the box naming a section, its trust on the right:
    /// `├ disk  bytes moved ───── ~low det ┤`.
    fn section(&mut self, name: &str, note: &str, trust: Option<&Trust>) {
        let (open, close) = if self.lines.is_empty() {
            ("┌", "┐")
        } else {
            ("├", "┤")
        };
        let mut right = Vec::new();
        if let Some(badges) = trust.map(Trust::badges).filter(|badges| !badges.is_empty()) {
            right.push(Span::raw(" "));
            right.extend(badges);
            right.push(Span::raw(" "));
        }
        right.push(Span::raw(close));
        let right_width: usize = right.iter().map(Span::width).sum();
        let mut left = vec![
            Span::raw(format!("{open} ")),
            Span::styled(name.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ];
        if !note.is_empty() {
            left.push(Span::styled(format!("{note} "), dim()));
        }
        let left_width: usize = left.iter().map(Span::width).sum();
        let total = self.inner + 2;
        let mut spans = if left_width + right_width + 1 > total {
            fit(left, total.saturating_sub(right_width + 1))
        } else {
            left
        };
        let used: usize = spans.iter().map(Span::width).sum();
        spans.push(Span::raw(
            "─".repeat(total.saturating_sub(used + right_width)),
        ));
        spans.extend(right);
        self.lines.push(Line::from(spans));
    }

    /// A row: its label, its chart in the chart's columns, its figure.
    fn row(&mut self, label: Span<'static>, chart: Vec<Span<'static>>, figure: Vec<Span<'static>>) {
        let mut spans = vec![label];
        spans.extend(fit(chart, self.chart));
        spans.push(Span::raw(" "));
        spans.extend(figure);
        self.lines.push(boxed(spans, self.inner));
    }

    /// A section the run does not hold: its refusal, in the refusal's own
    /// words, or that nothing asked for it.
    fn missing(&mut self, name: &str, held: Option<Held<'_>>) {
        self.section(name, "", None);
        let text = match held {
            Some(Held::Refused(refusal)) => Span::styled(
                format!("{} refused: {}", refusal.source, refusal.reason),
                Style::new().fg(palette::REFUSED),
            ),
            _ => Span::styled("not in this run", dim()),
        };
        self.lines.push(boxed(vec![label(""), text], self.inner));
    }

    fn tracks_of(&self, results: &[&Results]) -> Vec<Track> {
        results
            .iter()
            .flat_map(|results| tracks(self.data, results))
            .collect()
    }

    fn cpu(&mut self) {
        let held = served(self.data, "cpu.util@system", "cpu.util");
        let Some(Held::Served(results, trust)) = held else {
            return self.missing("cpu", held);
        };
        let all = self.tracks_of(&results);
        let track = all
            .iter()
            .find(|track| track.key == "system")
            .or(all.first())
            .cloned()
            .unwrap_or_default();
        self.section("cpu", "busy, as a share of the whole machine", Some(&trust));
        let style = trust.style();
        let top = ceiling(track.peak());
        let cells = self.axis.cells(&track.points, self.chart);
        let name = |word: &str| Span::styled(format!("{word:<5}"), dim());
        let mut figures = vec![
            vec![
                name("now"),
                Span::styled(
                    track.last().map_or("-".to_owned(), share),
                    style.add_modifier(Modifier::BOLD),
                ),
            ],
            vec![name("mean"), Span::styled(share(track.mean()), style)],
            vec![name("peak"), Span::styled(share(track.peak()), style)],
        ];
        // The kernel's own account of the same machine over the window, from
        // the run's cost: busy cpu-seconds per second, the figure a person
        // holds against a process's "100 %".
        let busy_cpus = results.iter().find_map(|results| {
            let overhead = &results.overhead;
            overhead
                .observed_cpu_ns
                .filter(|_| overhead.wall_ns > 0 && self.data.live.is_none())
                .map(|observed| observed as f64 / overhead.wall_ns as f64)
        });
        if let Some(cpus) = busy_cpus {
            figures.push(vec![
                Span::styled(format!("{cpus:.2}"), style),
                Span::styled(" cpus busy, on average", dim()),
            ]);
        }
        let parts = track.part_totals();
        let whole: f64 = parts.iter().map(|(_, total)| total).sum();
        if whole <= 0.0 {
            figures.push(vec![Span::styled("by state: in the events only", dim())]);
        }
        let rows = self.cpu_rows.max(figures.len());
        for (index, line) in chart(&cells, top, rows, style).into_iter().enumerate() {
            let scale = if index == 0 {
                format!("{:>8} ┤", share(top))
            } else if index == rows - 1 {
                format!("{:>8} ┤", "0")
            } else {
                format!("{:>8} │", "")
            };
            let figure = figures.get(index).cloned().unwrap_or_default();
            self.row(Span::styled(scale, dim()), line.spans, figure);
        }
        if whole > 0.0 {
            // The busy time by state, as one bar the chart's width: what of
            // it was the programs', the kernel's, waiting on I/O, stolen.
            const FILLS: [&str; 4] = ["█", "▓", "▒", "░"];
            let mut bar = Vec::new();
            let mut figure = Vec::new();
            let mut drawn = 0;
            for (index, (name, total)) in parts.iter().enumerate() {
                let fraction = total / whole;
                let cells = if index + 1 == parts.len() {
                    self.chart.saturating_sub(drawn)
                } else {
                    (fraction * self.chart as f64).round() as usize
                };
                drawn += cells;
                bar.push(Span::styled(
                    FILLS[index % FILLS.len()].repeat(cells),
                    style,
                ));
                if *total > 0.0 {
                    let gap = if figure.is_empty() { "" } else { " " };
                    figure.push(Span::styled(
                        format!("{gap}{}{name} ", FILLS[index % FILLS.len()]),
                        dim(),
                    ));
                    figure.push(Span::styled(share(fraction), style));
                }
            }
            self.row(dim_label("by state"), bar, figure);
        }
    }

    fn gpu(&mut self) {
        let busy = served(self.data, "gpu.engine_busy@system", "gpu.engine_busy");
        let memory = served(self.data, "gpu.mem@system", "gpu.mem");
        if !matches!(busy, Some(Held::Served(..))) && !matches!(memory, Some(Held::Served(..))) {
            return self.missing("gpu", busy.or(memory));
        }
        let mut joined = Trust::clean();
        for held in [&busy, &memory] {
            if let Some(Held::Served(_, trust)) = held {
                joined.merge(trust.clone());
            }
        }
        self.section("gpu", "the device, as its driver says", Some(&joined));
        match busy {
            Some(Held::Served(results, trust)) => {
                let style = trust.style();
                for track in self.tracks_of(&results) {
                    let cells = self.axis.cells(&track.points, self.chart);
                    let top = ceiling(track.peak());
                    let lines = chart(&cells, top, 2, style);
                    let mut lines = lines.into_iter();
                    self.row(
                        label("busy"),
                        lines.next().map(|line| line.spans).unwrap_or_default(),
                        vec![
                            Span::styled(share(track.mean()), style.add_modifier(Modifier::BOLD)),
                            Span::styled(" mean  ", dim()),
                            Span::styled(share(track.peak()), style),
                            Span::styled(" peak", dim()),
                        ],
                    );
                    self.row(
                        dim_label(&format!(" ≤{}", share(top))),
                        lines.next().map(|line| line.spans).unwrap_or_default(),
                        Vec::new(),
                    );
                }
                // The time error, drawn on the run's own axis: how far each
                // point's moment may be from where it is drawn.
                if let Some(error) = results
                    .iter()
                    .filter_map(|results| results.max_ts_err_ns())
                    .max()
                    .filter(|error| *error > 0)
                {
                    let half = (error as f64 / self.axis.span_ns as f64 * self.chart as f64).round()
                        as usize;
                    let cells = (2 * half + 1).clamp(3, self.chart.max(3));
                    let time = Style::new().fg(palette::TIME);
                    self.row(
                        dim_label("when"),
                        vec![Span::styled(format!("├{}┤", "─".repeat(cells - 2)), time)],
                        vec![Span::styled(
                            format!(
                                "±{}: the driver's sampling window",
                                units::duration(error as f64)
                            ),
                            time,
                        )],
                    );
                }
            }
            held => self.missing_row("busy", held),
        }
        match memory {
            Some(Held::Served(results, trust)) => {
                let style = trust.style();
                for track in self.tracks_of(&results) {
                    let now = track.last().unwrap_or(0.0);
                    let flat = track
                        .points
                        .iter()
                        .all(|point| point.shown == track.points[0].shown);
                    let (chart_spans, figure) = match track.ceiling {
                        Some(total) => (
                            chart(&self.axis.cells(&track.points, self.chart), total, 1, style)
                                .remove(0)
                                .spans,
                            vec![
                                Span::styled(units::bytes(now), style.add_modifier(Modifier::BOLD)),
                                Span::styled(format!(" held of {}", units::bytes(total)), dim()),
                            ],
                        ),
                        None if flat => (
                            vec![Span::styled(
                                "held the whole run, unmoved (the device's size: in its events)",
                                dim(),
                            )],
                            vec![
                                Span::styled(units::bytes(now), style.add_modifier(Modifier::BOLD)),
                                Span::styled(" held", dim()),
                            ],
                        ),
                        None => (
                            chart(
                                &self.axis.cells(&track.points, self.chart),
                                track.peak(),
                                1,
                                style,
                            )
                            .remove(0)
                            .spans,
                            vec![
                                Span::styled(units::bytes(now), style.add_modifier(Modifier::BOLD)),
                                Span::styled(" held, peak ", dim()),
                                Span::styled(units::bytes(track.peak()), style),
                            ],
                        ),
                    };
                    self.row(label("memory"), chart_spans, figure);
                }
            }
            held => self.missing_row("memory", held),
        }
    }

    /// One row of a section saying a part of it is not there, and why.
    fn missing_row(&mut self, name: &str, held: Option<Held<'_>>) {
        let text = match held {
            Some(Held::Refused(refusal)) => Span::styled(
                format!("refused: {}", refusal.reason),
                Style::new().fg(palette::REFUSED),
            ),
            _ => Span::styled("not in this run", dim()),
        };
        self.lines.push(boxed(vec![label(name), text], self.inner));
    }

    fn memory(&mut self) {
        let held = served(self.data, "mem.pressure@system", "mem.pressure");
        let Some(Held::Served(results, trust)) = held else {
            return self.missing("memory", held);
        };
        self.section(
            "memory",
            "time the machine stalled waiting for it",
            Some(&trust),
        );
        let style = trust.style();
        for track in self.tracks_of(&results) {
            let peak = track.peak();
            let figure = if peak <= 0.0 {
                vec![Span::styled("never stalled", style)]
            } else {
                vec![
                    Span::styled(share(track.mean()), style.add_modifier(Modifier::BOLD)),
                    Span::styled(" mean  ", dim()),
                    Span::styled(share(peak), style),
                    Span::styled(" peak", dim()),
                ]
            };
            let cells = self.axis.cells(&track.points, self.chart);
            self.row(
                label("stalled"),
                chart(&cells, ceiling(peak), 1, style).remove(0).spans,
                figure,
            );
        }
        // What the target holds, when the run measured it: the one memory
        // figure that is the target's rather than the machine's.
        if let Some(Held::Served(rss, rss_trust)) = served(self.data, "mem.rss", "mem.rss") {
            let rss_style = rss_trust.style();
            let rss_tracks = self.tracks_of(&rss);
            let several = rss_tracks.len() > 1;
            for track in rss_tracks {
                let peak = track.peak();
                let cells = self.axis.cells(&track.points, self.chart);
                let mut figure = vec![
                    Span::styled(
                        units::bytes(track.last().unwrap_or(0.0)),
                        rss_style.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" held, peak ", dim()),
                    Span::styled(units::bytes(peak), rss_style),
                ];
                if rss_trust != trust {
                    figure.push(Span::raw(" "));
                    figure.extend(rss_trust.badges());
                }
                let name = match task_of(&track.key) {
                    Some(tid) if several => tid.to_string(),
                    _ => "target".to_owned(),
                };
                self.row(
                    label(&name),
                    chart(&cells, peak, 1, rss_style).remove(0).spans,
                    figure,
                );
            }
        }
    }

    fn power(&mut self) {
        let held = served(self.data, "power.energy@system", "power.energy");
        let Some(Held::Served(results, trust)) = held else {
            return self.missing("power", held);
        };
        self.section("power", "watts, from the RAPL counters", Some(&trust));
        let style = trust.style();
        let all = self.tracks_of(&results);
        let top = all.iter().map(Track::peak).fold(0.0, f64::max);
        for track in &all {
            let cells = self.axis.cells(&track.points, self.chart);
            let name = if track.key == "system" {
                "package"
            } else {
                track.key.as_str()
            };
            self.row(
                label(name),
                chart(&cells, top, 1, style).remove(0).spans,
                vec![
                    Span::styled(
                        watts(track.total() * 1e9 / self.axis.span_ns as f64),
                        style.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" mean  ", dim()),
                    Span::styled(watts(track.peak()), style),
                    Span::styled(" peak  ", dim()),
                    Span::styled(format!("{}J", units::count(track.total().round())), style),
                ],
            );
        }
    }

    fn devices(&mut self, what: &Devices) {
        let held = served(self.data, what.label, what.source);
        let Some(Held::Served(results, trust)) = held else {
            return self.missing(what.name, held);
        };
        let mut all = self.tracks_of(&results);
        all.sort_by(|one, other| {
            other
                .total()
                .total_cmp(&one.total())
                .then_with(|| one.key.cmp(&other.key))
        });
        let split = all.iter().any(|track| !track.parts.is_empty());
        let note = if split {
            what.note.to_owned()
        } else {
            format!(
                "{}; {} apart from {}: in the events only",
                what.note, what.parts.0, what.parts.1
            )
        };
        self.section(what.name, &note, Some(&trust));
        let style = trust.style();
        // One scale for every row, so two devices compare by height.
        let top = all.iter().map(Track::peak).fold(0.0, f64::max);
        let idle: Vec<String> = all
            .iter()
            .filter(|track| track.total() <= 0.0)
            .map(|track| device_name(&track.key))
            .collect();
        let listed = self.idle;
        for track in all.iter().filter(|track| listed || track.total() > 0.0) {
            let cells = self.axis.cells(&track.points, self.chart);
            let parts = track.part_totals();
            let figure = if parts.len() == 2 {
                vec![
                    Span::styled(units::bytes(parts[0].1), style.add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {}  ", what.parts.0), dim()),
                    Span::styled(units::bytes(parts[1].1), style.add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {}", what.parts.1), dim()),
                ]
            } else {
                vec![
                    Span::styled(
                        units::bytes(track.total()),
                        style.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  peak ", dim()),
                    Span::styled(format!("{}/s", units::bytes(track.peak())), style),
                ]
            };
            self.row(
                label(&device_name(&track.key)),
                chart(&cells, top, 1, style).remove(0).spans,
                figure,
            );
        }
        if all.is_empty() {
            self.lines.push(boxed(
                vec![label(""), Span::styled("nothing in it", dim())],
                self.inner,
            ));
        } else if !self.idle && !idle.is_empty() {
            self.lines.push(boxed(
                vec![
                    label(""),
                    Span::styled(
                        format!("+{} moved nothing: {}", idle.len(), idle.join(" ")),
                        dim(),
                    ),
                ],
                self.inner,
            ));
        }
    }
}

/// A line of the box: its content between two borders.
fn boxed(spans: Vec<Span<'static>>, inner: usize) -> Line<'static> {
    let mut line = vec![Span::raw("│")];
    line.extend(fit(spans, inner));
    line.push(Span::raw("│"));
    Line::from(line)
}

/// What a section of devices draws: disks or interfaces.
struct Devices {
    name: &'static str,
    note: &'static str,
    label: &'static str,
    source: &'static str,
    /// The two parts the events split the bytes into.
    parts: (&'static str, &'static str),
}

const DISK: Devices = Devices {
    name: "disk",
    note: "bytes moved, per device",
    label: "io.bytes@system",
    source: "io.bytes",
    parts: ("read", "written"),
};

const NETWORK: Devices = Devices {
    name: "network",
    note: "bytes moved, per interface",
    label: "net.bytes@system",
    source: "net.bytes",
    parts: ("in", "out"),
};

/// A device's name as a person knows it: `wlan0` for `iface:wlan0`,
/// `259:0` for `dev:259+0`.
fn device_name(key: &str) -> String {
    if let Some(name) = key.strip_prefix("iface:") {
        return name.to_owned();
    }
    if let Some(dev) = key.strip_prefix("dev:") {
        return dev.replace('+', ":");
    }
    key.to_owned()
}

/// When the run asked for none of the machine's panels -- a session made by
/// `omn latency`, or by hand -- one paragraph says so, rather than seven
/// empty frames.
fn draw_no_machine(frame: &mut Frame<'_>, area: Rect, data: &Data) {
    let labels: Vec<String> = data
        .run
        .results
        .iter()
        .map(|results| {
            if results.label == results.source {
                results.label.clone()
            } else {
                format!("{} ({})", results.label, results.source)
            }
        })
        .collect();
    let text = vec![
        Line::from(Span::styled(
            "This run did not measure the machine: no cpu, memory, disk, network, GPU or power \
             source is in it. The session that made it asked for:",
            dim(),
        )),
        Line::from(labels.join(", ")),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .block(panel("the machine", "", None))
            .wrap(Wrap { trim: true }),
        area,
    );
}

// ---------------------------------------------------------------------------
// The process list.
// ---------------------------------------------------------------------------

/// A column of the process list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Col {
    Cpu,
    Spark,
    CpuTime,
    Rss,
    Faults,
    RunQueue,
    IoCalls,
    InIo,
    Lock,
    GpuMem,
}

/// Each column: its heading, its width, and the source it is read from, in
/// the order they give way when the panel is narrow -- the last first.
const COLUMNS: &[(Col, &str, usize, &str)] = &[
    (Col::Cpu, "CPU", 6, "cpu.oncpu_time"),
    (Col::Spark, "cpu over the run", 16, "cpu.oncpu_time"),
    (Col::CpuTime, "ON CPU", 7, "cpu.oncpu_time"),
    (Col::Rss, "RSS", 8, "mem.rss"),
    (Col::Faults, "FAULTS", 6, "mem.faults"),
    (Col::RunQueue, "RUN QUEUE", 9, "sched.runqlat"),
    (Col::IoCalls, "I/O CALLS", 9, "io.syscall"),
    (Col::InIo, "IN I/O", 7, "io.syscall"),
    (Col::Lock, "ON LOCKS", 8, "sched.futex"),
    (Col::GpuMem, "GPU MEM", 8, "gpu.mem"),
];

/// One process (or thread) of the list.
#[derive(Debug, Clone, Default)]
struct Proc {
    tid: i32,
    name: Option<String>,
    /// Whether the id came from a per-process reader (`/proc/<pid>`, which
    /// `cpu.oncpu_time` reads), so it is a process's and not only a thread's.
    process: bool,
    cpu: Option<Track>,
    cells: BTreeMap<Col, f64>,
}

impl Proc {
    fn focus(&self) -> Focus {
        Focus {
            pid: self.process.then_some(self.tid),
            tid: (!self.process).then_some(self.tid),
            request: None,
        }
    }
}

/// The list, and how far each column can be trusted.
struct Procs {
    rows: Vec<Proc>,
    columns: Vec<(Col, Trust)>,
    refused: Option<Refusal>,
}

/// The task a series key names, if it names one.
fn task_of(key: &str) -> Option<i32> {
    key.split(',')
        .find_map(|part| part.strip_prefix("task:"))
        .and_then(|tid| tid.parse().ok())
}

/// Per task, what a source's events added up to and how many there were:
/// from its `summary` (`count` x `mean`), or, live, from its events.
fn per_task(results: &Results) -> BTreeMap<i32, (f64, f64)> {
    let mut totals = BTreeMap::new();
    if let Some(Output::Summary { series }) = results.output("summary") {
        for series in series {
            if let Some(tid) = task_of(&series.key) {
                let count = series.count as f64;
                let entry = totals.entry(tid).or_insert((0.0, 0.0));
                entry.0 += series.mean * count;
                entry.1 += count;
            }
        }
        return totals;
    }
    let attr = reading(&results.source).attr;
    for event in results.events() {
        if let Some(tid) = event.tid() {
            let entry = totals.entry(tid).or_insert((0.0, 0.0));
            entry.0 += event.number(attr).unwrap_or(0.0);
            entry.1 += 1.0;
        }
    }
    totals
}

/// Every task the run measured, with what each source says of it. Sources
/// on the machine (`@system`) are not a process's and are left out.
fn processes(data: &Data, sort: Sort) -> Procs {
    let mut rows: BTreeMap<i32, Proc> = BTreeMap::new();
    let mut columns: Vec<(Col, Trust)> = Vec::new();
    for &(col, _, _, source) in COLUMNS {
        let held: Vec<&Results> = data
            .of_source(source)
            .filter(|results| !results.label.ends_with("@system"))
            .collect();
        if held.is_empty() {
            continue;
        }
        for results in &held {
            match col {
                Col::Spark | Col::CpuTime => {}
                Col::Cpu => {
                    for track in tracks(data, results) {
                        let Some(tid) = task_of(&track.key) else {
                            continue;
                        };
                        let row = rows.entry(tid).or_insert_with(|| Proc {
                            tid,
                            ..Proc::default()
                        });
                        row.process = true;
                        if track.name.is_some() {
                            row.name = track.name.clone();
                        }
                        row.cells.insert(Col::CpuTime, track.total());
                        row.cpu = Some(track);
                    }
                }
                Col::Rss | Col::GpuMem => {
                    for track in tracks(data, results) {
                        let Some(tid) = task_of(&track.key) else {
                            continue;
                        };
                        rows.entry(tid)
                            .or_insert_with(|| Proc {
                                tid,
                                ..Proc::default()
                            })
                            .cells
                            .insert(col, track.last().unwrap_or(0.0));
                    }
                }
                Col::Faults | Col::InIo | Col::RunQueue | Col::Lock | Col::IoCalls => {
                    for (tid, (total, count)) in per_task(results) {
                        let value = if col == Col::IoCalls { count } else { total };
                        rows.entry(tid)
                            .or_insert_with(|| Proc {
                                tid,
                                ..Proc::default()
                            })
                            .cells
                            .insert(col, value);
                    }
                }
            }
        }
        columns.push((col, Trust::joined(data, held.iter().copied())));
    }
    let span_ns = Axis::of(data).span_ns as f64;
    for row in rows.values_mut() {
        if let Some(cpu_ns) = row.cells.get(&Col::CpuTime).copied() {
            row.cells.insert(Col::Cpu, cpu_ns / span_ns);
        }
    }
    let by = sort.column();
    let mut rows: Vec<Proc> = rows.into_values().collect();
    rows.sort_by(|one, other| {
        let value = |row: &Proc| row.cells.get(&by).copied().unwrap_or(f64::NEG_INFINITY);
        value(other)
            .total_cmp(&value(one))
            .then_with(|| one.tid.cmp(&other.tid))
    });
    let refused = data
        .refused
        .iter()
        .find(|refusal| refusal.source == "cpu.oncpu_time")
        .cloned();
    Procs {
        rows,
        columns,
        refused,
    }
}

/// A column's cell, in its unit.
fn cell_text(col: Col, value: f64) -> String {
    match col {
        Col::Cpu => units::percent(value * 100.0),
        Col::CpuTime | Col::InIo | Col::RunQueue | Col::Lock => units::duration(value),
        Col::Rss | Col::GpuMem => units::bytes(value),
        Col::Faults | Col::IoCalls => units::count(value.round()),
        Col::Spark => String::new(),
    }
}

impl System {
    fn draw_processes(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        ctx: &Ctx<'_>,
        axis: &Axis,
        procs: &Procs,
    ) {
        let sort = SORTS[self.sort];
        let block = panel(
            "processes",
            &format!("the target's, by {}", sort.words()),
            None,
        );
        let inner = block.inner(area);
        if procs.rows.is_empty() {
            match &procs.refused {
                Some(refusal) => trust::render_refusal(frame, area, refusal),
                None => frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "no process in this run: nothing in it is measured per task",
                        dim(),
                    )))
                    .block(block),
                    area,
                ),
            }
            return;
        }
        frame.render_widget(block, area);

        let named = procs.rows.iter().any(|row| row.name.is_some());
        let fixed = 9 + if named { 13 } else { 0 };
        let mut room = usize::from(inner.width).saturating_sub(fixed);
        let mut columns: Vec<(Col, &str, usize, Trust)> = Vec::new();
        for &(col, head, width, _) in COLUMNS {
            let Some((_, trust)) = procs.columns.iter().find(|(held, _)| *held == col) else {
                continue;
            };
            if width + 1 > room {
                break;
            }
            room -= width + 1;
            columns.push((col, head, width, trust.clone()));
        }

        let bold = Style::new().add_modifier(Modifier::BOLD);
        let mut lines = Vec::new();
        let mut head = vec![Span::styled(format!("{:>8} ", "ID"), bold)];
        if named {
            head.push(Span::styled(format!("{:<12} ", "NAME"), bold));
        }
        for (col, title, width, _) in &columns {
            let style = if *col == sort.column() || (*col == Col::Cpu && sort == Sort::Cpu) {
                bold.add_modifier(Modifier::UNDERLINED)
            } else {
                bold
            };
            let text = if *col == Col::Spark {
                format!("{title:<width$} ")
            } else {
                format!("{title:>width$} ")
            };
            head.push(Span::styled(text, style));
        }
        lines.push(Line::from(head));

        let rows_room = usize::from(inner.height).saturating_sub(2).max(1);
        let selected = self
            .selected
            .and_then(|tid| procs.rows.iter().position(|row| row.tid == tid));
        let skip = selected.map_or(0, |index| (index + 1).saturating_sub(rows_room));
        for (index, row) in procs.rows.iter().enumerate().skip(skip).take(rows_room) {
            let chosen = selected == Some(index);
            let focused = ctx.focus.pid == Some(row.tid) || ctx.focus.tid == Some(row.tid);
            let marker = if chosen {
                Span::styled("▶", Style::new().fg(palette::ACCENT))
            } else if focused {
                Span::styled("●", Style::new().fg(palette::ACCENT))
            } else {
                Span::raw(" ")
            };
            let id_style = if chosen {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            let mut spans = vec![
                marker,
                Span::styled(format!("{:>7}", row.tid), id_style),
                Span::raw(" "),
            ];
            if named {
                let name: String = row
                    .name
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .take(12)
                    .collect();
                spans.push(Span::raw(format!("{name:<12} ")));
            }
            for (col, _, width, trust) in &columns {
                let style = trust.style();
                if *col == Col::Spark {
                    match &row.cpu {
                        Some(track) => {
                            let cells = axis.cells(&track.points, *width);
                            // Against one whole cpu, the scale a person
                            // reads a process's share on.
                            spans.extend(chart(&cells, 1e9, 1, style).remove(0).spans);
                            spans.push(Span::raw(" "));
                        }
                        None => spans.push(Span::raw(format!("{:width$} ", ""))),
                    }
                    continue;
                }
                let text = match row.cells.get(col) {
                    Some(value) => cell_text(*col, *value),
                    // Not measured for this task: not a zero.
                    None => "·".to_owned(),
                };
                spans.push(Span::styled(format!("{text:>width$} "), style));
            }
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(lines), inner);

        // What each column is not to be read at face value for, said once
        // beneath them, and what did not fit.
        let more = procs.rows.len().saturating_sub(skip + rows_room);
        let mut said: Vec<Span<'static>> = Vec::new();
        if more > 0 {
            said.push(Span::styled(format!("+{more} more  "), dim()));
        }
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (col, title, _, trust) in &columns {
            let badges = trust.badges();
            let text: String = badges.iter().map(|span| span.content.as_ref()).collect();
            if *col == Col::Spark || badges.is_empty() || !seen.insert(text) {
                continue;
            }
            said.push(Span::styled(format!("{title} "), dim()));
            said.extend(badges);
            said.push(Span::raw("  "));
        }
        if !said.is_empty() && inner.height > 1 {
            let foot = Rect {
                y: inner.y + inner.height - 1,
                height: 1,
                ..inner
            };
            frame.render_widget(Paragraph::new(Line::from(said)), foot);
        }
    }
}

// ---------------------------------------------------------------------------
// The cost corner.
// ---------------------------------------------------------------------------

/// What each of this view's collectors cost, as a share of one cpu over its
/// window: what drawing these panels asked of the machine.
fn draw_cost(frame: &mut Frame<'_>, area: Rect, data: &Data) {
    let style = Style::new().fg(palette::COST);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(" cost, share of one cpu ", style));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let say = |frame: &mut Frame<'_>, text: &'static str| {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                style.add_modifier(Modifier::DIM),
            )))
            .wrap(Wrap { trim: true }),
            inner,
        );
    };
    if data.live.is_some() {
        return say(
            frame,
            "said at the end of the run: a collector's cost is known when it stops",
        );
    }
    let mut costs: Vec<(String, f64, bool)> = Vec::new();
    for need in NEEDS {
        for results in data.for_need(&need.label(), need.source) {
            let overhead = &results.overhead;
            if overhead.wall_ns == 0 {
                continue;
            }
            let spent = overhead.attributed_cpu_ns + overhead.in_target_cpu_ns.unwrap_or(0);
            costs.push((
                results.source.clone(),
                spent as f64 * 100.0 / overhead.wall_ns as f64,
                overhead.in_target_cpu_ns.is_none(),
            ));
        }
    }
    if costs.is_empty() {
        return say(frame, "none of this view's collectors is in this run");
    }
    costs.sort_by(|one, other| other.1.total_cmp(&one.1).then_with(|| one.0.cmp(&other.0)));
    let top = costs.iter().map(|(_, share, _)| *share).fold(0.0, f64::max);
    let bar = 7usize;
    let name_width = usize::from(inner.width).saturating_sub(bar + 9).max(4);
    // Every collector, or as many as fit and the rest in one line: the
    // total is always all of them.
    let room = usize::from(inner.height).saturating_sub(1).max(1);
    let listed = if costs.len() > room {
        room.saturating_sub(1)
    } else {
        costs.len()
    };
    let at_least = |lower: bool| if lower { "≥" } else { "" };
    let figure_style = |lower: bool| {
        if lower {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    };
    let mut lines = Vec::new();
    for (source, share, lower) in &costs[..listed] {
        let name: String = source.chars().take(name_width).collect();
        let figure = format!("{}{}", at_least(*lower), units::percent(*share));
        let mut spans = vec![Span::raw(format!("{name:<name_width$} "))];
        spans.extend(
            trust::error_bar(*share, *share, *share, top, bar as u16)
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content, style)),
        );
        spans.push(Span::styled(format!(" {figure:>6}"), figure_style(*lower)));
        lines.push(Line::from(spans));
    }
    let rest = &costs[listed..];
    if !rest.is_empty() {
        let share: f64 = rest.iter().map(|(_, share, _)| share).sum();
        let lower = rest.iter().any(|(_, _, lower)| *lower);
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{:<width$}",
                    format!("+{} more", rest.len()),
                    width = name_width + 1 + bar
                ),
                dim(),
            ),
            Span::styled(
                format!(
                    " {:>6}",
                    format!("{}{}", at_least(lower), units::percent(share))
                ),
                figure_style(lower),
            ),
        ]));
    }
    let total: f64 = costs.iter().map(|(_, share, _)| share).sum();
    let any_lower = costs.iter().any(|(_, _, lower)| *lower);
    lines.push(Line::from(vec![
        Span::styled(
            format!("{:<width$}", "all of them", width = name_width + 1 + bar),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " {:>6}",
                format!("{}{}", at_least(any_lower), units::percent(total))
            ),
            style.add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Attr, AttrValue, Key, Live, Run};
    use crate::snapshot::{draw, fixture, text};
    use ratatui::crossterm::event::KeyModifiers;

    fn screen(data: Data) -> String {
        text(&draw(data, "system", 120, 40))
    }

    #[test]
    fn the_machine_is_drawn_panel_by_panel() {
        let screen = screen(fixture("machine"));
        for panel in [
            " cpu ",
            " memory ",
            " disk ",
            " gpu ",
            " power ",
            " network ",
            "cost, share of one cpu",
        ] {
            assert!(screen.contains(panel), "no {panel:?} in\n{screen}");
        }
        // The busiest interface listed, the idle ones folded into one line.
        assert!(screen.contains("wlan0"), "{screen}");
        assert!(screen.contains("moved nothing"), "{screen}");
        // The process list's source was refused on this run: its reason is
        // where the list would be.
        assert!(screen.contains("cpu.oncpu_time refused"), "{screen}");
    }

    #[test]
    fn the_gpu_sampling_window_is_drawn_on_the_time_axis() {
        let screen = screen(fixture("cpython-hot"));
        let ruler = screen
            .lines()
            .find(|line| line.contains("├") && line.contains("±1.00s"))
            .unwrap_or_else(|| panic!("no ruler in\n{screen}"));
        // One second each side, on a 3.89 s axis: about half the chart.
        let dashes = ruler.matches('─').count();
        assert!(dashes > 5, "{ruler}");
    }

    #[test]
    fn the_target_is_in_the_list_with_its_cpu() {
        let data = fixture("cpython-hot");
        let procs = processes(&data, Sort::Cpu);
        assert_eq!(procs.rows.len(), 1);
        let cpu = procs.rows[0].cells[&Col::Cpu];
        // The interpreter spins on one cpu for the whole run.
        assert!(cpu > 0.8 && cpu < 1.1, "{cpu}");
        let screen = screen(data);
        assert!(screen.contains("2779350"), "{screen}");
    }

    #[test]
    fn every_thread_any_source_saw_is_listed() {
        let procs = processes(&fixture("http-server"), Sort::Cpu);
        let tids: Vec<i32> = procs.rows.iter().map(|row| row.tid).collect();
        for tid in [2758953, 2758972, 2758973, 2758974] {
            assert!(tids.contains(&tid), "{tid} missing from {tids:?}");
        }
        // A thread `cpu.oncpu_time` does not read has no cpu: not a zero.
        let thread = procs.rows.iter().find(|row| row.tid == 2758974).unwrap();
        assert!(!thread.cells.contains_key(&Col::Cpu));
        assert!(thread.cells.contains_key(&Col::Faults));
    }

    #[test]
    fn a_run_without_the_machine_says_so_once() {
        let screen = screen(fixture("vertical-requests"));
        assert!(screen.contains("did not measure the machine"), "{screen}");
        assert_eq!(screen.matches("not in this run").count(), 0, "{screen}");
    }

    #[test]
    fn enter_opens_the_selected_process_in_its_ledger() {
        let data = fixture("http-server");
        let mut view = System {
            selected: None,
            sort: 0,
            idle: false,
        };
        let focus = Focus::default();
        let ctx = Ctx {
            data: &data,
            against: None,
            focus: &focus,
        };
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(view.key(press(KeyCode::Down), &ctx), Action::None);
        match view.key(press(KeyCode::Enter), &ctx) {
            Action::Goto { view, focus } => {
                assert_eq!(view, "ledger");
                // The first row is the process `cpu.oncpu_time` reads.
                assert_eq!(focus.pid, Some(2758953));
            }
            other => panic!("{other:?}"),
        }
        view.key(press(KeyCode::Down), &ctx);
        match view.key(press(KeyCode::Enter), &ctx) {
            Action::Goto { focus, .. } => {
                assert!(focus.tid.is_some() && focus.pid.is_none(), "{focus:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sorting_reorders_the_list() {
        let data = fixture("http-server");
        let by_cpu = processes(&data, Sort::Cpu);
        let by_faults = processes(&data, Sort::Faults);
        assert_eq!(by_cpu.rows[0].tid, 2758953);
        // The thread that listed the directory faulted most.
        assert_eq!(by_faults.rows[0].tid, 2758974);
    }

    fn event(ts_ns: u64, keys: Vec<Key>, attrs: &[(&str, AttrValue)]) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: String::new(),
            keys,
            attrs: attrs
                .iter()
                .map(|(name, value)| Attr {
                    name: (*name).to_owned(),
                    value: value.clone(),
                })
                .collect(),
            collector_id: String::new(),
            seq: 0,
        }
    }

    /// A live run holds events, not timelines: the view draws from them, and
    /// they say what a timeline does not -- bytes in apart from out, the cpu
    /// by state, a process's name.
    #[test]
    fn a_live_run_is_drawn_from_its_events_and_says_more() {
        let start = 1_000_000_000u64;
        let tick = 250_000_000u64;
        let iface = || Key::Iface {
            netns: 0,
            index: 2,
            name: "eth9".into(),
        };
        let net: Vec<Event> = (1..=8)
            .map(|n| {
                event(
                    start + n * tick,
                    vec![iface()],
                    &[
                        ("bytes", AttrValue::Uint(3000)),
                        ("rx", AttrValue::Uint(2000)),
                        ("tx", AttrValue::Uint(1000)),
                    ],
                )
            })
            .collect();
        let cpu: Vec<Event> = (1..=8)
            .map(|n| {
                event(
                    start + n * tick,
                    vec![],
                    &[
                        ("busy", AttrValue::Float(0.5)),
                        ("user", AttrValue::Float(0.3)),
                        ("system", AttrValue::Float(0.1)),
                        ("iowait", AttrValue::Float(0.1)),
                    ],
                )
            })
            .collect();
        let oncpu: Vec<Event> = (1..=8)
            .map(|n| {
                event(
                    start + n * tick,
                    vec![Key::Task { tid: 77 }],
                    &[
                        ("ns", AttrValue::Uint(tick / 2)),
                        ("comm", AttrValue::Text("busybee".into())),
                    ],
                )
            })
            .collect();
        let result = |label: &str, source: &str, events: Vec<Event>| Results {
            label: label.into(),
            source: source.into(),
            outputs: vec![Output::Raw { events }],
            ..Results::default()
        };
        let data = Data {
            run: Run {
                results: vec![
                    result("net.bytes@system", "net.bytes", net),
                    result("cpu.util@system", "cpu.util", cpu),
                    result("cpu.oncpu_time", "cpu.oncpu_time", oncpu),
                ],
                ..Run::default()
            },
            live: Some(Live {
                opened_ns: start,
                now_ns: start + 10 * tick,
            }),
            ..Data::default()
        };
        let screen = screen(data.clone());
        assert!(screen.contains("eth9"), "{screen}");
        assert!(screen.contains("15.6KiB in"), "{screen}");
        assert!(screen.contains("7.81KiB out"), "{screen}");
        assert!(screen.contains("user 60.0%"), "{screen}");
        assert!(screen.contains("iowait 20.0%"), "{screen}");
        assert!(screen.contains("busybee"), "{screen}");
        assert!(screen.contains("said at the end"), "{screen}");
        // Half a cpu for eight ticks of a ten-tick run, from the events' own
        // nanoseconds.
        let procs = processes(&data, Sort::Cpu);
        let cpu = procs.rows[0].cells[&Col::Cpu];
        assert!((cpu - 0.4).abs() < 0.01, "{cpu}");
    }

    #[test]
    fn cells_hold_a_value_until_the_next_and_keep_the_peak() {
        let axis = Axis { span_ns: 1000 };
        let point = |at_ns, shown| Point {
            at_ns,
            read: shown,
            shown,
        };
        let cells = axis.cells(&[point(0, 1.0), point(450, 5.0), point(480, 2.0)], 10);
        let mut expected = vec![Some(1.0); 4];
        expected.push(Some(5.0));
        expected.extend([None; 5]);
        assert_eq!(cells, expected);
    }

    #[test]
    fn amounts_are_drawn_per_second() {
        let point = |at_ns, read| Point {
            at_ns,
            read,
            shown: 0.0,
        };
        let mut points = vec![point(0, 10.0), point(250_000_000, 5.0)];
        per_second(&mut points, Per::Tick, &[]);
        assert_eq!(points[0].shown, 40.0);
        assert_eq!(points[1].shown, 20.0);
    }

    #[test]
    fn ceilings_are_round() {
        assert_eq!(ceiling(0.081), 0.1);
        assert_eq!(ceiling(0.02), 0.02);
        assert_eq!(ceiling(0.031), 0.05);
        assert_eq!(ceiling(0.0), 1.0);
    }
}
