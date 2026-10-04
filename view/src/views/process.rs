//! The `process` view: one process at a glance.
//!
//! The first screen of `omn top -- <program>` and of `--pid`: every view's
//! essence for that one process on one screen, a number and a colour per
//! tile, each tile opening the view that has the detail
//! (`docs/view/process.md`).
//!
//! A tile reads only what the other views already ask for, the way they read
//! it, and says what it could not read: a source refused is said with its
//! reason, a source the run does not hold is said as such, and nothing is
//! drawn as zero for want of a measurement.
//!
//! The colours say what the tool found, never a threshold: the first suspect
//! (the timebar's verdict on the program's bar) colours the time tile and the
//! border of the tile whose view explains it; a request's cause and a
//! function's state are drawn in the timebar's colour for that state; every
//! figure is dimmed and badged by its trust.

use std::collections::{BTreeMap, BTreeSet};

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use super::timebar::{self, Class, Verdict};
use super::{Action, Ctx, Focus, Need, Region, View};
use crate::model::{Data, Event, Key, Output, Results, SeriesSummary};
use crate::trust::{palette, Trust};
use crate::units;

/// The tiles read what the other views already ask for: the process view
/// asks for nothing of its own, so it never changes what a live run
/// collects.
const NEEDS: &[Need] = &[];

const KEYS: &[(&str, &str)] = &[
    ("↑↓ hjkl", "move between tiles"),
    ("enter", "open the tile's view"),
];

/// The grid: three tiles across.
const COLUMNS: usize = 3;

/// The cells a strip is drawn in.
const STRIP: usize = 24;

/// The latency sources, in the heatmap's own order of preference, so the
/// strip drawn here is of the source the heatmap opens on.
const LATENCY: &[(&str, &str)] = &[
    ("gpu.kernel", "duration_ns"),
    ("io.latency", "latency_ns"),
    ("net.socket", "lat_ns"),
    ("sched.runqlat", "wait_ns"),
];

const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

#[derive(Default)]
pub struct Process {
    selected: usize,
    regions: Vec<Region>,
}

/// One tile: what it is, its headline figure, a line or two under it, and
/// the view it opens.
#[derive(Debug, Clone)]
pub struct Tile {
    /// Its name in the grid: `time`, `cpu`, `cost`...
    pub name: &'static str,
    /// The view it opens.
    pub view: &'static str,
    /// The headline: a number, in the colour of what was found.
    pub figure: Line<'static>,
    /// The essence's other lines.
    pub detail: Vec<Line<'static>>,
    /// The headline as plain text.
    pub text: String,
    /// The tile explains the first suspect: its border is drawn in the
    /// suspect's colour.
    pub suspect: Option<Color>,
    /// What the view it opens is narrowed to.
    pub focus: Focus,
    /// The trust of every figure the tile draws, badged on its last line
    /// so the headline stays a number read at a glance.
    pub trust: Trust,
    /// The tile draws something read: false when its source is refused or
    /// absent, or held but empty (a plain headline and nothing under it),
    /// and then it is never the one marked as explaining a suspect.
    pub held: bool,
}

impl Tile {
    fn new(name: &'static str, view: &'static str, focus: &Focus) -> Tile {
        Tile {
            name,
            view,
            figure: Line::default(),
            detail: Vec::new(),
            text: String::new(),
            suspect: None,
            focus: focus.clone(),
            trust: Trust::clean(),
            held: true,
        }
    }

    /// The headline, in the trust style and the colour of what was found.
    fn headline(mut self, text: String, colour: Option<Color>, trust: &Trust) -> Tile {
        self.held = true;
        let mut style = trust.style();
        if let Some(colour) = colour {
            style = style.fg(colour).add_modifier(Modifier::BOLD);
        }
        self.figure = Line::from(Span::styled(text.clone(), style));
        self.text = text;
        self.trust.merge(trust.clone());
        self
    }

    /// A headline that is not a figure: nothing was there to read, so the
    /// tile holds nothing that could explain a suspect.
    fn plain(mut self, text: &str) -> Tile {
        self.held = false;
        self.text = text.to_owned();
        self.figure = dim(text.to_owned());
        self
    }

    /// A tile with nothing to read: refused with the reason, or not in the
    /// run.
    fn absent(self, data: &Data, sources: &[&str]) -> Tile {
        let refusal = data
            .refused
            .iter()
            .find(|refusal| sources.contains(&refusal.source.as_str()));
        let mut tile = self;
        tile.held = false;
        match refusal {
            Some(refusal) => {
                tile.text = format!("{} refused", refusal.source);
                tile.figure = Line::from(Span::styled(
                    tile.text.clone(),
                    Style::new().fg(palette::REFUSED),
                ));
                tile.detail = vec![dim(refusal.reason.clone())];
            }
            None => {
                tile = tile.plain("not in this run");
                tile.detail = vec![dim(format!("needs {}", sources.join(" or ")))];
            }
        }
        tile
    }

    fn line(mut self, line: Line<'static>) -> Tile {
        self.detail.push(line);
        self
    }
}

