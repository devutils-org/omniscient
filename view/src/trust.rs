//! How a figure says how far it can be trusted: one style for every view.
//!
//! `omn` never shows a number without what it lost, how far its clock may be
//! off and how reproducible its source is. On a terminal that is drawn rather
//! than footnoted:
//!
//! - a figure with **losses**, a **non-zero time error**, **low determinism**,
//!   or one that is still **provisional** (a live run, before its end) is
//!   drawn **dimmed**, with a short badge beside it saying which;
//! - a figure whose bounds are known is drawn with an **error bar**: the
//!   uncertain part of a bar in a lighter fill ([`error_bar`]);
//! - a source that was **refused** shows its reason **where its panel would
//!   be** ([`render_refusal`]), never an empty panel.
//!
//! A view asks [`Trust::of`] for a result and uses [`Trust::style`],
//! [`Trust::badges`] and [`figure`]; it never decides on its own what is
//! dimmed, so two views cannot disagree about one figure.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::model::{Data, Refusal, Results};
use crate::units;

/// The colours every view draws with, so the trust style reads the same
/// everywhere. Sixteen-colour names only: a terminal's own palette decides
/// what they look like, which keeps a light and a dark theme legible.
pub mod palette {
    use ratatui::style::Color;

    /// What a lost record is drawn in.
    pub const LOSS: Color = Color::Yellow;
    /// What a clock's error is drawn in.
    pub const TIME: Color = Color::Cyan;
    /// What a refusal is drawn in.
    pub const REFUSED: Color = Color::Red;
    /// What a provisional figure is drawn in.
    pub const LIVE: Color = Color::Blue;
    /// The view's own cost.
    pub const COST: Color = Color::Magenta;
    /// Something faster, or less, in a comparison.
    pub const BETTER: Color = Color::Blue;
    /// Something slower, or more, in a comparison.
    pub const WORSE: Color = Color::Red;
    /// The accent of a selection.
    pub const ACCENT: Color = Color::Green;
}

/// One reason a figure is not to be taken at face value.
#[derive(Debug, Clone, PartialEq)]
pub enum Mark {
    /// Records that never arrived or could not be read. `fraction` is the
    /// share of the whole they were, when `omn` could say it.
    Lost { missing: u64, fraction: Option<f64> },
    /// Rounds of reads that came late; a late read is a read, so nothing is
    /// missing, but the series' spacing is not what was asked.
    Late { ticks: u64 },
    /// The largest error a timestamp carries, in nanoseconds: the figure's
    /// time was converted from another clock.
    TimeError { ns: u64 },
    /// Nothing says how far the figure's timestamps may be off: it holds
    /// aggregates only, from a recording that does not carry their events'
    /// time error. Unknown, never zero, so it dims as a time error does.
    TimeErrorUnknown,
    /// The source is not reproducible between two runs of one workload.
    LowDeterminism,
    /// The run is still going: losses, cost and stack names are known only
    /// at its end.
    Provisional,
    /// A fallback served it, and said what it gives up.
    Degraded { count: usize },
}

impl Mark {
    /// Whether this mark dims the figure. Losses, a time error, low
    /// determinism and a live run do. A late round and a degradation are said
    /// in a badge only: a late read is a read, so nothing is missing, and a
    /// fallback said what it gives up, so the figure is exact in its own
    /// terms.
    pub fn dims(&self) -> bool {
        !matches!(self, Mark::Degraded { .. } | Mark::Late { .. })
    }

