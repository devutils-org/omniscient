//! The frame every view draws in: the header, the tabs, the help line, the
//! view's own cost, and the keys that move between views and narrow them.

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
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
    help_scroll: u16,
    source_guide: bool,
    guide_selected: usize,
    guide_scroll: u16,
    guide_pending_g: bool,
    vim_pending: Option<usize>,
    guide_deep: bool,
    guide_regions: Vec<Region>,
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
    /// Explicit open control in the footer, when the view provides a destination.
    enter_region: Option<Rect>,
    /// Where each tab was drawn, its id the view's index.
    tabs: Vec<Region>,
}

/// The keys the frame takes before a view sees any.
pub const FRAME_KEYS: &[(&str, &str)] = &[
    ("q", "quit"),
    ("←/→", "move in family grid or chart, otherwise view"),
    ("tab", "next view"),
    ("1-9/0", "jump"),
    ("f", "clear focus"),
    ("?", "help"),
];

/// The frame's keys the help lists beside [`FRAME_KEYS`]: the footer has no
/// room for them, and `⌫` is in it whenever there is somewhere to go back to.
pub const MORE_KEYS: &[(&str, &str)] = &[
    ("0", "the tenth view"),
    ("F", "resource families"),
    ("I", "view and source guide"),
    ("gg/G", "first/last in supported lists"),
    ("⌫", "back where enter came from"),
    ("M", "mouse on/off"),
];

/// What the mouse does, for the help.
const MOUSE: &str = "Mouse: click to select, use Enter or the open button to go in; the wheel scrolls \
                     (or zooms where the view zooms), and right click goes back. M returns text selection to the terminal.";

impl App {
    pub fn new(data: Data) -> App {
        let mut app = App {
            data,
            against: None,
            views: registry(),
            current: 0,
            focus: Focus::default(),
            help: false,
            help_scroll: 0,
            source_guide: false,
            guide_selected: 0,
            guide_scroll: 0,
            guide_pending_g: false,
            vim_pending: None,
            guide_deep: false,
            guide_regions: Vec::new(),
            gallery: false,
            cost: Cost::Measured(Meter::start()),
            quit: false,
            history: Vec::new(),
            mouse: true,
            enter_region: None,
            tabs: Vec::new(),
        };
        let _ = app.open("overview");
        app
    }

