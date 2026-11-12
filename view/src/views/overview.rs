//! A quiet entry point and fixed-position resource pages.
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row as TableRow, Table, Wrap};
use ratatui::Frame;

use super::{process, source_help, Action, Ctx, Need, Region, View};
use crate::model::{Output, Refusal, Results};
use crate::trust::Trust;
use crate::units;

const NAMES: [&str; 7] = ["CPU", "Memory", "Disk", "Network", "GPU", "Energy", "Time"];
const PAGES: [&str; 7] = [
    "family-cpu",
    "family-memory",
    "family-disk",
    "family-network",
    "family-gpu",
    "family-energy",
    "family-time",
];
const PREFIXES: [&[&str]; 7] = [
    &["cpu."],
    &["mem."],
    &["io."],
    &["net."],
    &["gpu."],
    &["power."],
    &["sched."],
];

fn figures(result: &Results) -> String {
    for output in &result.outputs {
        if let Output::Summary { series } = output {
            if let Some(item) = series.first() {
                return format!(
                    "series {}  last {}  mean {}  max {}",
                    item.key,
                    units::quantity(item.last, &result.unit),
                    units::quantity(item.mean, &result.unit),
                    units::quantity(item.max, &result.unit)
                );
            }
        }
    }
    if let Some(attr) = &result.value_attr {
        if let Some(value) = result
            .events()
            .iter()
            .rev()
            .find_map(|event| event.number(attr))
        {
            return format!("last {attr}: {}", units::quantity(value, &result.unit));
        }
    }
    String::new()
}

pub struct Overview {
    selected: usize,
    columns: usize,
    pending_g: bool,
    regions: Vec<Region>,
}
pub struct Family {
    index: usize,
    selected: usize,
    detail_scroll: u16,
    pending_g: bool,
    regions: Vec<Region>,
}
pub fn overview() -> Box<dyn View> {
    Box::new(Overview {
        selected: 0,
        columns: 1,
        pending_g: false,
        regions: Vec::new(),
    })
}
pub fn families() -> Vec<Box<dyn View>> {
    (0..NAMES.len())
        .map(|index| {
            Box::new(Family {
                index,
                selected: 0,
                detail_scroll: 0,
                pending_g: false,
                regions: Vec::new(),
            }) as Box<dyn View>
        })
        .collect()
}

