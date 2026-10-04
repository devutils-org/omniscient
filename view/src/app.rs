//! The frame every view draws in: the header, the tabs, the help line, the
//! view's own cost, and the keys that move between views and narrow them.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::cost::Meter;
use crate::model::Data;
use crate::trust::{palette, Mark};
use crate::units;
use crate::views::{registry, Action, Ctx, Focus, View};

/// How the view's own cost is shown.
#[derive(Debug, Clone)]
pub enum Cost {
    /// Measured on the drawing thread.
    Measured(Meter),
    /// Not measured: a snapshot, which must draw the same every time.
    Snapshot,
}

/// The frame's state.
pub struct App {
    pub data: Data,
    pub against: Option<Data>,
    pub views: Vec<Box<dyn View>>,
    pub current: usize,
    pub focus: Focus,
    pub help: bool,
    /// Stepping through every view on one run (`--gallery`).
    pub gallery: bool,
    pub cost: Cost,
    pub quit: bool,
}

/// The keys the frame takes before a view sees any.
pub const FRAME_KEYS: &[(&str, &str)] = &[
    ("q", "quit"),
    ("←/→ tab", "view"),
    ("1-9", "jump"),
    ("f", "clear focus"),
    ("?", "help"),
];

impl App {
    pub fn new(data: Data) -> App {
        App {
            data,
            against: None,
            views: registry(),
            current: 0,
            focus: Focus::default(),
            help: false,
            gallery: false,
            cost: Cost::Measured(Meter::start()),
            quit: false,
        }
    }

    /// Opens on the view of this name; an unknown name is refused with the
    /// names there are.
    pub fn open(&mut self, name: &str) -> Result<(), String> {
        match self.views.iter().position(|view| view.name() == name) {
            Some(index) => {
                self.current = index;
                Ok(())
            }
            None => Err(format!(
                "there is no view `{name}`; the views are {}",
                self.names().join(", ")
            )),
        }
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.views.iter().map(|view| view.name()).collect()
    }