    /// A badge of a few cells.
    pub fn badge(&self) -> Span<'static> {
        match self {
            Mark::Lost { missing, fraction } => Span::styled(
                match fraction {
                    Some(fraction) => format!(
                        "-{} lost ({})",
                        units::count(*missing as f64),
                        units::percent(fraction * 100.0)
                    ),
                    None => format!("-{} lost", units::count(*missing as f64)),
                },
                Style::new().fg(palette::LOSS),
            ),
            Mark::Late { ticks } => {
                Span::styled(format!("{ticks} late"), Style::new().fg(palette::LOSS))
            }
            Mark::TimeError { ns } => Span::styled(
                format!("±{}", units::duration(*ns as f64)),
                Style::new().fg(palette::TIME),
            ),
            Mark::TimeErrorUnknown => Span::styled("±?", Style::new().fg(palette::TIME)),
            Mark::LowDeterminism => Span::styled("~low det", Style::new().fg(palette::TIME)),
            Mark::Provisional => Span::styled(
                "live",
                Style::new()
                    .fg(palette::LIVE)
                    .add_modifier(Modifier::ITALIC),
            ),
            Mark::Degraded { count } => Span::styled(
                if *count == 1 {
                    "degraded".to_owned()
                } else {
                    format!("degraded x{count}")
                },
                Style::new().fg(palette::LOSS),
            ),
        }
    }

    /// The same in a sentence, for a detail pane.
    pub fn sentence(&self) -> String {
        match self {
            Mark::Lost { missing, fraction } => match fraction {
                Some(fraction) => format!(
                    "{missing} records were lost ({:.2} % of the whole): every figure built from \
                     them is a lower bound",
                    fraction * 100.0
                ),
                None => format!(
                    "{missing} records were lost: every figure built from them is a lower bound"
                ),
            },
            Mark::Late { ticks } => format!(
                "{ticks} rounds of reads came late: nothing is missing, but the series is not \
                 spaced as asked"
            ),
            Mark::TimeError { ns } => format!(
                "timestamps were converted from another clock and may be off by up to {}",
                units::duration(*ns as f64)
            ),
            Mark::TimeErrorUnknown => "how far the timestamps may be off is unknown: the result \
                 holds aggregates only, and the recording does not carry its events' time error"
                .to_owned(),
            Mark::LowDeterminism => {
                "the source is not reproducible between two runs of the same workload".to_owned()
            }
            Mark::Provisional => {
                "the run is still going: losses, cost and stack names are known at its end"
                    .to_owned()
            }
            Mark::Degraded { count } => {
                format!("served by a fallback that says what it gives up ({count})")
            }
        }
    }
}

/// How far one figure can be trusted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Trust {
    pub marks: Vec<Mark>,
}

impl Trust {
    /// Nothing to say: drawn plain.
    pub fn clean() -> Trust {
        Trust::default()
    }

    /// The trust of one result, in its run.
    pub fn of(data: &Data, results: &Results) -> Trust {
        let mut marks = Vec::new();
        let missing = results.losses.missing();
        if missing > 0 {
            let fraction = results.outputs.iter().find_map(|output| match output {
                crate::model::Output::Stack {
                    short: Some(short), ..
                } => short.fraction,
                _ => None,
            });
            let fraction = fraction.or_else(|| {
                let seen = results.events_seen;
                (seen > 0).then(|| missing as f64 / (seen + missing) as f64)
            });
            marks.push(Mark::Lost { missing, fraction });
        }
        if results.losses.late_ticks > 0 {
            marks.push(Mark::Late {
                ticks: results.losses.late_ticks,
            });
        }
        match results.max_ts_err_ns() {
            Some(0) => {}
            Some(ns) => marks.push(Mark::TimeError { ns }),
            None => marks.push(Mark::TimeErrorUnknown),
        }
        if data
            .facts
            .get(&results.source)
            .is_some_and(|facts| facts.determinism == "low")
        {
            marks.push(Mark::LowDeterminism);
        }
        if data.live.is_some() {
            marks.push(Mark::Provisional);
        }
        if !results.degradations.is_empty() {
            marks.push(Mark::Degraded {
                count: results.degradations.len(),
            });
        }
        Trust { marks }
    }

