//! The `process` view: the system view for one process.
//!
//! The first screen of `omn top -- <program>` and of `--pid`: one line per
//! resource -- CPU, memory, disk, network, GPU, energy -- each a sparkline
//! over the run, the current value, the mean, the peak and the share of the
//! machine, then the program's time split by state and the first suspect
//! `omn latency` names (`docs/view/process.md`). The detail lives in the
//! dedicated pages: `Enter` or a click on a line opens its page.
//!
//! A line reads only what the other views already ask for, and says what it
//! could not read in place: a source refused with its reason, a source the
//! run did not ask as such, dimmed, never drawn as zero.
//!
//! Colour says how full a resource is: its share of a capacity that exists
//! (cores, memory, the GPU's engines), green when free through yellow to red
//! when full. A resource with no known capacity keeps one neutral hue and the
//! screen says why. Trust never washes the colour out: a figure with a loss
//! or a time error keeps its hue, and its badges stand beside it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::timebar::{self, Class, Verdict};
use super::{flame, ledger};
use super::{Action, Ctx, Focus, Need, Region, View};
use crate::model::{Data, Event, Output, Results};
use crate::trust::{palette, usage, Trust};
use crate::units;

/// The lines read what the other views already ask for: the process view
/// asks for nothing of its own, so it never changes what a live run
/// collects.
const NEEDS: &[Need] = &[];

const KEYS: &[(&str, &str)] = &[
    ("↑↓ jk", "move between lines"),
    ("enter", "open the line's page"),
];

/// The cells a sparkline is drawn in.
pub const SPARK: usize = 24;

/// The label column.
const LABEL: usize = 9;

/// A sparkline's eight heights; a cell nothing was read in is a blank.
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// A taller sparkline's row: empty, then one to eight eighths filled.
const EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// The hue of a resource whose capacity is not known: neither free nor
/// full, the one every view draws such a resource in.
const NEUTRAL: Color = usage::NEUTRAL;

/// The most a line's figures are given before its sparkline takes the rest
/// of the width: a refusal's sentence longer than this is cut, not let push
/// every sparkline back to its narrowest.
const FIGURES: usize = 72;

/// The most terminal rows one line's sparkline is drawn over.
const TALLEST: usize = 12;

#[derive(Default)]
pub struct Process {
    /// The selected row: 0 is the header's cost, then the resources, then
    /// the suspect.
    selected: Option<usize>,
    regions: Vec<Region>,
}

/// One line of the screen: a resource, or the time.
#[derive(Debug, Clone)]
pub struct Row {
    /// `CPU`, `Memory`, `Disk`, `Network`, `GPU`, `Energy`, `Time`.
    pub name: &'static str,
    /// The page it opens.
    pub view: &'static str,
    /// What the page it opens is narrowed to.
    pub focus: Focus,
    /// The sparkline's cells, in the line's unit; `None` where nothing was
    /// read. The time line holds its states' amounts.
    pub values: Vec<Option<f64>>,
    /// The sparkline as drawn.
    pub graph: Vec<Span<'static>>,
    /// The figures, as drawn.
    pub figures: Vec<Span<'static>>,
    /// The figures as plain text.
    pub text: String,
    /// The current value's share of the capacity, when the capacity is
    /// known.
    pub fill: Option<f64>,
    /// Why the line has no capacity to be coloured by, when it has none.
    pub neutral: Option<&'static str>,
    pub trust: Trust,
    /// Something was read: false when the line's sources are refused or not
    /// asked, or held nothing.
    pub held: bool,
    /// The sparkline's width, in cells.
    pub cells: usize,
    /// Each cell's height against the line's peak, and its hue: what a
    /// sparkline taller than one row is drawn from. Empty for a line drawn
    /// as a bar of parts (the time).
    pub heights: Vec<Option<(f64, Color)>>,
}

impl Row {
    fn new(name: &'static str, view: &'static str, focus: &Focus, cells: usize) -> Row {
        Row {
            cells,
            heights: Vec::new(),
            name,
            view,
            focus: focus.clone(),
            values: Vec::new(),
            graph: Vec::new(),
            figures: Vec::new(),
            text: String::new(),
            fill: None,
            neutral: None,
            trust: Trust::clean(),
            held: true,
        }
    }

    /// A line with nothing to read, said in place and dimmed: refused with
    /// the reason, asked but holding nothing on the target, or not asked
    /// with what it needs.
    fn absent(mut self, data: &Data, sources: &[&str]) -> Row {
        self.held = false;
        self.graph = vec![Span::raw(" ".repeat(self.cells))];
        let refusal = data
            .refused
            .iter()
            .find(|refusal| sources.contains(&refusal.source.as_str()));
        match refusal {
            Some(refusal) => {
                self.text = format!("{} refused: {}", refusal.source, refusal.reason);
                self.figures = vec![
                    Span::styled(
                        format!("{} refused", refusal.source),
                        Style::new().fg(palette::REFUSED),
                    ),
                    dim_span(format!(": {}", refusal.reason)),
                ];
            }
            None => {
                // Asked and read, but nothing of it is the target's: the
                // machine's results only, or none this line can draw.
                let asked: Vec<&str> = sources
                    .iter()
                    .copied()
                    .filter(|source| {
                        data.run
                            .results
                            .iter()
                            .any(|results| results.source == *source)
                    })
                    .collect();
                self.text = if asked.is_empty() {
                    format!("not asked: needs {}", sources.join(" or "))
                } else {
                    format!("nothing on the target from {}", asked.join(" or "))
                };
                self.figures = vec![dim_span(self.text.clone())];
            }
        }
        self
    }

    /// A line whose sources are there and hold nothing to draw.
    fn empty(mut self, said: String) -> Row {
        self.held = false;
        self.graph = vec![Span::raw(" ".repeat(self.cells))];
        self.text = said;
        self.figures = vec![dim_span(self.text.clone())];
        self
    }

    /// The figures: the first in the usage colour, the rest plain, the
    /// trust's badges after them -- never dimming the hue away.
    fn says(mut self, figures: Vec<String>, trust: Trust) -> Row {
        let hue = self.hue(self.fill);
        self.text = figures.join("   ");
        self.figures.clear();
        for (index, figure) in figures.into_iter().enumerate() {
            if index > 0 {
                self.figures.push(Span::raw("   "));
            }
            let (style, width) = match index {
                0 => (Style::new().fg(hue).add_modifier(Modifier::BOLD), 12),
                _ => (Style::new(), 0),
            };
            self.figures
                .push(Span::styled(format!("{figure:<width$}"), style));
        }
        let badges = trust.badges();
        if !badges.is_empty() {
            self.figures.push(Span::raw("  "));
            self.figures.extend(badges);
        }
        self.trust.merge(trust);
        self
    }

    /// The colour a value of this line takes: by its fill when the capacity
    /// is known, the neutral hue otherwise.
    fn hue(&self, fill: Option<f64>) -> Color {
        match (self.neutral, fill) {
            (None, Some(fill)) => usage::colour(fill),
            _ => NEUTRAL,
        }
    }

    /// The sparkline: each cell's height against the line's peak, its colour
    /// by its own share of the capacity.
    fn spark(mut self, values: Vec<Option<f64>>, capacity: Option<f64>) -> Row {
        let top = values.iter().flatten().copied().fold(0.0, f64::max);
        self.heights = values
            .iter()
            .map(|value| {
                value.map(|value| {
                    let height = if top > 0.0 { value / top } else { 0.0 };
                    let fill = capacity.filter(|cap| *cap > 0.0).map(|cap| value / cap);
                    (height, self.hue(fill))
                })
            })
            .collect();
        self.graph = self
            .heights
            .iter()
            .map(|cell| match cell {
                None => Span::raw(" "),
                Some((height, hue)) => {
                    let level = (height * (BLOCKS.len() - 1) as f64).round() as usize;
                    Span::styled(
                        BLOCKS[level.min(BLOCKS.len() - 1)].to_string(),
                        Style::new().fg(*hue),
                    )
                }
            })
            .collect();
        self.values = values;
        self
    }