    /// Opens on the view of this name; an unknown name is refused with the
    /// names there are.
    pub fn open(&mut self, name: &str) -> Result<(), String> {
        match self.views.iter().position(|view| view.name() == name) {
            Some(index) => {
                self.vim_pending = None;
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
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?' | 'q') => self.help = false,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(5),
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(5),
                KeyCode::Home => self.help_scroll = 0,
                _ => {}
            }
            return;
        }
        if self.source_guide {
            let count = self.guide_sources().len() + 1;
            let was_g = self.guide_pending_g;
            self.guide_pending_g = key.code == KeyCode::Char('g');
            match key.code {
                KeyCode::Esc if self.guide_deep => {
                    self.guide_deep = false;
                    self.guide_scroll = 0;
                }
                KeyCode::Esc | KeyCode::Char('I') | KeyCode::Char('q') => self.source_guide = false,
                KeyCode::Char('d') => {
                    self.guide_deep = !self.guide_deep;
                    self.guide_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.guide_selected = (self.guide_selected + 1).min(count.saturating_sub(1));
                    self.guide_scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.guide_selected = self.guide_selected.saturating_sub(1);
                    self.guide_scroll = 0;
                }
                KeyCode::Home | KeyCode::Char('g') if key.code == KeyCode::Home || was_g => {
                    self.guide_selected = 0;
                    self.guide_scroll = 0;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.guide_selected = count.saturating_sub(1);
                    self.guide_scroll = 0;
                }
                KeyCode::PageDown => self.guide_scroll = self.guide_scroll.saturating_add(3),
                KeyCode::PageUp => self.guide_scroll = self.guide_scroll.saturating_sub(3),
                _ => {}
            }
            return;
        }
        let count = if self.gallery { self.views.len() } else { 10 };
        if self.views[self.current].takes_text() {
            // A search being typed: every key is the view's.
            self.pass_on(key);
            return;
        }
        // Views with list semantics opt in; others retain their own g/G keys.
        let jump_view = matches!(
            self.views[self.current].name(),
            "system" | "ledger" | "timebar" | "wake" | "flows" | "lanes"
        );
        if jump_view && key.code == KeyCode::Char('g') {
            if self.vim_pending == Some(self.current) {
                self.vim_pending = None;
                self.pass_on(KeyEvent::from(KeyCode::Home));
            } else {
                self.vim_pending = Some(self.current);
            }
            return;
        }
        self.vim_pending = None;
        if jump_view && key.code == KeyCode::Char('G') {
            self.pass_on(KeyEvent::from(KeyCode::End));
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('I') => {
                self.source_guide = true;
                self.guide_deep = false;
                self.guide_pending_g = false;
                self.guide_selected = 0;
                self.guide_scroll = 0;
            }
            KeyCode::Char('v') if self.current >= 10 => {
                let _ = self.open(if self.data.has_target() {
                    "process"
                } else {
                    "system"
                });
            }
            KeyCode::Char('F') | KeyCode::Char('O') => {
                let _ = self.open("overview");
            }
            KeyCode::Backspace if !self.history.is_empty() => self.back(),
            KeyCode::Char('M') => self.mouse = !self.mouse,
            KeyCode::Char('?') => {
                self.help_scroll = 0;
                self.help = true;
            }
            KeyCode::Right | KeyCode::Left
                if self.views[self.current].name() == "overview" && !self.gallery =>
            {
                self.pass_on(key)
            }
            KeyCode::Right | KeyCode::Left
                if matches!(
                    self.views[self.current].name(),
                    "flame" | "lanes" | "heatmap"
                ) && !self.gallery =>
            {
                let code = if key.code == KeyCode::Left {
                    KeyCode::Char('h')
                } else {
                    KeyCode::Char('l')
                };
                self.pass_on(KeyEvent::new(code, key.modifiers));
            }
            KeyCode::Right | KeyCode::Tab => self.current = (self.current + 1) % count,
            KeyCode::Char(' ') | KeyCode::Char('n') if self.gallery => {
                self.current = (self.current + 1) % count
            }
            KeyCode::Char('p') if self.gallery => {
                self.current = (self.current % count + count - 1) % count
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.current = (self.current % count + count - 1) % count
            }
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
            self.enter_region = None;
        }
    }