    /// The trust of a figure several results made: every mark of each, once.
    pub fn joined<'a>(data: &Data, results: impl IntoIterator<Item = &'a Results>) -> Trust {
        let mut joined = Trust::clean();
        for one in results {
            joined.merge(Trust::of(data, one));
        }
        joined
    }

    /// Adds another figure's marks, keeping the worse of two of a kind.
    pub fn merge(&mut self, other: Trust) {
        for mark in other.marks {
            let same = self
                .marks
                .iter_mut()
                .find(|held| std::mem::discriminant(*held) == std::mem::discriminant(&mark));
            match (same, mark) {
                (
                    Some(Mark::Lost { missing, fraction }),
                    Mark::Lost {
                        missing: more,
                        fraction: other,
                    },
                ) => {
                    *missing += more;
                    *fraction = match (*fraction, other) {
                        (Some(one), Some(two)) => Some(one.max(two)),
                        _ => None,
                    };
                }
                (Some(Mark::Late { ticks }), Mark::Late { ticks: more }) => *ticks += more,
                (Some(Mark::TimeError { ns }), Mark::TimeError { ns: more }) => {
                    *ns = (*ns).max(more)
                }
                (Some(Mark::Degraded { count }), Mark::Degraded { count: more }) => *count += more,
                (Some(_), _) => {}
                (None, mark) => self.marks.push(mark),
            }
        }
    }

    /// Whether the figure is drawn dimmed.
    pub fn dims(&self) -> bool {
        self.marks.iter().any(Mark::dims)
    }

    /// The style of the figure itself: dimmed when it must not be read at
    /// face value, plain otherwise.
    pub fn style(&self) -> Style {
        if self.dims() {
            Style::new().add_modifier(Modifier::DIM)
        } else {
            Style::new()
        }
    }

    /// The badges, separated by a space, for beside the figure.
    pub fn badges(&self) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for mark in &self.marks {
            if !spans.is_empty() {
                spans.push(Span::raw(" "));
            }
            spans.push(mark.badge());
        }
        spans
    }

    /// Every mark in a sentence.
    pub fn sentences(&self) -> Vec<String> {
        self.marks.iter().map(Mark::sentence).collect()
    }
}

/// A figure in the trust style: the text, dimmed if it must be, and its
/// badges after it.
pub fn figure(text: impl Into<String>, trust: &Trust) -> Line<'static> {
    let mut spans = vec![Span::styled(text.into(), trust.style())];
    let badges = trust.badges();
    if !badges.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(badges);
    }
    Line::from(spans)
}

/// Draws a refused source where its panel would have been: its name and the
/// reason, in the words of whatever refused it.
pub fn render_refusal(frame: &mut Frame<'_>, area: Rect, refusal: &Refusal) {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(palette::REFUSED))
        .title(Span::styled(
            format!(" {} refused ", refusal.label),
            Style::new().fg(palette::REFUSED),
        ));
    let paragraph = Paragraph::new(refusal_lines(refusal))
        .block(block)
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

/// The lines a refusal is said in, for a view that lays it out itself.
pub fn refusal_lines(refusal: &Refusal) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            format!("{} was not served:", refusal.source),
            Style::new().fg(palette::REFUSED),
        )),
        Line::from(refusal.reason.clone()),
    ]
}

/// A horizontal bar of `width` cells for `value` out of `max`, with the
/// interval `[low, high]` it may truly lie in drawn in a lighter fill: what is
/// certain is `█`, what is uncertain `▒`, the rest blank.
///
/// For a lower bound (a figure from a lossy run), `low` is the value and
/// `high` what it would be had nothing been lost; for a converted timestamp,
/// the value plus and minus its error.
pub fn error_bar(value: f64, low: f64, high: f64, max: f64, width: u16) -> Line<'static> {
    let width = usize::from(width);
    if width == 0 || max <= 0.0 || !max.is_finite() {
        return Line::default();
    }
    let cells = |x: f64| ((x / max).clamp(0.0, 1.0) * width as f64).round() as usize;
    let sure = cells(low.min(value));
    let unsure = cells(high.max(value)).max(sure);
    let mut solid = "█".repeat(sure);
    let shaded = "▒".repeat(unsure - sure);
    let mut drawn = unsure;
    if unsure == 0 && value > 0.0 {
        // Something, smaller than a cell: never drawn as nothing.
        solid = "▏".to_owned();
        drawn = 1;
    }
    let blank = " ".repeat(width.saturating_sub(drawn));
    Line::from(vec![
        Span::raw(solid),
        Span::styled(shaded, Style::new().fg(palette::LOSS)),
        Span::raw(blank),
    ])
}

