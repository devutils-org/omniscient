//! Aggregation: from events to what was asked for.
//!
//! An emit is a reduction over the event stream, and each one answers a
//! different question: `raw` keeps everything, `timeline` keeps the shape over
//! time, `summary` keeps the shape over the whole run. They are computed from
//! the same events, so asking for two of them costs one collection.
//!
//! Events are grouped into series by their primary key. Two processes polled by
//! one request produce two series, never one averaged number: an average across
//! targets is a value that describes nothing that exists.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::catalog::Unit;
use crate::collect::{Collection, Filtered, Losses};
use crate::event::{Event, KeyValue};
use crate::request::{Emit, EmitKind};
use crate::resolve::ResolvedRequest;

/// Everything one request produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Results {
    pub label: String,
    pub source: String,
    pub unit: Unit,
    /// The backend that produced this, repeated here so a result file stands on
    /// its own: two results obtained through different backends are not
    /// comparable, and nothing else in this structure would say which was used.
    pub backend: String,
    pub outputs: Vec<Output>,
    pub events_seen: usize,
    pub observed_ns: u64,
    pub stopped_because: String,
    pub losses: Losses,
    /// What the request's filter removed after the fact, when the resolver
    /// could not push it down. Absent when the request carried no filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered: Option<Filtered>,
    /// Requirements that were assumed rather than checked, carried from the
    /// resolver so they are visible next to the numbers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unverified: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degradations: Vec<String>,
}

/// One emit's worth of results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "emit")]
pub enum Output {
    Raw { events: Vec<Event> },
    Timeline { series: Vec<Series> },
    Summary { series: Vec<SeriesSummary> },
    Rate { series: Vec<SeriesRate> },
}

impl Output {
    pub fn kind(&self) -> EmitKind {
        match self {
            Output::Raw { .. } => EmitKind::Raw,
            Output::Timeline { .. } => EmitKind::Timeline,
            Output::Summary { .. } => EmitKind::Summary,
            Output::Rate { .. } => EmitKind::Rate,
        }
    }
}

/// How much of something happened, per second, over the observed window.
///
/// The window is the run's own observed duration, not the requested one: a
/// program that exits after 200 ms of a 30 s request did not produce its events
/// over 30 s, and dividing by the request would understate the rate by two
/// orders of magnitude.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesRate {
    pub key: String,
    pub events: usize,
    pub total: f64,
    /// Absent when the run had no measurable window.
    ///
    /// A window of zero is not a very short window, it is no window, and
    /// dividing by it produces an infinity that renders as a number. The total
    /// is still true and is still reported; the rate is the part that does not
    /// exist.
    pub per_second: Option<f64>,
    pub window_ns: u64,
}

/// Values over time for one key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Series {
    pub key: String,
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Nanoseconds since the first event of the collection, so a series reads
    /// the same whatever the machine's uptime.
    pub offset_ns: u64,
    pub value: f64,
}

/// One key's values reduced to the shape of the whole run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesSummary {
    pub key: String,
    pub count: usize,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub last: f64,
}

/// What an emit cannot be computed from.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EmitError {
    #[error(
        "`{emit}` is not implemented yet: this milestone computes raw, timeline, summary and rate"
    )]
    NotImplemented { emit: String },
    #[error("`{attr}` is not a numeric attribute, so it cannot be aggregated")]
    NotNumeric { attr: String },
}

/// Reduces a collection into the emits the request asked for.
pub fn aggregate(
    resolved: &ResolvedRequest,
    collection: Collection,
    unit: Unit,
    value_attr: &str,
) -> Result<Results, EmitError> {
    let request = &resolved.request;
    let first_ts_ns = collection
        .events
        .first()
        .map(|event| event.ts_ns)
        .unwrap_or(0);

    let mut outputs = Vec::new();
    for emit in &request.emit {
        outputs.push(match emit {
            Emit::Raw => Output::Raw {
                events: collection.events.clone(),
            },
            Emit::Timeline => Output::Timeline {
                series: timeline(&collection.events, first_ts_ns, value_attr)?,
            },
            Emit::Summary => Output::Summary {
                series: summary(&collection.events, value_attr)?,
            },
            Emit::Rate => Output::Rate {
                series: rate(&collection.events, collection.observed_ns, value_attr)?,
            },
            other => {
                return Err(EmitError::NotImplemented {
                    emit: other.to_string(),
                })
            }
        });
    }

    Ok(Results {
        label: request
            .label
            .clone()
            .unwrap_or_else(|| request.source.to_string()),
        source: request.source.to_string(),
        unit,
        backend: resolved.backend.id.clone(),
        outputs,
        events_seen: collection.events.len(),
        observed_ns: collection.observed_ns,
        stopped_because: collection.stopped_because,
        losses: collection.losses,
        filtered: collection.filtered,
        unverified: resolved.unverified.clone(),
        degradations: resolved.degradations.clone(),
    })
}