impl View for Overview {
    fn name(&self) -> &'static str {
        "overview"
    }
    fn title(&self) -> &'static str {
        "Resource families"
    }
    fn needs(&self) -> &'static [Need] {
        &[]
    }
    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        self.regions.clear();
        let [heading, cards] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
        let heading_lines = vec![
            Line::from(Span::styled(
                if ctx.data.has_target() {
                    " Target resources"
                } else {
                    " Machine resources"
                },
                Style::new().add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                " Select a family · Enter opens it · {} refused sources",
                ctx.data.refused.len()
            )),
        ];
        frame.render_widget(Paragraph::new(heading_lines), heading);
        // Card positions depend only on the terminal size, never on incoming data.
        let columns = if cards.width >= 70 && cards.height >= 12 {
            2usize
        } else {
            1
        };
        self.columns = columns;
        let card_height = if columns == 2 {
            (cards.height / 4).max(1)
        } else {
            3
        };
        let visible_rows = usize::from(cards.height / card_height).max(1);
        let selected_row = self.selected / columns;
        let start_row = selected_row.saturating_add(1).saturating_sub(visible_rows);
        let graph_cells = usize::from(if columns == 2 {
            cards.width / 2
        } else {
            cards.width
        })
        .saturating_sub(4)
        .clamp(1, 48);
        let screen = process::screen_at(ctx, graph_cells);
        for (index, row) in screen.rows.iter().enumerate() {
            let row_index = index / columns;
            if row_index < start_row || row_index >= start_row + visible_rows {
                continue;
            }
            let local_row = row_index - start_row;
            let x_offset = if columns == 2 && index % 2 == 1 {
                cards.width / 2 + 1
            } else {
                0
            };
            let width = if columns == 2 && index % 2 == 0 {
                cards.width / 2
            } else {
                cards.width.saturating_sub(x_offset)
            };
            let y_offset = (local_row as u16).saturating_mul(card_height);
            let height = card_height.min(cards.height.saturating_sub(y_offset));
            let rect = Rect::new(cards.x + x_offset, cards.y + y_offset, width, height);
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            let selected = index == self.selected;
            let title = format!(" {} {} ", if selected { "›" } else { " " }, NAMES[index]);
            let title_style = if selected {
                Style::new()
                    .fg(crate::theme::accent())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().add_modifier(Modifier::BOLD)
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(crate::theme::border())
                .title_style(crate::theme::title())
                .border_style(if selected {
                    Style::new().fg(crate::theme::accent())
                } else {
                    crate::theme::border()
                })
                .title(Span::styled(title, title_style));
            let inside = block.inner(rect);
            frame.render_widget(block, rect);
            let mut value = Vec::new();
            if ctx.data.has_target() {
                value.extend(row.figures.clone());
            } else if let Some(result) = ctx.data.run.results.iter().find(|r| {
                PREFIXES[index].iter().any(|p| r.source.starts_with(p)) && !figures(r).is_empty()
            }) {
                value.push(Span::raw(format!(
                    "{}  {}  ",
                    result.source,
                    figures(result)
                )));
                value.extend(Trust::of(ctx.data, result).badges());
            } else if let Some(refusal) = ctx
                .data
                .refused
                .iter()
                .find(|r| PREFIXES[index].iter().any(|p| r.source.starts_with(p)))
            {
                value.push(Span::styled(
                    format!("{} refused: {}", refusal.source, refusal.reason),
                    Style::new().fg(crate::trust::palette::refused()),
                ));
            }
            // Keep values and units whole when a compact card cannot fit
            // every figure; the family page carries the full detail.
            if ctx.data.has_target() {
                let mut width = 0usize;
                let limit = usize::from(inside.width);
                let mut fitted = Vec::new();
                for span in value {
                    let next = span.width();
                    if width + next > limit {
                        if width < limit {
                            fitted.push(Span::raw("…"));
                        }
                        break;
                    }
                    width += next;
                    fitted.push(span);
                }
                value = fitted;
            }
            let mut graph = if ctx.data.has_target() {
                let mut badges = row.trust.badges();
                if !badges.is_empty() {
                    badges.push(Span::raw("  "));
                }
                badges
            } else {
                Vec::new()
            };
            if ctx.data.has_target() {
                graph.extend(row.graph.clone());
            }
            frame.render_widget(
                Paragraph::new(vec![Line::from(value), Line::from(graph)]),
                inside,
            );
            self.regions.push(Region::new(rect, index));
        }
    }
    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        let was_g = self.pending_g;
        self.pending_g = key.code == KeyCode::Char('g');
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                let next = self.selected + self.columns;
                if next < NAMES.len() {
                    self.selected = next;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(self.columns)
            }
            KeyCode::Right | KeyCode::Char('l')
                if self.columns > 1
                    && self.selected % self.columns == 0
                    && self.selected + 1 < NAMES.len() =>
            {
                self.selected += 1
            }
            KeyCode::Left | KeyCode::Char('h')
                if self.columns > 1 && self.selected % self.columns != 0 =>
            {
                self.selected -= 1
            }
            KeyCode::Home | KeyCode::Char('g') if key.code == KeyCode::Home || was_g => {
                self.selected = 0
            }
            KeyCode::End | KeyCode::Char('G') => self.selected = NAMES.len() - 1,
            KeyCode::Enter => {
                return Action::Goto {
                    view: PAGES[self.selected],
                    focus: ctx.focus.clone(),
                }
            }
            _ => {}
        }
        Action::None
    }
    fn enter_label(&self, _: &Ctx<'_>) -> Option<String> {
        Some(format!("open {}", NAMES[self.selected]))
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("hjkl/arrows", "family"),
            ("gg/G", "first/last"),
            ("enter", "open"),
        ]
    }
    fn regions(&self) -> &[Region] {
        &self.regions
    }
    fn select(&mut self, id: usize, _: &Ctx<'_>) -> Action {
        self.selected = id.min(NAMES.len() - 1);
        self.pending_g = false;
        Action::None
    }
}