    /// The sparkline over `rows` terminal rows, the top one first: each
    /// cell's height in eighths of the whole, its hue kept. One row is the
    /// line as drawn by [`Row::spark`]; a line drawn as a bar of parts
    /// repeats its bar.
    fn graph_lines(&self, rows: usize) -> Vec<Vec<Span<'static>>> {
        if rows <= 1 {
            return vec![self.graph.clone()];
        }
        if self.heights.is_empty() {
            return vec![self.graph.clone(); rows];
        }
        let eighths = (rows * 8) as f64;
        (0..rows)
            .map(|line| {
                let floor = (rows - 1 - line) * 8;
                self.heights
                    .iter()
                    .map(|cell| match cell {
                        None => Span::raw(" "),
                        Some((height, hue)) => {
                            // A zero still shows its baseline, as one row does.
                            let filled = ((height * eighths).round() as usize).max(1);
                            let here = filled.saturating_sub(floor).min(8);
                            Span::styled(EIGHTHS[here].to_string(), Style::new().fg(*hue))
                        }
                    })
                    .collect()
            })
            .collect()
    }
}

impl View for Process {
    fn name(&self) -> &'static str {
        "process"
    }

    fn title(&self) -> &'static str {
        "the system view for one process: each resource over the run, the time by state, the suspect"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        let width = area.width as usize;
        let screen = screen_for(ctx, width);
        let count = screen.rows.len() + 2;
        let selected = self.selected.unwrap_or(1).min(count - 1);
        self.selected = Some(selected);
        self.regions.clear();
        // The header, two rules, the suspect, a blank and the notes are a
        // row each; the resources share the rest, so the screen is filled
        // top to bottom rather than drawn in its corner.
        // A line with a sparkline grows; one with nothing drawn (a refusal,
        // a GPU no kernel ran on) keeps one row, and the time's bar two.
        let timed = screen
            .rows
            .iter()
            .any(|row| row.held && row.heights.is_empty() && row.name == "Time");
        let fixed = 5 + screen.notes.len() + usize::from(timed);
        let spare = usize::from(area.height)
            .saturating_sub(fixed)
            .saturating_sub(screen.rows.len());
        let growing = screen
            .rows
            .iter()
            .filter(|row| !row.heights.is_empty())
            .count()
            .max(1);
        let grown = (spare / growing).min(TALLEST - 1);
        let tall = |row: &Row| {
            if !row.heights.is_empty() {
                1 + grown
            } else if row.held && row.name == "Time" {
                1 + grown.min(1)
            } else {
                1
            }
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut regions = Vec::new();
        let mut place =
            |lines: &mut Vec<Line<'static>>, drawn: Vec<Line<'static>>, id: Option<usize>| {
                let row = lines.len() as u16;
                if let Some(id) = id {
                    if row < area.height {
                        let height = (drawn.len() as u16).min(area.height - row);
                        regions.push(Region::new(
                            Rect::new(area.x, area.y + row, area.width, height),
                            id,
                        ));
                    }
                }
                lines.extend(drawn);
            };

        let mut header = screen.header.clone();
        if selected == 0 {
            for span in header.iter_mut().skip(screen.cost_at) {
                span.style = span.style.add_modifier(Modifier::REVERSED);
            }
        }
        place(&mut lines, vec![Line::from(header)], Some(0));
        place(&mut lines, vec![rule(width)], None);
        for (index, row) in screen.rows.iter().enumerate() {
            place(
                &mut lines,
                draw_row(row, selected == index + 1, tall(row)),
                Some(index + 1),
            );
        }
        place(&mut lines, vec![rule(width)], None);
        let mut suspect = screen.suspect.clone();
        if selected == count - 1 {
            if let Some(first) = suspect.first_mut() {
                first.style = first.style.add_modifier(Modifier::REVERSED);
            }
        }
        let used: usize = suspect.iter().map(|span| span.width()).sum();
        let hint = "Enter/click → detail";
        let hint_width = hint.chars().count();
        if used + hint_width + 2 <= width {
            suspect.push(Span::raw(" ".repeat(width - used - hint_width)));
            suspect.push(dim_span(hint.to_owned()));
        }
        place(&mut lines, vec![Line::from(suspect)], Some(count - 1));
        place(&mut lines, vec![Line::default()], None);
        for note in &screen.notes {
            place(&mut lines, vec![dim(note.clone())], None);
        }
        self.regions = regions;
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        let count = screen(ctx).rows.len() + 2;
        let at = self.selected.unwrap_or(1).min(count - 1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.selected = Some((at + 1).min(count - 1)),
            KeyCode::Up | KeyCode::Char('k') => self.selected = Some(at.saturating_sub(1)),
            KeyCode::Home => self.selected = Some(1),
            KeyCode::Enter => return self.open(ctx),
            _ => {}
        }
        Action::None
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        KEYS
    }

    fn regions(&self) -> &[Region] {
        &self.regions
    }

    fn select(&mut self, id: usize, _ctx: &Ctx<'_>) -> Action {
        self.selected = Some(id);
        Action::None
    }
}

impl Process {
    /// The selected line's page, narrowed to the process.
    fn open(&self, ctx: &Ctx<'_>) -> Action {
        let screen = screen(ctx);
        let at = self.selected.unwrap_or(1);
        let (view, focus) = if at == 0 {
            ("ledger", ctx.focus.clone())
        } else if let Some(row) = screen.rows.get(at - 1) {
            (row.view, row.focus.clone())
        } else {
            (screen.suspect_view, ctx.focus.clone())
        };
        Action::Goto { view, focus }
    }
}

pub fn view() -> Box<dyn View> {
    Box::<Process>::default()
}

// ---------------------------------------------------------------------------
// The screen
// ---------------------------------------------------------------------------

/// Everything the view draws, read once per frame.
#[derive(Debug, Clone)]
pub struct Screen {
    /// `process-main  pid 41207 · 3 threads · 2.37s · omn costs 0.8% cpu`.
    pub header: Vec<Span<'static>>,
    /// The span the header's cost starts at: what selecting it marks.
    pub cost_at: usize,
    /// CPU, Memory, Disk, Network, GPU, Energy, Time.
    pub rows: Vec<Row>,
    /// `▶ suspect: ...`, from the timebar's verdict.
    pub suspect: Vec<Span<'static>>,
    /// The page that explains the suspect.
    pub suspect_view: &'static str,
    /// What the colours could not say: the lines kept neutral, and why.
    pub notes: Vec<String>,
}

/// The screen for this run.
pub fn screen(ctx: &Ctx<'_>) -> Screen {
    screen_at(ctx, SPARK)
}

/// The screen for a terminal `width` columns wide: the sparklines take every
/// column the figures leave, and never fewer than [`SPARK`] cells.
pub fn screen_for(ctx: &Ctx<'_>, width: usize) -> Screen {
    let narrow = screen_at(ctx, SPARK);
    let figures = narrow
        .rows
        .iter()
        .map(|row| row.figures.iter().map(Span::width).sum::<usize>())
        .max()
        .unwrap_or(0)
        .min(FIGURES);
    // ` label` + the sparkline + `   ` + the figures, and a column spare.
    let cells = width.saturating_sub(LABEL + 3 + figures + 1);
    if cells <= SPARK {
        return narrow;
    }
    screen_at(ctx, cells)
}

/// The screen with its sparklines `cells` cells wide.
pub fn screen_at(ctx: &Ctx<'_>, cells: usize) -> Screen {
    let data = ctx.data;
    let target = Target::of(data);
    let axis = Axis::cut(data, cells);
    let capacity = Capacity::of(data);
    let main = main_bar(data);
    let cpu = cpu_row(ctx, &axis, &capacity);
    let energy = energy_row(ctx, &axis, &capacity, &cpu);
    let rows = vec![
        cpu,
        memory_row(ctx, &axis, &capacity),
        disk_row(ctx, &axis, &target),
        network_row(ctx, &axis, &target),
        gpu_row(ctx, &axis, &target),
        energy,
        time_row(ctx, &axis, main.as_ref()),
    ];
    let (suspect, suspect_view) = suspect_line(main.as_ref());
    // The lines kept neutral, grouped by why, in the screen's order.
    let mut neutral: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    for row in rows.iter().filter(|row| row.held) {
        let Some(why) = row.neutral else { continue };
        match neutral.iter_mut().find(|(held, _)| *held == why) {
            Some((_, names)) => names.push(row.name),
            None => neutral.push((why, vec![row.name])),
        }
    }
    let notes = neutral
        .into_iter()
        .map(|(why, names)| format!("{} in one hue: {why}", names.join(", ")))
        .collect();
    let (header, cost_at) = header(data, &target, main.as_ref(), &axis);
    Screen {
        header,
        cost_at,
        rows,
        suspect,
        suspect_view,
        notes,
    }
}

/// The resources the run's machine has, as a bound a usage is a share of.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Capacity {
    pub cores: Option<f64>,
    pub memory: Option<f64>,
}