    /// Handles one mouse event: a click selects what is under it, a click on
    /// on the open control goes in, the wheel moves as `↑`/`↓` do, a right click
    /// comes back. Every one of these is a key too: nothing is reachable by
    /// the mouse only.
    pub fn mouse(&mut self, event: MouseEvent) {
        if !self.mouse {
            return;
        }
        if self.source_guide {
            match event.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(region) = self
                        .guide_regions
                        .iter()
                        .find(|r| r.contains(event.column, event.row))
                    {
                        self.guide_selected = region.id;
                        self.guide_pending_g = false;
                        self.guide_scroll = 0;
                    }
                }
                MouseEventKind::Down(MouseButton::Right) => self.source_guide = false,
                MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                    let down = event.kind == MouseEventKind::ScrollDown;
                    if self
                        .guide_regions
                        .iter()
                        .any(|r| r.contains(event.column, event.row))
                    {
                        self.guide_selected = if down {
                            (self.guide_selected + 1).min(self.guide_sources().len())
                        } else {
                            self.guide_selected.saturating_sub(1)
                        };
                        self.guide_scroll = 0;
                    } else {
                        self.guide_scroll = if down {
                            self.guide_scroll.saturating_add(3)
                        } else {
                            self.guide_scroll.saturating_sub(3)
                        };
                    }
                }
                _ => {}
            }
            return;
        }
        if self.help {
            match event.kind {
                MouseEventKind::ScrollDown => self.help_scroll = self.help_scroll.saturating_add(3),
                MouseEventKind::ScrollUp => self.help_scroll = self.help_scroll.saturating_sub(3),
                MouseEventKind::Down(MouseButton::Right) => self.help = false,
                _ => {}
            }
            return;
        }
        self.vim_pending = None;
        let (column, row) = (event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self
                    .enter_region
                    .is_some_and(|rect| Region::new(rect, 0).contains(column, row))
                {
                    self.pass_on(KeyEvent::from(KeyCode::Enter));
                    return;
                }
                if let Some(tab) = self.tabs.iter().find(|tab| tab.contains(column, row)) {
                    if tab.id == usize::MAX {
                        let _ = self.open("overview");
                    } else if tab.id == usize::MAX - 1 {
                        self.source_guide = true;
                        self.guide_deep = false;
                        self.guide_pending_g = false;
                        self.guide_selected = 0;
                        self.guide_scroll = 0;
                    } else {
                        self.current = tab.id;
                    }
                    self.enter_region = None;
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
                let ctx = Ctx {
                    data: &self.data,
                    against: self.against.as_ref(),
                    focus: &self.focus,
                };
                let action = self.views[self.current].select(id, &ctx);
                // Selecting a region never changes the view.
                if !matches!(action, Action::Goto { .. }) {
                    self.act(action);
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
        } else if self.source_guide {
            self.draw_source_guide(frame, body);
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
                Style::new().fg(palette::accent()),
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
            Span::styled(
                "omn top",
                Style::new()
                    .fg(crate::theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
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
        let style = Style::new().fg(palette::cost());
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
        let families = if area.width < 55 {
            " [F] "
        } else {
            " [F] Families "
        };
        if area.width > 0 && area.height > 0 {
            self.tabs.push(Region::new(
                Rect::new(area.x, area.y, area.width.min(families.len() as u16), 1),
                usize::MAX,
            ));
        }
        spans.push(Span::styled(
            families,
            Style::new()
                .fg(crate::theme::accent())
                .add_modifier(Modifier::BOLD),
        ));
        let guide = if area.width < 55 {
            " [I] "
        } else {
            " [I] Guide "
        };
        let x = area.x.saturating_add(families.len() as u16);
        let width = area
            .x
            .saturating_add(area.width)
            .saturating_sub(x)
            .min(guide.len() as u16);
        if width > 0 && area.height > 0 {
            self.tabs
                .push(Region::new(Rect::new(x, area.y, width, 1), usize::MAX - 1));
        }
        spans.push(Span::styled(
            guide,
            Style::new()
                .fg(crate::theme::accent())
                .add_modifier(Modifier::BOLD),
        ));
        if self.current >= 10 && !self.gallery && area.width >= 110 {
            spans.push(Span::raw(format!(
                " › {}   |   v views",
                self.views[self.current].title()
            )));
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }
        if area.width < 110 && !self.gallery {
            for index in 0..10 {
                let label = format!(" {}", (index + 1) % 10);
                let x = area
                    .x
                    .saturating_add(spans.iter().map(Span::width).sum::<usize>() as u16);
                let width = area
                    .x
                    .saturating_add(area.width)
                    .saturating_sub(x)
                    .min(label.len() as u16);
                if width > 0 && area.height > 0 {
                    self.tabs
                        .push(Region::new(Rect::new(x, area.y, width, 1), index));
                }
                spans.push(Span::styled(
                    label,
                    if index == self.current {
                        Style::new()
                            .fg(crate::theme::accent())
                            .add_modifier(Modifier::REVERSED | Modifier::BOLD)
                    } else {
                        Style::new()
                    },
                ));
            }
            spans.push(Span::raw(format!("  {}", self.views[self.current].name())));
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }
        for (index, view) in
            self.views
                .iter()
                .enumerate()
                .take(if self.gallery { self.views.len() } else { 10 })
        {
            let label = format!(" {} {}", (index + 1) % 10, view.name());
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
                    Style::new()
                        .fg(crate::theme::accent())
                        .add_modifier(Modifier::REVERSED | Modifier::BOLD),
                ));
            } else {
                spans.push(Span::raw(label));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_footer(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.enter_region = None;
        let ctx = Ctx {
            data: &self.data,
            against: self.against.as_ref(),
            focus: &self.focus,
        };
        let enter_label = if self.views[self.current].takes_text() {
            None
        } else {
            self.views[self.current].enter_label(&ctx)
        };
        let mut spans = Vec::new();
        if let Some(label) = enter_label {
            let text = format!("[Enter] {label}");
            let width = (text.chars().count() as u16).min(area.width);
            if width > 0 && area.height > 0 {
                self.enter_region = Some(Rect::new(area.x, area.y, width, 1));
            }
            spans.push(Span::styled(
                text,
                Style::new()
                    .fg(crate::theme::accent())
                    .add_modifier(Modifier::BOLD),
            ));
        }
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
        push("q", "quit");
        push("F", "families");
        push("I", "guide");
        push("?", "help");
        if let Some((view, _)) = self.history.last() {
            push("⌫", &format!("back to {}", self.views[*view].name()));
        }
        for (key, what) in self.views[self.current].keys() {
            push(key, what);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn guide_sources(&self) -> Vec<&str> {
        let mut sources: Vec<&str> = self.views[self.current]
            .needs()
            .iter()
            .map(|need| need.source)
            .collect();
        if sources.is_empty() {
            sources.extend(
                self.data
                    .run
                    .results
                    .iter()
                    .map(|result| result.source.as_str()),
            );
            sources.extend(
                self.data
                    .refused
                    .iter()
                    .map(|refusal| refusal.source.as_str()),
            );
        }
        let prefix = match self.views[self.current].name() {
            "family-cpu" => Some("cpu."),
            "family-memory" => Some("mem."),
            "family-disk" => Some("io."),
            "family-network" => Some("net."),
            "family-gpu" => Some("gpu."),
            "family-energy" => Some("power."),
            "family-time" => Some("sched."),
            _ => None,
        };
        if let Some(prefix) = prefix {
            sources.retain(|source| source.starts_with(prefix));
        }
        sources.sort_unstable();
        sources.dedup();
        sources
    }

    fn draw_source_guide(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let [popup] = Layout::horizontal([Constraint::Percentage(90)])
            .flex(Flex::Center)
            .areas(area);
        let [popup] = Layout::vertical([Constraint::Percentage(90)])
            .flex(Flex::Center)
            .areas(popup);
        let inner = popup.inner(ratatui::layout::Margin::new(1, 1));
        let (list, divider, detail, stacked) = if inner.width < 60 {
            let [list, divider, detail] = Layout::vertical([
                Constraint::Percentage(35),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(inner);
            (list, divider, detail, true)
        } else {
            let [list, divider, detail] = Layout::horizontal([
                Constraint::Percentage(42),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(inner);
            (list, divider, detail, false)
        };
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(crate::theme::accent()))
                .title(" Guide · ↑↓/jk · gg/G · d · PgUp/Dn · I close "),
            popup,
        );
        let separator = if stacked {
            vec![Line::from("─".repeat(divider.width as usize))]
        } else {
            (0..divider.height).map(|_| Line::from("│")).collect()
        };
        frame.render_widget(
            Paragraph::new(separator).style(Style::new().fg(crate::theme::accent())),
            divider,
        );
        let sources: Vec<String> = self
            .guide_sources()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let selected = self.guide_selected.min(sources.len());
        let capacity = (list.height as usize / 2).max(1);
        let start = selected.saturating_add(1).saturating_sub(capacity);
        let mut regions = Vec::new();
        let mut list_lines: Vec<Line<'_>> = Vec::new();
        let names = std::iter::once("About this view").chain(sources.iter().map(String::as_str));
        for (i, name) in names.enumerate().skip(start).take(capacity) {
            let style = if i == selected {
                Style::new()
                    .fg(crate::theme::accent())
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::new().add_modifier(Modifier::BOLD)
            };
            let description = if i == 0 {
                "Purpose, reading and limits"
            } else {
                crate::views::source_help::short(name)
                    .or_else(|| {
                        self.data
                            .facts
                            .get(name)
                            .map(|fact| fact.summary.as_str())
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or("Description unavailable")
            };
            let y = list.y.saturating_add(list_lines.len() as u16);
            if y < list.bottom() && list.width > 0 {
                regions.push(Region::new(
                    Rect::new(list.x, y, list.width, 2.min(list.bottom() - y)),
                    i,
                ));
            }
            list_lines.push(Line::from(Span::styled(format!(" {}", name), style)));
            list_lines.push(Line::from(Span::styled(
                format!("   {description}"),
                Style::new().add_modifier(Modifier::DIM),
            )));
        }
        frame.render_widget(Paragraph::new(list_lines), list);
        let mut lines = Vec::new();
        if selected == 0 {
            let name = self.views[self.current].name();
            let (purpose, reading, limits) = crate::views::view_help::explanation(name);
            lines.push(Line::from(Span::styled(
                format!("About {name}"),
                Style::new()
                    .fg(crate::theme::accent())
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::default());
            lines.push(Line::from(purpose));
            lines.push(Line::default());
            lines.push(Line::from(format!("How to use it: {reading}")));
            lines.push(Line::default());
            lines.push(Line::from(format!("Limits: {limits}")));
            lines.push(Line::default());
            lines.push(Line::from(
                "Select a source on the left for its reading guide and quality.",
            ));
        } else if let Some(source) = sources.get(selected - 1) {
            lines.push(Line::from(Span::styled(
                source.to_owned(),
                Style::new()
                    .fg(crate::theme::accent())
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::default());
            if let Some((what, reading)) = crate::views::source_help::explanation(source.as_str()) {
                lines.push(Line::from(what));
                lines.push(Line::default());
                lines.push(Line::from(format!("How to read it: {reading}")));
            } else if let Some(fact) = self
                .data
                .facts
                .get(source.as_str())
                .filter(|fact| !fact.summary.is_empty())
            {
                lines.push(Line::from(fact.summary.clone()));
                lines.push(Line::from("No detailed reading guide is available yet."));
            } else {
                lines.push(Line::from("No description is available for this source."));
            }
            lines.push(Line::default());
            for result in self
                .data
                .run
                .results
                .iter()
                .filter(|result| result.source == *source)
            {
                lines.push(Line::from(format!(
                    "{} · unit: {}",
                    result.label,
                    if result.unit.is_empty() {
                        "not specified"
                    } else {
                        &result.unit
                    }
                )));
                let badges = crate::trust::Trust::of(&self.data, result).badges();
                if !badges.is_empty() {
                    let mut spans = vec![Span::raw("Quality: ")];
                    spans.extend(badges);
                    lines.push(Line::from(spans));
                }
            }
            for refusal in self
                .data
                .refused
                .iter()
                .filter(|refusal| refusal.source == *source)
            {
                lines.push(Line::from(format!(
                    "{} refused: {}",
                    refusal.label, refusal.reason
                )));
            }
            lines.push(Line::default());
            lines.push(Line::from("Series group observations by task, device or other key. Last, mean and max describe observations, not necessarily totals."));
        } else {
            lines.push(Line::from("No sources are listed for this view."));
        }
        if self.guide_deep {
            let lesson = if selected == 0 {
                crate::views::lessons::view(self.views[self.current].name())
            } else {
                sources
                    .get(selected - 1)
                    .and_then(|source| crate::views::lessons::source(source))
            };
            lines.clear();
            let name = if selected == 0 {
                self.views[self.current].name()
            } else {
                sources.get(selected - 1).map(String::as_str).unwrap_or("")
            };
            lines.push(Line::from(Span::styled(
                format!("Deep dive · {name}"),
                Style::new()
                    .fg(crate::theme::accent())
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::default());
            if let Some(lesson) = lesson {
                let article = if selected == 0 {
                    crate::views::articles::view(name)
                } else {
                    crate::views::articles::source(name)
                };
                if let Some(paragraphs) = article {
                    lines.push(Line::from(Span::styled(
                        "Understanding the measurement",
                        Style::new().add_modifier(Modifier::BOLD),
                    )));
                    for paragraph in paragraphs {
                        lines.push(Line::from(paragraph));
                        lines.push(Line::default());
                    }
                }
                for (heading, text) in [
                    ("What to look for", lesson.observe),
                    ("Avoid assuming", lesson.beware),
                    ("What to check next", lesson.next),
                ] {
                    lines.push(Line::from(Span::styled(
                        heading,
                        Style::new().add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(text));
                    lines.push(Line::default());
                }
            } else {
                lines.push(Line::from("No deep dive is available for this item yet. Press d to return to its short guide."));
            }
            lines.push(Line::from("d: short guide · PgUp/PgDn: scroll"));
        } else {
            lines.push(Line::default());
            lines.push(Line::from("d: deeper explanation"));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.guide_scroll, 0)),
            detail,
        );
        self.guide_regions = regions;
    }

    fn draw_help(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let [popup] = Layout::horizontal([Constraint::Percentage(90)])
            .flex(Flex::Center)
            .areas(area);
        let [popup] = Layout::vertical([Constraint::Percentage(90)])
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
            Span::styled("refused", Style::new().fg(palette::refused())),
            Span::raw("    a source that was not served shows why where its panel would be"),
        ]));
        lines.push(Line::default());

        let content = Paragraph::new(lines).wrap(Wrap { trim: false });
        let max_scroll = content
            .line_count(popup.width.saturating_sub(2))
            .saturating_sub(popup.height.saturating_sub(2) as usize);
        self.help_scroll = self
            .help_scroll
            .min(max_scroll.min(u16::MAX as usize) as u16);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            content.scroll((self.help_scroll, 0)).block(
                Block::new()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(crate::theme::accent()))
                    .title(" Help · ↑↓/jk · PgUp/Dn · ? close "),
            ),
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

    #[test]
    fn help_scrolls_on_small_terminals_and_closes_like_the_guide() {
        let mut app = app("http-server");
        press(&mut app, KeyCode::Char('?'));
        let initial = crate::snapshot::text(&draw_app(&mut app, 60, 18));
        press(&mut app, KeyCode::PageDown);
        let scrolled = crate::snapshot::text(&draw_app(&mut app, 60, 18));
        assert_ne!(initial, scrolled);
        assert!(app.help);
        press(&mut app, KeyCode::Esc);
        assert!(!app.help);
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
        assert_eq!(current(&app("http-server")), "overview");
        assert_eq!(current(&app("machine")), "overview");
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
    fn distinct_family_and_process_shortcuts_work_from_any_view() {
        let mut app = app("http-server");
        app.open("flame").unwrap();
        press(&mut app, KeyCode::Char('F'));
        assert_eq!(current(&app), "overview");
        press(&mut app, KeyCode::Char('0'));
        assert_eq!(current(&app), "process");
        press(&mut app, KeyCode::Char('F'));
        assert_eq!(current(&app), "overview");
    }

    #[test]
    fn families_button_and_source_guide_work_from_existing_views() {
        let mut app = app("http-server");
        app.open("flame").unwrap();
        app.key(KeyEvent::from(KeyCode::Char('I')));
        let screen = draw_app(&mut app, WIDTH, HEIGHT);
        assert!(find(&screen, "About flame").is_some());
        assert!(find(&screen, "cpu.profile").is_some());
        app.key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(current(&app), "flame");
        let screen = draw_app(&mut app, WIDTH, HEIGHT);
        let (column, row) = find(&screen, "[F] Families").unwrap();
        click(&mut app, column + 1, row);
        assert_eq!(current(&app), "overview");
    }

    #[test]
    fn guide_has_view_and_source_pages_and_a_clickable_button() {
        let mut app = app("http-server");
        app.open("family-memory").unwrap();
        let screen = draw_app(&mut app, WIDTH, HEIGHT);
        let (x, y) = find(&screen, "[I] Guide").unwrap();
        click(&mut app, x + 1, y);
        let screen = draw_app(&mut app, WIDTH, HEIGHT);
        assert!(find(&screen, "About family-memory").is_some());
        app.key(KeyEvent::from(KeyCode::Down));
        let screen = draw_app(&mut app, WIDTH, HEIGHT);
        assert!(find(&screen, "Page faults").is_some());
        app.key(KeyEvent::from(KeyCode::Char('I')));
        assert!(!app.source_guide);
    }

    #[test]
    fn deep_guide_covers_views_sources_and_missing_lessons() {
        let mut app = app("http-server");
        app.open("family-memory").unwrap();
        app.data.run.results.push(Results {
            source: "mem.custom".into(),
            label: "mem.custom".into(),
            ..Results::default()
        });
        press(&mut app, KeyCode::Char('I'));
        press(&mut app, KeyCode::Char('d'));
        let screen = draw_app(&mut app, WIDTH, HEIGHT);
        assert!(find(&screen, "Deep dive · family-memory").is_some());
        assert!(find(&screen, "What to look for").is_some());
        let custom = app
            .guide_sources()
            .iter()
            .position(|name| *name == "mem.custom")
            .unwrap()
            + 1;
        app.guide_selected = custom;
        let screen = draw_app(&mut app, WIDTH, HEIGHT);
        assert!(find(&screen, "No deep dive is available").is_some());
        press(&mut app, KeyCode::Esc);
        assert!(!app.guide_deep && app.source_guide);
        press(&mut app, KeyCode::Esc);
        assert!(!app.source_guide);
    }

    #[test]
    fn lessons_cover_registered_views_and_default_family_sources() {
        for view in crate::views::registry() {
            assert!(
                crate::views::lessons::view(view.name()).is_some(),
                "{}",
                view.name()
            );
            let paragraphs = crate::views::articles::view(view.name())
                .unwrap_or_else(|| panic!("missing article for {}", view.name()));
            assert!(
                paragraphs.iter().map(|p| p.len()).sum::<usize>() > 250,
                "{}",
                view.name()
            );
            for need in view.needs() {
                if crate::views::source_help::explanation(need.source).is_some() {
                    assert!(
                        crate::views::lessons::source(need.source).is_some(),
                        "{}",
                        need.source
                    );
                    let paragraphs =
                        crate::views::articles::source(need.source).expect(need.source);
                    assert!(
                        paragraphs.iter().map(|p| p.len()).sum::<usize>() > 250,
                        "{}",
                        need.source
                    );
                }
            }
        }
    }

    #[test]
    fn guide_lists_descriptions_and_click_selects_source() {
        let mut app = app("http-server");
        app.open("family-memory").unwrap();
        app.key(KeyEvent::from(KeyCode::Char('I')));
        let screen = draw_app(&mut app, 80, 24);
        assert!(find(&screen, "Memory page faults").is_some());
        let (x, y) = find(&screen, "Process memory held in RAM").unwrap();
        click(&mut app, x, y);
        let screen = draw_app(&mut app, 80, 24);
        assert!(find(&screen, "Resident memory:").is_some());
    }

    #[test]
    fn vim_jumps_match_home_end_in_supported_views() {
        for (view, fixture) in [
            ("system", "http-server"),
            ("ledger", "http-server"),
            ("timebar", "workload-timebar"),
            ("wake", "workload-wake"),
            ("flows", "workload-flows"),
            ("lanes", "workload-lanes"),
        ] {
            let mut vim = app(fixture);
            let mut keys = app(fixture);
            vim.open(view).unwrap();
            keys.open(view).unwrap();
            draw_app(&mut vim, WIDTH, HEIGHT);
            draw_app(&mut keys, WIDTH, HEIGHT);
            press(&mut vim, KeyCode::Char('G'));
            press(&mut keys, KeyCode::End);
            assert_eq!(
                draw_app(&mut vim, WIDTH, HEIGHT),
                draw_app(&mut keys, WIDTH, HEIGHT),
                "{view} G"
            );
            press(&mut vim, KeyCode::Char('g'));
            press(&mut vim, KeyCode::Char('g'));
            press(&mut keys, KeyCode::Home);
            assert_eq!(
                draw_app(&mut vim, WIDTH, HEIGHT),
                draw_app(&mut keys, WIDTH, HEIGHT),
                "{view} gg"
            );
        }
    }

    #[test]
    fn arrows_match_hl_in_flame_without_switching_views() {
        let mut arrow = app("cpython-hot");
        let mut vim = app("cpython-hot");
        arrow.open("flame").unwrap();
        vim.open("flame").unwrap();
        draw_app(&mut arrow, WIDTH, HEIGHT);
        draw_app(&mut vim, WIDTH, HEIGHT);
        press(&mut arrow, KeyCode::Right);
        press(&mut vim, KeyCode::Char('l'));
        assert_eq!(current(&arrow), "flame");
        assert_eq!(
            crate::snapshot::text(&draw_app(&mut arrow, WIDTH, HEIGHT)),
            crate::snapshot::text(&draw_app(&mut vim, WIDTH, HEIGHT))
        );
    }

    #[test]
    fn guide_vim_jumps_do_not_leave_guide() {
        let mut app = app("http-server");
        app.open("family-memory").unwrap();
        press(&mut app, KeyCode::Char('I'));
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.guide_selected, app.guide_sources().len());
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.guide_selected, 0);
        assert!(app.source_guide);
    }

    #[test]
    fn compact_nav_keeps_all_ten_views_clickable() {
        for width in [40, 60, 80] {
            let mut app = app("http-server");
            app.open("system").unwrap();
            draw_app(&mut app, width, 24);
            for index in 0..10 {
                let tab = app
                    .tabs
                    .iter()
                    .find(|tab| tab.id == index)
                    .unwrap_or_else(|| panic!("tab {index} missing at {width}"));
                assert!(tab.rect.width > 0);
            }
            let tab = *app.tabs.iter().find(|tab| tab.id == 9).unwrap();
            click(&mut app, tab.rect.x + tab.rect.width - 1, tab.rect.y);
            assert_eq!(current(&app), "process");
        }
    }

    #[test]
    fn open_button_names_destination_and_requires_an_explicit_click() {
        let mut app = app("http-server");
        app.open("process").unwrap();
        let screen = draw_app(&mut app, 80, 24);
        assert!(find(&screen, "[Enter] open flame").is_some());
        let (x, y) = find(&screen, "[Enter] open flame").unwrap();
        click(&mut app, x + 1, y);
        assert_eq!(current(&app), "flame");
    }

    #[test]
    fn guide_has_separate_panes_at_narrow_and_wide_sizes() {
        for width in [40, 55, 80, 120] {
            let mut app = app("http-server");
            app.open("family-memory").unwrap();
            press(&mut app, KeyCode::Char('I'));
            let screen = draw_app(&mut app, width, 24);
            assert!(find(&screen, "About this view").is_some(), "{width}");
            assert!(find(&screen, "About family-memory").is_some(), "{width}");
            assert!(app
                .guide_regions
                .iter()
                .all(|region| region.rect.x + region.rect.width <= width));
        }
    }

    #[test]
    fn compact_flows_and_heatmap_keep_useful_measurements() {
        let mut flows = app("workload-flows");
        flows.open("flows").unwrap();
        let screen = draw_app(&mut flows, 60, 18);
        assert!(find(&screen, "Compact view").is_some());
        assert!(!flows.views[flows.current].regions().is_empty());
        let mut heatmap = app("workload-heatmap");
        heatmap.open("heatmap").unwrap();
        let screen = draw_app(&mut heatmap, 60, 18);
        assert!(find(&screen, "Compact summary").is_some());
        assert!(find(&screen, "p99").is_some());
    }

    #[test]
    fn all_views_draw_on_small_terminals() {
        for (width, height) in [(60, 18), (80, 24)] {
            let mut app = app("http-server");
            for index in 0..app.views.len() {
                app.current = index;
                draw_app(&mut app, width, height);
            }
        }
    }

    #[test]
    fn a_click_on_a_tab_opens_its_view() {
        let mut app = app("machine");
        app.open("system").unwrap();
        let screen = draw_app(&mut app, WIDTH, HEIGHT);
        let (column, row) = find(&screen, "6 heatmap").expect("the tab is drawn");
        click(&mut app, column + 2, row);
        assert_eq!(current(&app), "heatmap");
    }

    #[test]
    fn clicks_select_enter_opens_and_right_click_comes_back() {
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
        assert_eq!(current(&app), "system", "a second click only selects");
        press(&mut app, KeyCode::Enter);
        assert_eq!(current(&app), "ledger", "Enter opens the announced view");
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
        app.open("system").unwrap();
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
