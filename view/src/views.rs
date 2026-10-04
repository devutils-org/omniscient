//! The views, and the one contract they all draw through.
//!
//! A view is a file under `views/` that implements [`View`]: what it is
//! called, what it needs measured ([`Need`]), how it draws one [`Data`] into
//! a rectangle, and what its keys do. It reads only what it is handed -- it
//! never opens a source, reads a file or starts a program -- and it draws every
//! figure through [`crate::trust`], so a lossy figure looks the same in every
//! view.
//!
//! Every view is registered in [`registry`] from the start, as a stub that
//! lists its needs and whether the run has them; each view's stream replaces
//! the stub's `draw` in its own file and touches nothing else.

use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::model::Data;
use crate::trust::{self, palette, Trust};

pub mod flame;
pub mod flows;
pub mod heatmap;
pub mod lanes;
pub mod ledger;
pub mod process;
pub mod request;
pub mod system;
pub mod timebar;
pub mod wake;

/// What a view is handed to draw.
pub struct Ctx<'a> {
    pub data: &'a Data,
    /// The run to compare against, for `omn top --replay <a> --against <b>`:
    /// a view that draws a difference reads it, the others ignore it.
    pub against: Option<&'a Data>,
    /// What the user has narrowed the screen to: a process, a thread, a
    /// request. Every view honours what it can of it.
    pub focus: &'a Focus,
}

/// What the screen is narrowed to. Set by a view's [`Action::Focus`] or
/// [`Action::Goto`]; cleared by the frame's `f` key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Focus {
    pub pid: Option<i32>,
    pub tid: Option<i32>,
    /// A request's identity, as its `req` key spells it.
    pub request: Option<String>,
}

impl Focus {
    pub fn is_empty(&self) -> bool {
        self.pid.is_none() && self.tid.is_none() && self.request.is_none()
    }

    /// The focus in a few words, for the header.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(pid) = self.pid {
            parts.push(format!("pid {pid}"));
        }
        if let Some(tid) = self.tid {
            parts.push(format!("tid {tid}"));
        }
        if let Some(request) = &self.request {
            parts.push(format!("request {request}"));
        }
        parts.join(", ")
    }
}

/// What a key did, for the frame to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The view handled it, or ignored it.
    None,
    /// Narrow every view to this.
    Focus(Focus),
    /// Open another view, by name, narrowed to this: from a process in the
    /// overview into its ledger, from a request into its waterfall. The frame
    /// remembers where it came from, and `Backspace` (or a right click) goes
    /// back there.
    Goto { view: &'static str, focus: Focus },
}

/// Something a view drew that a click can select: a tile, a frame, a lane, a
/// ledger line, a request, a heatmap cell. `id` is the view's own name for
/// it, handed back to [`View::select`]; `rect` is where it was drawn, in the
/// terminal's cells.
///
/// A click selects what is under it; a click on what is already selected
/// goes in, as `Enter` does. So a view that reports its regions and answers
/// [`View::select`] is clickable with nothing else to write: going in is its
/// `Enter`, and coming back is the frame's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub rect: Rect,
    pub id: usize,
}

impl Region {
    pub fn new(rect: Rect, id: usize) -> Region {
        Region { rect, id }
    }

    /// Whether the cell at `column`, `row` is inside it.
    pub fn contains(&self, column: u16, row: u16) -> bool {
        column >= self.rect.x
            && column < self.rect.x.saturating_add(self.rect.width)
            && row >= self.rect.y
            && row < self.rect.y.saturating_add(self.rect.height)
    }
}

/// Whether a source is measured on the target `omn top` was given, or on the
/// whole machine whatever the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// On the target: the launched program, the pid, or the machine when
    /// none was given.
    Target,
    /// On the machine: an overview's cpu and network panels are the
    /// machine's even beside a launched program.
    System,
}

/// One source a view needs, as `omn top` asks for it live.
///
/// Spelled in the session's own words -- `mode` as `-m` takes it, `emit` as
/// `-e` does -- so `omn` turns it into a request through the same parser as
/// a command line, and the validator refuses it with its reasons if a view
/// asks for something that cannot mean anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Need {
    pub source: &'static str,
    pub mode: &'static str,
    pub emit: &'static [&'static str],
    pub keys: &'static [&'static str],
    /// `fp`, `dwarf`... for a source that takes stacks; `None` otherwise.
    pub stack: Option<&'static str>,
    pub scope: Scope,
    /// What the view draws from it, for `docs/view/README.md` and the stub.
    pub why: &'static str,
}