impl View for Process {
    fn name(&self) -> &'static str {
        "process"
    }

    fn title(&self) -> &'static str {
        "one process at a glance: every view's essence, each tile opening its view"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        let tiles = tiles(ctx);
        self.selected = self.selected.min(tiles.len().saturating_sub(1));
        self.regions.clear();
        let block = Block::new()
            .borders(Borders::ALL)
            .title(format!(" process -- {} ", target_says(ctx.data)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [head, grid] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
        frame.render_widget(Paragraph::new(verdict_lines(&tiles)), head);
        let rows = tiles.len().div_ceil(COLUMNS).max(1);
        let row_areas = Layout::vertical(vec![Constraint::Ratio(1, rows as u32); rows]).split(grid);
        for (index, tile) in tiles.iter().enumerate() {
            let cells = Layout::horizontal(vec![Constraint::Ratio(1, COLUMNS as u32); COLUMNS])
                .split(row_areas[index / COLUMNS]);
            let rect = cells[index % COLUMNS];
            draw_tile(frame, rect, tile, index == self.selected);
            self.regions.push(Region::new(rect, index));
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        let count = tiles(ctx).len();
        if count == 0 {
            return Action::None;
        }
        let at = self.selected.min(count - 1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if at + COLUMNS < count {
                    self.selected = at + COLUMNS;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = at.saturating_sub(COLUMNS),
            KeyCode::Char('l') => self.selected = (at + 1).min(count - 1),
            KeyCode::Char('h') => self.selected = at.saturating_sub(1),
            KeyCode::Home => self.selected = 0,
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
        self.selected = id;
        Action::None
    }
}

impl Process {
    /// The selected tile's view, narrowed to what the tile is about.
    fn open(&self, ctx: &Ctx<'_>) -> Action {
        match tiles(ctx).into_iter().nth(self.selected) {
            Some(tile) => Action::Goto {
                view: tile.view,
                focus: tile.focus,
            },
            None => Action::None,
        }
    }
}

pub fn view() -> Box<dyn View> {
    Box::<Process>::default()
}

// ---------------------------------------------------------------------------
// The tiles
// ---------------------------------------------------------------------------

/// Every tile, in the grid's order: time, cpu, cost; request, latency,
/// threads; wake, flows, gpu.
pub fn tiles(ctx: &Ctx<'_>) -> Vec<Tile> {
    let target = Target::of(ctx.data);
    let (time, suspect) = time_tile(ctx);
    let mut tiles = vec![
        time,
        cpu_tile(ctx),
        cost_tile(ctx),
        request_tile(ctx),
        latency_tile(ctx, &target),
        threads_tile(ctx, &target),
        wake_tile(ctx, &target),
        flows_tile(ctx, &target),
        gpu_tile(ctx, &target),
    ];
    if let Some(class) = suspect {
        let explains = explained_by(class);
        for tile in tiles
            .iter_mut()
            .filter(|tile| tile.name == explains && tile.held)
        {
            tile.suspect = Some(class.colour());
        }
    }
    tiles
}

/// Which tile explains a suspect state: the view a reader goes to next.
fn explained_by(class: Class) -> &'static str {
    match class {
        Class::OnCpu | Class::Sleep | Class::Blocked => "cpu",
        Class::RunQueue => "threads",
        Class::Lock => "wake",
        Class::Io | Class::Network => "flows",
        Class::Child | Class::Other => "cost",
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
    /// event when the run says neither (a recording of one source).
    fn holds(&self, tid: Option<i32>, pid: Option<i32>) -> bool {
        if self.tids.is_empty() && self.pid.is_none() {
            return true;
        }
        match (tid, pid) {
            (Some(tid), _) if self.tids.contains(&tid) => true,
            (_, Some(pid)) => self.pid == Some(pid),
            _ => false,
        }
    }

    fn holds_event(&self, event: &Event) -> bool {
        self.holds(tid(event), pid(event))
    }
}

/// The wall-clock bar of the program's thread and its first suspect, as the
/// timebar decides it.
fn time_tile(ctx: &Ctx<'_>) -> (Tile, Option<Class>) {
    let tile = Tile::new("time", "timebar", ctx.focus);
    let sheets = timebar::sheets(ctx.data);
    let main = sheets
        .iter()
        .find_map(|sheet| sheet.main.as_ref().map(|(index, _)| (sheet, *index)))
        .or_else(|| {
            sheets
                .iter()
                .find(|sheet| !sheet.bars.is_empty())
                .map(|sheet| (sheet, 0))
        });
    let Some((sheet, index)) = main else {
        let tile = tile.absent(
            ctx.data,
            &[
                "cpu.profile",
                "sched.offcpu",
                "sched.runqlat",
                "cpu.oncpu_time",
            ],
        );
        return (tile, None);
    };
    let bar = &sheet.bars[index];
    let who = match (bar.tid, &bar.comm) {
        (Some(tid), Some(comm)) => format!("{comm} ({tid})"),
        (Some(tid), None) => format!("thread {tid}"),
        _ => bar.key.clone(),
    };
    let mut tile = tile;
    if bar.request.is_some() {
        tile.focus.request = bar.request.clone();
    }
    let (tile, suspect) = match &bar.verdict {
        Verdict::Names {
            part,
            amount,
            share,
            ..
        } => {
            let part = &bar.parts[*part];
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
                tile.headline(
                    format!("▶ {} {figure}", part.class.short()),
                    Some(part.class.colour()),
                    &bar.trust,
                ),
                Some(part.class),
            )
        }
        Verdict::Undecided(_) => (
            tile.headline("≈ no single state".to_owned(), None, &bar.trust),
            None,
        ),
    };
    let wall = bar
        .whole
        .map(|whole| format!(", {} wall", units::duration(whole)))
        .unwrap_or_default();
    let tile = tile
        .line(state_strip(bar))
        .line(dim(format!("{who}{wall}")));
    (tile, suspect)
}

/// The bar in a few cells: each state its glyph and colour, in proportion.
fn state_strip(bar: &timebar::Bar) -> Line<'static> {
    let amounts: Vec<f64> = bar
        .parts
        .iter()
        .map(|part| part.amount.unwrap_or(0.0))
        .collect();
    let scale = bar.whole.unwrap_or(0.0).max(amounts.iter().sum());
    if scale <= 0.0 {
        return dim("no state read".to_owned());
    }
    let cells = timebar::allocate(&amounts, scale, STRIP);
    let mut spans: Vec<Span<'static>> = bar
        .parts
        .iter()
        .zip(cells)
        .filter(|(_, cells)| *cells > 0)
        .map(|(part, cells)| {
            Span::styled(
                part.class.glyph().to_string().repeat(cells),
                Style::new().fg(part.class.colour()),
            )
        })
        .collect();
    let drawn: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    if drawn < STRIP {
        spans.push(dim_span("·".repeat(STRIP - drawn)));
    }
    Line::from(spans)
}

/// The hottest code on cpu and off it, over the stacks the flame's two panes
/// draw.
fn cpu_tile(ctx: &Ctx<'_>) -> Tile {
    let data = ctx.data;
    let tile = Tile::new("cpu", "flame", ctx.focus);
    let on = on_target(data, "cpu.profile");
    let off = on_target(data, "sched.offcpu");
    if on.is_empty() && off.is_empty() {
        return tile.absent(data, &["cpu.profile", "sched.offcpu"]);
    }
    let mut tile = match hottest(&on) {
        Some((name, share)) => tile
            .headline(
                format!("{name} {}", units::percent(share * 100.0)),
                Some(Class::OnCpu.colour()),
                &Trust::joined(data, on.iter().copied()),
            )
            .line(dim("on cpu: the hottest own code".to_owned())),
        None if on.is_empty() => tile.plain("on cpu: not in this run"),
        None => tile.plain("no cpu sample"),
    };
    let line = match hottest(&off) {
        Some((name, share)) => {
            let trust = Trust::joined(data, off.iter().copied());
            let line = Line::from(Span::styled(
                format!("off cpu: {name} {}", units::percent(share * 100.0)),
                trust.style().fg(Class::Blocked.colour()),
            ));
            tile.trust.merge(trust);
            // Off cpu is what explains a sleep or a block: the tile holds it
            // even when on cpu has no sample.
            tile.held = true;
            line
        }
        None if off.is_empty() => dim("off cpu: not in this run".to_owned()),
        None => dim("off cpu: no block recorded".to_owned()),
    };
    tile.detail.push(line);
    tile
}

/// What a stack is charged to here: its two deepest frames of the
/// program's own code, caller first (`crunch › burn`) -- an interpreter's
/// frames (`py::name:file`) when the stack has any, its resolved frames
/// otherwise. A frame left as `library+0x...` or a bare address names
/// nothing a reader can act on; the caller is kept because a leaf shared by
/// every caller (a helper, a system call) does not say whose work it was.
pub fn charged_to(frames: &[String]) -> Option<String> {
    let interpreted: Vec<&str> = frames
        .iter()
        .filter_map(|frame| frame.strip_prefix("py::"))
        .map(|name| name.split(':').next().unwrap_or(name))
        .collect();
    let own: Vec<&str> = if interpreted.is_empty() {
        frames
            .iter()
            .map(String::as_str)
            .filter(|frame| {
                !frame.contains("+0x") && !frame.starts_with("0x") && !frame.starts_with('[')
            })
            .collect()
    } else {
        interpreted
    };
    let deepest = own.len().checked_sub(1)?;
    Some(own[deepest.saturating_sub(1)..].join(" › "))
}

/// The function with the largest weight across the stacks, and its share of
/// the whole (the stacks no function was charged for included in the whole).
fn hottest(results: &[&Results]) -> Option<(String, f64)> {
    let mut by: BTreeMap<String, f64> = BTreeMap::new();
    let mut whole = 0.0;
    for one in results {
        for output in &one.outputs {
            if let Output::Stack { stacks, .. } = output {
                for stack in stacks {
                    whole += stack.total;
                    if let Some(name) = charged_to(one.frames(stack.stack)) {
                        *by.entry(name).or_default() += stack.total;
                    }
                }
            }
        }
    }
    if whole <= 0.0 {
        return None;
    }
    by.into_iter()
        .max_by(|x, y| x.1.total_cmp(&y.1).then_with(|| y.0.cmp(&x.0)))
        .map(|(name, weight)| (name, weight / whole))
}

/// The cost line by resource: cpu time, the resident peak, the page faults.
fn cost_tile(ctx: &Ctx<'_>) -> Tile {
    let data = ctx.data;
    let tile = Tile::new("cost", "ledger", ctx.focus);
    let cpu = on_target(data, "cpu.oncpu_time");
    let rss = on_target(data, "mem.rss");
    let faults = on_target(data, "mem.faults");
    let lifetime_cpu = data
        .run
        .results
        .iter()
        .find_map(|results| results.lifetime.as_ref()?.get("cpu_ns")?.as_f64());
    let cpu_ns = lifetime_cpu.or_else(|| summed(&cpu, |series| series.last));
    if cpu_ns.is_none() && rss.is_empty() && faults.is_empty() {
        return tile.absent(data, &["cpu.oncpu_time", "mem.rss", "mem.faults"]);
    }
    let trust = Trust::joined(data, cpu.iter().chain(&rss).chain(&faults).copied());
    let headline = match cpu_ns {
        Some(ns) => format!("cpu {}", units::duration(ns)),
        None => "cpu not read".to_owned(),
    };
    let mut tile = tile.headline(headline, None, &trust);
    let mut parts = Vec::new();
    if let Some(peak) = peak(&rss) {
        parts.push(format!("rss peak {}", units::bytes(peak)));
    }
    if let Some(count) = summed(&faults, |series| series.count as f64) {
        parts.push(format!("{} faults", units::count(count)));
    }
    if !parts.is_empty() {
        tile = tile.line(Line::from(Span::styled(parts.join(" · "), trust.style())));
    }
    tile.line(dim(if lifetime_cpu.is_some() {
        "cpu: the program's own account".to_owned()
    } else {
        "cpu: polled per thread".to_owned()
    }))
}

/// The largest value any summary of these results holds.
fn peak(results: &[&Results]) -> Option<f64> {
    results
        .iter()
        .flat_map(|one| one.outputs.iter())
        .filter_map(|output| match output {
            Output::Summary { series } => series.iter().map(|one| one.max).reduce(f64::max),
            _ => None,
        })
        .reduce(f64::max)
}

/// The sum over every series of a summary's figure, or `None` when no
/// result holds a summary.
fn summed(results: &[&Results], read: impl Fn(&SeriesSummary) -> f64) -> Option<f64> {
    let mut any = false;
    let mut sum = 0.0;
    for one in results {
        for output in &one.outputs {
            if let Output::Summary { series } = output {
                any = true;
                sum += series.iter().map(&read).sum::<f64>();
            }
        }
    }
    any.then_some(sum)
}

/// The slowest request and the state it spent most in.
fn request_tile(ctx: &Ctx<'_>) -> Tile {
    let data = ctx.data;
    let tile = Tile::new("request", "request", ctx.focus);
    let held = on_target(data, "net.request");
    if held.is_empty() {
        // A program declares its requests or does not: a refusal here is
        // the common case, said in full but not drawn as a failure.
        return match data
            .refused
            .iter()
            .find(|refusal| refusal.source == "net.request")
        {
            Some(refusal) => tile
                .plain("no request declared")
                .line(dim(format!("refused: {}", refusal.reason))),
            None => tile.absent(data, &["net.request"]),
        };
    }
    let trust = Trust::joined(data, held.iter().copied());
    let events: Vec<&Event> = held.iter().flat_map(|one| one.events()).collect();
    let total = |event: &&&Event| event.number("total_ns").unwrap_or(0.0);
    let Some(slowest) = events
        .iter()
        .filter(|event| event.number("total_ns").is_some())
        .max_by(|x, y| total(x).total_cmp(&total(y)))
    else {
        return tile.plain("no request ended");
    };
    let id = slowest.keys.iter().find_map(|key| match key {
        Key::Req { id } => Some(id.clone()),
        _ => None,
    });
    let cause = request_cause(slowest);
    let mut tile = tile;
    if let Some(id) = &id {
        tile.focus.request = Some(id.clone());
    }
    let why = cause
        .map(|(class, share)| format!(" ▶ {} {}", class.short(), units::percent(share * 100.0)))
        .unwrap_or_default();
    tile.headline(
        format!(
            "{} {}{why}",
            id.as_deref().unwrap_or("?"),
            units::duration(slowest.number("total_ns").unwrap_or(0.0))
        ),
        cause.map(|(class, _)| class.colour()),
        &trust,
    )
    .line(dim(format!(
        "the slowest of {} requests",
        units::count(events.len() as f64)
    )))
}

/// The state a request spent most of its time in, by the fields
/// `net.request` writes, and its share of the request.
pub fn request_cause(event: &Event) -> Option<(Class, f64)> {
    let total = event.number("total_ns").filter(|total| *total > 0.0)?;
    let held = event.number("holder_ns").unwrap_or(0.0);
    let sleep = event.number("sleep_ns").unwrap_or(0.0);
    let read = |name: &str| event.number(name).unwrap_or(0.0);
    let parts = [
        (Class::OnCpu, read("oncpu_ns")),
        (Class::RunQueue, read("runq_ns")),
        (Class::Io, read("disk_ns")),
        (Class::Network, read("rx_wait_ns") + read("tx_wait_ns")),
        // A sleep another thread's lock explains is the lock's.
        (Class::Lock, held.min(sleep)),
        (Class::Sleep, (sleep - held).max(0.0)),
    ];
    parts
        .into_iter()
        .filter(|(_, amount)| *amount > 0.0)
        .max_by(|x, y| x.1.total_cmp(&y.1))
        .map(|(class, amount)| (class, amount / total))
}

/// The latency strip of the source the heatmap opens on: the slowest
/// measurement in each slice of the run.
fn latency_tile(ctx: &Ctx<'_>, target: &Target) -> Tile {
    let data = ctx.data;
    let tile = Tile::new("latency", "heatmap", ctx.focus);
    let chosen = LATENCY.iter().find_map(|(source, attr)| {
        let held = on_target(data, source);
        let points: Vec<(u64, f64)> = held
            .iter()
            .flat_map(|one| {
                let attr = one.value_attr.clone().unwrap_or_else(|| (*attr).to_owned());
                one.events()
                    .iter()
                    .filter(|event| target.holds_event(event))
                    .filter_map(|event| Some((event.ts_ns, event.number(&attr)?)))
                    .collect::<Vec<_>>()
            })
            .collect();
        (!points.is_empty()).then_some((*source, held, points))
    });
    let Some((source, held, points)) = chosen else {
        let sources: Vec<&str> = LATENCY.iter().map(|(source, _)| *source).collect();
        return tile.absent(data, &sources);
    };
    let trust = Trust::joined(data, held.iter().copied());
    let mut sorted: Vec<f64> = points.iter().map(|(_, value)| *value).collect();
    sorted.sort_by(f64::total_cmp);
    let max = sorted[sorted.len() - 1];
    let median = sorted[sorted.len() / 2];
    let strip = strip(data, &points, |values| {
        values.iter().copied().fold(0.0, f64::max)
    });
    tile.headline(format!("max {}", units::duration(max)), None, &trust)
        .line(Line::from(Span::styled(
            strip,
            trust.style().fg(palette::TIME),
        )))
        .line(dim(format!(
            "{source}: {} measured, median {}",
            units::count(points.len() as f64),
            units::duration(median)
        )))
}

/// The threads' activity: how many switches each slice of the run holds,
/// over the target's threads.
fn threads_tile(ctx: &Ctx<'_>, target: &Target) -> Tile {
    let data = ctx.data;
    let tile = Tile::new("threads", "lanes", ctx.focus);
    let held = on_target(data, "sched.switches");
    let events: Vec<&Event> = held
        .iter()
        .flat_map(|one| one.events())
        .filter(|event| target.holds_event(event))
        .collect();
    let threads: BTreeSet<i32> = events
        .iter()
        .filter_map(|event| tid(event))
        .chain(target.tids.iter().copied())
        .collect();
    if held.is_empty() && threads.is_empty() {
        return tile.absent(data, &["sched.switches", "cpu.oncpu_time"]);
    }
    let trust = Trust::joined(data, held.iter().copied());
    let plural = if threads.len() == 1 { "" } else { "s" };
    let tile = tile.headline(format!("{} thread{plural}", threads.len()), None, &trust);
    if events.is_empty() {
        // A thread count alone does not explain a run-queue wait.
        let mut tile = tile.line(dim("no switch recorded".to_owned()));
        tile.held = false;
        return tile;
    }
    let points: Vec<(u64, f64)> = events
        .iter()
        .map(|event| (event.ts_ns, event.number("count").unwrap_or(1.0)))
        .collect();
    let switches: f64 = points.iter().map(|(_, count)| *count).sum();
    tile.line(Line::from(Span::styled(
        strip(data, &points, |values| values.iter().sum()),
        trust.style().fg(Class::OnCpu.colour()),
    )))
    .line(dim(format!("{} context switches", units::count(switches))))
}

/// What woke the target's threads most: a thread, by its name, or the
/// kernel's interrupt context.
fn wake_tile(ctx: &Ctx<'_>, target: &Target) -> Tile {
    let data = ctx.data;
    let tile = Tile::new("wake", "wake", ctx.focus);
    let held = on_target(data, "sched.wakeup");
    if held.is_empty() {
        return tile.absent(data, &["sched.wakeup"]);
    }
    let trust = Trust::joined(data, held.iter().copied());
    let mut by: BTreeMap<String, f64> = BTreeMap::new();
    let mut whole = 0.0;
    for event in held.iter().flat_map(|one| one.events()) {
        let wakee_tid = event.number("wakee_tid").map(|tid| tid as i32);
        let wakee_pid = event.number("wakee_pid").map(|pid| pid as i32);
        if !target.holds(wakee_tid, wakee_pid) {
            continue;
        }
        let count = event.number("count").unwrap_or(1.0);
        whole += count;
        let context = event
            .attr("waker_context")
            .and_then(|value| value.as_str())
            .unwrap_or("task");
        let waker = if context == "task" {
            let tid = event.number("waker_tid").map(|tid| tid as i32);
            let comm = event.attr("waker_comm").and_then(|value| value.as_str());
            match (comm, tid) {
                (Some(comm), Some(tid)) => format!("{comm} ({tid})"),
                (None, Some(tid)) => format!("thread {tid}"),
                _ => "a thread".to_owned(),
            }
        } else {
            context.to_owned()
        };
        *by.entry(waker).or_default() += count;
    }
    let Some((waker, count)) = by
        .iter()
        .max_by(|x, y| x.1.total_cmp(y.1).then_with(|| y.0.cmp(x.0)))
    else {
        return tile.plain("no wake-up of the target");
    };
    tile.headline(format!("{waker} ×{}", units::count(*count)), None, &trust)
        .line(dim(format!(
            "{} of {} wake-ups; {} wakers",
            units::percent(count * 100.0 / whole),
            units::count(whole),
            by.len()
        )))
}

/// The bytes in and out: over the sockets, and to and from the devices.
fn flows_tile(ctx: &Ctx<'_>, target: &Target) -> Tile {
    let data = ctx.data;
    let tile = Tile::new("flows", "flows", ctx.focus);
    let sockets = on_target(data, "net.socket");
    let devices = on_target(data, "io.latency");
    if sockets.is_empty() && devices.is_empty() {
        return tile.absent(data, &["net.socket", "io.latency"]);
    }
    let (mut received, mut sent) = (0.0, 0.0);
    for event in sockets
        .iter()
        .flat_map(|one| one.events())
        .filter(|event| target.holds_event(event))
    {
        let bytes = event.number("bytes").unwrap_or(0.0);
        match event.attr("op").and_then(|value| value.as_str()) {
            Some("recv") => received += bytes,
            Some("send") => sent += bytes,
            _ => {}
        }
    }
    let (mut read, mut written) = (0.0, 0.0);
    for event in devices
        .iter()
        .flat_map(|one| one.events())
        .filter(|event| target.holds_event(event))
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
    }
    let net = if sockets.is_empty() {
        "net not in this run".to_owned()
    } else {
        format!("net ↓{} ↑{}", units::bytes(received), units::bytes(sent))
    };
    let tile = tile.headline(
        net,
        Some(Class::Network.colour()),
        &Trust::joined(data, sockets.iter().copied()),
    );
    let disk = if devices.is_empty() {
        dim("disk: not in this run".to_owned())
    } else {
        let trust = Trust::joined(data, devices.iter().copied());
        Line::from(Span::styled(
            format!("disk ↓{} ↑{}", units::bytes(read), units::bytes(written)),
            trust.style().fg(Class::Io.colour()),
        ))
    };
    tile.line(disk)
        .line(dim("↓ received or read · ↑ sent or written".to_owned()))
}

