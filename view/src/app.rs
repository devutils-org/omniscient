//! The frame every view draws in: the header, the tabs, the help line, the
//! view's own cost, and the keys that move between views and narrow them.

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::cost::Meter;
use crate::model::{Data, Results};
use crate::trust::{palette, Mark};
use crate::units;
use crate::views::{registry, Action, Ctx, Focus, Region, View};

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
    /// Where each `Goto` came from -- the view and its focus -- most recent
    /// last: `Backspace` and a right click go back one.
    pub history: Vec<(usize, Focus)>,
    /// Whether the mouse is captured. On by default; `M` turns it off, which
    /// gives the terminal its own text selection back.
    pub mouse: bool,
    /// The region last clicked, as (view, id): a click on it again goes in.
    clicked: Option<(usize, usize)>,
    /// Where each tab was drawn, its id the view's index.
    tabs: Vec<Region>,
}

/// The keys the frame takes before a view sees any.
pub const FRAME_KEYS: &[(&str, &str)] = &[
    ("q", "quit"),
    ("←/→ tab", "view"),
    ("1-9", "jump"),
    ("f", "clear focus"),
    ("?", "help"),
];

/// The frame's keys the help lists beside [`FRAME_KEYS`]: the footer has no
/// room for them, and `⌫` is in it whenever there is somewhere to go back to.
pub const MORE_KEYS: &[(&str, &str)] = &[
    ("0", "the tenth view"),
    ("⌫", "back where enter came from"),
    ("M", "mouse on/off"),
];

/// What the mouse does, for the help.
const MOUSE: &str = "Mouse: a click selects, a click on the selection goes in, the wheel scrolls \
                     (or zooms where the view zooms), a right click goes back; M gives the \
                     terminal its own text selection back.";

impl App {
    pub fn new(data: Data) -> App {
        let mut app = App {
            data,
            against: None,
            views: registry(),
            current: 0,
            focus: Focus::default(),
            help: false,
            gallery: false,
            cost: Cost::Measured(Meter::start()),
            quit: false,
            history: Vec::new(),
            mouse: true,
            clicked: None,
            tabs: Vec::new(),
        };
        // The first screen: one process's when the run has a target of its
        // own, the machine's otherwise.
        let first = if app.data.has_target() {
            "process"
        } else {
            "system"
        };
        let _ = app.open(first);
        app
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
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.views[self.current].takes_text() {
            // A search being typed: every key is the view's.
            self.pass_on(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Backspace if !self.history.is_empty() => self.back(),
            KeyCode::Char('M') => self.mouse = !self.mouse,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Right | KeyCode::Tab => self.current = (self.current + 1) % count,
            KeyCode::Char(' ') | KeyCode::Char('n') if self.gallery => {
                self.current = (self.current + 1) % count
            }
            KeyCode::Char('p') if self.gallery => self.current = (self.current + count - 1) % count,
            KeyCode::Left | KeyCode::BackTab => self.current = (self.current + count - 1) % count,
            KeyCode::Char(digit @ '0'..='9') => {
                // `0` is the tenth.
                let index = (digit as usize + 9 - '0' as usize) % 10;
                if index < count {
                    self.current = index;
                }
            }
            KeyCode::Char('f') if !self.focus.is_empty() => self.focus = Focus::default(),
            _ => self.pass_on(key),
        }
    }

    fn pass_on(&mut self, key: KeyEvent) {
        let ctx = Ctx {
            data: &self.data,
            against: self.against.as_ref(),
            focus: &self.focus,
        };
        let action = self.views[self.current].key(key, &ctx);
        self.act(action);
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Focus(focus) => self.focus = focus,
            Action::Goto { view, focus } => {
                let from = (self.current, self.focus.clone());
                if self.open(view).is_ok() {
                    if from != (self.current, focus.clone()) {
                        self.history.push(from);
                    }
                    self.focus = focus;
                }
            }
        }
    }

    /// Back to where the last `Goto` came from: its view and its focus.
    pub fn back(&mut self) {
        if let Some((view, focus)) = self.history.pop() {
            self.current = view;
            self.focus = focus;
            self.clicked = None;
        }
    }