impl Capacity {
    /// A live run's machine is the one the view runs on: its cores the
    /// scheduler lets this process use, its memory `MemTotal`. A recording
    /// does not carry its machine's (`docs/fixes/w21-process.md` asks for
    /// it), and this machine's would be a guess.
    fn of(data: &Data) -> Capacity {
        if data.live.is_none() {
            return Capacity::default();
        }
        static HERE: OnceLock<Capacity> = OnceLock::new();
        HERE.get_or_init(|| Capacity {
            cores: std::thread::available_parallelism()
                .ok()
                .map(|cores| cores.get() as f64),
            memory: std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|text| mem_total(&text)),
        })
        .clone()
    }
}

/// `MemTotal` from `/proc/meminfo`'s text, in bytes.
fn mem_total(text: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024.0)
}

/// The run's time, cut into [`SPARK`] cells: the program's life when the run
/// says it, the run's window otherwise.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Axis {
    pub start: u64,
    pub end: u64,
    /// How many cells the time is cut into: the sparklines' width.
    pub cells: usize,
}

impl Axis {
    #[cfg(test)]
    fn of(data: &Data) -> Axis {
        Axis::cut(data, SPARK)
    }

    /// The run's time cut into `cells` cells.
    fn cut(data: &Data, cells: usize) -> Axis {
        let life = data.run.results.iter().find_map(|results| {
            let lifetime = results.lifetime.as_ref()?;
            let released = lifetime.get("released_ns")?.as_u64()?;
            let exited = lifetime.get("exited_ns").and_then(|value| value.as_u64());
            Some((released, exited))
        });
        let (start, end) = match life {
            Some((released, Some(exited))) if exited > released && data.live.is_none() => {
                (released, exited)
            }
            Some((released, _)) => (released, data.closed_ns()),
            None => (data.opened_ns(), data.closed_ns()),
        };
        Axis {
            start,
            end: end.max(start + 1),
            cells: cells.max(1),
        }
    }

    fn span(&self) -> f64 {
        (self.end - self.start) as f64
    }

    fn cell_ns(&self) -> f64 {
        self.span() / self.cells as f64
    }

    /// Adds `amount`, spread evenly over `[from, to)`, to the cells it
    /// covers: the part of an interval outside the axis is outside the
    /// program's life and is left out. An instant (`from == to`) goes whole
    /// to its cell, the nearest end's when it is outside.
    fn spread(&self, cells: &mut [Option<f64>], from: u64, to: u64, amount: f64) {
        let cell = self.cell_ns();
        let at = |ts: u64| (ts as f64 - self.start as f64) / cell;
        let (first, last) = (at(from), at(to.max(from)));
        if last - first < 1e-9 {
            let index = first.clamp(0.0, (self.cells - 1) as f64) as usize;
            *cells[index].get_or_insert(0.0) += amount;
            return;
        }
        let rate = amount / (last - first);
        let mut index = first.max(0.0).floor() as usize;
        while (index as f64) < last && index < self.cells {
            let low = first.max(index as f64);
            let high = last.min(index as f64 + 1.0);
            if high > low {
                *cells[index].get_or_insert(0.0) += rate * (high - low);
            }
            index += 1;
        }
    }
}

/// The target as the run holds it: its threads, and its pid when a source
/// says it.
#[derive(Debug, Default)]
struct Target {
    pid: Option<i32>,
    tids: BTreeSet<i32>,
}

impl Target {
    fn of(data: &Data) -> Target {
        let mut tids = BTreeSet::new();
        let mut pids: BTreeMap<i32, u64> = BTreeMap::new();
        for results in on_target(data, "cpu.oncpu_time") {
            for output in &results.outputs {
                if let Output::Summary { series } = output {
                    tids.extend(series.iter().filter_map(|one| task_of(&one.key)));
                }
            }
        }
        for results in on_target(data, "sched.switches") {
            for event in results.events() {
                if let Some(tid) = event.number("tid") {
                    tids.insert(tid as i32);
                }
                if let Some(pid) = event.number("pid") {
                    *pids.entry(pid as i32).or_default() += 1;
                }
            }
        }
        let pid = pids
            .into_iter()
            .max_by_key(|(pid, count)| (*count, -pid))
            .map(|(pid, _)| pid)
            .or_else(|| tids.first().copied());
        Target { pid, tids }
    }