/// The target's share of the GPU: its kernels' time over the run's wall
/// time, and its device memory.
fn gpu_tile(ctx: &Ctx<'_>, target: &Target) -> Tile {
    let data = ctx.data;
    let tile = Tile::new("gpu", "lanes", ctx.focus);
    let kernels = on_target(data, "gpu.kernel");
    let memory = on_target(data, "gpu.mem");
    if kernels.is_empty() && memory.is_empty() {
        return tile.absent(data, &["gpu.kernel", "gpu.mem"]);
    }
    let trust = Trust::joined(data, kernels.iter().chain(&memory).copied());
    let launched: Vec<&Event> = kernels
        .iter()
        .flat_map(|one| one.events())
        .filter(|event| pid(event).is_none() || target.holds_event(event))
        .collect();
    let busy: f64 = launched
        .iter()
        .filter_map(|event| event.number("duration_ns"))
        .sum();
    let wall = data.closed_ns().saturating_sub(data.opened_ns()) as f64;
    let mut tile = tile;
    tile.trust.merge(trust.clone());
    let tile = if launched.is_empty() {
        tile.plain(if kernels.is_empty() {
            "kernels not in this run"
        } else {
            "no kernel launched"
        })
    } else if wall > 0.0 {
        tile.headline(
            format!("{} busy", units::percent(busy * 100.0 / wall)),
            None,
            &trust,
        )
        .line(dim(format!(
            "{} kernels, {} on the device",
            units::count(launched.len() as f64),
            units::duration(busy)
        )))
    } else {
        tile.headline(
            format!("{} on the device", units::duration(busy)),
            None,
            &trust,
        )
    };
    match peak(&memory) {
        Some(peak) => tile.line(dim(format!("device memory peak {}", units::bytes(peak)))),
        None => tile,
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// The verdict line: the first suspect, and the tile that explains it.
fn verdict_lines(tiles: &[Tile]) -> Vec<Line<'static>> {
    let time = tiles.iter().find(|tile| tile.name == "time");
    let next = tiles.iter().find(|tile| tile.suspect.is_some());
    let mut first = vec![Span::styled(
        "first suspect: ",
        Style::new().add_modifier(Modifier::BOLD),
    )];
    match (time, next) {
        (Some(time), Some(next)) => {
            first.extend(time.figure.spans.iter().cloned());
            first.push(Span::raw(format!(
                " -- the {} tile ({}) has the detail",
                next.name, next.view
            )));
        }
        (Some(time), None) => first.extend(time.figure.spans.iter().cloned()),
        _ => first.push(dim_span("none read".to_owned())),
    }
    vec![
        Line::from(first),
        Line::from(dim_span(
            "one view's essence per tile: enter, or a click on the selected tile, opens it"
                .to_owned(),
        )),
    ]
}

fn draw_tile(frame: &mut Frame<'_>, area: Rect, tile: &Tile, selected: bool) {
    let mut border = Style::new();
    if let Some(colour) = tile.suspect {
        border = border.fg(colour);
    }
    let mut title = Style::new().add_modifier(Modifier::BOLD);
    if selected {
        title = title.add_modifier(Modifier::REVERSED);
    }
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(if tile.suspect.is_some() {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(border)
        .title(Line::from(vec![
            Span::styled(format!(" {} ", tile.name), title),
            dim_span(format!("→ {} ", tile.view)),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![tile.figure.clone()];
    // A detail line is wrapped, not cut: a refusal's reason is read whole.
    lines.extend(
        tile.detail
            .iter()
            .flat_map(|line| wrap(line, inner.width as usize)),
    );
    let height = inner.height as usize;
    if lines.len() > height && height > 0 {
        // More than the tile holds: its last row says so, and the view the
        // tile opens has the rest.
        lines.truncate(height);
        if let Some(last) = lines.last_mut() {
            ellipsis(last, inner.width as usize);
        }
    }
    let badges = tile.trust.badges();
    if !badges.is_empty() && inner.height as usize > lines.len() {
        // The trust on the tile's last row: the headline stays a number.
        let blank = inner.height as usize - lines.len() - 1;
        lines.extend(std::iter::repeat_n(Line::default(), blank));
        lines.push(Line::from(badges));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A line broken to `width` columns, at a space when the row has one, the
/// spans keeping their styles.
fn wrap(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line.width() <= width {
        return vec![line.clone()];
    }
    let cells: Vec<(char, Style, usize)> = line
        .spans
        .iter()
        .flat_map(|span| {
            span.content
                .chars()
                .map(move |ch| (ch, span.style, Span::raw(ch.to_string()).width()))
        })
        .collect();
    let mut rows: Vec<Vec<(char, Style, usize)>> = Vec::new();
    let mut row: Vec<(char, Style, usize)> = Vec::new();
    let mut used = 0;
    for cell in cells {
        if used + cell.2 > width && !row.is_empty() {
            let rest = match row.iter().rposition(|(ch, _, _)| *ch == ' ') {
                Some(space) if space > 0 => {
                    let rest = row.split_off(space + 1);
                    row.pop();
                    rest
                }
                _ => Vec::new(),
            };
            rows.push(std::mem::replace(&mut row, rest));
            used = row.iter().map(|cell| cell.2).sum();
        }
        if row.is_empty() && cell.0 == ' ' {
            continue;
        }
        used += cell.2;
        row.push(cell);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows.into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (ch, style, _) in row {
                match spans.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push(ch),
                    _ => spans.push(Span::styled(ch.to_string(), style)),
                }
            }
            Line::from(spans).style(line.style)
        })
        .collect()
}

/// A row cut short: its end replaced by `…` within `width`.
fn ellipsis(line: &mut Line<'static>, width: usize) {
    while line.width() + 1 > width {
        let Some(span) = line.spans.last_mut() else {
            break;
        };
        if span.content.to_mut().pop().is_none() || span.content.is_empty() {
            line.spans.pop();
        }
    }
    let style = line.spans.last().map(|span| span.style).unwrap_or_default();
    line.spans.push(Span::styled("…", style));
}

/// What the header says of the target: its pid, and how long it ran.
fn target_says(data: &Data) -> String {
    let target = Target::of(data);
    let mut parts = Vec::new();
    if let Some(pid) = target.pid {
        parts.push(format!("pid {pid}"));
    }
    let wall = data.run.results.iter().find_map(|results| {
        let lifetime = results.lifetime.as_ref()?;
        let released = lifetime.get("released_ns")?.as_u64()?;
        let exited = lifetime.get("exited_ns")?.as_u64()?;
        Some(exited.saturating_sub(released))
    });
    if let Some(wall) = wall {
        parts.push(format!("ran {}", units::duration(wall as f64)));
    }
    if parts.is_empty() {
        "one process at a glance".to_owned()
    } else {
        format!("one process at a glance: {}", parts.join(", "))
    }
}

/// A run-wide strip: the run cut into [`STRIP`] slices, each drawn by the
/// height of `reduce` over the values in it, against the tallest. An empty
/// slice is a blank, never a zero.
fn strip(data: &Data, points: &[(u64, f64)], reduce: impl Fn(&[f64]) -> f64) -> String {
    let first = points.iter().map(|(ts, _)| *ts).min().unwrap_or(0);
    let last = points.iter().map(|(ts, _)| *ts).max().unwrap_or(0);
    let opened = data.opened_ns();
    let start = if opened > 0 { opened.min(first) } else { first };
    let end = data.closed_ns().max(last);
    let span = end.saturating_sub(start).max(1) as f64;
    let mut slices: Vec<Vec<f64>> = vec![Vec::new(); STRIP];
    for (ts, value) in points {
        let at = (ts.saturating_sub(start) as f64 / span * STRIP as f64) as usize;
        slices[at.min(STRIP - 1)].push(*value);
    }
    let heights: Vec<Option<f64>> = slices
        .iter()
        .map(|values| (!values.is_empty()).then(|| reduce(values)))
        .collect();
    let top = heights.iter().flatten().copied().fold(0.0, f64::max);
    heights
        .iter()
        .map(|height| match height {
            None => ' ',
            Some(_) if top <= 0.0 => BLOCKS[0],
            Some(height) => {
                let level = (height / top * (BLOCKS.len() - 1) as f64).round() as usize;
                BLOCKS[level.min(BLOCKS.len() - 1)]
            }
        })
        .collect()
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
    use crate::model::{AttrValue, Refusal};
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

    fn tiles_of(data: &Data) -> Vec<Tile> {
        let focus = Focus::default();
        tiles(&Ctx {
            data,
            against: None,
            focus: &focus,
        })
    }

    fn tile<'a>(tiles: &'a [Tile], name: &str) -> &'a Tile {
        tiles
            .iter()
            .find(|tile| tile.name == name)
            .unwrap_or_else(|| panic!("no {name} tile"))
    }

    fn all_text(tile: &Tile) -> String {
        let mut text = tile.text.clone();
        for line in &tile.detail {
            text.push('\n');
            for span in &line.spans {
                text.push_str(&span.content);
            }
        }
        text
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

    /// Each tile against the workload's manifest, within the trust it states.
    #[test]
    fn the_tiles_find_what_the_workload_did() {
        let data = recording();
        let truth = manifest();
        let tiles = tiles_of(&data);
        assert_eq!(tiles.len(), 9);

        // cpu: `crunch` is the hottest code on cpu -- four times `tidy`.
        let cpu = tile(&tiles, "cpu");
        let hot = truth["cpu"]["function"].as_str().unwrap();
        assert!(
            cpu.text.starts_with(&format!("{hot} › ")),
            "{}",
            all_text(cpu)
        );

        // cost: the resident peak is at least the bytes the workload holds.
        let rss = peak(&on_target(&data, "mem.rss")).expect("an rss");
        assert!(
            rss >= truth["memory"]["bytes"].as_f64().unwrap(),
            "rss peak {rss}"
        );
        assert!(tile(&tiles, "cost").text.starts_with("cpu "));

        // flows: the socket bytes, exactly, as the system calls moved them.
        let flows = tile(&tiles, "flows");
        let sent = units::bytes(truth["socket"]["sent"].as_f64().unwrap());
        let received = units::bytes(truth["socket"]["received"].as_f64().unwrap());
        assert_eq!(flows.text, format!("net ↓{received} ↑{sent}"));

        // request: python3 declares none, and the tile says so, not red.
        let request = tile(&tiles, "request");
        assert_eq!(request.text, "no request declared");
        let said = all_text(request);
        assert!(
            said.contains("refused: cannot start collecting `net.request`"),
            "{said}"
        );
        assert!(
            said.contains("no USDT probe"),
            "the reason is kept whole: {said}"
        );

        // threads: the main thread, the lock holder, the peer's at least.
        let threads = tile(&tiles, "threads");
        let count: usize = threads.text.split(' ').next().unwrap().parse().unwrap();
        assert!(count >= 2, "{}", threads.text);

        // gpu: the workload launches none, and the tile says so.
        assert_eq!(tile(&tiles, "gpu").text, "no kernel launched");

        // time: the program's bar has a verdict or says why not, and the
        // tile that explains a suspect is the only one marked.
        let marked: Vec<&str> = tiles
            .iter()
            .filter(|tile| tile.suspect.is_some())
            .map(|tile| tile.name)
            .collect();
        assert!(marked.len() <= 1, "{marked:?}");
        let time = tile(&tiles, "time");
        assert!(
            time.text.starts_with('▶') == (marked.len() == 1),
            "{} {marked:?}",
            time.text
        );
    }

    /// Every tile opens the view its essence came from, and the frame can
    /// come back.
    #[test]
    fn each_tile_opens_its_view_by_key_and_by_click() {
        let data = recording();
        let views: Vec<&str> = tiles_of(&data).iter().map(|tile| tile.view).collect();
        assert_eq!(
            views,
            [
                "timebar", "flame", "ledger", "request", "heatmap", "lanes", "wake", "flows",
                "lanes"
            ]
        );
        for (index, view) in views.iter().enumerate() {
            let mut app = App::new(recording());
            app.cost = Cost::Snapshot;
            snapshot::draw_app(&mut app, WIDTH, HEIGHT);
            for _ in 0..index / COLUMNS {
                app.key(KeyEvent::from(KeyCode::Char('j')));
            }
            for _ in 0..index % COLUMNS {
                app.key(KeyEvent::from(KeyCode::Char('l')));
            }
            app.key(KeyEvent::from(KeyCode::Enter));
            assert_eq!(name(&app), *view, "tile {index} by keys");
            app.key(KeyEvent::from(KeyCode::Backspace));
            assert_eq!(name(&app), "process", "back from {view}");
        }
        // By the mouse: a click selects the tile, a second goes in.
        let mut app = App::new(recording());
        app.cost = Cost::Snapshot;
        let buffer = snapshot::draw_app(&mut app, WIDTH, HEIGHT);
        let (column, row) = snapshot::find(&buffer, " wake → wake").expect("the wake tile");
        snapshot::click(&mut app, column + 1, row + 1);
        snapshot::draw_app(&mut app, WIDTH, HEIGHT);
        snapshot::click(&mut app, column + 1, row + 1);
        assert_eq!(name(&app), "wake");
        snapshot::right_click(&mut app, 0, 0);
        assert_eq!(name(&app), "process");
    }

    /// A run without the sources says so per tile, a refusal with its
    /// reason; nothing is drawn as a zero.
    #[test]
    fn a_tile_without_its_sources_says_so_and_a_refusal_says_why() {
        let mut data = Data::default();
        data.refused.push(Refusal {
            label: "sched.wakeup".to_owned(),
            source: "sched.wakeup".to_owned(),
            reason: "needs CAP_BPF".to_owned(),
        });
        let tiles = tiles_of(&data);
        let wake = tile(&tiles, "wake");
        assert_eq!(wake.text, "sched.wakeup refused");
        assert!(all_text(wake).contains("needs CAP_BPF"));
        for tile in tiles.iter().filter(|tile| tile.name != "wake") {
            assert_eq!(tile.text, "not in this run", "{}", tile.name);
            assert!(tile.suspect.is_none());
        }
    }

    #[test]
    fn a_long_reason_is_wrapped_not_cut() {
        let line = dim(
            "net.request refused: cannot start collector: python3 carries no USDT probe".to_owned(),
        );
        let rows = wrap(&line, 20);
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|row| row.width() <= 20));
        let joined: Vec<String> = rows
            .iter()
            .map(|row| row.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert_eq!(
            joined.join(" "),
            "net.request refused: cannot start collector: python3 carries no USDT probe"
        );
        assert!(rows.iter().all(|row| row
            .spans
            .iter()
            .all(|span| span.style == line.spans[0].style)));
    }

    #[test]
    fn a_row_cut_short_ends_in_an_ellipsis_within_the_width() {
        let mut line = Line::from(vec![Span::raw("abc "), Span::raw("defgh")]);
        ellipsis(&mut line, 6);
        assert_eq!(line.width(), 6);
        assert_eq!(line.to_string(), "abc d…");
    }

    #[test]
    fn an_empty_tile_is_never_marked() {
        for tile in [
            Tile::new("wake", "wake", &Focus::default()).plain("no wake-up of the target"),
            Tile::new("cpu", "flame", &Focus::default()).plain("no cpu sample"),
        ] {
            assert!(!tile.held, "{}", tile.text);
        }
        let tile = Tile::new("cpu", "flame", &Focus::default())
            .plain("no cpu sample")
            .headline("x".to_owned(), None, &Trust::clean());
        assert!(tile.held);
    }

    #[test]
    fn a_stack_is_charged_to_the_program_s_own_code() {
        let frames = |names: &[&str]| -> Vec<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        };
        assert_eq!(
            charged_to(&frames(&[
                "py::main:/w.py",
                "libpython3.so+0x10",
                "py::crunch:/w.py",
                "libpython3.so+0x20",
                "PyNumber_Remainder",
            ])),
            Some("main › crunch".to_owned())
        );
        assert_eq!(
            charged_to(&frames(&[
                "main",
                "sqlite3_step",
                "libc.so.6+0x99",
                "0x7f00"
            ])),
            Some("main › sqlite3_step".to_owned())
        );
        assert_eq!(charged_to(&frames(&["libc.so.6+0x99", "[unknown]"])), None);
    }

    #[test]
    fn a_request_is_charged_to_its_largest_state_a_held_lock_first() {
        let event = |attrs: &[(&str, u64)]| Event {
            ts_ns: 0,
            ts_err_ns: 0,
            kind: "net.request".to_owned(),
            keys: Vec::new(),
            attrs: attrs
                .iter()
                .map(|(name, value)| crate::model::Attr {
                    name: (*name).to_owned(),
                    value: AttrValue::Uint(*value),
                })
                .collect(),
            collector_id: String::new(),
            seq: 0,
        };
        let (class, share) = request_cause(&event(&[
            ("total_ns", 100),
            ("oncpu_ns", 10),
            ("sleep_ns", 80),
            ("holder_ns", 70),
        ]))
        .unwrap();
        assert_eq!(class, Class::Lock);
        assert!((share - 0.7).abs() < 1e-9);
        let (class, _) = request_cause(&event(&[("total_ns", 100), ("disk_ns", 60)])).unwrap();
        assert_eq!(class, Class::Io);
        assert!(request_cause(&event(&[("oncpu_ns", 5)])).is_none());
    }
}