/// Groups events by their first key, keeping the order they arrived in.
///
/// The first key is the series key by convention, and the collector decides it:
/// it is the one that knows whether a source is naturally per-thread, per
/// device or per socket. Grouping page faults by address, for instance, would
/// produce one series per page.
fn grouped(events: &[Event]) -> BTreeMap<String, Vec<&Event>> {
    let mut groups: BTreeMap<String, Vec<&Event>> = BTreeMap::new();
    for event in events {
        let key = event
            .keys
            .first()
            .map(KeyValue::to_string)
            .unwrap_or_else(|| "system".to_owned());
        groups.entry(key).or_default().push(event);
    }
    groups
}

fn value_of(event: &Event, attr: &str) -> Result<f64, EmitError> {
    event
        .attr(attr)
        .and_then(|value| value.as_f64())
        .ok_or_else(|| EmitError::NotNumeric {
            attr: attr.to_owned(),
        })
}

fn timeline(events: &[Event], first_ts_ns: u64, attr: &str) -> Result<Vec<Series>, EmitError> {
    let mut series = Vec::new();
    for (key, events) in grouped(events) {
        let mut points = Vec::with_capacity(events.len());
        for event in events {
            points.push(Point {
                offset_ns: event.ts_ns.saturating_sub(first_ts_ns),
                value: value_of(event, attr)?,
            });
        }
        series.push(Series { key, points });
    }
    Ok(series)
}

fn rate(events: &[Event], window_ns: u64, attr: &str) -> Result<Vec<SeriesRate>, EmitError> {
    let seconds = (window_ns > 0).then(|| window_ns as f64 / 1e9);
    let mut rates = Vec::new();
    for (key, events) in grouped(events) {
        let total: f64 = events
            .iter()
            .map(|event| value_of(event, attr))
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .sum();
        rates.push(SeriesRate {
            key,
            events: events.len(),
            total,
            per_second: seconds.map(|seconds| total / seconds),
            window_ns,
        });
    }
    Ok(rates)
}

fn summary(events: &[Event], attr: &str) -> Result<Vec<SeriesSummary>, EmitError> {
    let mut summaries = Vec::new();
    for (key, events) in grouped(events) {
        let values: Vec<f64> = events
            .iter()
            .map(|event| value_of(event, attr))
            .collect::<Result<_, _>>()?;
        if values.is_empty() {
            continue;
        }
        let count = values.len();
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean = values.iter().sum::<f64>() / count as f64;
        let last = *values.last().expect("checked above");
        summaries.push(SeriesSummary {
            key,
            count,
            min,
            max,
            mean,
            last,
        });
    }
    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Attr, CollectorId, KeyValue};

    fn event(ts_ns: u64, tid: i32, bytes: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.rss".into(),
            keys: vec![KeyValue::task(tid)],
            attrs: vec![Attr::uint("bytes", bytes)],
            collector_id: CollectorId::new("test"),
            seq: 0,
        }
    }

    #[test]
    fn a_timeline_is_relative_to_the_first_event() {
        let events = vec![event(1_000, 7, 10), event(2_000, 7, 20)];
        let series = timeline(&events, 1_000, "bytes").unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].key, "task:7");
        assert_eq!(series[0].points[0].offset_ns, 0);
        assert_eq!(series[0].points[1].offset_ns, 1_000);
    }

    #[test]
    fn two_targets_stay_two_series_rather_than_one_average() {
        let events = vec![event(0, 1, 10), event(0, 2, 30), event(1, 1, 20)];
        let summaries = summary(&events, "bytes").unwrap();
        assert_eq!(summaries.len(), 2);
        let first = summaries.iter().find(|s| s.key == "task:1").unwrap();
        assert_eq!(
            (first.count, first.min, first.max, first.mean),
            (2, 10.0, 20.0, 15.0)
        );
    }

    /// A rate is a total over a window, and a window of zero is not a very
    /// short window: it is no window. Dividing by it would produce infinity and
    /// render as a number.
    #[test]
    fn a_rate_over_no_window_does_not_produce_infinity() {
        let events = vec![event(0, 1, 10), event(0, 1, 20)];
        let rates = rate(&events, 0, "bytes").unwrap();
        assert_eq!(rates[0].total, 30.0, "the total is still true");
        assert_eq!(
            rates[0].per_second, None,
            "a window of zero has no rate, and no number should be invented for it"
        );

        // A window that exists, however short, does have one.
        let measured = rate(&events, 1_000_000, "bytes").unwrap();
        assert_eq!(measured[0].per_second, Some(30_000.0));
    }

    /// An emit with no implementation says which ones have one, rather than
    /// returning an empty result that reads as "nothing happened".
    #[test]
    fn an_emit_without_an_implementation_names_the_ones_that_have_one() {
        let error = EmitError::NotImplemented {
            emit: "hist:log2".into(),
        };
        let message = error.to_string();
        assert!(message.contains("hist:log2"), "{message}");
        for implemented in ["raw", "timeline", "summary", "rate"] {
            assert!(message.contains(implemented), "{message}");
        }
    }

    #[test]
    fn a_non_numeric_attribute_is_refused_rather_than_counted_as_zero() {
        let mut broken = event(0, 1, 10);
        broken.attrs = vec![Attr::text("bytes", "a lot")];
        assert!(summary(&[broken], "bytes").is_err());
    }
}