    /// Handles one key: the frame's first, the current view's otherwise.
    pub fn key(&mut self, key: KeyEvent) {
        if self.help {
            // Any key closes the help; it does nothing else.
            self.help = false;
            return;
        }
        let count = self.views.len();
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Right | KeyCode::Tab => self.current = (self.current + 1) % count,
            KeyCode::Char(' ') | KeyCode::Char('n') if self.gallery => {
                self.current = (self.current + 1) % count
            }
            KeyCode::Char('p') if self.gallery => self.current = (self.current + count - 1) % count,
            KeyCode::Left | KeyCode::BackTab => self.current = (self.current + count - 1) % count,
            KeyCode::Char(digit @ '1'..='9') => {
                let index = digit as usize - '1' as usize;
                if index < count {
                    self.current = index;
                }
            }
            KeyCode::Char('f') if !self.focus.is_empty() => self.focus = Focus::default(),
            _ => {
                let ctx = Ctx {
                    data: &self.data,
                    against: self.against.as_ref(),
                    focus: &self.focus,
                };
                let action = self.views[self.current].key(key, &ctx);
                self.act(action);
            }
        }
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Focus(focus) => self.focus = focus,
            Action::Goto { view, focus } => {
                self.focus = focus;
                let _ = self.open(view);
            }
        }
    }

    /// Draws the whole screen.
    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let [header, tabs, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.draw_header(frame, header);
        self.draw_tabs(frame, tabs);
        let ctx = Ctx {
            data: &self.data,
            against: self.against.as_ref(),
            focus: &self.focus,
        };
        self.views[self.current].draw(frame, body, &ctx);
        self.draw_footer(frame, footer);
        if self.help {
            self.draw_help(frame, body);
        }
    }

    fn draw_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let span_ns = self.data.closed_ns().saturating_sub(self.data.opened_ns());
        let mut after = vec![Span::raw(" │ "), Span::raw(units::duration(span_ns as f64))];
        if self.data.live.is_some() {
            after.push(Span::raw(" "));
            after.push(Mark::Provisional.badge());
        }
        if self.data.trigger.is_some() {
            after.push(Span::raw(" │ a flight recorder document"));
        }
        if !self.focus.is_empty() {
            after.push(Span::raw(" │ "));
            after.push(Span::styled(
                format!("focus: {}", self.focus.describe()),
                Style::new().fg(palette::ACCENT),
            ));
        }
        // On a narrow terminal the corner says the view's cost alone: the
        // collectors' is in every result, and the frame must stay legible.
        let right = self.cost_line(area.width >= 100);
        let title = "omn top │ ";
        let fixed =
            title.chars().count() + after.iter().map(Span::width).sum::<usize>() + right.width();
        // The origin gives way first, from its start: the end of a path is
        // the part that tells two recordings apart.
        let room = usize::from(area.width).saturating_sub(fixed);
        let origin = ellipsis_start(&self.data.origin, room);
        let mut left = vec![
            Span::styled("omn top", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" │ "),
            Span::raw(origin),
        ];
        left.extend(after);
        let width = right.width() as u16;
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(area);
        frame.render_widget(Paragraph::new(Line::from(left)), left_area);
        frame.render_widget(Paragraph::new(right), right_area);
    }

    /// The corner: the view's cpu, beside the collectors' and the target's,
    /// each as a share of one cpu over the run's wall-clock -- one measure
    /// for the three, so they can be read against each other.
    fn cost_line(&self, with_others: bool) -> Line<'static> {
        let style = Style::new().fg(palette::COST);
        let view = match &self.cost {
            Cost::Measured(meter) => match meter.percent() {
                Some(percent) => format!("view {} cpu", units::percent(percent)),
                None => "view: measuring".to_owned(),
            },
            Cost::Snapshot => "view: snapshot".to_owned(),
        };
        if !with_others {
            return Line::from(vec![Span::styled(view, style), Span::raw(" ")]);
        }
        let mut spans = vec![Span::styled(view, style), Span::raw(" │ ")];
        if self.data.live.is_some() {
            spans.push(Span::styled("collectors and target: at the end", style));
        } else {
            let costs = run_costs(&self.data);
            let share = |ns: u64| {
                if costs.wall_ns == 0 {
                    "?".to_owned()
                } else {
                    units::percent(ns as f64 * 100.0 / costs.wall_ns as f64)
                }
            };
            // What a collector spends inside its target is part of its cost;
            // where some of it is unknown the figure is a lower bound, and is
            // drawn as one: `≥`, dimmed like every figure not to be read at
            // face value.
            let collectors = costs.collectors_cpu_ns + costs.in_target_cpu_ns;
            if costs.in_target_unknown > 0 {
                spans.push(Span::styled("collectors ", style));
                spans.push(Span::styled(
                    format!("≥{}", share(collectors)),
                    style.add_modifier(Modifier::DIM),
                ));
            } else {
                spans.push(Span::styled(
                    format!("collectors {}", share(collectors)),
                    style,
                ));
            }
            match costs.target_cpu_ns {
                Some(target) => spans.push(Span::styled(
                    format!(" │ target {} cpu", share(target)),
                    style,
                )),
                None => spans.push(Span::styled(" cpu", style)),
            }
        }
        spans.push(Span::raw(" "));
        Line::from(spans)
    }

    fn draw_tabs(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut spans = Vec::new();
        if self.gallery {
            spans.push(Span::styled(
                format!("gallery {}/{} ", self.current + 1, self.views.len()),
                Style::new().add_modifier(Modifier::BOLD),
            ));
        }
        for (index, view) in self.views.iter().enumerate() {
            let label = format!(" {} {} ", index + 1, view.name());
            if index == self.current {
                spans.push(Span::styled(
                    label,
                    Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
                ));
            } else {
                spans.push(Span::raw(label));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut spans = Vec::new();
        let mut push = |key: &str, what: &str| {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                key.to_owned(),
                Style::new().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!(" {what}")));
        };
        if self.gallery {
            push("space/p", "next/previous view");
        }
        for (key, what) in FRAME_KEYS {
            push(key, what);
        }
        for (key, what) in self.views[self.current].keys() {
            push(key, what);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_help(&self, frame: &mut Frame<'_>, area: Rect) {
        let [popup] = Layout::horizontal([Constraint::Percentage(80)])
            .flex(Flex::Center)
            .areas(area);
        let [popup] = Layout::vertical([Constraint::Percentage(80)])
            .flex(Flex::Center)
            .areas(popup);
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let mut lines = vec![Line::from(Span::styled("Keys", bold))];
        for (key, what) in FRAME_KEYS
            .iter()
            .chain(self.views[self.current].keys().iter())
        {
            lines.push(Line::from(format!("  {key:<10} {what}")));
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "How far a figure can be trusted",
            bold,
        )));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("dimmed", Style::new().add_modifier(Modifier::DIM)),
            Span::raw("     not to be read at face value; its badge says why"),
        ]));
        for mark in [
            Mark::Lost {
                missing: 12,
                fraction: None,
            },
            Mark::Late { ticks: 3 },
            Mark::TimeError { ns: 2_500 },
            Mark::TimeErrorUnknown,
            Mark::LowDeterminism,
            Mark::Provisional,
            Mark::Degraded { count: 1 },
        ] {
            let badge = mark.badge();
            let pad = 11usize.saturating_sub(badge.width());
            lines.push(Line::from(vec![
                Span::raw("  "),
                badge,
                Span::raw(" ".repeat(pad)),
                Span::raw(mark.sentence()),
            ]));
        }
        lines.push(Line::from(vec![
            Span::raw("  ███▒▒      "),
            Span::raw("an error bar: what is certain solid, what is uncertain shaded"),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("refused", Style::new().fg(palette::REFUSED)),
            Span::raw("    a source that was not served shows why where its panel would be"),
        ]));
        lines.push(Line::default());
        lines.push(Line::from("Any key closes this."));
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::new().borders(Borders::ALL).title(" help ")),
            popup,
        );
    }
}

