//! The `heatmap` view: latency over time -- time across, latency up, density
//! in colour.
//!
//! A histogram says how a latency is distributed and nothing about when; a
//! timeline of the mean says when and flattens the distribution. This view
//! keeps both axes: each column is a slice of the run, each row a band of
//! latency on a log2 scale, and a cell's glyph and colour say how many
//! measurements fell in it. Two regimes that alternate draw as two bands; a
//! phase change draws as a band that moves; a spike draws as a lone cell far
//! above the rest. Beside it, the whole run's histogram (what a `hist` emit
//! says), above it the count per column, below it where records were lost,
//! and under the cursor the column's own percentiles and its slowest
//! measurement.
//!
//! It reads only `raw` events, so a live run draws exactly as a finished one.
//! A source bucketed and drained in the kernel (`sched.runqlat`,
//! `net.socket`) hands up one row per bucket per window: the row is placed at
//! its bucket's mean (`total_ns` / `weight`, exact when the row stands for one
//! wait) and spread over the window it was counted in, since the kernel does
//! not say when inside the window each wait happened. Where that window lies
//! depends on how the collector stamps the row ([`Stamp`]): `net.socket` at
//! the drain that closed it, `sched.runqlat` where its first wait began. A
//! row kept for the whole run (`io.latency` bucketed in the kernel: a
//! `weight` and no `window_ns`) has no time to place and is said, not drawn.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use ratatui::Frame;

use super::{Action, Ctx, Focus, Need, Region, Scope, View};
use crate::model::{Data, Event, Key, Output, Results};
use crate::trust::{self, palette, Trust};
use crate::units;