/// The colour of a difference between two runs: red where the second is
/// worse, blue where it is better, by `ratio` = second / first.
pub fn difference_colour(ratio: f64) -> Option<Color> {
    if !ratio.is_finite() {
        return Some(palette::WORSE);
    }
    if ratio > 1.05 {
        Some(palette::WORSE)
    } else if ratio < 0.95 {
        Some(palette::BETTER)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Event, Losses, Output, SourceFacts};

    fn results(losses: Losses, ts_err_ns: u64) -> Results {
        Results {
            label: "io.latency".into(),
            source: "io.latency".into(),
            events_seen: 90,
            losses,
            outputs: vec![Output::Raw {
                events: vec![Event {
                    ts_ns: 1,
                    ts_err_ns,
                    kind: "io.latency".into(),
                    keys: vec![],
                    attrs: vec![],
                    collector_id: String::new(),
                    seq: 0,
                }],
            }],
            ..Results::default()
        }
    }

    #[test]
    fn a_clean_result_is_drawn_plain() {
        let data = Data::default();
        let trust = Trust::of(&data, &results(Losses::default(), 0));
        assert!(!trust.dims());
        assert!(trust.badges().is_empty());
        assert_eq!(trust.style(), Style::new());
    }

    #[test]
    fn losses_time_error_and_low_determinism_each_dim() {
        let lossy = Losses {
            dropped: 10,
            ..Losses::default()
        };
        let mut data = Data::default();
        let trust = Trust::of(&data, &results(lossy, 0));
        assert_eq!(
            trust.marks,
            vec![Mark::Lost {
                missing: 10,
                fraction: Some(0.1)
            }]
        );
        assert!(trust.dims());

        let trust = Trust::of(&data, &results(Losses::default(), 2_000));
        assert_eq!(trust.marks, vec![Mark::TimeError { ns: 2_000 }]);
        assert!(trust.dims());

        data.facts.insert(
            "io.latency".into(),
            SourceFacts {
                determinism: "low".into(),
                summary: String::new(),
            },
        );
        let trust = Trust::of(&data, &results(Losses::default(), 0));
        assert_eq!(trust.marks, vec![Mark::LowDeterminism]);
        assert!(trust.dims());
    }

    #[test]
    fn a_live_run_is_provisional() {
        let data = Data {
            live: Some(crate::model::Live::default()),
            ..Data::default()
        };
        let trust = Trust::of(&data, &results(Losses::default(), 0));
        assert_eq!(trust.marks, vec![Mark::Provisional]);
    }

    #[test]
    fn merged_marks_keep_the_worse() {
        let mut one = Trust {
            marks: vec![
                Mark::TimeError { ns: 5 },
                Mark::Lost {
                    missing: 1,
                    fraction: Some(0.1),
                },
            ],
        };
        one.merge(Trust {
            marks: vec![
                Mark::TimeError { ns: 9 },
                Mark::Lost {
                    missing: 2,
                    fraction: Some(0.3),
                },
            ],
        });
        assert_eq!(
            one.marks,
            vec![
                Mark::TimeError { ns: 9 },
                Mark::Lost {
                    missing: 3,
                    fraction: Some(0.3)
                }
            ]
        );
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn an_error_bar_shades_what_is_uncertain() {
        assert_eq!(text(&error_bar(5.0, 4.0, 6.0, 10.0, 10)), "████▒▒    ");
        assert_eq!(text(&error_bar(10.0, 10.0, 10.0, 10.0, 4)), "████");
        // Smaller than a cell, still drawn.
        assert_eq!(text(&error_bar(0.01, 0.01, 0.01, 10.0, 4)), "▏   ");
        assert_eq!(text(&error_bar(0.0, 0.0, 0.0, 10.0, 4)), "    ");
    }
}