    /// Handles one mouse event: a click selects what is under it, a click on
    /// what is selected goes in, the wheel moves as `↑`/`↓` do, a right click
    /// comes back. Every one of these is a key too: nothing is reachable by
    /// the mouse only.
    pub fn mouse(&mut self, event: MouseEvent) {
        if !self.mouse {
            return;
        }
        if self.help {
            if matches!(event.kind, MouseEventKind::Down(_)) {
                self.help = false;
            }
            return;
        }
        let (column, row) = (event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(tab) = self.tabs.iter().find(|tab| tab.contains(column, row)) {
                    self.current = tab.id;
                    self.clicked = None;
                    return;
                }
                let hit = self.views[self.current]
                    .regions()
                    .iter()
                    .rev()
                    .find(|region| region.contains(column, row))
                    .map(|region| region.id);
                let Some(id) = hit else {
                    return;
                };
                let here = (self.current, id);
                let ctx = Ctx {
                    data: &self.data,
                    against: self.against.as_ref(),
                    focus: &self.focus,
                };
                let action = self.views[self.current].select(id, &ctx);
                if self.clicked == Some(here) {
                    // A click on the selection goes in, as `Enter` does.
                    self.act(action);
                    self.clicked = None;
                    self.pass_on(KeyEvent::from(KeyCode::Enter));
                } else {
                    self.act(action);
                    self.clicked = Some(here);
                }
            }
            MouseEventKind::Down(MouseButton::Right) => self.back(),
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let ctx = Ctx {
                    data: &self.data,
                    against: self.against.as_ref(),
                    focus: &self.focus,
                };
                let down = event.kind == MouseEventKind::ScrollDown;
                let action = self.views[self.current].wheel(down, &ctx);
                self.act(action);
            }
            _ => {}
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
        // One cell is kept between the left side and the corner, so a cut
        // origin never runs the run's length into the cost.
        let fixed = title.chars().count()
            + after.iter().map(Span::width).sum::<usize>()
            + 1
            + right.width();
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
        let [left_area, _, right_area] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(width),
        ])
        .areas(area);
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

    fn draw_tabs(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let mut spans = Vec::new();
        self.tabs.clear();
        if self.gallery {
            spans.push(Span::styled(
                format!("gallery {}/{} ", self.current + 1, self.views.len()),
                Style::new().add_modifier(Modifier::BOLD),
            ));
        }
        for (index, view) in self.views.iter().enumerate() {
            let label = format!(" {} {} ", (index + 1) % 10, view.name());
            let x = area.x + spans.iter().map(Span::width).sum::<usize>() as u16;
            let width = (label.chars().count() as u16).min((area.x + area.width).saturating_sub(x));
            if width > 0 {
                self.tabs.push(Region::new(
                    Rect::new(x, area.y, width, area.height.min(1)),
                    index,
                ));
            }
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
        if let Some((view, _)) = self.history.last() {
            push("⌫", &format!("back to {}", self.views[*view].name()));
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
            .chain(MORE_KEYS.iter())
            .chain(self.views[self.current].keys().iter())
        {
            lines.push(Line::from(format!("  {key:<10} {what}")));
        }
        lines.push(Line::from(format!(
            "  {MOUSE}{}",
            if self.mouse { "" } else { " (off now)" }
        )));
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
        target_cpu_ns: target_cpu_ns(results),
        wall_ns: results
            .iter()
            .map(|results| results.overhead.wall_ns)
            .max()
            .unwrap_or(0),
    }
}

/// The target's own cpu: the most a request on a target observed.
fn target_cpu_ns(results: &[Results]) -> Option<u64> {
    let observed = |system: bool| {
        results
            .iter()
            .filter(|results| metered_on_the_machine(results) == system)
            .filter_map(|results| results.overhead.observed_cpu_ns)
            .max()
    };
    // A recording older than the meter's words: a machine run's task-keyed
    // requests carry the same figure as its `@system` ones.
    observed(false).filter(|_| observed(false) != observed(true))
}

/// Whether a request's own account is the machine's rather than a
/// target's: a request on the machine (`@system`), or one whose run had no
/// target and so measured the cpus (an eBPF `cpu.oncpu_time` over every
/// task, `sched.wakeup` machine-wide), as `omn` says in `observed_from`.
pub fn metered_on_the_machine(results: &Results) -> bool {
    results.label.ends_with("@system")
        || results
            .overhead
            .observed_from
            .starts_with("everything but idle on")
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
    use crate::snapshot::{click, draw_app, find, fixture, right_click, wheel, HEIGHT, WIDTH};

    fn app(name: &str) -> App {
        let mut app = App::new(fixture(name));
        app.cost = Cost::Snapshot;
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.key(KeyEvent::from(code));
    }

    /// A run with no target claims no target's cpu, even where a request on
    /// it is keyed per task: its account is the machine's.
    #[test]
    fn a_run_on_the_machine_claims_no_target() {
        for name in ["machine", "system-wakeup"] {
            assert_eq!(run_costs(&fixture(name)).target_cpu_ns, None, "{name}");
        }
        for name in ["http-server", "cpython-hot"] {
            assert!(run_costs(&fixture(name)).target_cpu_ns.is_some(), "{name}");
        }
    }

    fn current(app: &App) -> &'static str {
        app.views[app.current].name()
    }

    #[test]
    fn the_first_screen_is_the_process_s_with_a_target_and_the_machine_s_without() {
        assert_eq!(current(&app("http-server")), "process");
        assert_eq!(current(&app("machine")), "system");
    }

    #[test]
    fn enter_goes_in_and_backspace_comes_back_with_the_focus_it_had() {
        let mut app = app("http-server");
        app.open("system").unwrap();
        draw_app(&mut app, WIDTH, HEIGHT);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(current(&app), "ledger");
        assert!(
            !app.focus.is_empty(),
            "the process is carried into its ledger"
        );
        press(&mut app, KeyCode::Enter);
        let deeper = current(&app);
        assert_ne!(deeper, "ledger", "a ledger line opens the view behind it");
        press(&mut app, KeyCode::Backspace);
        assert_eq!(current(&app), "ledger");
        press(&mut app, KeyCode::Backspace);
        assert_eq!(current(&app), "system");
        assert!(app.focus.is_empty(), "back where it was, unfocused");
        assert!(app.history.is_empty());
    }

    #[test]
    fn zero_jumps_to_the_tenth_view() {
        let mut app = app("machine");
        press(&mut app, KeyCode::Char('0'));
        assert_eq!(current(&app), "process");
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(current(&app), "system");
    }

    #[test]
    fn a_view_taking_text_gets_every_key_but_ctrl_c() {
        let mut app = app("cpython-hot");
        app.open("flame").unwrap();
        draw_app(&mut app, WIDTH, HEIGHT);
        press(&mut app, KeyCode::Char('/'));
        for c in "q?3f".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(!app.quit && !app.help, "q and ? were typed, not obeyed");
        assert_eq!(current(&app), "flame", "3 was typed, not a jump");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(current(&app), "ledger", "the search is done: 3 jumps again");
        app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn a_click_on_a_tab_opens_its_view() {
        let mut app = app("machine");
        let screen = draw_app(&mut app, WIDTH, HEIGHT);
        let (column, row) = find(&screen, "6 heatmap").expect("the tab is drawn");
        click(&mut app, column + 2, row);
        assert_eq!(current(&app), "heatmap");
    }

    #[test]
    fn a_click_selects_a_second_goes_in_and_a_right_click_comes_back() {
        let mut app = app("http-server");
        app.open("system").unwrap();
        draw_app(&mut app, WIDTH, HEIGHT);
        let regions = app.views[app.current].regions().to_vec();
        assert!(regions.len() >= 2, "the process rows are clickable");
        let second = regions[1];
        click(&mut app, second.rect.x + 2, second.rect.y);
        assert_eq!(current(&app), "system", "one click only selects");
        let mut keyed = self::app("http-server");
        keyed.open("system").unwrap();
        draw_app(&mut keyed, WIDTH, HEIGHT);
        press(&mut keyed, KeyCode::Down);
        press(&mut keyed, KeyCode::Down);
        press(&mut keyed, KeyCode::Enter);
        click(&mut app, second.rect.x + 2, second.rect.y);
        assert_eq!(current(&app), "ledger", "a click on the selection goes in");
        assert_eq!(
            app.focus, keyed.focus,
            "the click selected what the keys do"
        );
        right_click(&mut app, 0, 0);
        assert_eq!(current(&app), "system");
    }

    #[test]
    fn with_the_mouse_off_a_click_does_nothing() {
        let mut app = app("machine");
        let screen = draw_app(&mut app, WIDTH, HEIGHT);
        press(&mut app, KeyCode::Char('M'));
        let (column, row) = find(&screen, "6 heatmap").unwrap();
        click(&mut app, column + 2, row);
        wheel(&mut app, true, 10, 10);
        assert_eq!(current(&app), "system");
    }

    /// Every view reports regions inside the area it drew, on every
    /// fixture: a click can only land on what is on screen.
    #[test]
    fn every_region_is_inside_the_view_s_area() {
        for name in crate::snapshot::fixture_names() {
            let mut app = app(&name);
            for index in 0..app.views.len() {
                app.current = index;
                draw_app(&mut app, WIDTH, HEIGHT);
                for region in app.views[index].regions() {
                    let rect = region.rect;
                    assert!(
                        rect.y >= 2
                            && rect.y + rect.height < HEIGHT
                            && rect.x + rect.width <= WIDTH
                            && rect.width > 0,
                        "{} on {name}: {rect:?} is outside the body",
                        app.views[index].name()
                    );
                }
            }
        }
    }

    /// Each view that draws something to select reports it on a fixture
    /// that has it, and a click on each region is answered without a panic.
    #[test]
    fn every_view_with_a_selection_is_clickable() {
        for (view, fixture) in [
            ("system", "http-server"),
            ("flame", "cpython-hot"),
            ("ledger", "http-server"),
            ("request", "vertical-requests"),
            ("timebar", "sqlite-latency"),
            ("heatmap", "http-server"),
            ("lanes", "http-server"),
            ("wake", "http-server"),
            ("flows", "http-server"),
        ] {
            let mut app = app(fixture);
            app.open(view).unwrap();
            draw_app(&mut app, WIDTH, HEIGHT);
            let regions = app.views[app.current].regions().to_vec();
            assert!(
                !regions.is_empty(),
                "{view} on {fixture} reported no region"
            );
            for region in regions.iter().take(20) {
                app.open(view).unwrap();
                app.history.clear();
                draw_app(&mut app, WIDTH, HEIGHT);
                click(&mut app, region.rect.x, region.rect.y);
                draw_app(&mut app, WIDTH, HEIGHT);
            }
        }
    }

    #[test]
    fn a_long_origin_gives_way_from_its_start() {
        assert_eq!(ellipsis_start("replay of /a/b/c", 20), "replay of /a/b/c");
        assert_eq!(ellipsis_start("replay of /a/b/c", 6), "…a/b/c");
        assert_eq!(ellipsis_start("abc", 0), "");
    }
}