    /// Whether an event is the target's: by its thread when the run says
    /// which threads are the target's, by its process otherwise; and every
    /// event when the run says neither.
    fn holds(&self, event: &Event) -> bool {
        if self.tids.is_empty() && self.pid.is_none() {
            return true;
        }
        match (tid(event), pid(event)) {
            (Some(tid), _) if self.tids.contains(&tid) => true,
            (_, Some(pid)) => self.pid == Some(pid),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// The lines
// ---------------------------------------------------------------------------

/// One reading: the interval it covers, on `CLOCK_MONOTONIC`, and its value.
type Reading = (u64, u64, f64);

/// The points of every timeline series of these results, each with the
/// interval it covers: since the series' point before it, or the poll
/// period, or the typical gap.
fn timeline(results: &[&Results]) -> Vec<Vec<Reading>> {
    let mut all = Vec::new();
    for one in results {
        for output in &one.outputs {
            let Output::Timeline {
                series,
                first_ts_ns,
            } = output
            else {
                continue;
            };
            let origin = first_ts_ns.unwrap_or(one.window.opened_ns);
            for line in series {
                let stamps: Vec<u64> = line
                    .points
                    .iter()
                    .map(|point| origin + point.offset_ns)
                    .collect();
                let mut gaps: Vec<u64> = stamps
                    .windows(2)
                    .map(|pair| pair[1].saturating_sub(pair[0]))
                    .filter(|gap| *gap > 0)
                    .collect();
                gaps.sort_unstable();
                let typical = one.period_ns.or(gaps.get(gaps.len() / 2).copied());
                let points = line
                    .points
                    .iter()
                    .enumerate()
                    .map(|(index, point)| {
                        let at = stamps[index];
                        let since = match index {
                            0 => typical.map(|gap| at.saturating_sub(gap)).unwrap_or(at),
                            _ => stamps[index - 1],
                        };
                        (since, at, point.value)
                    })
                    .collect();
                all.push(points);
            }
        }
    }
    all
}

/// An amount read per interval (cpu nanoseconds, joules), per cell as a
/// rate: the amount in the cell over the cell's nanoseconds, times `scale`.
fn rate_cells(axis: &Axis, series: &[Vec<Reading>], scale: f64) -> Vec<Option<f64>> {
    let mut cells = vec![None; axis.cells];
    for points in series {
        for (since, at, value) in points {
            axis.spread(&mut cells, *since, *at, *value);
        }
    }
    let cell = axis.cell_ns();
    cells
        .into_iter()
        .map(|value| value.map(|value| value * scale / cell))
        .collect()
}

/// A level (bytes held), per cell: each series' largest reading in the cell,
/// carried forward between readings, summed over the series.
fn level_cells(axis: &Axis, series: &[Vec<Reading>]) -> Vec<Option<f64>> {
    let mut cells: Vec<Option<f64>> = vec![None; axis.cells];
    for points in series {
        let mut own: Vec<Option<f64>> = vec![None; axis.cells];
        for (_, at, value) in points {
            let index = (at.saturating_sub(axis.start) as f64 / axis.cell_ns()) as usize;
            let slot = &mut own[index.min(axis.cells - 1)];
            *slot = Some(slot.map_or(*value, |held: f64| held.max(*value)));
        }
        let first = own.iter().position(Option::is_some);
        let last = own.iter().rposition(Option::is_some);
        if let (Some(first), Some(last)) = (first, last) {
            let mut held = None;
            for slot in own.iter_mut().take(last + 1).skip(first) {
                match slot {
                    Some(value) => held = Some(*value),
                    None => *slot = held,
                }
            }
        }
        for (cell, value) in cells.iter_mut().zip(own) {
            if let Some(value) = value {
                *cell.get_or_insert(0.0) += value;
            }
        }
    }
    cells
}

fn last_of(cells: &[Option<f64>]) -> Option<f64> {
    cells.iter().rev().find_map(|cell| *cell)
}

fn peak_of(cells: &[Option<f64>]) -> Option<f64> {
    cells.iter().flatten().copied().reduce(f64::max)
}

/// CPU: the cores the process kept busy, from its threads' on-cpu time.
fn cpu_row(ctx: &Ctx<'_>, axis: &Axis, capacity: &Capacity) -> Row {
    let data = ctx.data;
    let row = Row::new("CPU", "flame", ctx.focus, axis.cells);
    let held = on_target(data, "cpu.oncpu_time");
    let series = timeline(&held);
    if series.iter().all(Vec::is_empty) {
        return row.absent(data, &["cpu.oncpu_time"]);
    }
    let cells = rate_cells(axis, &series, 1.0);
    let polled: f64 = series.iter().flatten().map(|(_, _, value)| value).sum();
    // The program's own account when it has one: what its threads burnt
    // from start to exit, exact, not polled.
    let burnt = data
        .run
        .results
        .iter()
        .find_map(|results| results.lifetime.as_ref()?.get("cpu_ns")?.as_f64())
        .filter(|_| data.live.is_none())
        .unwrap_or(polled);
    let mean = burnt / axis.span();
    let now = last_of(&cells).unwrap_or(0.0);
    let peak = peak_of(&cells).unwrap_or(0.0);
    let mut row = row;
    row.neutral = capacity
        .cores
        .is_none()
        .then_some("the machine's cores are not in the recording");
    row.fill = capacity.cores.map(|cap| now / cap);
    let mut figures = vec![
        format!("{} cores", units::approx(now)),
        format!("avg {}", units::approx(mean)),
        format!("peak {}", units::approx(peak)),
    ];
    if let Some(cap) = capacity.cores {
        figures.push(format!("{} of machine", units::percent(mean * 100.0 / cap)));
    }
    // The function the flame it opens names first, in the flame's words.
    if let Some(hot) = flame::hottest(data, ctx.focus, false) {
        let name: String = hot.name.chars().take(32).collect();
        figures.push(format!(
            "hottest {name} {}",
            units::percent(hot.share * 100.0)
        ));
    }
    let trust = Trust::joined(data, held.iter().copied());
    row.spark(cells, capacity.cores).says(figures, trust)
}

/// Memory: the resident size, its growth over the run, its peak.
fn memory_row(ctx: &Ctx<'_>, axis: &Axis, capacity: &Capacity) -> Row {
    let data = ctx.data;
    let row = Row::new("Memory", "ledger", ctx.focus, axis.cells);
    let held = on_target(data, "mem.rss");
    let series = timeline(&held);
    if series.iter().all(Vec::is_empty) {
        return row.absent(data, &["mem.rss"]);
    }
    let cells = level_cells(axis, &series);
    let now = last_of(&cells).unwrap_or(0.0);
    let first = cells.iter().find_map(|cell| *cell).unwrap_or(0.0);
    // The peak the ledger this line opens draws, so the two never differ;
    // the cells' own when the ledger holds nothing of the focus.
    let peak = ledger::reading(data, ctx.focus)
        .into_iter()
        .find(|line| line.resource == "memory held")
        .and_then(|line| match line.reading {
            ledger::Reading::Served {
                value,
                blank: false,
                ..
            } => Some(value),
            _ => None,
        })
        .unwrap_or_else(|| peak_of(&cells).unwrap_or(0.0));
    let growth = (now - first) * 1e9 / axis.span();
    let mut row = row;
    row.neutral = capacity
        .memory
        .is_none()
        .then_some("the machine's memory is not in the recording");
    row.fill = capacity.memory.map(|cap| now / cap);
    let sign = if growth >= 0.0 { "+" } else { "-" };
    let mut figures = vec![
        units::bytes(now),
        format!("{sign}{}/s", units::bytes(growth.abs())),
        format!("peak {}", units::bytes(peak)),
    ];
    if let Some(cap) = capacity.memory {
        figures.push(format!("{} of machine", units::percent(peak * 100.0 / cap)));
    }
    let trust = Trust::joined(data, held.iter().copied());
    row.spark(cells, capacity.memory).says(figures, trust)
}

/// Disk: the bytes the process's block requests moved, read and written, and
/// their p99 latency. Bytes on a device of unknown speed: one neutral hue.
fn disk_row(ctx: &Ctx<'_>, axis: &Axis, target: &Target) -> Row {
    let data = ctx.data;
    let row = Row::new("Disk", "heatmap", ctx.focus, axis.cells);
    let held = on_target(data, "io.latency");
    if held.is_empty() {
        let mut row = row.absent(data, &["io.latency"]);
        // Without the latency, the bytes a page has are the flows'.
        row.view = "flows";
        return row;
    }
    let mut cells = vec![None; axis.cells];
    let (mut read, mut written) = (0.0, 0.0);
    let mut latencies = Vec::new();
    for event in held
        .iter()
        .flat_map(|one| one.events())
        .filter(|event| target.holds(event))
    {
        let bytes = event.number("bytes").unwrap_or(0.0);
        let rwbs = event
            .attr("rwbs")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if rwbs.contains('W') {
            written += bytes;
        } else if rwbs.contains('R') {
            read += bytes;
        }
        axis.spread(&mut cells, event.ts_ns, event.ts_ns, bytes);
        if let Some(latency) = event.number("latency_ns") {
            latencies.push(latency);
        }
    }
    let mut row = row;
    row.neutral = Some("a device's speed is not known");
    if latencies.is_empty() {
        return row.empty("no block request of the process".to_owned());
    }
    let cells: Vec<Option<f64>> = cells
        .into_iter()
        .map(|cell| Some(cell.unwrap_or(0.0) * 1e9 / axis.cell_ns()))
        .collect();
    let seconds = axis.span() / 1e9;
    latencies.sort_by(f64::total_cmp);
    let p99 = latencies[((latencies.len() as f64 * 0.99).ceil() as usize).saturating_sub(1)];
    let figures = vec![
        format!("R {}/s", units::bytes(read / seconds)),
        format!("W {}/s", units::bytes(written / seconds)),
        format!("p99 {}", units::duration(p99)),
    ];
    let trust = Trust::joined(data, held.iter().copied());
    row.spark(cells, None).says(figures, trust)
}

/// Network: the bytes the process's sockets received and sent, and the
/// retransmissions.
fn network_row(ctx: &Ctx<'_>, axis: &Axis, target: &Target) -> Row {
    let data = ctx.data;
    let row = Row::new("Network", "flows", ctx.focus, axis.cells);
    let held = on_target(data, "net.socket");
    if held.is_empty() {
        return row.absent(data, &["net.socket"]);
    }
    let mut cells = vec![None; axis.cells];
    let (mut received, mut sent) = (0.0, 0.0);
    for event in held
        .iter()
        .flat_map(|one| one.events())
        .filter(|event| target.holds(event))
    {
        let bytes = event.number("bytes").unwrap_or(0.0);
        match event.attr("op").and_then(|value| value.as_str()) {
            Some("recv") => received += bytes,
            Some("send") => sent += bytes,
            _ => {}
        }
        // A row aggregated over a window covers the window before it.
        let window = event.number("window_ns").unwrap_or(0.0) as u64;
        axis.spread(
            &mut cells,
            event.ts_ns.saturating_sub(window),
            event.ts_ns,
            bytes,
        );
    }
    let mut row = row;
    row.neutral = Some("a link's speed is not known");
    if received + sent <= 0.0 {
        return row.empty(format!("no socket bytes   {}", retransmits(data)));
    }
    let cells: Vec<Option<f64>> = cells
        .into_iter()
        .map(|cell| Some(cell.unwrap_or(0.0) * 1e9 / axis.cell_ns()))
        .collect();
    let seconds = axis.span() / 1e9;
    let figures = vec![
        format!("↓ {}/s", units::bytes(received / seconds)),
        format!("↑ {}/s", units::bytes(sent / seconds)),
        retransmits(data),
    ];
    let trust = Trust::joined(data, held.iter().copied());
    row.spark(cells, None).says(figures, trust)
}

/// The retransmissions: the process's when the run has them, the machine's
/// said as such otherwise.
fn retransmits(data: &Data) -> String {
    let count = |results: &Results| -> f64 {
        results
            .outputs
            .iter()
            .map(|output| match output {
                Output::Summary { series } => {
                    0.0 + series.iter().map(|one| one.count as f64).sum::<f64>()
                }
                Output::Raw { events } => {
                    0.0 + events
                        .iter()
                        .map(|event| event.number("count").unwrap_or(1.0))
                        .sum::<f64>()
                }
                _ => 0.0,
            })
            .sum()
    };
    let own = on_target(data, "net.tcp.retransmit");
    if !own.is_empty() {
        // `0.0 +`: an empty sum of floats is `-0`.
        let total: f64 = 0.0 + own.iter().map(|one| count(one)).sum::<f64>();
        return format!("{} retrans", units::count(total));
    }
    match data
        .of_source("net.tcp.retransmit")
        .map(count)
        .reduce(|x, y| x + y)
    {
        Some(total) => format!("{} retrans (machine)", units::count(total)),
        None => "retrans not asked".to_owned(),
    }
}

/// GPU: the share of the run the process's kernels kept the device busy,
/// and its device memory. Busy is a share of the engine's time: a capacity
/// that exists.
fn gpu_row(ctx: &Ctx<'_>, axis: &Axis, target: &Target) -> Row {
    let data = ctx.data;
    let row = Row::new("GPU", "lanes", ctx.focus, axis.cells);
    let kernels = on_target(data, "gpu.kernel");
    let memory = on_target(data, "gpu.mem");
    if kernels.is_empty() && memory.is_empty() {
        return row.absent(data, &["gpu.kernel", "gpu.mem"]);
    }
    let trust = Trust::joined(data, kernels.iter().chain(&memory).copied());
    let launched: Vec<&Event> = kernels
        .iter()
        .flat_map(|one| one.events())
        .filter(|event| pid(event).is_none() || target.holds(event))
        .collect();
    let vram = peak_of(&level_cells(axis, &timeline(&memory)));
    let vram_text = match vram {
        Some(bytes) => format!("VRAM {}", units::bytes(bytes)),
        None => "VRAM not asked".to_owned(),
    };
    if launched.is_empty() {
        let said = if kernels.is_empty() {
            "kernels not asked"
        } else {
            "no kernel launched"
        };
        let mut row = row.empty(format!("{said}   {vram_text}"));
        row.trust = trust;
        return row;
    }
    let mut cells = vec![None; axis.cells];
    for event in &launched {
        let duration = event.number("duration_ns").unwrap_or(0.0);
        axis.spread(
            &mut cells,
            event.ts_ns,
            event.ts_ns + duration as u64,
            duration,
        );
    }
    let cells: Vec<Option<f64>> = cells
        .into_iter()
        .map(|cell| Some((cell.unwrap_or(0.0) / axis.cell_ns()).min(1.0)))
        .collect();
    let busy: f64 = launched
        .iter()
        .filter_map(|event| event.number("duration_ns"))
        .sum();
    let now = last_of(&cells).unwrap_or(0.0);
    let mut row = row;
    row.fill = Some(now);
    let figures = vec![
        units::percent(now * 100.0),
        format!("avg {}", units::percent(busy * 100.0 / axis.span())),
        vram_text,
    ];
    row.spark(cells, Some(1.0)).says(figures, trust)
}

/// Energy: the machine's draw, and the process's estimated share of it by
/// its share of the busy cores. Energy has no capacity: one neutral hue.
fn energy_row(ctx: &Ctx<'_>, axis: &Axis, capacity: &Capacity, cpu: &Row) -> Row {
    let data = ctx.data;
    let row = Row::new("Energy", "system", ctx.focus, axis.cells);
    let held: Vec<&Results> = data.of_source("power.energy").collect();
    let series = timeline(&held);
    if series.iter().all(Vec::is_empty) {
        return row.absent(data, &["power.energy"]);
    }
    let watts = rate_cells(axis, &series, 1e9);
    let util: Vec<&Results> = data.of_source("cpu.util").collect();
    let busy = level_cells(axis, &timeline(&util));
    // The process's share of the machine's busy cores in each cell: needs
    // the cores, which only a live run knows.
    let share: Option<Vec<Option<f64>>> = capacity.cores.map(|cores| {
        (0..axis.cells)
            .map(|index| {
                let mine = cpu.values.get(index).copied().flatten()?;
                let machine = busy[index]? * cores;
                (machine > 0.0).then(|| (mine / machine).min(1.0))
            })
            .collect()
    });
    let mut row = row;
    row.neutral = Some("energy has no capacity");
    let trust = Trust::joined(data, held.iter().copied());
    let (cells, figures) = match share {
        Some(share) => {
            let cells: Vec<Option<f64>> = watts
                .iter()
                .zip(&share)
                .map(|(watts, share)| Some((*watts)? * (*share)?))
                .collect();
            let joules: f64 = cells.iter().flatten().sum::<f64>() * axis.cell_ns() / 1e9;
            let figures = vec![
                format!("≈ {} W", units::approx(last_of(&cells).unwrap_or(0.0))),
                format!("avg {} W", units::approx(joules * 1e9 / axis.span())),
                "estimated: the machine's by the busy cores' share".to_owned(),
            ];
            (cells, figures)
        }
        None => {
            let joules: f64 = series.iter().flatten().map(|(_, _, value)| value).sum();
            let figures = vec![
                format!("{} W", units::approx(last_of(&watts).unwrap_or(0.0))),
                format!("avg {} W", units::approx(joules * 1e9 / axis.span())),
                "the machine's, not apportioned".to_owned(),
            ];
            (watts, figures)
        }
    };
    row.spark(cells, None).says(figures, trust)
}

/// The program's thread's wall-clock bar, as the timebar reads it.
fn main_bar(data: &Data) -> Option<timebar::Bar> {
    let sheets = timebar::sheets(data);
    sheets
        .iter()
        .find_map(|sheet| {
            sheet
                .main
                .as_ref()
                .map(|(index, _)| sheet.bars[*index].clone())
        })
        .or_else(|| sheets.iter().find_map(|sheet| sheet.bars.first().cloned()))
}

/// Time: the program's wall clock split by state, each state its share.
fn time_row(ctx: &Ctx<'_>, axis: &Axis, bar: Option<&timebar::Bar>) -> Row {
    let data = ctx.data;
    let mut row = Row::new("Time", "timebar", ctx.focus, axis.cells);
    let Some(bar) = bar else {
        return row.absent(
            data,
            &[
                "cpu.profile",
                "sched.offcpu",
                "sched.runqlat",
                "cpu.oncpu_time",
            ],
        );
    };
    if bar.request.is_some() {
        row.focus.request = bar.request.clone();
    }
    let amounts: Vec<f64> = bar
        .parts
        .iter()
        .map(|part| part.amount.unwrap_or(0.0))
        .collect();
    let scale = bar.whole.unwrap_or(0.0).max(amounts.iter().sum());
    row.trust = bar.trust.clone();
    if scale <= 0.0 {
        return row.empty("no state read".to_owned());
    }
    let cells = timebar::allocate(&amounts, scale, axis.cells);
    let mut graph: Vec<Span<'static>> = bar
        .parts
        .iter()
        .zip(&cells)
        .filter(|(_, cells)| **cells > 0)
        .map(|(part, cells)| {
            Span::styled(
                part.class.glyph().to_string().repeat(*cells),
                Style::new().fg(part.class.colour()),
            )
        })
        .collect();
    let drawn: usize = cells.iter().sum();
    if drawn < axis.cells {
        graph.push(dim_span("·".repeat(axis.cells - drawn)));
    }
    row.graph = graph;
    // By class, largest first: the four that matter most.
    let mut by: Vec<(Class, f64)> = Vec::new();
    for (part, amount) in bar.parts.iter().zip(&amounts) {
        match by.iter_mut().find(|(class, _)| *class == part.class) {
            Some((_, held)) => *held += amount,
            None => by.push((part.class, *amount)),
        }
    }
    by.sort_by(|x, y| y.1.total_cmp(&x.1));
    let mut text = Vec::new();
    for (class, amount) in by.iter().filter(|(_, amount)| *amount > 0.0).take(4) {
        let said = format!(
            "{} {}",
            class.short(),
            units::percent(amount * 100.0 / scale)
        );
        if !row.figures.is_empty() {
            row.figures.push(Span::raw("   "));
        }
        row.figures
            .push(Span::styled(said.clone(), Style::new().fg(class.colour())));
        text.push(said);
    }
    row.text = text.join("   ");
    let badges = bar.trust.badges();
    if !badges.is_empty() {
        row.figures.push(Span::raw("  "));
        row.figures.extend(badges);
    }
    row.values = amounts.into_iter().map(Some).collect();
    row
}

/// The suspect: the state the timebar's verdict names, with what names it,
/// and the page that explains it.
fn suspect_line(bar: Option<&timebar::Bar>) -> (Vec<Span<'static>>, &'static str) {
    let lead = Span::styled("▶ suspect: ", Style::new().add_modifier(Modifier::BOLD));
    let Some(bar) = bar else {
        return (vec![lead, dim_span("none read".to_owned())], "timebar");
    };
    match &bar.verdict {
        Verdict::Names { part, share, .. } => {
            let part = &bar.parts[*part];
            // What names it, when that is a name: a sentence about a stack
            // is the flame's to show.
            let what = part
                .named
                .first()
                .filter(|named| named.chars().count() <= 32)
                .cloned()
                .unwrap_or_else(|| part.state.clone());
            let what = if what == part.class.short() {
                String::new()
            } else {
                format!(" ({what})")
            };
            let share = share
                .map(|share| format!("  {}", units::percent(share * 100.0)))
                .unwrap_or_default();
            (
                vec![
                    lead,
                    Span::styled(
                        format!("{}{what}{share}", part.class.short()),
                        Style::new()
                            .fg(part.class.colour())
                            .add_modifier(Modifier::BOLD),
                    ),
                ],
                explained_by(part.class),
            )
        }
        Verdict::Undecided(why) => (
            vec![lead, dim_span(format!("no single state -- {why}"))],
            "timebar",
        ),
    }
}

/// The page that explains a suspect state.
fn explained_by(class: Class) -> &'static str {
    match class {
        Class::OnCpu | Class::Sleep | Class::Blocked | Class::Lock => "flame",
        Class::RunQueue => "lanes",
        Class::Io => "heatmap",
        Class::Network => "flows",
        Class::Child | Class::Other => "timebar",
    }
}

/// The header: what the program is, its pid, its threads, how long it ran,
/// and what measuring it cost -- that last part opening the ledger.
fn header(
    data: &Data,
    target: &Target,
    bar: Option<&timebar::Bar>,
    axis: &Axis,
) -> (Vec<Span<'static>>, usize) {
    // A recording of the machine has no process of its own: what the lines
    // read is every task's, and the header says so rather than name one.
    if data.live.is_none() && !data.has_target() {
        let spans = vec![
            Span::raw(" "),
            Span::styled(
                "no target: this run measured the machine",
                Style::new().add_modifier(Modifier::BOLD),
            ),
            dim_span(format!(
                "  {} · every task's, not one process's · ",
                units::duration(axis.span())
            )),
        ];
        let cost_at = spans.len();
        let mut spans = spans;
        spans.push(Span::styled(omn_cost(data), Style::new().fg(palette::COST)));
        return (spans, cost_at);
    }
    let name = bar
        .and_then(|bar| bar.comm.clone())
        .unwrap_or_else(|| "the target".to_owned());
    let mut said = Vec::new();
    if let Some(pid) = target.pid.or(bar.and_then(|bar| bar.pid)) {
        said.push(format!("pid {pid}"));
    }
    if !target.tids.is_empty() {
        let count = target.tids.len();
        said.push(format!(
            "{count} thread{}",
            if count == 1 { "" } else { "s" }
        ));
    }
    said.push(units::duration(axis.span()));
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(name, Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!("  {} · ", said.join(" · "))),
    ];
    let cost_at = spans.len();
    spans.push(Span::styled(omn_cost(data), Style::new().fg(palette::COST)));
    (spans, cost_at)
}

/// What measuring the program cost, as the frame's corner says it: every
/// collector's cpu and what it spent inside the target, as a share of one cpu
/// over the run; `≥` where some of what was spent inside is not known. Only
/// known at the run's end.
fn omn_cost(data: &Data) -> String {
    if data.live.is_some() {
        return "omn's cost at the end".to_owned();
    }
    let costs = crate::app::run_costs(data);
    if costs.wall_ns == 0 {
        return "omn's cost not measured".to_owned();
    }
    let spent = costs.collectors_cpu_ns + costs.in_target_cpu_ns;
    let bound = if costs.in_target_unknown > 0 {
        "≥"
    } else {
        ""
    };
    format!(
        "omn costs {bound}{} cpu",
        units::percent(spent as f64 * 100.0 / costs.wall_ns as f64)
    )
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// A line over `rows` terminal rows: its name and figures on the first, its
/// sparkline over all of them.
fn draw_row(row: &Row, selected: bool, rows: usize) -> Vec<Line<'static>> {
    let mut label = Style::new().add_modifier(Modifier::BOLD);
    if selected {
        label = label.add_modifier(Modifier::REVERSED);
    }
    if !row.held {
        label = label.add_modifier(Modifier::DIM);
    }
    row.graph_lines(rows)
        .into_iter()
        .enumerate()
        .map(|(index, graph)| {
            let name = if index == 0 { row.name } else { "" };
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(format!("{name:<width$}", width = LABEL - 1), label),
            ];
            spans.extend(graph);
            if index == 0 {
                spans.push(Span::raw("   "));
                spans.extend(row.figures.iter().cloned());
            }
            Line::from(spans)
        })
        .collect()
}