/// What a finished run cost, as its results say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunCosts {
    /// The cpu every collector spent in this process, summed: each one's
    /// share of `getrusage`. What measuring the cost itself took
    /// (`measurement_ns`) is this process's cpu too, and is already in it.
    pub collectors_cpu_ns: u64,
    /// What the collectors spent inside their targets, where it is known.
    pub in_target_cpu_ns: u64,
    /// How many collectors spent something inside their target that is not
    /// known (an eBPF program without the kernel's run statistics): the
    /// collectors' figure is then a lower bound.
    pub in_target_unknown: usize,
    /// The target's own cpu, when a result measured one: a request on the
    /// target rather than on the machine (`@system`), since a machine-wide
    /// request's is the machine's.
    pub target_cpu_ns: Option<u64>,
    /// The longest any collection watched.
    pub wall_ns: u64,
}

/// The run's costs, from each result's `overhead`.
pub fn run_costs(data: &Data) -> RunCosts {
    let results = &data.run.results;
    RunCosts {
        collectors_cpu_ns: results
            .iter()
            .map(|results| results.overhead.attributed_cpu_ns)
            .sum(),
        in_target_cpu_ns: results
            .iter()
            .filter_map(|results| results.overhead.in_target_cpu_ns)
            .sum(),
        in_target_unknown: results
            .iter()
            .filter(|results| results.overhead.in_target_cpu_ns.is_none())
            .count(),
        target_cpu_ns: results
            .iter()
            .filter(|results| !results.label.ends_with("@system"))
            .filter_map(|results| results.overhead.observed_cpu_ns)
            .max(),
        wall_ns: results
            .iter()
            .map(|results| results.overhead.wall_ns)
            .max()
            .unwrap_or(0),
    }
}

/// `text` in at most `room` cells, its start replaced by `…` when it does
/// not fit.
fn ellipsis_start(text: &str, room: usize) -> String {
    let count = text.chars().count();
    if count <= room {
        return text.to_owned();
    }
    if room == 0 {
        return String::new();
    }
    let kept: String = text.chars().skip(count - (room - 1)).collect();
    format!("…{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_origin_gives_way_from_its_start() {
        assert_eq!(ellipsis_start("replay of /a/b/c", 20), "replay of /a/b/c");
        assert_eq!(ellipsis_start("replay of /a/b/c", 6), "…a/b/c");
        assert_eq!(ellipsis_start("abc", 0), "");
    }
}