impl Need {
    /// The label `omn top` gives the request: the source, and `@system` for
    /// one measured on the machine, so two scopes of one source are two
    /// results a view can tell apart.
    pub fn label(&self) -> String {
        match self.scope {
            Scope::Target => self.source.to_owned(),
            Scope::System => format!("{}@system", self.source),
        }
    }
}

/// One view.
pub trait View {
    /// Its name on the command line (`--view <name>`) and in the tabs.
    fn name(&self) -> &'static str;
    /// One line saying what it shows.
    fn title(&self) -> &'static str;
    /// What it needs measured.
    fn needs(&self) -> &'static [Need];
    /// Draws the run into `area`.
    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>);
    /// A key the frame did not take.
    fn key(&mut self, _key: KeyEvent, _ctx: &Ctx<'_>) -> Action {
        Action::None
    }
    /// Its keys, for the help line: pairs of a key and what it does.
    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }
    /// Whether the view is taking text -- a search being typed -- so every
    /// printable key is its own, `q`, `?`, `f`, digits and `Backspace`
    /// included. The frame keeps only `Ctrl-C`.
    fn takes_text(&self) -> bool {
        false
    }
    /// What it drew that a click can select, as last drawn. Topmost last: a
    /// click goes to the last region that holds it.
    fn regions(&self) -> &[Region] {
        &[]
    }
    /// Selects the region `id` reported by [`View::regions`], as the keys
    /// that move the selection would have: the same state, the same action.
    fn select(&mut self, _id: usize, _ctx: &Ctx<'_>) -> Action {
        Action::None
    }
    /// One notch of the wheel: it scrolls as `↑`/`↓` do, unless the view
    /// zooms, where it zooms.
    fn wheel(&mut self, down: bool, ctx: &Ctx<'_>) -> Action {
        let code = if down {
            ratatui::crossterm::event::KeyCode::Down
        } else {
            ratatui::crossterm::event::KeyCode::Up
        };
        self.key(KeyEvent::from(code), ctx)
    }
}

/// Every view, in the order of the tabs.
pub fn registry() -> Vec<Box<dyn View>> {
    vec![
        system::view(),
        flame::view(),
        ledger::view(),
        request::view(),
        timebar::view(),
        heatmap::view(),
        lanes::view(),
        wake::view(),
        flows::view(),
        process::view(),
    ]
}

/// Every need of every view, merged: one per source and scope, with the
/// union of their emits and keys. What `omn top` asks for live when no
/// session is given.
///
/// Two views that ask one source, in one scope, in different modes or with
/// different stacks cannot both be served by one request, and neither is
/// given the other's silently: the conflict is refused, naming both.
pub fn all_needs() -> Result<Vec<MergedNeed>, String> {
    merge_needs(registry().iter().map(|view| (view.name(), view.needs())))
}

/// [`all_needs`], over any views' needs.
pub fn merge_needs<'a>(
    views: impl IntoIterator<Item = (&'static str, &'a [Need])>,
) -> Result<Vec<MergedNeed>, String> {
    let mut merged: Vec<MergedNeed> = Vec::new();
    for (name, needs) in views {
        for need in needs {
            match merged
                .iter_mut()
                .find(|held| held.source == need.source && held.scope == need.scope)
            {
                Some(held) => {
                    if held.mode != need.mode || held.stack != need.stack {
                        return Err(format!(
                            "the views {} and {name} both need `{}`, as `{}`{} and as `{}`{}: \
                             one request cannot be both; give them the same mode and stack, or \
                             ask one of them in a session (`omn top -f`)",
                            held.views.join(", "),
                            held.label,
                            held.mode,
                            held.stack
                                .map(|stack| format!(" with {stack} stacks"))
                                .unwrap_or_default(),
                            need.mode,
                            need.stack
                                .map(|stack| format!(" with {stack} stacks"))
                                .unwrap_or_default(),
                        ));
                    }
                    for emit in need.emit {
                        if !held.emit.contains(emit) {
                            held.emit.push(emit);
                        }
                    }
                    for key in need.keys {
                        if !held.keys.contains(key) {
                            held.keys.push(key);
                        }
                    }
                    held.views.push(name);
                }
                None => merged.push(MergedNeed {
                    source: need.source,
                    mode: need.mode,
                    emit: need.emit.to_vec(),
                    keys: need.keys.to_vec(),
                    stack: need.stack,
                    scope: need.scope,
                    label: need.label(),
                    views: vec![name],
                }),
            }
        }
    }
    Ok(merged)
}