// Include every matching result and refusal, including sources supplied by a
// custom session, plus the standard sources that have not been requested.
struct SourceRow<'a> {
    source: &'a str,
    label: &'a str,
    result: Option<&'a Results>,
    refusal: Option<&'a Refusal>,
}

fn sources<'a>(ctx: &'a Ctx<'_>, family: usize) -> Vec<SourceRow<'a>> {
    let matches = |source: &str| PREFIXES[family].iter().any(|p| source.starts_with(p));
    let mut rows: Vec<_> = ctx
        .data
        .run
        .results
        .iter()
        .filter(|r| matches(&r.source))
        .map(|r| SourceRow {
            source: &r.source,
            label: &r.label,
            result: Some(r),
            refusal: None,
        })
        .collect();
    rows.extend(
        ctx.data
            .refused
            .iter()
            .filter(|r| matches(&r.source))
            .map(|r| SourceRow {
                source: &r.source,
                label: &r.label,
                result: None,
                refusal: Some(r),
            }),
    );
    for view in super::registry() {
        for need in view.needs().iter().filter(|need| matches(need.source)) {
            if !rows.iter().any(|r| r.source == need.source) {
                rows.push(SourceRow {
                    source: need.source,
                    label: need.source,
                    result: None,
                    refusal: None,
                });
            }
        }
    }
    rows.sort_by(|a, b| (a.source, a.label).cmp(&(b.source, b.label)));
    rows
}

fn summary(result: &Results) -> (String, String, String, String) {
    for output in &result.outputs {
        if let Output::Summary { series } = output {
            if let Some(item) = series.first() {
                return (
                    item.key.clone(),
                    units::quantity(item.last, &result.unit),
                    units::quantity(item.mean, &result.unit),
                    units::quantity(item.max, &result.unit),
                );
            }
        }
    }
    if let Some(attr) = &result.value_attr {
        if let Some(value) = result
            .events()
            .iter()
            .rev()
            .find_map(|event| event.number(attr))
        {
            return (
                format!("last {attr}"),
                units::quantity(value, &result.unit),
                String::new(),
                String::new(),
            );
        }
    }
    (String::new(), String::new(), String::new(), String::new())
}