fn rule(width: usize) -> Line<'static> {
    dim("─".repeat(width))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A source's results on the target: those not labelled as the machine's.
fn on_target<'a>(data: &'a Data, source: &str) -> Vec<&'a Results> {
    data.run
        .results
        .iter()
        .filter(|results| results.source == source && !results.label.ends_with("@system"))
        .collect()
}

fn task_of(key: &str) -> Option<i32> {
    key.strip_prefix("task:")?.parse().ok()
}

fn tid(event: &Event) -> Option<i32> {
    event
        .tid()
        .or_else(|| event.number("tid").map(|tid| tid as i32))
}

fn pid(event: &Event) -> Option<i32> {
    event
        .pid()
        .or_else(|| event.number("pid").map(|pid| pid as i32))
}

fn dim(text: String) -> Line<'static> {
    Line::from(dim_span(text))
}

fn dim_span(text: String) -> Span<'static> {
    Span::styled(text, Style::new().add_modifier(Modifier::DIM))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    use super::*;
    use crate::app::{App, Cost};
    use crate::model::{Live, Refusal};
    use crate::snapshot::{self, HEIGHT, WIDTH};

    /// The recording this view was judged on: the `process` workload
    /// (`view/workloads/process/`), recorded by `omn top --headless` into
    /// `docs/view/gallery/process-run/`.
    fn recording() -> Data {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/view/gallery/process-run");
        let mut data = Data::load(&path).expect("the process recording");
        data.origin = "recording process-run".to_owned();
        data
    }

    /// The workload's own truth, beside it.
    fn manifest() -> serde_json::Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("workloads/process/manifest.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("the manifest"))
            .expect("the manifest is JSON")
    }

    fn name(app: &App) -> &'static str {
        app.views[app.current].name()
    }

    fn screen_of(data: &Data) -> Screen {
        let focus = Focus::default();
        screen(&Ctx {
            data,
            against: None,
            focus: &focus,
        })
    }

    fn row<'a>(screen: &'a Screen, name: &str) -> &'a Row {
        screen
            .rows
            .iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("no {name} line"))
    }

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn integral(row: &Row, axis: &Axis) -> f64 {
        row.values.iter().flatten().sum::<f64>() * axis.cell_ns() / 1e9
    }

    #[test]
    fn snapshot_on_the_process_workload() {
        let mut app = App::new(recording());
        app.cost = Cost::Snapshot;
        assert_eq!(
            name(&app),
            "process",
            "a run with a target opens on the process view"
        );
        let buffer = snapshot::draw_app(&mut app, WIDTH, HEIGHT);
        snapshot::assert_snapshot("process--process-run", &buffer);
    }

    /// Each line against the workload's manifest, within the trust it states.
    #[test]
    fn the_lines_find_what_the_workload_did() {
        let data = recording();
        let truth = manifest();
        let screen = screen_of(&data);
        let names: Vec<&str> = screen.rows.iter().map(|row| row.name).collect();
        assert_eq!(
            names,
            ["CPU", "Memory", "Disk", "Network", "GPU", "Energy", "Time"]
        );
        let axis = Axis::of(&data);
        let seconds = axis.span() / 1e9;
        let number = |text: &str, after: &str| -> f64 {
            let rest = text
                .split(after)
                .nth(1)
                .unwrap_or_else(|| panic!("{after} in {text}"));
            rest.split(' ').next().unwrap().parse().unwrap()
        };

        // CPU: the program burns `crunch` and `tidy` on one thread; the mean
        // is its own cpu account over its life.
        let burnt = truth["cpu"]["seconds"].as_f64().unwrap()
            + truth["cpu"]["minor"]["seconds"].as_f64().unwrap();
        let cpu = row(&screen, "CPU");
        let mean = number(&cpu.text, "avg ");
        assert!(
            mean >= burnt / seconds * 0.95 && mean <= (burnt + 0.6) / seconds,
            "{burnt} s burnt over {seconds:.2} s; the line says {}",
            cpu.text
        );
        // The cells hold cores: their integral is the polled cpu time.
        let polled = integral(cpu, &axis);
        assert!(
            polled >= burnt * 0.9 && polled <= burnt + 0.6,
            "the sparkline holds {polled:.3} s of cpu, the program burns {burnt}"
        );
        assert!(
            peak_of(&cpu.values).unwrap() <= 1.5,
            "one thread burns: at most a core and a poll's jitter, {:?}",
            cpu.values
        );

        // Memory: the peak is at least the bytes held, and it grew.
        let memory = row(&screen, "Memory");
        let peak = peak_of(&memory.values).unwrap();
        assert!(
            peak >= truth["memory"]["bytes"].as_f64().unwrap(),
            "{}",
            memory.text
        );
        assert!(memory.text.contains("   +"), "{}", memory.text);

        // Network: the socket bytes, exactly, as rates over the life.
        let network = row(&screen, "Network");
        let sent = truth["socket"]["sent"].as_f64().unwrap();
        let received = truth["socket"]["received"].as_f64().unwrap();
        assert!(
            network.text.starts_with(&format!(
                "↓ {}/s   ↑ {}/s",
                units::bytes(received / seconds),
                units::bytes(sent / seconds)
            )),
            "{}",
            network.text
        );
        let moved = integral(network, &axis);
        assert!(
            (moved - sent - received).abs() < 1.0,
            "the sparkline holds {moved} bytes, the program moved {}",
            sent + received
        );

        // Disk: the file goes through the device as the process's own block
        // requests, 128 KiB each, plus the filesystem's metadata the fsync
        // commits. The line counts the requests issued in the process's
        // context; one the block layer hands to kblockd's worker to dispatch
        // is the worker's, and leaves a hole of one request in the file's
        // sectors (one of 32 in both recordings made, docs/fixes/w21-record.md
        // reconciles the bytes). So: the file less a few such requests, and
        // no more than the file and a bounded metadata share.
        let disk = row(&screen, "Disk");
        assert!(disk.held, "{}", disk.text);
        assert!(disk.text.contains(" p99 "), "{}", disk.text);
        let written = truth["file"]["written"].as_f64().unwrap();
        let request = 131_072.0;
        let moved = integral(disk, &axis);
        assert!(
            moved >= written - 4.0 * request && moved <= written + 1_048_576.0,
            "the device moved {moved} bytes of the process's, the file is {written}"
        );

        // GPU: the workload launches none, and the line says so, dimmed.
        let gpu = row(&screen, "GPU");
        assert!(gpu.text.starts_with("no kernel launched"), "{}", gpu.text);
        assert!(!gpu.held);

        // Energy: a recording has no cores, so the machine's, said as such.
        let energy = row(&screen, "Energy");
        assert!(
            energy.text.contains("the machine's, not apportioned"),
            "{}",
            energy.text
        );

        // No capacity in a recording: CPU and Memory neutral, and said so.
        assert!(cpu.neutral.is_some() && memory.neutral.is_some());
        assert!(
            screen
                .notes
                .iter()
                .any(|note| note.starts_with("CPU in one hue")),
            "{:?}",
            screen.notes
        );

        // Time: the bar's states, and the suspect names one of them.
        let time = row(&screen, "Time");
        assert!(time.text.contains("on cpu"), "{}", time.text);
        let suspect = text(&screen.suspect);
        assert!(suspect.starts_with("▶ suspect: "), "{suspect}");

        // The header: the main thread by name, and omn's cost.
        let header = text(&screen.header);
        assert!(
            header.contains(truth["thread"].as_str().unwrap()),
            "{header}"
        );
        assert!(header.contains("omn costs "), "{header}");
    }

    /// Every line opens its page by key and by click, and the frame comes
    /// back.
    #[test]
    fn each_line_opens_its_page_by_key_and_by_click() {
        let data = recording();
        let screen = screen_of(&data);
        let mut pages = vec!["ledger"];
        pages.extend(screen.rows.iter().map(|row| row.view));
        pages.push(screen.suspect_view);
        assert_eq!(
            &pages[..8],
            ["ledger", "flame", "ledger", "heatmap", "flows", "lanes", "system", "timebar"]
        );
        for (index, page) in pages.iter().enumerate() {
            let mut app = App::new(recording());
            app.cost = Cost::Snapshot;
            snapshot::draw_app(&mut app, WIDTH, HEIGHT);
            app.key(KeyEvent::from(KeyCode::Char('k')));
            for _ in 0..index {
                app.key(KeyEvent::from(KeyCode::Char('j')));
            }
            app.key(KeyEvent::from(KeyCode::Enter));
            assert_eq!(name(&app), *page, "line {index} by keys");
            app.key(KeyEvent::from(KeyCode::Backspace));
            assert_eq!(name(&app), "process", "back from {page}");
        }
        // By the mouse: a click selects the line, a second goes in.
        for (needle, page) in [
            (" Network", "flows"),
            (" Time", "timebar"),
            ("omn costs", "ledger"),
        ] {
            let mut app = App::new(recording());
            app.cost = Cost::Snapshot;
            let buffer = snapshot::draw_app(&mut app, WIDTH, HEIGHT);
            let (column, row) = snapshot::find(&buffer, needle).expect(needle);
            snapshot::click(&mut app, column + 1, row);
            snapshot::draw_app(&mut app, WIDTH, HEIGHT);
            snapshot::click(&mut app, column + 1, row);
            assert_eq!(name(&app), page, "{needle}");
            snapshot::right_click(&mut app, 0, 0);
            assert_eq!(name(&app), "process");
        }
    }

    /// A run without the sources says so per line, dimmed, a refusal with
    /// its reason; nothing is drawn as a zero.
    #[test]
    fn a_line_without_its_sources_says_so_and_a_refusal_says_why() {
        let mut data = Data::default();
        data.refused.push(Refusal {
            label: "net.socket".to_owned(),
            source: "net.socket".to_owned(),
            reason: "needs CAP_BPF".to_owned(),
        });
        let screen = screen_of(&data);
        let network = row(&screen, "Network");
        assert_eq!(network.text, "net.socket refused: needs CAP_BPF");
        for row in &screen.rows {
            assert!(!row.held, "{}", row.name);
            assert!(row.values.iter().flatten().next().is_none(), "{}", row.name);
            if row.name != "Network" {
                assert!(
                    row.text.starts_with("not asked: needs "),
                    "{}: {}",
                    row.name,
                    row.text
                );
                assert!(
                    row.figures
                        .iter()
                        .all(|span| span.style.add_modifier.contains(Modifier::DIM)),
                    "{} is dimmed",
                    row.name
                );
            }
        }
        // A line with nothing drawn is never listed as kept neutral.
        assert!(screen.notes.is_empty(), "{:?}", screen.notes);
    }

    /// A source the run asked for but that read nothing on the target -- the
    /// machine's results only -- is not said to be "not asked".
    #[test]
    fn a_source_asked_on_the_machine_only_is_not_said_unasked() {
        let mut data = Data::default();
        data.run.results.push(crate::model::Results {
            label: "cpu.oncpu_time@system".to_owned(),
            source: "cpu.oncpu_time".to_owned(),
            ..Default::default()
        });
        let screen = screen_of(&data);
        for name in ["CPU", "Time"] {
            let line = row(&screen, name);
            assert!(!line.held, "{name}");
            assert!(
                line.text.starts_with("nothing on the target from ")
                    && line.text.contains("cpu.oncpu_time"),
                "{name}: {}",
                line.text
            );
        }
        assert_eq!(
            row(&screen, "Time").text,
            "nothing on the target from cpu.oncpu_time"
        );
        assert!(row(&screen, "Memory").text.starts_with("not asked: needs "));
    }

    /// Usage is coloured by how full the resource is, a live run reading its
    /// machine's capacity; a trust mark keeps the hue.
    #[test]
    fn usage_takes_the_gradient_by_the_capacity_and_trust_keeps_the_hue() {
        assert_eq!(usage::colour(0.1), usage::RAMP[0]);
        assert_eq!(usage::colour(0.6), usage::RAMP[3]);
        assert_eq!(usage::colour(0.95), usage::RAMP[5]);

        let mut data = recording();
        data.live = Some(Live {
            opened_ns: data.opened_ns(),
            now_ns: data.closed_ns(),
        });
        let screen = screen_of(&data);
        let cpu = row(&screen, "CPU");
        assert!(
            Capacity::of(&data).cores.is_some(),
            "a live run reads the cores"
        );
        assert!(cpu.neutral.is_none());
        assert_eq!(
            cpu.figures[0].style.fg,
            Some(usage::colour(cpu.fill.unwrap())),
            "the current value in the gradient"
        );
        // Live: provisional, a trust mark, and still the hue, never dimmed.
        assert!(!cpu.trust.marks.is_empty());
        assert!(!cpu.figures[0].style.add_modifier.contains(Modifier::DIM));
        assert!(cpu.text.contains("of machine"), "{}", cpu.text);
        let energy = row(&screen, "Energy");
        assert!(energy.text.contains("estimated"), "{}", energy.text);
        // A resource with no capacity keeps the neutral hue.
        let network = row(&screen, "Network");
        assert_eq!(network.figures[0].style.fg, Some(NEUTRAL));
    }

    /// The lock the workload's main thread waits on is the timebar's lock
    /// part, apart from its sleep; the suspect names the largest state,
    /// which on this workload is the sleep (`doze` and the idle hold), not
    /// the lock.
    #[test]
    fn the_time_line_splits_the_lock_and_the_suspect_names_the_largest() {
        let data = recording();
        let truth = manifest();
        let bar = main_bar(&data).expect("the program's bar");
        let seconds = |class: Class| -> f64 {
            bar.parts
                .iter()
                .filter(|part| part.class == class)
                .filter_map(|part| part.amount)
                .sum::<f64>()
                / 1e9
        };
        let lock = seconds(Class::Lock);
        let held = truth["lock"]["seconds"].as_f64().unwrap();
        assert!(
            lock >= held * 0.9 && lock <= held * 1.3,
            "the lock part holds {lock:.3} s, the holder sleeps {held} s under the lock"
        );
        let slept = seconds(Class::Sleep);
        let asked = truth["sleep"]["seconds"].as_f64().unwrap()
            + truth["memory"]["hold_s"].as_f64().unwrap();
        assert!(
            slept >= asked * 0.9,
            "the sleep part holds {slept:.3} s, the program sleeps at least {asked} s"
        );
        assert!(slept > lock, "{slept} against {lock}");
        let screen = screen_of(&data);
        let time = row(&screen, "Time");
        assert!(time.text.contains("lock "), "{}", time.text);
        let suspect = text(&screen.suspect);
        assert!(suspect.starts_with("▶ suspect: sleep"), "{suspect}");
    }

    /// The lines say what the pages they open say: the memory peak is the
    /// ledger's, the cpu's hottest function is the flame's.
    #[test]
    fn the_lines_read_the_ledger_and_the_flame() {
        let data = recording();
        let focus = Focus::default();
        let screen = screen_of(&data);
        let held = ledger::reading(&data, &focus)
            .into_iter()
            .find(|line| line.resource == "memory held")
            .expect("the ledger's memory line");
        let ledger::Reading::Served { value, .. } = held.reading else {
            panic!("the ledger holds the memory: {:?}", held.reading);
        };
        let memory = row(&screen, "Memory");
        assert!(
            memory
                .text
                .contains(&format!("peak {}", units::bytes(value))),
            "{} against the ledger's {value}",
            memory.text
        );
        let hot = flame::hottest(&data, &focus, false).expect("the flame's hottest");
        let cpu = row(&screen, "CPU");
        assert!(
            cpu.text.contains(&format!("hottest {}", hot.name)),
            "{} against the flame's {}",
            cpu.text,
            hot.name
        );
        // And the cpu's mean over the life is the ledger's cpu time.
        let line = ledger::reading(&data, &focus)
            .into_iter()
            .find(|line| line.resource == "cpu time")
            .expect("the ledger's cpu line");
        let ledger::Reading::Served { value: cpu_ns, .. } = line.reading else {
            panic!("the ledger holds the cpu");
        };
        let axis = Axis::of(&data);
        let mean: f64 = cpu
            .text
            .split("avg ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|mean| mean.parse().ok())
            .expect("the mean");
        let burnt = mean * axis.span();
        assert!(
            (burnt - cpu_ns).abs() <= cpu_ns * 0.02,
            "the line's {burnt} ns against the ledger's {cpu_ns}"
        );
    }

    /// The screen fills the terminal: the sparklines take the width the
    /// figures leave, and the lines share the height.
    #[test]
    fn the_screen_fills_the_terminal() {
        for (width, height) in [(80u16, 24u16), (160, 48), (240, 60)] {
            let mut app = App::new(recording());
            app.cost = Cost::Snapshot;
            let buffer = snapshot::draw_app(&mut app, width, height);
            let text = snapshot::text(&buffer);
            let lines: Vec<&str> = text.lines().collect();
            let cpu = lines
                .iter()
                .position(|line| line.starts_with(" CPU "))
                .expect("the cpu line");
            let time = lines
                .iter()
                .position(|line| line.starts_with(" Time "))
                .expect("the time line");
            let last = lines
                .iter()
                .rposition(|line| line.contains("in one hue"))
                .expect("the notes");
            // The frame's help line is the last row; the notes end above it,
            // at most a row or two left over from the division.
            assert!(
                last + 2 + 7 >= usize::from(height) || height <= 24,
                "{width}x{height}: the notes end at row {last}"
            );
            assert!(time > cpu, "{width}x{height}");
            if height > 24 {
                assert!(
                    text.contains("Energy in one hue"),
                    "{width}x{height}: every note is drawn\n{text}"
                );
                assert!(
                    time - cpu > 7,
                    "{width}x{height}: the lines keep one row each"
                );
            }
            let graph = |line: &str| {
                line.chars()
                    .skip(LABEL)
                    .take_while(|c| !c.is_ascii_alphanumeric())
                    .count()
            };
            let drawn = graph(lines[time]);
            assert!(
                drawn + LABEL + 3 + FIGURES + 1 >= usize::from(width),
                "{width}x{height}: the time bar is {drawn} cells wide"
            );
        }
    }

    /// The manifest's phases repeat the figures the program reads: they
    /// must not drift from them.
    #[test]
    fn the_manifest_s_phases_are_the_program_s_figures() {
        let truth = manifest();
        let phases = truth["phases"].as_array().expect("phases");
        let of = |line: &str, field: &str| -> Vec<f64> {
            phases
                .iter()
                .filter(|phase| phase["line"] == line)
                .filter_map(|phase| phase[field].as_f64())
                .collect()
        };
        let cpu = truth["cpu"]["seconds"].as_f64().unwrap()
            + truth["cpu"]["minor"]["seconds"].as_f64().unwrap();
        assert_eq!(of("CPU", "seconds"), [cpu]);
        assert_eq!(
            of("Time", "seconds"),
            [
                truth["sleep"]["seconds"].as_f64().unwrap(),
                truth["lock"]["seconds"].as_f64().unwrap()
            ]
        );
        assert_eq!(
            of("Memory", "bytes"),
            [truth["memory"]["bytes"].as_f64().unwrap()]
        );
        assert_eq!(
            of("Memory", "seconds"),
            [truth["memory"]["hold_s"].as_f64().unwrap()]
        );
        assert_eq!(
            of("Disk", "written"),
            [truth["file"]["written"].as_f64().unwrap()]
        );
        assert_eq!(
            of("Network", "sent"),
            [truth["socket"]["sent"].as_f64().unwrap()]
        );
        assert_eq!(
            of("Network", "received"),
            [truth["socket"]["received"].as_f64().unwrap()]
        );
        // Every line the view draws is a phase, or is said not exercised.
        for line in ["CPU", "Memory", "Disk", "Network", "GPU", "Energy", "Time"] {
            assert!(
                phases.iter().any(|phase| phase["line"] == line)
                    || truth["not_exercised"][line].is_string(),
                "{line}"
            );
        }
    }

    #[test]
    fn an_amount_is_spread_over_the_cells_it_covers() {
        let axis = Axis {
            start: 0,
            end: SPARK as u64 * 100,
            cells: SPARK,
        };
        let mut cells = vec![None; SPARK];
        axis.spread(&mut cells, 50, 250, 200.0);
        assert_eq!(cells[0], Some(50.0));
        assert_eq!(cells[1], Some(100.0));
        assert_eq!(cells[2], Some(50.0));
        assert_eq!(cells[3], None);
        axis.spread(&mut cells, 2_400, 2_400, 7.0);
        assert_eq!(cells[SPARK - 1], Some(7.0));
        // Half of it after the axis' end: that half is left out.
        axis.spread(&mut cells, 2_300, 2_500, 10.0);
        assert_eq!(cells[SPARK - 1], Some(12.0));
    }

    #[test]
    fn mem_total_is_read_in_bytes() {
        assert_eq!(
            mem_total("MemTotal:       65536 kB\nMemFree: 1 kB\n"),
            Some(65536.0 * 1024.0)
        );
        assert_eq!(mem_total("nothing"), None);
    }
}