/// What this view needs measured.
pub const NEEDS: &[Need] = &[
    Need {
        source: "io.latency",
        mode: "trace",
        emit: &["raw", "hist"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "block request latency, of the requests the target issued",
    },
    Need {
        source: "sched.runqlat",
        mode: "trace",
        emit: &["raw", "summary"],
        keys: &["task"],
        stack: None,
        scope: Scope::Target,
        why: "run queue latency",
    },
    Need {
        source: "net.socket",
        mode: "trace",
        emit: &["raw", "hist"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "socket read and write latency",
    },
    Need {
        source: "gpu.kernel",
        mode: "trace",
        emit: &["raw", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "kernel durations",
    },
];

/// Which source is drawn first when the user has not chosen: the program's
/// own latencies before the scheduler's, which every run has.
const FIRST: &[&str] = &["gpu.kernel", "io.latency", "net.socket", "sched.runqlat"];

/// The density ramp, lowest first: a glyph, so the plain-text snapshot
/// carries it, and a colour, so a terminal shows it at a glance.
const RAMP: [(&str, Color); 6] = [
    ("░", Color::Blue),
    ("▒", Color::Blue),
    ("▒", Color::Cyan),
    ("▓", Color::Cyan),
    ("▓", Color::Yellow),
    ("█", Color::LightRed),
];

/// The count above the map, per column.
const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// The width of the latency labels left of the map.
const LABEL: u16 = 7;
/// The width of the whole run's histogram right of the map.
const MARGIN: u16 = 17;
/// How far the view zooms in time: `2^MAX_ZOOM`.
const MAX_ZOOM: u8 = 6;

#[derive(Default)]
pub struct Heatmap {
    /// The need drawn, by index into [`NEEDS`]; `None` until the user picks.
    chosen: Option<usize>,
    /// The cursor, as a moment of the run (`CLOCK_MONOTONIC`); `None` puts it
    /// on the slowest measurement.
    cursor_ns: Option<u64>,
    /// Zoom in time: the window is the run's span over `2^zoom`, centred on
    /// the cursor.
    zoom: u8,
    /// The width of a column at the last draw, for the cursor keys.
    column_ns: f64,
    /// Where the last drawn window started, for a click on a column.
    window_ns: f64,
    /// The map's columns as last drawn, each its index from the left.
    regions: Vec<Region>,
}

impl View for Heatmap {
    fn name(&self) -> &'static str {
        "heatmap"
    }

    fn title(&self) -> &'static str {
        "latency over time: time across, latency up, density in colour"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("s", "source"),
            ("h/l", "cursor"),
            ("+/-", "zoom"),
            ("t", "narrow to slowest"),
            ("⏎", "open it"),
        ]
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        let chosen = self.selected(ctx.data);
        match key.code {
            KeyCode::Char('s') => self.choose((chosen + 1) % NEEDS.len()),
            KeyCode::Char('S') => self.choose((chosen + NEEDS.len() - 1) % NEEDS.len()),
            KeyCode::Char(step @ ('h' | 'l' | 'H' | 'L')) => {
                let samples = Samples::of(ctx, chosen);
                let (start, end) = run_span(ctx.data);
                let cursor = self
                    .cursor_ns
                    .or_else(|| samples.slowest_ns())
                    .unwrap_or(start) as f64;
                let columns = if step.is_ascii_uppercase() { 8.0 } else { 1.0 };
                let delta = self.column_ns.max(1.0) * columns;
                let moved = if step.eq_ignore_ascii_case(&'h') {
                    cursor - delta
                } else {
                    cursor + delta
                };
                self.cursor_ns = Some(moved.clamp(start as f64, end as f64) as u64);
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.pin_cursor(ctx, chosen);
                self.zoom = (self.zoom + 1).min(MAX_ZOOM);
            }
            KeyCode::Char('-') => self.zoom = self.zoom.saturating_sub(1),
            KeyCode::Char('t') | KeyCode::Enter => {
                self.pin_cursor(ctx, chosen);
                let Some(cursor) = self.cursor_ns else {
                    return Action::None;
                };
                let samples = Samples::of(ctx, chosen);
                let half = (self.column_ns / 2.0).max(1.0);
                let Some(slowest) = samples.slowest_in(cursor as f64 - half, cursor as f64 + half)
                else {
                    return Action::None;
                };
                let focus = Focus {
                    pid: slowest.pid,
                    tid: slowest.tid,
                    request: slowest.request.clone(),
                };
                if focus.is_empty() {
                    return Action::None;
                }
                // A measurement made for a request opens the request's
                // waterfall; any other, its thread's lane.
                let view = if focus.request.is_some() {
                    "request"
                } else {
                    "lanes"
                };
                return if key.code == KeyCode::Enter {
                    Action::Goto { view, focus }
                } else {
                    Action::Focus(focus)
                };
            }
            _ => {}
        }
        Action::None
    }

    fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// The wheel zooms in time, on the cursor, as `+`/`-` do.
    fn wheel(&mut self, down: bool, ctx: &Ctx<'_>) -> Action {
        let code = if down {
            KeyCode::Char('-')
        } else {
            KeyCode::Char('+')
        };
        self.key(KeyEvent::from(code), ctx)
    }

    /// A column clicked puts the cursor on its middle, as `h`/`l` would.
    fn select(&mut self, id: usize, _ctx: &Ctx<'_>) -> Action {
        let at = self.window_ns + (id as f64 + 0.5) * self.column_ns;
        self.cursor_ns = Some(at.max(0.0) as u64);
        Action::None
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        self.regions.clear();
        let chosen = self.selected(ctx.data);
        let need = &NEEDS[chosen];
        let mut title = format!(" heatmap -- {} latency over time ", need.source);
        if !ctx.focus.is_empty() {
            title.push_str(&format!("-- {} ", ctx.focus.describe()));
        }
        let block = Block::new().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 4 || inner.width < 30 {
            return;
        }
        frame.render_widget(
            Paragraph::new(tabs(ctx.data, chosen)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let body = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);

        let results = held(ctx.data, chosen);
        if results.is_empty() {
            match ctx
                .data
                .refused
                .iter()
                .find(|refusal| refusal.label == need.label() || refusal.source == need.source)
            {
                Some(refusal) => trust::render_refusal(frame, body, refusal),
                None => draw_absent(frame, body, ctx, need),
            }
            return;
        }
        let trust = Trust::joined(ctx.data, results.iter().copied());
        let samples = Samples::of(ctx, chosen);
        if samples.points.is_empty() {
            draw_empty(frame, body, ctx, need, &results, &samples, &trust);
            return;
        }
        self.draw_map(frame, body, ctx, need, &results, &samples, &trust);
    }
}

impl Heatmap {
    /// The need drawn: the user's choice, or the first of [`FIRST`] the run
    /// holds events of.
    fn selected(&self, data: &Data) -> usize {
        if let Some(chosen) = self.chosen {
            return chosen;
        }
        let ordered = || {
            FIRST
                .iter()
                .filter_map(|source| NEEDS.iter().position(|need| need.source == *source))
        };
        ordered()
            .find(|&need| {
                held(data, need)
                    .iter()
                    .any(|results| !results.events().is_empty())
            })
            .or_else(|| ordered().find(|&need| !held(data, need).is_empty()))
            .unwrap_or(0)
    }

    fn choose(&mut self, need: usize) {
        self.chosen = Some(need);
        self.cursor_ns = None;
        self.zoom = 0;
    }

    /// Fixes the cursor where it is drawn, so a zoom centres on it.
    fn pin_cursor(&mut self, ctx: &Ctx<'_>, chosen: usize) {
        if self.cursor_ns.is_none() {
            self.cursor_ns = Samples::of(ctx, chosen).slowest_ns();
        }
    }

    /// The span drawn: the whole run, or a `2^zoom` share of it centred on
    /// the cursor.
    fn window(&self, data: &Data, samples: &Samples) -> (f64, f64) {
        let (start, end) = run_span(data);
        let start = start.min(samples.first_ns()) as f64;
        let end = end.max(samples.last_ns()) as f64;
        let span = (end - start).max(1.0);
        if self.zoom == 0 {
            return (start, start + span);
        }
        let width = span / f64::from(1u32 << self.zoom);
        let centre = self
            .cursor_ns
            .or_else(|| samples.slowest_ns())
            .map_or(start + span / 2.0, |cursor| cursor as f64);
        let left = (centre - width / 2.0).clamp(start, start + span - width);
        (left, left + width)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_map(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        ctx: &Ctx<'_>,
        need: &Need,
        results: &[&Results],
        samples: &Samples,
        trust: &Trust,
    ) {
        if area.height < 14 || area.width < 40 {
            let text = format!(
                "{}: too small a screen for the map, which needs 40x14 inside its frame",
                need.source
            );
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
            return;
        }
        let unit = results[0].unit.clone();
        let noun = noun(need.source);
        let dim = Style::new().add_modifier(Modifier::DIM);
        let margin = if area.width >= 80 { MARGIN } else { 0 };
        let cols = area.width.saturating_sub(LABEL + 1 + margin).max(1);
        // Above the map: the figure, the regimes on two lines, the counts.
        // Below it: the axis, its ticks, the losses, the cursor's two lines,
        // the legend.
        let map_rows = area.height.saturating_sub(4 + 7).max(2);
        let window = self.window(ctx.data, samples);
        self.column_ns = (window.1 - window.0) / f64::from(cols);
        let scale = Scale::of(samples, map_rows);
        let grid = Grid::build(samples, &scale, window, cols);
        let cursor_ns = self
            .cursor_ns
            .or_else(|| samples.slowest_ns())
            .map_or(window.0, |cursor| cursor as f64);
        let cursor_col = grid.column_of(cursor_ns);
        let run_start = run_span(ctx.data).0 as f64;

        let x0 = area.x;
        let map_x = x0 + LABEL + 1;
        let mut y = area.y;
        let buf = frame.buffer_mut();

        // The figure: what is drawn, how many, its range, and its trust.
        let all = samples.quantiles(f64::NEG_INFINITY, f64::INFINITY);
        let figure = format!(
            "{}, {} to {}, p50 {}, p99 {}",
            counted(all.weight, noun),
            units::quantity(all.min, &unit),
            units::quantity(all.max, &unit),
            units::quantity(all.p50, &unit),
            units::quantity(all.p99, &unit),
        );
        buf.set_line(x0, y, &trust::figure(figure, trust), area.width);
        y += 1;

        // The regimes: the modes a histogram would show, and when each one
        // happened, which it would not.
        let (regimes, found) = regimes(samples, &scale, &grid, run_start);
        let in_window: f64 = grid.per_row.iter().sum();
        let mut spans = vec![Span::styled(
            if found > regimes.len() {
                format!("{found} regimes, {} shown  ", regimes.len())
            } else if found > 1 {
                format!("{found} regimes  ")
            } else {
                "one regime  ".to_owned()
            },
            Style::new().add_modifier(Modifier::BOLD),
        )];
        for (index, regime) in regimes.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled("  │  ", dim));
            }
            spans.push(Span::styled(
                format!("~{}", units::quantity(regime.median, &unit)),
                Style::new().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(
                    " {} ({}) {}",
                    share(regime.weight / in_window.max(f64::MIN_POSITIVE)),
                    units::count(regime.weight.round()),
                    regime.when
                ),
                trust.style(),
            ));
        }
        Paragraph::new(Line::from(spans))
            .wrap(Wrap { trim: true })
            .render(Rect::new(x0, y, area.width, 2), buf);
        y += 2;

        // The count per column.
        buf.set_string(
            x0,
            y,
            format!("{:>w$}", "count", w = usize::from(LABEL)),
            dim,
        );
        let top = grid.per_col.iter().copied().fold(0.0, f64::max);
        for (col, count) in grid.per_col.iter().enumerate() {
            if *count > 0.0 {
                let level = ((count / top) * 7.0).round().clamp(0.0, 7.0) as usize;
                buf.set_string(map_x + col as u16, y, BARS[level], trust.style());
            }
        }
        if margin > 0 {
            let what = if self.zoom > 0 {
                format!("zoom x{}", 1u32 << self.zoom)
            } else {
                "the whole run".to_owned()
            };
            buf.set_string(map_x + cols + 2, y, what, dim);
        }
        y += 1;

        // The map, its latency axis, and the histogram beside it.
        let map_top = y + (map_rows - scale.rows);
        self.window_ns = window.0;
        self.regions = (0..cols)
            .map(|col| {
                Region::new(
                    Rect::new(map_x + col, map_top, 1, scale.rows.max(1)),
                    usize::from(col),
                )
            })
            .collect();
        let row_top = grid.per_row.iter().copied().fold(0.0, f64::max);
        let levels = Levels::of(grid.max_cell());
        let bar_w = margin.saturating_sub(8);
        for row in 0..scale.rows {
            let screen_y = map_top + (scale.rows - 1 - row);
            let label = scale.label(row, &unit);
            if let Some(label) = &label {
                buf.set_string(
                    x0,
                    screen_y,
                    format!("{label:>w$}", w = usize::from(LABEL)),
                    Style::new(),
                );
            }
            buf.set_string(
                x0 + LABEL,
                screen_y,
                if label.is_some() { "┤" } else { "│" },
                dim,
            );
            for col in 0..cols {
                let x = map_x + col;
                if let Some(level) = levels.level(grid.cell(row, col)) {
                    let (glyph, colour) = RAMP[level];
                    buf.set_string(
                        x,
                        screen_y,
                        glyph,
                        Style::new().fg(colour).patch(trust.style()),
                    );
                } else if Some(col) == cursor_col {
                    buf.set_string(x, screen_y, "│", Style::new().fg(Color::DarkGray));
                } else if label.is_some() && col % 4 == 0 {
                    buf.set_string(x, screen_y, "·", Style::new().fg(Color::DarkGray));
                }
            }
            let count = grid.per_row[usize::from(row)];
            if margin > 0 && count > 0.0 {
                let cells = (count / row_top * f64::from(bar_w)).round() as usize;
                let bar = if cells == 0 {
                    "▏".to_owned()
                } else {
                    "█".repeat(cells)
                };
                buf.set_string(map_x + cols + 2, screen_y, &bar, trust.style());
                buf.set_string(
                    map_x + cols + 2 + bar_w,
                    screen_y,
                    format!("{:>6}", units::count(count.round().max(1.0))),
                    trust.style(),
                );
            }
        }
        y += map_rows;

        // The time axis, its ticks, and the cursor on it.
        buf.set_string(x0 + LABEL, y, "└", dim);
        buf.set_string(map_x, y, "─".repeat(usize::from(cols)), dim);
        let step = nice_step((window.1 - window.0) / (f64::from(cols) / 12.0));
        let mut tick = ((window.0 - run_start) / step).ceil() * step;
        let mut free_from = map_x;
        while tick + run_start <= window.1 {
            if let Some(col) = grid.column_of(tick + run_start) {
                buf.set_string(map_x + col, y, "┬", dim);
                let text = offset(tick);
                let width = text.chars().count() as u16;
                let at = (map_x + col).saturating_sub(width / 2).max(map_x);
                if at >= free_from && at + width <= map_x + cols + margin {
                    buf.set_string(at, y + 1, &text, dim);
                    free_from = at + width + 1;
                }
            }
            tick += step;
        }
        if let Some(col) = cursor_col {
            buf.set_string(
                map_x + col,
                y,
                "▲",
                Style::new().add_modifier(Modifier::BOLD),
            );
        }
        y += 2;

        // Where records were lost, as far as the run says.
        buf.set_string(
            x0,
            y,
            format!("{:>w$}", "lost", w = usize::from(LABEL)),
            dim,
        );
        let losses = Losses::of(results, samples);
        buf.set_line(
            map_x,
            y,
            &losses.strip(&grid, cols, cols + margin),
            cols + margin,
        );
        y += 2;

        // The cursor's column.
        let (low, high) = grid.column_span(cursor_col.unwrap_or(0));
        let here = samples.quantiles(low, high);
        let mut spans = vec![Span::styled(
            format!(
                "▲ {} to {}   ",
                offset(low - run_start),
                offset(high - run_start)
            ),
            Style::new().add_modifier(Modifier::BOLD),
        )];
        if here.weight > 0.0 {
            spans.push(Span::styled(
                format!(
                    "{}   p50 {}   p99 {}   max {}",
                    counted(here.weight, noun),
                    units::quantity(here.p50, &unit),
                    units::quantity(here.p99, &unit),
                    units::quantity(here.max, &unit),
                ),
                trust.style(),
            ));
        } else {
            spans.push(Span::styled(format!("no {noun} here"), dim));
        }
        buf.set_line(x0, y, &Line::from(spans), area.width);
        y += 1;
        if let Some(slowest) = samples.slowest_in(low, high) {
            let mut text = format!("  its slowest {}", units::quantity(slowest.value, &unit));
            if let Some(what) = &slowest.what {
                text.push_str(&format!(": {what}"));
            }
            if slowest.span_ns > 0.0 {
                text.push_str(&format!(
                    " ({} in a {} kernel window)",
                    if slowest.weight > 1.0 {
                        format!("one of {}", units::count(slowest.weight))
                    } else {
                        "alone".to_owned()
                    },
                    units::duration(slowest.span_ns)
                ));
            }
            buf.set_stringn(x0, y, text, usize::from(area.width), dim);
        }
        y += 1;

        // The legend.
        let mut spans = vec![Span::styled("per cell ", dim)];
        for (level, from) in levels.legend(samples.drained) {
            let (glyph, colour) = RAMP[level];
            spans.push(Span::styled(glyph, Style::new().fg(colour)));
            spans.push(Span::styled(format!("{from} "), dim));
        }
        let mut how = format!(
            "  rows {}/octave, columns {} each",
            scale.per_octave_text(),
            units::duration(self.column_ns)
        );
        if samples.drained {
            how.push_str(&format!(
                ", kernel rows spread over their ~{} window",
                units::duration(samples.typical_span())
            ));
        }
        spans.push(Span::styled(how, dim));
        buf.set_line(x0, y, &Line::from(spans), area.width);
    }
}