impl View for Family {
    fn name(&self) -> &'static str {
        PAGES[self.index]
    }
    fn title(&self) -> &'static str {
        NAMES[self.index]
    }
    fn needs(&self) -> &'static [Need] {
        &[]
    }
    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        self.regions.clear();
        let items = sources(ctx, self.index);
        self.selected = self.selected.min(items.len().saturating_sub(1));
        let [overview, table_area, detail] = Layout::vertical([
            Constraint::Length(5),
            Constraint::Percentage(48),
            Constraint::Min(4),
        ])
        .areas(area);
        let resource =
            &process::screen_at(ctx, usize::from(area.width.saturating_sub(4)).clamp(1, 80)).rows
                [self.index];
        let headline = if ctx.data.has_target() {
            resource.figures.clone()
        } else {
            vec![Span::raw(
                "Machine-wide and target sources are labelled separately below",
            )]
        };
        let graph = if ctx.data.has_target() {
            resource.graph.clone()
        } else {
            Vec::new()
        };
        let summary_lines = vec![Line::from(headline), Line::from(graph)];
        frame.render_widget(
            Paragraph::new(summary_lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(ratatui::widgets::BorderType::Rounded)
                    .border_style(crate::theme::border())
                    .title_style(crate::theme::title())
                    .title(format!(" {} ", NAMES[self.index])),
            ),
            overview,
        );
        let header = TableRow::new([
            "Source / scope",
            "Series (first)",
            "Last",
            "Mean",
            "Max",
            "Status",
        ])
        .style(Style::new().add_modifier(Modifier::BOLD));
        let narrow = area.width < 90;
        let table_rows: Vec<_> = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let (series, last, mean, max) = item.result.map(summary).unwrap_or_default();
                let status = if item.refusal.is_some() {
                    "refused".to_owned()
                } else if let Some(result) = item.result {
                    let badges = Trust::of(ctx.data, result).badges();
                    if badges.is_empty() {
                        "available".to_owned()
                    } else {
                        badges
                            .iter()
                            .map(|b| b.content.as_ref())
                            .collect::<Vec<_>>()
                            .join(" ")
                    }
                } else {
                    "not requested".to_owned()
                };
                let cells = if narrow {
                    vec![
                        Cell::from(item.label.to_owned()),
                        Cell::from(last),
                        Cell::from(status),
                    ]
                } else {
                    vec![
                        Cell::from(item.label.to_owned()),
                        Cell::from(series),
                        Cell::from(last),
                        Cell::from(mean),
                        Cell::from(max),
                        Cell::from(status),
                    ]
                };
                let style = if i == self.selected {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new()
                };
                TableRow::new(cells).style(style)
            })
            .collect();
        let visible = table_area.height.saturating_sub(3) as usize;
        let start = self.selected.saturating_add(1).saturating_sub(visible);
        for (offset, _) in items.iter().skip(start).take(visible).enumerate() {
            let y = table_area.y + 2 + offset as u16;
            if table_area.width > 2 {
                self.regions.push(Region::new(
                    Rect::new(table_area.x + 1, y, table_area.width - 2, 1),
                    start + offset,
                ));
            }
        }
        let widths = if narrow {
            vec![
                Constraint::Percentage(45),
                Constraint::Percentage(20),
                Constraint::Percentage(35),
            ]
        } else {
            vec![
                Constraint::Percentage(25),
                Constraint::Percentage(20),
                Constraint::Percentage(10),
                Constraint::Percentage(10),
                Constraint::Percentage(10),
                Constraint::Percentage(25),
            ]
        };
        let header = if narrow {
            TableRow::new(["Source / scope", "Last", "Status"])
                .style(Style::new().add_modifier(Modifier::BOLD))
        } else {
            header
        };
        let title = format!(
            " Sources {}–{} / {} ",
            if visible == 0 { 0 } else { start + 1 },
            (start + visible).min(items.len()),
            items.len()
        );
        let table = Table::new(table_rows.into_iter().skip(start), widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(ratatui::widgets::BorderType::Rounded)
                    .border_style(crate::theme::border())
                    .title_style(crate::theme::title())
                    .title(title),
            )
            .column_spacing(1);
        frame.render_widget(table, table_area);
        let mut lines = Vec::new();
        if let Some(item) = items.get(self.selected) {
            if let Some((description, _)) = source_help::explanation(item.source) {
                lines.push(Line::from(description));
            } else if let Some(fact) = ctx
                .data
                .facts
                .get(item.source)
                .filter(|f| !f.summary.is_empty())
            {
                lines.push(Line::from(fact.summary.clone()));
            } else {
                lines.push(Line::from("No description available for this source."));
            }
            lines.push(Line::from("Press I for the view and source guide."));
            if let Some(refusal) = item.refusal {
                lines.push(Line::from(format!("Refused: {}", refusal.reason)));
            } else if let Some(result) = item.result {
                let trust = Trust::of(ctx.data, result);
                let badges = trust.badges();
                if !badges.is_empty() {
                    let mut line = vec![Span::raw("Quality: ")];
                    line.extend(badges);
                    lines.push(Line::from(line));
                }
                let mut series_found = false;
                for output in &result.outputs {
                    if let Output::Summary { series } = output {
                        series_found = true;
                        lines.push(Line::from(
                            "Series                         Last          Mean          Max",
                        ));
                        for value in series {
                            lines.push(Line::from(format!(
                                "{:<29} {:<13} {:<13} {}",
                                value.key,
                                units::quantity(value.last, &result.unit),
                                units::quantity(value.mean, &result.unit),
                                units::quantity(value.max, &result.unit)
                            )));
                        }
                    }
                }
                if !series_found {
                    lines.push(Line::from(format!(
                        "Available outputs: {}",
                        result
                            .outputs
                            .iter()
                            .map(Output::emit)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                    let text = figures(result);
                    if !text.is_empty() {
                        lines.push(Line::from(text));
                    }
                    lines.push(Line::from(
                        "Open the detailed view for events, histograms and stacks.",
                    ));
                }
            } else {
                lines.push(Line::from("Not requested in this recording."));
            }
        }
        let title = items
            .get(self.selected)
            .map(|item| item.label)
            .unwrap_or("Source detail");
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.detail_scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(ratatui::widgets::BorderType::Rounded)
                        .border_style(crate::theme::border())
                        .title_style(crate::theme::title())
                        .title(format!(" {title} · detail (PgUp/PgDn) ")),
                ),
            detail,
        );
    }
    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        let was_g = self.pending_g;
        self.pending_g = key.code == KeyCode::Char('g');
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected =
                    (self.selected + 1).min(sources(ctx, self.index).len().saturating_sub(1));
                self.detail_scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.detail_scroll = 0;
            }
            KeyCode::Home | KeyCode::Char('g') if key.code == KeyCode::Home || was_g => {
                self.selected = 0;
                self.detail_scroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = sources(ctx, self.index).len().saturating_sub(1);
                self.detail_scroll = 0;
            }
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(3),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(3),
            KeyCode::Enter => {
                let row = &process::screen(ctx).rows[self.index];
                return Action::Goto {
                    view: if ctx.data.has_target() {
                        row.view
                    } else {
                        "system"
                    },
                    focus: row.focus.clone(),
                };
            }
            _ => {}
        }
        Action::None
    }
    fn enter_label(&self, ctx: &Ctx<'_>) -> Option<String> {
        let row = &process::screen(ctx).rows[self.index];
        Some(format!(
            "open {}",
            if ctx.data.has_target() {
                row.view
            } else {
                "system"
            }
        ))
    }
    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("↑↓", "source"),
            ("gg/G", "first/last"),
            ("PgUp/Dn", "detail"),
            ("enter", "detailed view"),
        ]
    }
    fn regions(&self) -> &[Region] {
        &self.regions
    }
    fn select(&mut self, id: usize, _: &Ctx<'_>) -> Action {
        self.selected = id;
        self.pending_g = false;
        self.detail_scroll = 0;
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Cost};
    use crate::snapshot::{draw_app, find, fixture};

    #[test]
    fn family_navigation_and_back_keep_the_selection() {
        let mut app = App::new(fixture("http-server"));
        app.cost = Cost::Snapshot;
        draw_app(&mut app, 120, 40);
        app.key(KeyEvent::from(KeyCode::Right));
        app.key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.views[app.current].name(), "family-memory");
        app.key(KeyEvent::from(KeyCode::Backspace));
        let screen = draw_app(&mut app, 120, 40);
        assert!(find(&screen, "› Memory").is_some());
    }

    #[test]
    fn every_default_family_source_has_a_reading_guide() {
        for view in super::super::registry() {
            for need in view.needs() {
                if PREFIXES.iter().any(|prefixes| {
                    prefixes
                        .iter()
                        .any(|prefix| need.source.starts_with(prefix))
                }) {
                    assert!(
                        source_help::explanation(need.source).is_some(),
                        "{}",
                        need.source
                    );
                }
            }
        }
    }

    #[test]
    fn family_cards_fit_and_are_clickable_in_common_terminal_sizes() {
        for (width, height) in [(80, 24), (120, 40)] {
            let mut app = App::new(fixture("http-server"));
            let screen = draw_app(&mut app, width, height);
            for label in NAMES {
                assert!(
                    find(&screen, label).is_some(),
                    "{label} at {width}x{height}"
                );
            }
            let (x, y) = find(&screen, "Memory").unwrap();
            crate::snapshot::click(&mut app, x, y);
            draw_app(&mut app, width, height);
            app.key(KeyEvent::from(KeyCode::Enter));
            assert_eq!(app.views[app.current].name(), "family-memory");
        }
    }

    #[test]
    fn arrows_follow_the_visible_grid_and_single_column() {
        let mut app = App::new(fixture("http-server"));
        draw_app(&mut app, 120, 40);
        app.key(KeyEvent::from(KeyCode::Right));
        app.key(KeyEvent::from(KeyCode::Down));
        let screen = draw_app(&mut app, 120, 40);
        assert!(find(&screen, "› Network").is_some());
        app.key(KeyEvent::from(KeyCode::Left));
        let screen = draw_app(&mut app, 120, 40);
        assert!(find(&screen, "› Disk").is_some());
        draw_app(&mut app, 60, 24);
        app.key(KeyEvent::from(KeyCode::Down));
        let screen = draw_app(&mut app, 60, 24);
        assert!(find(&screen, "› Network").is_some());
    }

    #[test]
    fn vim_jumps_work_in_family_source_lists() {
        let data = fixture("http-server");
        let focus = super::super::Focus::default();
        let ctx = Ctx {
            data: &data,
            against: None,
            focus: &focus,
        };
        let mut family = Family {
            index: 1,
            selected: 0,
            detail_scroll: 0,
            pending_g: false,
            regions: Vec::new(),
        };
        family.key(KeyEvent::from(KeyCode::Char('G')), &ctx);
        assert_eq!(family.selected, sources(&ctx, 1).len() - 1);
        family.key(KeyEvent::from(KeyCode::Char('g')), &ctx);
        family.key(KeyEvent::from(KeyCode::Char('g')), &ctx);
        assert_eq!(family.selected, 0);
    }

    #[test]
    fn vim_keys_move_in_grid_and_jump_to_ends() {
        let mut app = App::new(fixture("http-server"));
        draw_app(&mut app, 120, 40);
        for key in ['l', 'j'] {
            app.key(KeyEvent::from(KeyCode::Char(key)));
        }
        let screen = draw_app(&mut app, 120, 40);
        assert!(find(&screen, "› Network").is_some());
        app.key(KeyEvent::from(KeyCode::Char('h')));
        let screen = draw_app(&mut app, 120, 40);
        assert!(find(&screen, "› Disk").is_some());
        app.key(KeyEvent::from(KeyCode::Char('G')));
        let screen = draw_app(&mut app, 120, 40);
        assert!(find(&screen, "› Time").is_some());
        for _ in 0..2 {
            app.key(KeyEvent::from(KeyCode::Char('g')));
        }
        let screen = draw_app(&mut app, 120, 40);
        assert!(find(&screen, "› CPU").is_some());
    }

    #[test]
    fn custom_sources_and_multiple_scopes_are_kept() {
        let mut data = fixture("http-server");
        data.run.results.push(Results {
            source: "mem.custom".into(),
            label: "custom target".into(),
            ..Results::default()
        });
        data.run.results.push(Results {
            source: "mem.custom".into(),
            label: "custom system".into(),
            ..Results::default()
        });
        let focus = super::super::Focus::default();
        let ctx = Ctx {
            data: &data,
            against: None,
            focus: &focus,
        };
        let rows = sources(&ctx, 1);
        assert_eq!(
            rows.iter().filter(|row| row.source == "mem.custom").count(),
            2
        );
    }

    #[test]
    fn rows_do_not_move_when_results_arrive() {
        let data = fixture("http-server");
        let mut empty = data.clone();
        empty.run.results.clear();
        empty.refused.clear();
        let mut before = App::new(empty);
        let mut after = App::new(data);
        let before = draw_app(&mut before, 120, 40);
        let after = draw_app(&mut after, 120, 40);
        for label in NAMES {
            assert_eq!(
                find(&before, label).map(|p| p.1),
                find(&after, label).map(|p| p.1)
            );
        }
    }
}