/// A source as `omn top` asks for it: the needs of every view that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedNeed {
    pub source: &'static str,
    pub mode: &'static str,
    pub emit: Vec<&'static str>,
    pub keys: Vec<&'static str>,
    pub stack: Option<&'static str>,
    pub scope: Scope,
    pub label: String,
    pub views: Vec<&'static str>,
}

/// What a view draws until its stream fills it: its title, then each source
/// it needs and what the run holds of it, in the trust style -- served with
/// its badges, refused with the reason, or not in the run at all.
pub fn draw_stub(frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>, view: &dyn View) {
    let block =
        Block::new()
            .borders(Borders::ALL)
            .title(format!(" {} -- {} ", view.name(), view.title()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [head, body] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Not drawn yet. What this view needs, and what this run holds of it:",
            Style::new().add_modifier(Modifier::ITALIC),
        ))),
        head,
    );
    let mut lines = Vec::new();
    for need in view.needs() {
        let label = need.label();
        let held = ctx.data.for_need(&label, need.source);
        let mut line = vec![Span::styled(
            format!("{label:<24} "),
            Style::new().add_modifier(Modifier::BOLD),
        )];
        if let Some(refusal) = ctx
            .data
            .refused
            .iter()
            .find(|refusal| refusal.label == label)
        {
            line.push(Span::styled(
                format!("refused: {}", refusal.reason),
                Style::new().fg(palette::REFUSED),
            ));
        } else if held.is_empty() {
            line.push(Span::styled(
                "not in this run",
                Style::new().add_modifier(Modifier::DIM),
            ));
        } else {
            let trust = Trust::joined(ctx.data, held.iter().copied());
            let events: u64 = held.iter().map(|results| results.events_seen).sum();
            let emits: Vec<&str> = held
                .iter()
                .flat_map(|results| results.outputs.iter().map(|output| output.emit()))
                .collect();
            let figure = trust::figure(
                format!(
                    "{} events via {} [{}]",
                    crate::units::count(events as f64),
                    held[0].backend,
                    emits.join(",")
                ),
                &trust,
            );
            line.extend(figure.spans);
        }
        lines.push(Line::from(line));
        lines.push(Line::from(Span::styled(
            format!("{:<25}{}", "", need.why),
            Style::new().add_modifier(Modifier::DIM),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_views_each_with_a_name_of_its_own() {
        let views = registry();
        assert_eq!(views.len(), 10);
        let mut names: Vec<_> = views.iter().map(|view| view.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 10, "two views share a name");
    }

    #[test]
    fn two_views_asking_one_source_in_two_modes_are_refused_naming_both() {
        let need = |mode: &'static str, stack: Option<&'static str>| Need {
            source: "cpu.profile",
            mode,
            emit: &["stack"],
            keys: &[],
            stack,
            scope: Scope::Target,
            why: "",
        };
        let one = [need("sample@99hz", Some("fp"))];
        let other = [need("sample@999hz", Some("fp"))];
        let refused = merge_needs([("flame", &one[..]), ("ledger", &other[..])])
            .expect_err("two modes of one source cannot be one request");
        assert!(
            refused.contains("flame")
                && refused.contains("ledger")
                && refused.contains("sample@999hz"),
            "{refused}"
        );
        let stacked = [need("sample@99hz", Some("dwarf"))];
        assert!(merge_needs([("flame", &one[..]), ("ledger", &stacked[..])]).is_err());
        let same = [need("sample@99hz", Some("fp"))];
        assert_eq!(
            merge_needs([("flame", &one[..]), ("ledger", &same[..])])
                .expect("the same mode merges")[0]
                .views,
            ["flame", "ledger"]
        );
    }

    #[test]
    fn merged_needs_are_one_per_source_and_scope() {
        let merged = all_needs().expect("no two views ask one source two ways");
        for (index, one) in merged.iter().enumerate() {
            assert!(
                !merged[index + 1..]
                    .iter()
                    .any(|other| other.source == one.source && other.scope == one.scope),
                "{} asked twice",
                one.source
            );
            assert!(!one.emit.is_empty(), "{} has no emit", one.source);
        }
    }
}