/// One tab per need: its name and what the run holds of it.
fn tabs(data: &Data, chosen: usize) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, need) in NEEDS.iter().enumerate() {
        let results = held(data, index);
        let refused = results.is_empty()
            && data
                .refused
                .iter()
                .any(|refusal| refusal.label == need.label() || refusal.source == need.source);
        let what = if refused {
            "refused".to_owned()
        } else if results.is_empty() {
            "not asked".to_owned()
        } else {
            let events: usize = results.iter().map(|results| results.events().len()).sum();
            let measured = Samples::weights(&results);
            if results
                .iter()
                .all(|results| results.output("raw").is_none())
            {
                "no events".to_owned()
            } else if (measured - events as f64).abs() < 0.5 {
                units::count(events as f64)
            } else {
                format!(
                    "{} in {} rows",
                    units::count(measured),
                    units::count(events as f64)
                )
            }
        };
        let style = if index == chosen {
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else if refused {
            Style::new().fg(palette::REFUSED)
        } else if results.iter().all(|results| results.events().is_empty()) {
            Style::new().add_modifier(Modifier::DIM)
        } else {
            Style::new()
        };
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(format!(" {} {what} ", need.source), style));
    }
    Line::from(spans)
}

/// The other needs the run holds events of, for "`s` steps to it".
fn others_with_events(data: &Data, need: &Need) -> Vec<&'static str> {
    NEEDS
        .iter()
        .enumerate()
        .filter(|(index, other)| {
            other.source != need.source
                && held(data, *index)
                    .iter()
                    .any(|results| !results.events().is_empty())
        })
        .map(|(_, other)| other.source)
        .collect()
}

/// A source the run did not ask for: what it would draw, and how to get it.
fn draw_absent(frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>, need: &Need) {
    let others = others_with_events(ctx.data, need);
    let mut lines = vec![
        Line::default(),
        Line::from(vec![
            Span::styled(
                format!("{} is not in this run.", need.source),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" It would draw {}.", need.why)),
        ]),
        Line::default(),
        Line::from(
            "The session that recorded the run did not ask for it. Given no session, `omn top` \
             asks every view's sources, and `omn top --dump-session` prints what it asks.",
        ),
        Line::default(),
    ];
    lines.push(if others.is_empty() {
        Line::from(Span::styled(
            "The run holds no event of any source this view draws: nothing in it has a latency \
             to place along time.",
            Style::new().add_modifier(Modifier::DIM),
        ))
    } else {
        Line::from(format!(
            "This run holds events of {}: `s` steps to it.",
            others.join(", ")
        ))
    });
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// A source served, with no event to place: nothing happened, or it was
/// kept as aggregates only.
fn draw_empty(
    frame: &mut Frame<'_>,
    area: Rect,
    ctx: &Ctx<'_>,
    need: &Need,
    results: &[&Results],
    samples: &Samples,
    trust: &Trust,
) {
    let backends: Vec<&str> = results
        .iter()
        .map(|results| results.backend.as_str())
        .collect();
    let mut lines = vec![
        Line::default(),
        trust::figure(
            format!("{} via {}", need.source, backends.join(", ")),
            trust,
        ),
        Line::default(),
    ];
    let bold = Style::new().add_modifier(Modifier::BOLD);
    if samples.summed > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "Its {} events are each the sum of several measurements, whose own values the \
                 backend does not keep: none has a single latency to place.",
                units::count(samples.summed as f64)
            ),
            bold,
        )));
    } else if samples.whole_run > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "Its {} events are a histogram of the whole run, built in the kernel: the {} \
                 they count hold no time, so there is nothing to place along it.",
                units::count(samples.whole_run as f64),
                counted(samples.whole_run_weight, noun(need.source)),
            ),
            bold,
        )));
    } else if results
        .iter()
        .any(|results| results.output("raw").is_some())
    {
        let (start, end) = run_span(ctx.data);
        let mut text = format!(
            "No {} in the {} the run lasted: there is no latency to draw.",
            noun(need.source),
            units::duration((end - start) as f64)
        );
        if !ctx.focus.is_empty() {
            text = format!(
                "No {} of {}: `f` clears the narrowing.",
                noun(need.source),
                ctx.focus.describe()
            );
        }
        lines.push(Line::from(Span::styled(text, bold)));
    } else {
        lines.push(Line::from(Span::styled(
            "Served without its events: a summary or a histogram holds no time, so there is \
             nothing to place along it.",
            bold,
        )));
        lines.push(Line::from(
            "The session that recorded the run asked no `raw` emit of it. What it kept:",
        ));
        lines.push(Line::default());
        for one in results {
            for output in &one.outputs {
                let said = match output {
                    Output::Summary { series } => series
                        .iter()
                        .map(|series| {
                            format!(
                                "  {:<18} {} measured, mean {}, max {}",
                                series.key,
                                units::count(series.count as f64),
                                units::quantity(series.mean, &one.unit),
                                units::quantity(series.max, &one.unit)
                            )
                        })
                        .collect(),
                    Output::Hist { series, .. } => series
                        .iter()
                        .map(|series| {
                            format!(
                                "  {:<18} {} measured, {} to {}",
                                series.key,
                                units::count(series.count as f64),
                                units::quantity(series.min, &one.unit),
                                units::quantity(series.max, &one.unit)
                            )
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                lines.extend(said.into_iter().map(|text| trust::figure(text, trust)));
            }
        }
    }
    let others = others_with_events(ctx.data, need);
    if !others.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(format!(
            "This run holds events of {}: `s` steps to it.",
            others.join(", ")
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// The results serving one need.
fn held(data: &Data, need: usize) -> Vec<&Results> {
    let need = &NEEDS[need];
    data.for_need(&need.label(), need.source)
}

/// The run's span, on `CLOCK_MONOTONIC`.
fn run_span(data: &Data) -> (u64, u64) {
    let start = data.opened_ns();
    (start, data.closed_ns().max(start + 1))
}

/// A count and its noun, singular for one: `1 block request`.
fn counted(weight: f64, noun: &str) -> String {
    let count = weight.round().max(1.0);
    if count == 1.0 {
        format!("1 {}", noun.strip_suffix('s').unwrap_or(noun))
    } else {
        format!("{} {noun}", units::count(count))
    }
}

/// What one measurement is called, per source.
fn noun(source: &str) -> &'static str {
    match source {
        "io.latency" => "block requests",
        "sched.runqlat" => "run queue waits",
        "net.socket" => "socket calls",
        "gpu.kernel" => "GPU operations",
        _ => "measurements",
    }
}

/// The attribute that holds a measurement's value, per source; the first
/// numeric attribute otherwise, which is where `omn` writes it.
fn value_attr(source: &str) -> &'static str {
    match source {
        "io.latency" => "latency_ns",
        "sched.runqlat" => "wait_ns",
        "net.socket" => "lat_ns",
        "gpu.kernel" => "duration_ns",
        _ => "",
    }
}

/// An offset from the run's start, as the time axis spells it.
fn offset(ns: f64) -> String {
    if ns.abs() < 0.5 {
        "0".to_owned()
    } else {
        units::duration(ns)
    }
}

/// A round step for the time axis: 1, 2 or 5 times a power of ten, at least
/// `rough`.
fn nice_step(rough: f64) -> f64 {
    let rough = rough.max(1.0);
    let power = 10f64.powf(rough.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|factor| power * factor)
        .find(|step| *step >= rough)
        .unwrap_or(power * 10.0)
}

/// One measurement, or one kernel row standing for several.
#[derive(Debug, Clone)]
struct Point {
    /// When, on `CLOCK_MONOTONIC`: a moment, or the start of the window a
    /// kernel row was counted in.
    start: f64,
    /// The window's width; 0 for a moment.
    span_ns: f64,
    value: f64,
    /// How many measurements it stands for.
    weight: f64,
    tid: Option<i32>,
    pid: Option<i32>,
    /// The request it was made for, when the source says.
    request: Option<String>,
    /// What it was, in a few words: the program, the operation, the size.
    what: Option<String>,
}

/// Every measurement of one need that the focus lets through.
#[derive(Default)]
struct Samples {
    points: Vec<Point>,
    /// Events whose value is a sum of measurements, left out.
    summed: u64,
    /// Kernel rows of the whole run, which hold no time, left out; and how
    /// many measurements they stand for.
    whole_run: u64,
    whole_run_weight: f64,
    /// Whether any point is a kernel row spread over its window.
    drained: bool,
}

/// Where a collector stamps a row it drains from the kernel, which is where
/// the row's window is found from its `ts_ns` and `window_ns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stamp {
    /// At the drain that closed the window (`collect::io::histogram`, which
    /// serves `net.socket`): the window is `[ts - window, ts]`.
    Close,
    /// Where the row's first wait began (`collect::sched::runqlat`, finding
    /// 44): the window is from there to the drain's close, found from the
    /// drain's other rows ([`Samples::close_first_waits`]).
    FirstWait,
}

impl Stamp {
    /// How a result's rows are stamped: as the run says (`row_stamp`), and
    /// by the source for a recording made before it said.
    fn of(results: &Results, source: &str) -> Stamp {
        match results.row_stamp.as_deref() {
            Some("first_event") => Stamp::FirstWait,
            Some("drain_close") => Stamp::Close,
            _ => match source {
                "sched.runqlat" => Stamp::FirstWait,
                _ => Stamp::Close,
            },
        }
    }
}

impl Samples {
    fn of(ctx: &Ctx<'_>, need: usize) -> Samples {
        let attr = value_attr(NEEDS[need].source);
        let mut samples = Samples::default();
        // The rows stamped at their first wait, as (result, window, point):
        // a drain's rows share their result and their window to the
        // nanosecond, which is how their common close is found.
        let mut firsts: Vec<(usize, u64, usize)> = Vec::new();
        for (index, results) in held(ctx.data, need).into_iter().enumerate() {
            // The run names the attribute when it says it; the table is for
            // a recording made before it did.
            let stamp = Stamp::of(results, NEEDS[need].source);
            let attr = results
                .value_attr
                .as_deref()
                .filter(|named| !named.is_empty())
                .unwrap_or(attr);
            for event in results.events() {
                if !focused(ctx.focus, event) {
                    continue;
                }
                if event.attr("total_of").is_some() {
                    samples.summed += 1;
                    continue;
                }
                let Some(mut value) = event
                    .number(attr)
                    .or_else(|| event.attrs.first().and_then(|first| first.value.as_f64()))
                else {
                    continue;
                };
                let weight = event.number("weight").filter(|weight| *weight > 0.0);
                let mut start = event.ts_ns as f64;
                let mut span_ns = 0.0;
                if let Some(weight) = weight {
                    // A kernel row: its value is its bucket's low bound, and
                    // `total_ns` the exact sum of the waits behind it, so
                    // their mean is in the bucket and exact for one wait.
                    if let Some(total) = event.number("total_ns") {
                        let mean = total / weight;
                        if (value == 0.0 && mean < 1.0) || (mean >= value && mean < value * 2.0) {
                            value = mean;
                        }
                    }
                    match event.number("window_ns") {
                        Some(window) if window > 0.0 => {
                            span_ns = window;
                            if stamp == Stamp::Close {
                                start -= window;
                            } else {
                                firsts.push((index, window.to_bits(), samples.points.len()));
                            }
                        }
                        Some(_) => {}
                        None => {
                            // A row of the whole run, stamped at its open:
                            // nothing says when any of it happened.
                            samples.whole_run += 1;
                            samples.whole_run_weight += weight;
                            continue;
                        }
                    }
                }
                samples.points.push(Point {
                    start,
                    span_ns,
                    value: value.max(0.0),
                    weight: weight.unwrap_or(1.0),
                    tid: tid_of(event),
                    pid: pid_of(event),
                    request: request_of(event).map(str::to_owned),
                    what: describe(event),
                });
            }
        }
        samples.close_first_waits(&firsts);
        // A row is counted inside the run: a window reaching past either end
        // of it is cut there.
        let (run_start, run_end) = run_span(ctx.data);
        let (run_start, run_end) = (run_start as f64, run_end as f64);
        for point in &mut samples.points {
            if point.span_ns <= 0.0 {
                continue;
            }
            let start = point.start.max(run_start);
            let end = (point.start + point.span_ns).min(run_end);
            if end > start {
                point.start = start;
                point.span_ns = end - start;
            } else {
                point.start = start.min(run_end);
                point.span_ns = 0.0;
            }
        }
        samples.drained = samples.points.iter().any(|point| point.span_ns > 0.0);
        samples
    }

    /// Ends each row stamped at its first wait at its drain's close. The
    /// waits of a drain ended inside `[close - window, close]`, and the first
    /// wait of each row began at its stamp and ended `value` later, so the
    /// close is at most the earliest such end plus the window: the row is
    /// spread from its stamp to there, which holds every wait it counted, and
    /// never past the drain that read it.
    fn close_first_waits(&mut self, firsts: &[(usize, u64, usize)]) {
        let mut closes: Vec<((usize, u64), f64)> = Vec::new();
        for &(results, window, point) in firsts {
            let point = &self.points[point];
            let end = point.start + point.value + f64::from_bits(window);
            match closes.iter_mut().find(|(key, _)| *key == (results, window)) {
                Some((_, close)) => *close = close.min(end),
                None => closes.push(((results, window), end)),
            }
        }
        for &(results, window, index) in firsts {
            let close = closes
                .iter()
                .find(|(key, _)| *key == (results, window))
                .map_or(0.0, |(_, close)| *close);
            let point = &mut self.points[index];
            // Two drains a nanosecond-equal window apart would share a close
            // too early for the later one's rows: never before the row's own
            // first wait ended.
            let close = close.max(point.start + point.value);
            point.span_ns = (close - point.start).max(0.0);
        }
    }

    /// How many measurements a need's events stand for, unfocused.
    fn weights(results: &[&Results]) -> f64 {
        results
            .iter()
            .flat_map(|results| results.events())
            .map(|event| {
                event
                    .number("weight")
                    .filter(|weight| *weight > 0.0)
                    .unwrap_or(1.0)
            })
            .sum()
    }

    fn first_ns(&self) -> u64 {
        self.points
            .iter()
            .map(|point| point.start)
            .fold(f64::INFINITY, f64::min)
            .clamp(0.0, u64::MAX as f64) as u64
    }

    fn last_ns(&self) -> u64 {
        self.points
            .iter()
            .map(|point| point.start + point.span_ns)
            .fold(0.0, f64::max) as u64
    }

    /// Where the slowest measurement is: the middle of its window.
    fn slowest_ns(&self) -> Option<u64> {
        self.slowest_in(f64::NEG_INFINITY, f64::INFINITY)
            .map(|point| (point.start + point.span_ns / 2.0) as u64)
    }

    /// The slowest measurement overlapping `[low, high)`.
    fn slowest_in(&self, low: f64, high: f64) -> Option<&Point> {
        self.points
            .iter()
            .filter(|point| overlap(point, low, high) > 0.0)
            .max_by(|one, two| one.value.total_cmp(&two.value))
    }

    /// The typical window of the kernel rows, for the legend.
    fn typical_span(&self) -> f64 {
        let mut spans: Vec<f64> = self
            .points
            .iter()
            .filter(|point| point.span_ns > 0.0)
            .map(|point| point.span_ns)
            .collect();
        spans.sort_by(f64::total_cmp);
        spans.get(spans.len() / 2).copied().unwrap_or(0.0)
    }

    /// The measurements overlapping `[low, high)`, each weighed by how much
    /// of it falls inside, and their quantiles.
    fn quantiles(&self, low: f64, high: f64) -> Quantiles {
        let mut held: Vec<(f64, f64)> = self
            .points
            .iter()
            .filter_map(|point| {
                let share = overlap(point, low, high);
                (share > 0.0).then_some((point.value, point.weight * share))
            })
            .collect();
        held.sort_by(|one, two| one.0.total_cmp(&two.0));
        quantiles(&held)
    }
}

/// The share of a point inside `[low, high)`: 1 or 0 for a moment, the
/// share of its window for a kernel row.
fn overlap(point: &Point, low: f64, high: f64) -> f64 {
    if point.span_ns <= 0.0 {
        return if point.start >= low && point.start < high {
            1.0
        } else {
            0.0
        };
    }
    let start = point.start.max(low);
    let end = (point.start + point.span_ns).min(high);
    ((end - start) / point.span_ns).max(0.0)
}

fn tid_of(event: &Event) -> Option<i32> {
    event
        .tid()
        .or_else(|| event.number("tid").map(|tid| tid as i32))
}

fn pid_of(event: &Event) -> Option<i32> {
    event
        .pid()
        .or_else(|| event.number("pid").map(|pid| pid as i32))
}

fn request_of(event: &Event) -> Option<&str> {
    event.keys.iter().find_map(|key| match key {
        Key::Req { id } => Some(id.as_str()),
        _ => None,
    })
}

/// Whether an event is inside what the screen is narrowed to.
fn focused(focus: &Focus, event: &Event) -> bool {
    if focus.tid.is_some() && tid_of(event) != focus.tid {
        return false;
    }
    if focus.pid.is_some() && pid_of(event).is_some_and(|pid| Some(pid) != focus.pid) {
        return false;
    }
    if let Some(request) = &focus.request {
        if request_of(event) != Some(request.as_str()) {
            return false;
        }
    }
    true
}

/// A measurement in a few words: the program and thread, and what the
/// source says it was.
fn describe(event: &Event) -> Option<String> {
    let text = |name: &str| event.attr(name).and_then(|value| value.as_str());
    let mut parts = Vec::new();
    if let Some(comm) = text("comm") {
        parts.push(comm.to_owned());
    }
    if let Some(tid) = tid_of(event) {
        parts.push(format!("tid {tid}"));
    }
    if let Some(kernel) = text("kernel") {
        parts.push(kernel.to_owned());
    } else if let Some(op) = text("op") {
        parts.push(op.to_owned());
    }
    if let Some(rwbs) = text("rwbs") {
        parts.push(rwbs.to_owned());
    }
    if let Some(bytes) = event.number("bytes").filter(|bytes| *bytes > 0.0) {
        parts.push(units::bytes(bytes));
    }
    if let Some(dev) = text("dev") {
        parts.push(format!("on {dev}"));
    }
    if let Some(request) = request_of(event) {
        parts.push(format!("request {request}"));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

#[derive(Debug, Clone, Copy, Default)]
struct Quantiles {
    weight: f64,
    min: f64,
    p50: f64,
    p99: f64,
    max: f64,
}

/// Weighed quantiles of `(value, weight)` pairs sorted by value.
fn quantiles(sorted: &[(f64, f64)]) -> Quantiles {
    let weight: f64 = sorted.iter().map(|(_, weight)| weight).sum();
    let (Some(first), Some(last)) = (sorted.first(), sorted.last()) else {
        return Quantiles::default();
    };
    if weight <= 0.0 {
        return Quantiles::default();
    }
    let at = |q: f64| {
        let mut seen = 0.0;
        for (value, held) in sorted {
            seen += held;
            if seen >= q * weight {
                return *value;
            }
        }
        last.0
    };
    Quantiles {
        weight,
        min: first.0,
        p50: at(0.5),
        p99: at(0.99),
        max: last.0,
    }
}

/// The latency axis: log2, whole octaves, and a row of its own for zero.
#[derive(Debug, Clone, Copy)]
struct Scale {
    /// The lowest and highest octave, as powers of two.
    low: i32,
    high: i32,
    /// Rows of the map, the zero row included.
    rows: u16,
    /// Whether the bottom row holds the zeros.
    zero_row: bool,
}

impl Scale {
    fn of(samples: &Samples, available: u16) -> Scale {
        let (min, max) = samples
            .points
            .iter()
            .map(|point| point.value)
            .filter(|value| *value >= 1.0)
            .fold((f64::INFINITY, 0.0f64), |(min, max), value| {
                (min.min(value), max.max(value))
            });
        let zero_row = samples.points.iter().any(|point| point.value < 1.0);
        let (mut low, mut high) = if max >= 1.0 {
            (min.log2().floor() as i32, max.log2().floor() as i32 + 1)
        } else {
            (0, 1)
        };
        let value_rows = available.saturating_sub(u16::from(zero_row)).max(1);
        let octaves = (high - low).max(1) as u16;
        let rows = if octaves <= value_rows {
            // Every row in use: an octave may span a fraction of a row more
            // or less than the next, which the labels follow.
            value_rows
        } else {
            // More octaves than rows: several octaves a row, the span
            // widened to a whole number of rows.
            let per = octaves.div_ceil(value_rows);
            let rows = octaves.div_ceil(per);
            let widened = i32::from(rows * per) - i32::from(octaves);
            low -= widened / 2;
            high += widened - widened / 2;
            rows
        };
        Scale {
            low,
            high,
            rows: rows + u16::from(zero_row),
            zero_row,
        }
    }

    fn value_rows(&self) -> u16 {
        self.rows - u16::from(self.zero_row)
    }

    /// Rows per octave, a fraction when an octave is less than a row.
    fn per_octave(&self) -> f64 {
        f64::from(self.value_rows()) / f64::from((self.high - self.low).max(1))
    }

    fn per_octave_text(&self) -> String {
        let per = self.per_octave();
        if per >= 1.0 && (per - per.round()).abs() < 0.05 {
            format!("{}", per.round())
        } else if per >= 1.0 {
            format!("{per:.1}")
        } else {
            format!("1/{}", (1.0 / per).round())
        }
    }

    /// The row a value falls in, counted from the bottom.
    fn row(&self, value: f64) -> u16 {
        if value < 1.0 {
            return 0;
        }
        let at = ((value.log2() - f64::from(self.low)) * self.per_octave()).floor();
        u16::from(self.zero_row) + (at.max(0.0) as u16).min(self.value_rows() - 1)
    }

    /// The values a row holds, `[low, high)`.
    fn bounds(&self, row: u16) -> (f64, f64) {
        if self.zero_row && row == 0 {
            return (0.0, 1.0);
        }
        let row = f64::from(row - u16::from(self.zero_row));
        let per = self.per_octave();
        (
            2f64.powf(f64::from(self.low) + row / per),
            2f64.powf(f64::from(self.low) + (row + 1.0) / per),
        )
    }

    /// The label a row carries: the power of ten inside it, or, when the
    /// axis spans fewer than two of them, the octave it starts.
    fn label(&self, row: u16, unit: &str) -> Option<String> {
        if self.zero_row && row == 0 {
            return Some("0".to_owned());
        }
        let (low, high) = self.bounds(row);
        let log10_2 = std::f64::consts::LOG10_2;
        let decades = ((f64::from(self.low) * log10_2).ceil() as i32
            ..=(f64::from(self.high) * log10_2).floor() as i32)
            .count();
        if decades >= 2 {
            let power = 10f64.powf(low.log10().ceil());
            (power >= low && power < high).then(|| short(power, unit))
        } else {
            let per = self.per_octave().max(1.0).round() as u16;
            let index = row - u16::from(self.zero_row);
            (index % per == 0).then(|| units::quantity(low, unit))
        }
    }
}

/// A power of ten in as few cells as it takes: `1µs`, `100ms`.
fn short(value: f64, unit: &str) -> String {
    if unit != "ns" {
        return units::quantity(value, unit);
    }
    let (scaled, suffix) = if value >= 1e9 {
        (value / 1e9, "s")
    } else if value >= 1e6 {
        (value / 1e6, "ms")
    } else if value >= 1e3 {
        (value / 1e3, "µs")
    } else {
        (value, "ns")
    };
    format!("{}{suffix}", scaled.round())
}

/// The measurements, counted per cell.
struct Grid {
    cols: u16,
    window: (f64, f64),
    cells: Vec<f64>,
    per_col: Vec<f64>,
    per_row: Vec<f64>,
}

impl Grid {
    fn build(samples: &Samples, scale: &Scale, window: (f64, f64), cols: u16) -> Grid {
        let mut grid = Grid {
            cols,
            window,
            cells: vec![0.0; usize::from(scale.rows) * usize::from(cols)],
            per_col: vec![0.0; usize::from(cols)],
            per_row: vec![0.0; usize::from(scale.rows)],
        };
        let width = (window.1 - window.0) / f64::from(cols);
        for point in &samples.points {
            let row = scale.row(point.value);
            if point.span_ns <= 0.0 {
                if let Some(col) = grid.column_of(point.start) {
                    grid.add(row, col, point.weight);
                }
                continue;
            }
            // A kernel row: its weight spread over the columns its window
            // covers, in proportion.
            let end = point.start + point.span_ns;
            let first = ((point.start.max(window.0) - window.0) / width)
                .floor()
                .max(0.0) as u16;
            let last = ((end.min(window.1) - window.0) / width).ceil().max(0.0) as u16;
            for col in first..last.min(cols) {
                let (low, high) = grid.column_span(col);
                let share = overlap(point, low, high);
                if share > 0.0 {
                    grid.add(row, col, point.weight * share);
                }
            }
        }
        grid
    }

    fn add(&mut self, row: u16, col: u16, weight: f64) {
        self.cells[usize::from(row) * usize::from(self.cols) + usize::from(col)] += weight;
        self.per_col[usize::from(col)] += weight;
        self.per_row[usize::from(row)] += weight;
    }

    fn cell(&self, row: u16, col: u16) -> f64 {
        self.cells[usize::from(row) * usize::from(self.cols) + usize::from(col)]
    }

    fn max_cell(&self) -> f64 {
        self.cells.iter().copied().fold(0.0, f64::max)
    }

    /// The column a moment falls in, if it is in the window.
    fn column_of(&self, ns: f64) -> Option<u16> {
        let (start, end) = self.window;
        if ns < start || ns > end {
            return None;
        }
        let col = ((ns - start) / (end - start) * f64::from(self.cols)).floor() as u16;
        Some(col.min(self.cols - 1))
    }

    /// The moments a column covers, `[low, high)`.
    fn column_span(&self, col: u16) -> (f64, f64) {
        let (start, end) = self.window;
        let width = (end - start) / f64::from(self.cols);
        (
            start + width * f64::from(col),
            start + width * f64::from(col + 1),
        )
    }
}

/// How a cell's count maps onto the ramp: logarithmically, so one
/// measurement shows beside a thousand.
struct Levels {
    max: f64,
}

impl Levels {
    fn of(max: f64) -> Levels {
        Levels { max: max.max(1.0) }
    }

    fn level(&self, value: f64) -> Option<usize> {
        if value <= 0.0 {
            return None;
        }
        let fraction = (1.0 + value).ln() / (1.0 + self.max).ln();
        Some(((fraction * RAMP.len() as f64).ceil() as usize).clamp(1, RAMP.len()) - 1)
    }

    /// The ramp as the legend says it: each level drawn, with the smallest
    /// whole count drawn in it -- a level no whole count reaches is left
    /// out, so a run of a few events per cell does not list six glyphs for
    /// three counts.
    /// A kernel row spread over its window puts fractions of a count in a
    /// cell (`spread`): the lowest glyph then says so.
    fn legend(&self, spread: bool) -> Vec<(usize, String)> {
        let mut legend: Vec<(usize, String)> = Vec::new();
        if spread && self.level(1.0) != Some(0) {
            legend.push((0, "<1".to_owned()));
        }
        let top = self.max.ceil() as u64;
        let mut count = 1u64;
        while count <= top {
            let level = self.level(count as f64).unwrap_or(0);
            if legend.last().is_none_or(|(held, _)| *held != level) {
                legend.push((level, units::count(count as f64)));
            }
            // Counts grow fast at the top: step geometrically past a hundred.
            count = if count < 100 {
                count + 1
            } else {
                count + count / 50
            };
        }
        legend
    }
}

/// One mode of the distribution, and when it was seen.
struct Regime {
    median: f64,
    weight: f64,
    when: String,
}

/// The distribution's modes: groups of rows apart by at least an empty
/// octave, lowest latency first, each with when it was seen; and how many
/// there were. More than three are cut to the slowest and the two heaviest of
/// the rest, so a lone spike far above the others is never the one left out.
fn regimes(samples: &Samples, scale: &Scale, grid: &Grid, run_start: f64) -> (Vec<Regime>, usize) {
    let gap = scale.per_octave().ceil().max(1.0) as u16;
    let mut groups: Vec<(u16, u16)> = Vec::new();
    let mut empty = 0u16;
    for row in 0..scale.rows {
        if grid.per_row[usize::from(row)] > 0.0 {
            match groups.last_mut() {
                Some((_, end)) if empty < gap => *end = row,
                _ => groups.push((row, row)),
            }
            empty = 0;
        } else {
            empty += 1;
        }
    }
    let mut regimes: Vec<(u16, Regime)> = groups
        .iter()
        .map(|&(first, last)| {
            let inside: Vec<&Point> = samples
                .points
                .iter()
                .filter(|point| (first..=last).contains(&scale.row(point.value)))
                .filter(|point| in_window(point, grid.window) > 0.0)
                .collect();
            // Weighed by their share inside the window, as the map counts
            // them, so the shares add up to the map's whole.
            let mut values: Vec<(f64, f64)> = inside
                .iter()
                .map(|point| (point.value, point.weight * in_window(point, grid.window)))
                .collect();
            values.sort_by(|one, two| one.0.total_cmp(&two.0));
            let held = quantiles(&values);
            (
                first,
                Regime {
                    median: held.p50,
                    weight: held.weight,
                    when: when(&inside, grid, run_start),
                },
            )
        })
        .collect();
    let found = regimes.len();
    if found > 3 {
        let slowest = regimes.pop().expect("more than three");
        regimes.sort_by(|one, two| two.1.weight.total_cmp(&one.1.weight));
        regimes.truncate(2);
        regimes.push(slowest);
        regimes.sort_by_key(|(first, _)| *first);
    }
    (
        regimes.into_iter().map(|(_, regime)| regime).collect(),
        found,
    )
}

/// The share of a point inside the drawn window, `[start, end]` as the grid
/// places moments: 1 or 0 for a moment, the share of its span for a row.
fn in_window(point: &Point, window: (f64, f64)) -> f64 {
    if point.span_ns <= 0.0 {
        return if point.start >= window.0 && point.start <= window.1 {
            1.0
        } else {
            0.0
        };
    }
    overlap(point, window.0, window.1)
}

/// When a regime was seen, in a few words: once, all through or between
/// two moments, and whether steadily, in bursts, or every so often.
fn when(points: &[&Point], grid: &Grid, run_start: f64) -> String {
    let start = run_start;
    let first = points
        .iter()
        .map(|point| point.start)
        .fold(f64::INFINITY, f64::min)
        .max(grid.window.0);
    let last = points
        .iter()
        .map(|point| point.start + point.span_ns)
        .fold(0.0, f64::max)
        .min(grid.window.1);
    let count: f64 = points.iter().map(|point| point.weight).sum();
    if count <= 1.5 {
        return format!("once, at {}", offset(first - start));
    }
    let width = (grid.window.1 - grid.window.0).max(1.0);
    let place = if (last - first) / width >= 0.8 {
        "all through".to_owned()
    } else {
        format!("{} to {}", offset(first - start), offset(last - start))
    };
    // How much of the stretch it spans it fills, column by column.
    let mut columns: Vec<u16> = points
        .iter()
        .filter_map(|point| grid.column_of(point.start.max(grid.window.0)))
        .collect();
    columns.sort_unstable();
    columns.dedup();
    let spanned = match (columns.first(), columns.last()) {
        (Some(one), Some(two)) => f64::from(two - one + 1),
        _ => 1.0,
    };
    let filled = columns.len() as f64 / spanned;
    let instants = points.iter().all(|point| point.span_ns <= 0.0);
    if instants && points.len() >= 3 && points.len() as f64 <= spanned / 2.0 {
        let every = (last - first) / (points.len() - 1) as f64;
        format!("every ~{}, {place}", units::duration(every))
    } else if filled < 0.6 && columns.len() >= 3 {
        // Runs of adjacent columns: how many, and how far apart they start.
        let starts: Vec<u16> = columns
            .iter()
            .enumerate()
            .filter(|(index, col)| *index == 0 || columns[index - 1] + 1 != **col)
            .map(|(_, col)| *col)
            .collect();
        let column_ns = (grid.window.1 - grid.window.0) / f64::from(grid.cols);
        let gaps: Vec<u16> = starts.windows(2).map(|pair| pair[1] - pair[0]).collect();
        let (shortest, longest) = (
            gaps.iter().copied().min().unwrap_or(0),
            gaps.iter().copied().max().unwrap_or(0),
        );
        match starts.len() {
            0 | 1 => place,
            2 => format!("in two bursts, {place}"),
            // Bursts at a steady pace: how often.
            _ if longest <= shortest * 2 => {
                let mean = f64::from(starts[starts.len() - 1] - starts[0]) * column_ns
                    / (starts.len() - 1) as f64;
                format!("in bursts every ~{}, {place}", units::duration(mean))
            }
            count => format!("in {count} bursts, {place}"),
        }
    } else {
        place
    }
}

/// A share in as few cells as it reads: `76%`, `0.4%`.
fn share(fraction: f64) -> String {
    let percent = fraction * 100.0;
    if percent >= 9.5 || percent == 0.0 {
        format!("{percent:.0}%")
    } else {
        format!("{percent:.1}%")
    }
}

/// What the run lost, and where in time it can be put.
struct Losses {
    /// Records missing, from no known moment.
    unplaced: u64,
    /// Records a live run let go from its start, before the first one kept.
    evicted: u64,
    evicted_until: f64,
    late: u64,
}

impl Losses {
    fn of(results: &[&Results], samples: &Samples) -> Losses {
        let mut losses = Losses {
            unplaced: 0,
            evicted: 0,
            evicted_until: samples.first_ns() as f64,
            late: 0,
        };
        for results in results {
            losses.evicted += results.losses.evicted;
            losses.unplaced += results
                .losses
                .missing()
                .saturating_sub(results.losses.evicted);
            losses.late += results.losses.late_ticks;
        }
        losses
    }

    /// The strip under the time axis, `width` cells from the map's left
    /// edge (the map's `cols` and the margin beside it): shaded where records
    /// were lost, and said in words, which the shading makes room for when
    /// the margin is too narrow to hold them.
    fn strip(&self, grid: &Grid, cols: u16, width: u16) -> Line<'static> {
        let shade = Style::new().fg(palette::LOSS);
        let late = (self.late > 0).then(|| {
            format!(
                "{} rounds of reads came late: nothing is missing, the drains were further apart \
                 than asked",
                self.late
            )
        });
        if self.unplaced == 0 && self.evicted == 0 {
            let mut text = "nothing lost".to_owned();
            if let Some(late) = late {
                text.push_str(&format!("; {late}"));
            }
            return Line::from(Span::styled(text, Style::new().add_modifier(Modifier::DIM)));
        }
        let mut words = String::new();
        if self.evicted > 0 {
            words.push_str(&format!(" -{} let go", units::count(self.evicted as f64)));
        }
        if self.unplaced > 0 {
            words.push_str(&format!(
                " -{} lost, when unknown",
                units::count(self.unplaced as f64)
            ));
        }
        // The words are the strip's sentence: the shading gives way to them.
        let room = width.saturating_sub(words.chars().count() as u16).min(cols);
        let mut spans = Vec::new();
        if self.evicted > 0 {
            // A live run lets its oldest events go: they were before the
            // first one it kept.
            let until = grid
                .column_of(self.evicted_until.clamp(grid.window.0, grid.window.1))
                .unwrap_or(0)
                .min(room);
            spans.push(Span::styled("▒".repeat(usize::from(until)), shade));
            spans.push(Span::raw(" ".repeat(usize::from(room - until))));
        } else {
            // Nothing says when: shaded lightly across the whole window.
            spans.push(Span::styled("░".repeat(usize::from(room)), shade));
        }
        spans.push(Span::styled(words, shade));
        Line::from(spans)
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Heatmap::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Cost};
    use crate::snapshot::{self, assert_snapshot, fixture, HEIGHT, WIDTH};
    use ratatui::crossterm::event::KeyModifiers;

    fn app(data: Data) -> App {
        let mut app = App::new(data);
        app.cost = Cost::Snapshot;
        app.open("heatmap").expect("the heatmap is registered");
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn need(source: &str) -> usize {
        NEEDS
            .iter()
            .position(|need| need.source == source)
            .expect("a need of the view")
    }

    fn ctx<'a>(data: &'a Data, focus: &'a Focus) -> Ctx<'a> {
        Ctx {
            data,
            against: None,
            focus,
        }
    }

    #[test]
    fn the_program_s_own_latency_is_drawn_before_the_scheduler_s() {
        for (fixture_name, source) in [
            ("sqlite-commits", "io.latency"),
            ("http-server", "net.socket"),
            ("cpython-hot", "sched.runqlat"),
        ] {
            let drawn = snapshot::text(&snapshot::draw_app(
                &mut app(fixture(fixture_name)),
                WIDTH,
                HEIGHT,
            ));
            assert!(
                drawn.contains(&format!("{source} latency over time")),
                "{fixture_name}:\n{drawn}"
            );
        }
    }

    /// The sqlite run commits forty times with a full sync: each commit's
    /// flush takes milliseconds, every other block request tens of
    /// microseconds. The map must find them as two regimes, apart by more
    /// than an order of magnitude, the slow one far rarer.
    #[test]
    fn the_sync_flushes_are_a_regime_apart_from_the_writes() {
        let data = fixture("sqlite-commits");
        let samples = Samples::of(&ctx(&data, &Focus::default()), need("io.latency"));
        let scale = Scale::of(&samples, 26);
        let window = (data.opened_ns() as f64, data.closed_ns() as f64);
        let grid = Grid::build(&samples, &scale, window, 93);
        let (found, _) = regimes(&samples, &scale, &grid, window.0);
        assert!(found.len() >= 2, "one regime only");
        let (fast, slow) = (&found[0], &found[found.len() - 1]);
        assert!(
            slow.median > fast.median * 10.0,
            "the slow regime ({}) is not apart from the fast one ({})",
            slow.median,
            fast.median
        );
        assert!(fast.weight > slow.weight * 5.0);
        // Every measurement is in the map exactly once.
        let placed: f64 = grid.per_row.iter().sum();
        assert!((placed - samples.points.len() as f64).abs() < 1e-6);
    }

    /// A kernel row stands for `weight` waits and is spread over its window:
    /// what the map holds adds up to the waits, not to the rows, and each
    /// row lands in its own bucket.
    #[test]
    fn a_kernel_row_counts_as_the_waits_it_stands_for() {
        let data = fixture("sqlite-commits");
        let samples = Samples::of(&ctx(&data, &Focus::default()), need("sched.runqlat"));
        assert!(samples.drained);
        let waits: f64 = samples.points.iter().map(|point| point.weight).sum();
        assert!(waits > samples.points.len() as f64);
        let scale = Scale::of(&samples, 26);
        let window = (samples.first_ns() as f64, samples.last_ns() as f64);
        let grid = Grid::build(&samples, &scale, window, 93);
        let placed: f64 = grid.per_col.iter().sum();
        assert!(
            (placed - waits).abs() < 1e-6,
            "{placed} of {waits} waits placed"
        );
        for point in &samples.points {
            let (low, high) = scale.bounds(scale.row(point.value));
            assert!(
                point.value >= low && point.value < high,
                "{} drawn in [{low}, {high})",
                point.value
            );
        }
    }

    /// The events of one need, in the order `Samples::of` reads them.
    fn events_of(data: &Data, source: &str) -> Vec<Event> {
        held(data, need(source))
            .iter()
            .flat_map(|results| results.events().iter().cloned())
            .collect()
    }

    /// `net.socket` stamps each row at the drain that closed its window: the
    /// row is drawn over `[ts - window, ts]`, the window it was counted in,
    /// and never past the run's end. On http-server the 807µs call is in the
    /// window that closed at 218.5ms, not the one after.
    #[test]
    fn a_row_stamped_at_its_close_is_drawn_over_the_window_before() {
        let data = fixture("http-server");
        let samples = Samples::of(&ctx(&data, &Focus::default()), need("net.socket"));
        let events = events_of(&data, "net.socket");
        assert_eq!(samples.points.len(), events.len());
        let (run_start, run_end) = run_span(&data);
        for (point, event) in samples.points.iter().zip(&events) {
            let ts = event.ts_ns as f64;
            let window = event.number("window_ns").expect("a drained row");
            assert!(
                point.start >= (ts - window).max(run_start as f64) - 0.5
                    && point.start + point.span_ns <= ts + 0.5,
                "row stamped {ts} over {window} drawn from {} for {}",
                point.start,
                point.span_ns
            );
            assert!(point.start + point.span_ns <= run_end as f64);
        }
        // The slowest call, which the heatmap names: drawn over a window
        // that ends where its row was stamped, not as a moment there.
        let slowest = samples
            .slowest_in(f64::NEG_INFINITY, f64::INFINITY)
            .expect("a slowest call");
        let index = samples
            .points
            .iter()
            .position(|point| std::ptr::eq(point, slowest))
            .unwrap();
        let stamped = events[index].ts_ns as f64;
        let (from, to) = (slowest.start, slowest.start + slowest.span_ns);
        assert!(
            slowest.span_ns > 0.0 && (to - stamped).abs() <= 0.5,
            "the slowest call, stamped {stamped}, drawn {from} to {to}"
        );
    }

    /// `sched.runqlat` stamps each row where its first wait began: the row
    /// is drawn from there to its drain's close, found from the drain's
    /// other rows -- never more than a window, never past the run's end.
    #[test]
    fn a_row_stamped_at_its_first_wait_ends_at_its_drain() {
        for name in ["http-server", "sqlite-commits", "cpython-hot"] {
            let data = fixture(name);
            let samples = Samples::of(&ctx(&data, &Focus::default()), need("sched.runqlat"));
            let events = events_of(&data, "sched.runqlat");
            assert_eq!(samples.points.len(), events.len(), "{name}");
            let (run_start, run_end) = run_span(&data);
            let mut shorter = 0;
            for (point, event) in samples.points.iter().zip(&events) {
                let ts = (event.ts_ns as f64).max(run_start as f64);
                let window = event.number("window_ns").expect("a drained row");
                let end = point.start + point.span_ns;
                assert!(
                    (point.start - ts).abs() < 0.5,
                    "{name}: moved from its stamp"
                );
                assert!(
                    end <= ts + window + point.value + 0.5,
                    "{name}: past a window"
                );
                assert!(end <= run_end as f64, "{name}: past the run");
                if point.span_ns + 0.5 < window {
                    shorter += 1;
                }
            }
            // A row whose first wait began late in its window ends before a
            // whole window has gone by.
            assert!(shorter > 0, "{name}: every row spread over a whole window");
        }
    }

    /// How a row is stamped is the run's to say when it says it: the same
    /// `net.socket` rows, said stamped at their first event, are drawn from
    /// their stamp onwards rather than over the window before it.
    #[test]
    fn a_result_s_own_row_stamp_wins_over_the_table() {
        let mut data = fixture("http-server");
        let before = Samples::of(&ctx(&data, &Focus::default()), need("net.socket"));
        for results in &mut data.run.results {
            if results.source == "net.socket" {
                results.row_stamp = Some("first_event".to_owned());
            }
        }
        let after = Samples::of(&ctx(&data, &Focus::default()), need("net.socket"));
        let events = events_of(&data, "net.socket");
        let (run_start, run_end) = run_span(&data);
        assert_eq!(after.points.len(), events.len());
        let mut moved = 0;
        for ((then, now), event) in before.points.iter().zip(&after.points).zip(&events) {
            // The stop's drain stamps its rows after the run's end, and a
            // row is never drawn past either end of the run.
            let ts = (event.ts_ns as f64).clamp(run_start as f64, run_end as f64);
            assert!((now.start - ts).abs() < 0.5, "drawn from its stamp");
            if (then.start - now.start).abs() > 0.5 {
                moved += 1;
            }
        }
        assert!(moved > 0, "nothing moved: the table still decided");
    }

    /// `io.latency` bucketed in the kernel keeps one row per bucket for the
    /// whole run, stamped at its open: no time to place, so said rather than
    /// drawn as a burst at the start.
    #[test]
    fn a_whole_run_histogram_is_said_not_drawn_at_the_start() {
        let mut data = fixture("sqlite-commits");
        let opened = data.opened_ns();
        let attr = |name: &str, value: u64| crate::model::Attr {
            name: name.to_owned(),
            value: crate::model::AttrValue::Uint(value),
        };
        for results in &mut data.run.results {
            if results.source != "io.latency" {
                continue;
            }
            for output in &mut results.outputs {
                if let Output::Raw { events } = output {
                    *events = [(13u64, 4096u64, 900u64), (23, 4_194_304, 40)]
                        .into_iter()
                        .map(|(bucket, low, count)| Event {
                            ts_ns: opened,
                            ts_err_ns: 0,
                            kind: "io.latency".to_owned(),
                            keys: Vec::new(),
                            attrs: vec![
                                attr("latency_ns", low),
                                attr("count", count),
                                attr("weight", count),
                                attr("total_ns", low * count * 3 / 2),
                                attr("bucket", bucket),
                            ],
                            collector_id: String::new(),
                            seq: bucket,
                        })
                        .collect();
                }
            }
        }
        let samples = Samples::of(&ctx(&data, &Focus::default()), need("io.latency"));
        assert!(samples.points.is_empty());
        assert_eq!(samples.whole_run, 2);
        let drawn = snapshot::text(&snapshot::draw_app(&mut app(data), WIDTH, HEIGHT));
        assert!(
            drawn.contains("histogram of the whole") && drawn.contains("940 block"),
            "{drawn}"
        );
    }

    /// More than three regimes are cut to three, the slowest kept, and the
    /// header says how many there were.
    #[test]
    fn a_lone_spike_is_kept_among_many_regimes() {
        let point = |start: f64, value: f64, weight: f64| Point {
            start,
            span_ns: 0.0,
            value,
            weight,
            tid: None,
            pid: None,
            request: None,
            what: None,
        };
        let mut points = Vec::new();
        for (index, value) in [10.0, 1e3, 1e5, 1e7].into_iter().enumerate() {
            for at in 0..20 {
                points.push(point(f64::from(at) * 10.0, value, 10.0 + index as f64));
            }
        }
        points.push(point(55.0, 1e10, 1.0));
        let samples = Samples {
            points,
            ..Samples::default()
        };
        let scale = Scale::of(&samples, 40);
        let grid = Grid::build(&samples, &scale, (0.0, 200.0), 50);
        let (shown, found) = regimes(&samples, &scale, &grid, 0.0);
        assert_eq!(found, 5);
        assert_eq!(shown.len(), 3);
        assert!(shown[2].median >= 1e10, "the spike was left out");
        let total: f64 = shown.iter().map(|regime| regime.weight).sum();
        assert!(total <= grid.per_row.iter().sum::<f64>() + 1e-6);
    }

    /// Zoomed on a drained source, a regime's share counts only what of its
    /// rows is inside the window: the shares add up to the map, not more.
    #[test]
    fn regime_shares_add_up_to_the_window_when_zoomed() {
        let data = fixture("http-server");
        let samples = Samples::of(&ctx(&data, &Focus::default()), need("sched.runqlat"));
        let scale = Scale::of(&samples, 26);
        let (start, end) = run_span(&data);
        let mid = (start + end) as f64 / 2.0;
        let width = (end - start) as f64 / 16.0;
        let grid = Grid::build(&samples, &scale, (mid - width / 2.0, mid + width / 2.0), 93);
        let (shown, found) = regimes(&samples, &scale, &grid, start as f64);
        assert_eq!(shown.len(), found.min(3));
        let total: f64 = shown.iter().map(|regime| regime.weight).sum();
        let in_window: f64 = grid.per_row.iter().sum();
        assert!(total <= in_window + 1e-6, "{total} of {in_window}");
    }

    /// The loss strip's words survive a narrow screen: the shading gives way.
    #[test]
    fn the_loss_strip_keeps_its_words_without_a_margin() {
        let losses = Losses {
            unplaced: 1234,
            evicted: 56,
            evicted_until: 50.0,
            late: 0,
        };
        let grid = Grid {
            cols: 40,
            window: (0.0, 100.0),
            cells: Vec::new(),
            per_col: Vec::new(),
            per_row: Vec::new(),
        };
        for width in [40u16, 57] {
            let line = losses.strip(&grid, 40, width);
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(text.chars().count() <= usize::from(width), "{text:?}");
            assert!(
                text.ends_with(" -56 let go -1.23k lost, when unknown"),
                "{text:?}"
            );
            assert!(text.starts_with('▒'), "{text:?}");
        }
    }

    #[test]
    fn a_focus_narrows_the_map_to_one_thread() {
        let data = fixture("http-server");
        let unfocused = Focus::default();
        let every = Samples::of(&ctx(&data, &unfocused), need("sched.runqlat"));
        let tid = every.points[0].tid;
        let focus = Focus {
            tid,
            ..Focus::default()
        };
        let narrowed = Samples::of(&ctx(&data, &focus), need("sched.runqlat"));
        assert!(!narrowed.points.is_empty() && narrowed.points.len() < every.points.len());
        assert!(narrowed.points.iter().all(|point| point.tid == tid));
    }

    #[test]
    fn t_narrows_to_the_slowest_and_enter_opens_its_lane() {
        let data = fixture("sqlite-commits");
        let unfocused = Focus::default();
        let ctx = ctx(&data, &unfocused);
        let mut view = Heatmap::default();
        let narrowed = view.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), &ctx);
        let Action::Focus(focus) = narrowed else {
            panic!("t did not narrow: {narrowed:?}");
        };
        assert!(focus.tid.is_some());
        let opened = view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctx);
        assert_eq!(
            opened,
            Action::Goto {
                view: "lanes",
                focus
            }
        );
    }

    /// A block request made for a request of the vertical case opens that
    /// request's waterfall, narrowed to it.
    #[test]
    fn enter_on_a_request_s_measurement_opens_the_request() {
        let data = fixture("vertical-requests");
        let unfocused = Focus::default();
        let ctx = ctx(&data, &unfocused);
        let mut view = Heatmap::default();
        let opened = view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctx);
        let Action::Goto {
            view: "request",
            focus,
        } = opened
        else {
            panic!("enter did not open the request: {opened:?}");
        };
        let request = focus
            .request
            .clone()
            .expect("the slowest names its request");
        let narrowed = Samples::of(&self::ctx(&data, &focus), need("io.latency"));
        assert!(!narrowed.points.is_empty());
        assert!(narrowed
            .points
            .iter()
            .all(|point| point.request.as_deref() == Some(request.as_str())));
    }

    #[test]
    fn the_scale_puts_every_value_in_a_row_of_its_own_bounds() {
        let point = |value: f64| Point {
            start: 0.0,
            span_ns: 0.0,
            value,
            weight: 1.0,
            tid: None,
            pid: None,
            request: None,
            what: None,
        };
        for (values, rows) in [
            (vec![0.0, 3.0, 4.0e6], 26u16),
            (vec![1_000.0, 1_100.0], 26),
            (vec![1.0, 1e12], 8),
        ] {
            let samples = Samples {
                points: values.iter().copied().map(point).collect(),
                ..Samples::default()
            };
            let scale = Scale::of(&samples, rows);
            assert!(scale.rows <= rows, "{} rows of {rows}", scale.rows);
            for value in values {
                let (low, high) = scale.bounds(scale.row(value));
                assert!(value >= low && value < high, "{value} in [{low}, {high})");
            }
        }
    }

    /// Every fixture, every source, at sizes from a phone's terminal to a
    /// wide one, zoomed in and out: the view draws inside the area it is
    /// given and nowhere else. It is drawn into a frame larger than its area
    /// on every side, filled beforehand with a mark, so a cell drawn outside
    /// the area is caught even where the frame would have clipped it.
    #[test]
    fn every_size_draws_inside_its_area() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        const MARK: &str = "#";
        let keys = |code: char| KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE);
        for name in snapshot::fixture_names() {
            let data = fixture(&name);
            let unfocused = Focus::default();
            let ctx = ctx(&data, &unfocused);
            for (width, height) in [(30u16, 8u16), (50, 16), (80, 24), (200, 60)] {
                let area = Rect::new(3, 2, width, height);
                let mut terminal =
                    Terminal::new(TestBackend::new(width + 6, height + 4)).expect("a terminal");
                let mut view = Heatmap::default();
                for _ in 0..NEEDS.len() {
                    view.key(keys('s'), &ctx);
                    for code in ['+', '+', 'l', 'L', 'h', '-'] {
                        view.key(keys(code), &ctx);
                        let frame = terminal
                            .draw(|frame| {
                                let whole = frame.area();
                                let buf = frame.buffer_mut();
                                for y in whole.top()..whole.bottom() {
                                    for x in whole.left()..whole.right() {
                                        buf[(x, y)].set_symbol(MARK);
                                    }
                                }
                                view.draw(frame, area, &ctx);
                            })
                            .expect("a draw");
                        let buf = frame.buffer;
                        for y in buf.area.top()..buf.area.bottom() {
                            for x in buf.area.left()..buf.area.right() {
                                let inside = x >= area.left()
                                    && x < area.right()
                                    && y >= area.top()
                                    && y < area.bottom();
                                assert!(
                                    inside || buf[(x, y)].symbol() == MARK,
                                    "{name} at {width}x{height}: ({x}, {y}) drawn outside \
                                     {area:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// A live run half way draws from what it holds so far, provisional.
    #[test]
    fn a_live_run_draws_what_it_holds_so_far() {
        let data = fixture("sqlite-commits");
        let half = data.opened_ns() + (data.closed_ns() - data.opened_ns()) / 2;
        let mut live = app(crate::feed::as_live_until(&data, half));
        let buffer = snapshot::draw_app(&mut live, WIDTH, HEIGHT);
        assert!(snapshot::text(&buffer).contains("live"));
        assert_snapshot("heatmap--sqlite-commits--live-half-way", &buffer);
    }

    /// The sources and states the gallery's one snapshot per fixture does
    /// not reach.
    #[test]
    fn snapshots_beyond_the_default() {
        let mut runqlat = app(fixture("http-server"));
        press(&mut runqlat, KeyCode::Char('S'));
        assert_snapshot(
            "heatmap--http-server--sched.runqlat",
            &snapshot::draw_app(&mut runqlat, WIDTH, HEIGHT),
        );

        let mut zoomed = app(fixture("sqlite-commits"));
        for _ in 0..3 {
            press(&mut zoomed, KeyCode::Char('+'));
        }
        assert_snapshot(
            "heatmap--sqlite-commits--zoom-x8",
            &snapshot::draw_app(&mut zoomed, WIDTH, HEIGHT),
        );

        let mut gpu = app(fixture("http-server"));
        press(&mut gpu, KeyCode::Char('s'));
        let buffer = snapshot::draw_app(&mut gpu, WIDTH, HEIGHT);
        // Served on this run, by CUPTI, on a target that launched no
        // kernel: said as none, not refused and not left blank.
        let text = snapshot::text(&buffer);
        assert!(!text.contains("gpu.kernel refused"), "{text}");
        assert!(text.contains("No GPU operations"), "{text}");
        assert_snapshot("heatmap--http-server--gpu.kernel", &buffer);
    }
}
