//! Aggregation: from events to what was asked for.
//!
//! An emit is a reduction over the event stream, and each one answers a
//! different question: `raw` keeps everything, `timeline` keeps the shape over
//! time, `summary` keeps the shape over the whole run, `rate` keeps how fast it
//! happened, `hist` keeps the distribution, `topk` keeps who contributed most
//! and `stack` keeps where it happened. They are computed from the same events,
//! so asking for two of them costs one collection.
//!
//! Events are grouped into series by their [`SeriesKey`]. Two processes polled
//! by one request produce two series, never one averaged number: an average
//! across targets is a value that describes nothing that exists.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::catalog::Unit;
use crate::collect::{Collection, Filtered, Losses};
use crate::error::ParseError;
use crate::event::{Event, KeyValue};
use crate::request::{Emit, EmitKind, HistScale};
use crate::resolve::ResolvedRequest;

/// The attribute a collector puts on an event to say which stack it was taken
/// on. A `uint`, and the identity `stack` aggregates by.
pub const STACK_ID_ATTR: &str = "stack_id";

/// The attribute carrying the frames of a stack, root first, separated by `;`
/// -- the same spelling the `folded` format uses.
///
/// A collector need only put it on the first event bearing a given
/// [`STACK_ID_ATTR`], because the frames are stored once in
/// [`Results::stacks`] and every later event with that id shares them. That is
/// the whole reason the id exists: a profile of a deep stack must not cost more
/// to aggregate than it cost to collect.
pub const STACK_ATTR: &str = "stack";

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
    /// What this measurement gave up. Two sources feed it: the resolver, which
    /// knows what a fallback backend costs before anything is attached, and the
    /// collector, which only learns at open time that the machine would not
    /// count kernel mode or that the hardware had to time-share the counters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degradations: Vec<String>,
    /// The frames behind each stack identity seen in this run, root first.
    ///
    /// A side table rather than a field on `Event`: the same stack is seen
    /// thousands of times and stored once, and the event model stays the few
    /// fields it is. `Output::Stack` names ids; this says what they were.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stacks: BTreeMap<u64, Vec<String>>,
}

/// What a series is keyed on.
///
/// A series is every event that shares one *primary key*, and an event's
/// primary key is the first key its collector put on it. The collector decides,
/// because it is the only thing that knows whether a source is naturally
/// per-thread, per-device or per-socket -- grouping page faults by address, for
/// instance, would produce one series per page.
///
/// The convention had no name before `topk` needed one. `topk` ranks series,
/// and "the N heaviest contributors" is only a question once what a contributor
/// *is* has an answer: devices for `io.bytes`, threads for `mem.heap.alloc`,
/// sockets for `net.tcp.rtt`. Every other emit but `raw` and `stack` produces
/// one result per series for the same reason.
///
/// An event with no key at all belongs to the series `system`: it measured the
/// machine, and the machine is one thing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SeriesKey(String);

impl SeriesKey {
    /// The series a keyless event belongs to.
    pub const SYSTEM: &'static str = "system";

    pub fn of(event: &Event) -> Self {
        Self(
            event
                .keys
                .first()
                .map(KeyValue::to_string)
                .unwrap_or_else(|| Self::SYSTEM.to_owned()),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SeriesKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SeriesKey {
    fn from(key: &str) -> Self {
        Self(key.to_owned())
    }
}

impl PartialEq<str> for SeriesKey {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for SeriesKey {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// One emit's worth of results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "emit")]
pub enum Output {
    Raw {
        events: Vec<Event>,
    },
    Timeline {
        series: Vec<Series>,
    },
    Summary {
        series: Vec<SeriesSummary>,
    },
    Rate {
        series: Vec<SeriesRate>,
    },
    Hist {
        layout: BucketLayout,
        series: Vec<SeriesHist>,
    },
    #[serde(rename = "topk")]
    TopK {
        /// How many series were asked for, which is not how many came back:
        /// a run with three series answers `topk:10` with three.
        k: u32,
        series: Vec<SeriesTotal>,
        rest: Rest,
    },
    Stack {
        stacks: Vec<StackCount>,
        /// Events that carried no stack identity at all. A profile assembled
        /// from a tenth of its samples is not a profile, and nothing else here
        /// would say so.
        unattributed: usize,
    },
}

impl Output {
    pub fn kind(&self) -> EmitKind {
        match self {
            Output::Raw { .. } => EmitKind::Raw,
            Output::Timeline { .. } => EmitKind::Timeline,
            Output::Summary { .. } => EmitKind::Summary,
            Output::Rate { .. } => EmitKind::Rate,
            Output::Hist { .. } => EmitKind::Hist,
            Output::TopK { .. } => EmitKind::TopK,
            Output::Stack { .. } => EmitKind::Stack,
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
    pub key: SeriesKey,
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
    pub key: SeriesKey,
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
    pub key: SeriesKey,
    pub count: usize,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub last: f64,
}

/// One key's total, which is what `topk` ranks on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesTotal {
    pub key: SeriesKey,
    pub events: usize,
    pub total: f64,
}

/// What a `topk` left out.
///
/// The top ten of five hundred devices is a true statement about ten devices
/// and says nothing about the other four hundred and ninety. Reporting what
/// they came to is the difference between a ranking and a claim that the rest
/// did not matter.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rest {
    pub series: usize,
    pub events: usize,
    pub total: f64,
}

impl Rest {
    pub fn is_empty(&self) -> bool {
        self.series == 0
    }
}

/// How a histogram's buckets are laid out, carried beside them so a result file
/// says how to read itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketLayout {
    /// Powers of two: bucket 0 holds nothing but the values below one, and
    /// bucket *k* holds `[2^(k-1), 2^k)`.
    ///
    /// This is the layout an eBPF array map builds for free -- one
    /// `log2` of the value, one index, one atomic increment, no per-event
    /// record -- which is what makes moving a histogram into the kernel viable
    /// later. A prettier layout here would have to be abandoned then, and two
    /// runs of the same source would not be comparable across the change.
    Log2,
    /// Fixed-width buckets: bucket *k* holds `[k*width, (k+1)*width)`.
    Linear { width: u64 },
}

impl BucketLayout {
    /// The most buckets a series will be rendered with.
    ///
    /// Only reachable with a linear width far smaller than the spread it is
    /// asked to cover; a log2 histogram of a `u64` has 65 buckets and can never
    /// hit it. Past it the tail folds into one open-ended bucket, which is
    /// visibly unresolved rather than quietly truncated.
    pub const MAX_BUCKETS: u64 = 1024;

    fn index(self, value: f64) -> u64 {
        match self {
            // `floor` first, so that the bucket boundaries stay integers and a
            // fractional value still lands in the range that contains it: 0.9
            // is in `[0, 1)` and 1.5 is in `[1, 2)`.
            BucketLayout::Log2 => {
                let value = value.floor() as u64;
                if value < 1 {
                    0
                } else {
                    u64::from(u64::BITS - value.leading_zeros())
                }
            }
            BucketLayout::Linear { width } => (value / width as f64).floor() as u64,
        }
    }

    /// The half-open range a bucket covers. `None` for the upper bound of a
    /// bucket that runs off the end of a `u64`.
    fn range(self, index: u64) -> (u64, Option<u64>) {
        match self {
            BucketLayout::Log2 => match index {
                0 => (0, Some(1)),
                // A `u64` has no value above this and `index` never exceeds it,
                // but a shift that would overflow is a panic rather than a
                // wrong answer, so the top is written as a range and not as an
                // unreachable case.
                k if k >= 64 => (1 << 63, None),
                k => (1 << (k - 1), Some(1 << k)),
            },
            BucketLayout::Linear { width } => {
                let low = index.saturating_mul(width);
                (low, low.checked_add(width))
            }
        }
    }
}

impl From<HistScale> for BucketLayout {
    fn from(scale: HistScale) -> Self {
        match scale {
            HistScale::Log2 => BucketLayout::Log2,
            HistScale::Linear { width } => BucketLayout::Linear { width },
        }
    }
}

impl fmt::Display for BucketLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BucketLayout::Log2 => f.write_str("log2"),
            BucketLayout::Linear { width } => write!(f, "linear:{width}"),
        }
    }
}

impl FromStr for BucketLayout {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().split_once(':') {
            None if s.trim() == "log2" => Ok(BucketLayout::Log2),
            Some(("linear", width)) => width
                .parse()
                .map(|width| BucketLayout::Linear { width })
                .map_err(|_| ParseError::new(format!("`{width}` is not a bucket width"))),
            _ => Err(ParseError::new(format!(
                "unknown bucket layout `{s}` (expected log2 or linear:<width>)"
            ))),
        }
    }
}

serde_as_string!(BucketLayout);

/// One bucket of a histogram, `[low, high)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    pub low: u64,
    /// Exclusive. `None` when the bucket is open-ended: the top of a `u64`, or
    /// the folded tail of a linear histogram too wide to render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<u64>,
    pub count: usize,
}

/// One key's values as a distribution.
///
/// The buckets run from the lowest non-empty one to the highest, empty ones in
/// between included: a gap between two modes is the thing a histogram is read
/// for, and a renderer that skipped it would turn a bimodal distribution back
/// into the single blur the mean already was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesHist {
    pub key: SeriesKey,
    /// How many values went into the buckets.
    pub count: usize,
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub buckets: Vec<Bucket>,
    /// Values no bucket can hold: negative, or not a number. There is no bucket
    /// below zero and inventing one would put a value in a range it is not in,
    /// so they are counted here instead and left out of `count`, `min`, `max`
    /// and `sum`.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub unbucketable: usize,
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

/// One stack identity and what was measured on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackCount {
    pub stack: u64,
    pub events: usize,
    pub total: f64,
}

/// What an emit cannot be computed from.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EmitError {
    #[error("`{attr}` is not a numeric attribute, so it cannot be aggregated")]
    NotNumeric { attr: String },
    /// Reachable because `hist:linear:0` parses. It is refused rather than
    /// bucketed: every value would divide into the same bucket of zero width,
    /// which is not a histogram of anything.
    #[error("a linear histogram needs a bucket width above zero, as in `hist:linear:1ms`")]
    ZeroBucketWidth,
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

    // What the resolver knew, plus what the collectors found out once they were
    // attached. Both are degradations of the same measurement.
    let mut degradations = resolved.degradations.clone();
    degradations.extend(collection.notes.iter().cloned());

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
            Emit::Hist(scale) => {
                let layout = BucketLayout::from(*scale);
                Output::Hist {
                    layout,
                    series: hist(&collection.events, value_attr, layout)?,
                }
            }
            Emit::TopK(k) => topk(&collection.events, value_attr, *k)?,
            Emit::Stack => stack(&collection.events, value_attr)?,
        });
    }

    // Only walked when something asked for stacks: it is a scan of every event
    // for a table nothing else reads.
    let stacks = if request.emit.contains(&Emit::Stack) {
        stack_table(&collection.events)
    } else {
        BTreeMap::new()
    };

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
        degradations,
        stacks,
    })
}

/// Groups events by their [`SeriesKey`], keeping the order they arrived in.
fn grouped(events: &[Event]) -> BTreeMap<SeriesKey, Vec<&Event>> {
    let mut groups: BTreeMap<SeriesKey, Vec<&Event>> = BTreeMap::new();
    for event in events {
        groups.entry(SeriesKey::of(event)).or_default().push(event);
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

fn hist(events: &[Event], attr: &str, layout: BucketLayout) -> Result<Vec<SeriesHist>, EmitError> {
    if layout == (BucketLayout::Linear { width: 0 }) {
        return Err(EmitError::ZeroBucketWidth);
    }
    let mut histograms = Vec::new();
    for (key, events) in grouped(events) {
        let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
        let mut unbucketable = 0;
        let mut count = 0;
        let mut sum = 0.0;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for event in events {
            let value = value_of(event, attr)?;
            if !value.is_finite() || value < 0.0 {
                unbucketable += 1;
                continue;
            }
            *counts.entry(layout.index(value)).or_insert(0) += 1;
            count += 1;
            sum += value;
            min = min.min(value);
            max = max.max(value);
        }
        if count == 0 && unbucketable == 0 {
            continue;
        }
        histograms.push(SeriesHist {
            key,
            count,
            min: if count == 0 { 0.0 } else { min },
            max: if count == 0 { 0.0 } else { max },
            sum,
            buckets: buckets(&counts, layout),
            unbucketable,
        });
    }
    Ok(histograms)
}

/// Lays the counted indices out as a contiguous run of buckets.
fn buckets(counts: &BTreeMap<u64, usize>, layout: BucketLayout) -> Vec<Bucket> {
    let (Some(lowest), Some(highest)) = (
        counts.keys().next().copied(),
        counts.keys().next_back().copied(),
    ) else {
        return Vec::new();
    };

    let bucket = |index: u64, count: usize| {
        let (low, high) = layout.range(index);
        Bucket { low, high, count }
    };

    let span = (highest - lowest).saturating_add(1);
    if span <= BucketLayout::MAX_BUCKETS {
        return (lowest..=highest)
            .map(|index| bucket(index, counts.get(&index).copied().unwrap_or(0)))
            .collect();
    }

    // Too wide to render bucket by bucket: everything from the last one on
    // folds into a single open-ended bucket. It is one unresolved range rather
    // than a truncated histogram, so nothing is counted twice and nothing is
    // dropped.
    let last = lowest.saturating_add(BucketLayout::MAX_BUCKETS - 1);
    let mut rendered: Vec<Bucket> = (lowest..last)
        .map(|index| bucket(index, counts.get(&index).copied().unwrap_or(0)))
        .collect();
    rendered.push(Bucket {
        low: layout.range(last).0,
        high: None,
        count: counts.range(last..).map(|(_, count)| count).sum(),
    });
    rendered
}

fn topk(events: &[Event], attr: &str, k: u32) -> Result<Output, EmitError> {
    let mut totals = Vec::new();
    for (key, events) in grouped(events) {
        let mut total = 0.0;
        for event in &events {
            total += value_of(event, attr)?;
        }
        totals.push(SeriesTotal {
            key,
            events: events.len(),
            total,
        });
    }

    // Heaviest first, and by key where two are equal: a ranking that reorders
    // itself between two runs of the same workload is not a ranking.
    totals.sort_by(|left, right| {
        right
            .total
            .partial_cmp(&left.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.key.cmp(&right.key))
    });

    let dropped = totals.split_off((k as usize).min(totals.len()));
    Ok(Output::TopK {
        k,
        series: totals,
        rest: Rest {
            series: dropped.len(),
            events: dropped.iter().map(|series| series.events).sum(),
            total: dropped.iter().map(|series| series.total).sum(),
        },
    })
}

/// Aggregates by stack identity rather than by frame list.
///
/// A deep stack is a long string; the same stack seen ten thousand times is one
/// `u64` compared ten thousand times, and the frames are looked up once in
/// [`Results::stacks`].
fn stack(events: &[Event], attr: &str) -> Result<Output, EmitError> {
    let mut by_stack: BTreeMap<u64, (usize, f64)> = BTreeMap::new();
    let mut unattributed = 0;
    for event in events {
        let Some(id) = stack_id(event) else {
            unattributed += 1;
            continue;
        };
        let entry = by_stack.entry(id).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += value_of(event, attr)?;
    }

    let mut stacks: Vec<StackCount> = by_stack
        .into_iter()
        .map(|(stack, (events, total))| StackCount {
            stack,
            events,
            total,
        })
        .collect();
    stacks.sort_by(|left, right| {
        right
            .total
            .partial_cmp(&left.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.stack.cmp(&right.stack))
    });

    Ok(Output::Stack {
        stacks,
        unattributed,
    })
}

fn stack_id(event: &Event) -> Option<u64> {
    match event.attr(STACK_ID_ATTR)? {
        crate::event::AttrValue::Uint(id) => Some(*id),
        crate::event::AttrValue::Int(id) => u64::try_from(*id).ok(),
        _ => None,
    }
}

/// Collects the frames behind every stack identity the run saw.
fn stack_table(events: &[Event]) -> BTreeMap<u64, Vec<String>> {
    let mut table: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for event in events {
        let Some(id) = stack_id(event) else { continue };
        if table.contains_key(&id) {
            continue;
        }
        let Some(frames) = event.attr(STACK_ATTR) else {
            continue;
        };
        let frames = frames.to_string();
        if frames.is_empty() {
            continue;
        }
        table.insert(
            id,
            frames.split(';').map(|frame| frame.to_owned()).collect(),
        );
    }
    table
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

    #[test]
    fn a_non_numeric_attribute_is_refused_rather_than_counted_as_zero() {
        let mut broken = event(0, 1, 10);
        broken.attrs = vec![Attr::text("bytes", "a lot")];
        assert!(summary(&[broken], "bytes").is_err());
    }

    /// An event with no key is not an error and not a series per event: it
    /// measured the machine.
    #[test]
    fn an_event_with_no_key_belongs_to_the_system_series() {
        let mut keyless = event(0, 1, 10);
        keyless.keys.clear();
        assert_eq!(SeriesKey::of(&keyless), SeriesKey::from(SeriesKey::SYSTEM));
    }

    /// The bucket boundaries are the ones an eBPF array map indexed by
    /// `log2(value)` would produce, because that is where this histogram has to
    /// be computable later without changing what it means.
    #[test]
    fn log2_buckets_are_the_ones_a_kernel_map_would_build() {
        let layout = BucketLayout::Log2;
        assert_eq!((layout.index(0.0), layout.range(0)), (0, (0, Some(1))));
        assert_eq!((layout.index(1.0), layout.range(1)), (1, (1, Some(2))));
        assert_eq!((layout.index(2.0), layout.range(2)), (2, (2, Some(4))));
        assert_eq!((layout.index(3.0), layout.range(2)), (2, (2, Some(4))));
        assert_eq!((layout.index(4.0), layout.range(3)), (3, (4, Some(8))));
        assert_eq!(
            (layout.index(1023.0), layout.range(10)),
            (10, (512, Some(1024)))
        );
        assert_eq!(
            (layout.index(1024.0), layout.range(11)),
            (11, (1024, Some(2048)))
        );
        // A fractional value lands in the range that actually contains it.
        assert_eq!(layout.index(0.9), 0);
        assert_eq!(layout.index(1.5), 1);
    }

    #[test]
    fn linear_buckets_are_floor_divided_by_the_width() {
        let layout = BucketLayout::Linear { width: 1_000_000 };
        assert_eq!(layout.index(0.0), 0);
        assert_eq!(layout.index(999_999.0), 0);
        assert_eq!(layout.index(1_000_000.0), 1);
        assert_eq!(layout.range(3), (3_000_000, Some(4_000_000)));
    }

    /// The reason a histogram exists: a mean cannot say that a run had two
    /// modes, and here it actively lies -- the mean falls in a bucket where
    /// nothing was ever measured.
    #[test]
    fn a_bimodal_distribution_keeps_both_of_its_modes_and_the_empty_gap() {
        let mut events = Vec::new();
        for _ in 0..100 {
            events.push(event(0, 1, 1_000));
        }
        for _ in 0..2 {
            events.push(event(0, 1, 1_000_000));
        }

        let series = hist(&events, "bytes", BucketLayout::Log2).unwrap();
        assert_eq!(series.len(), 1);
        let histogram = &series[0];
        assert_eq!(histogram.count, 102);

        let bucket = |value: u64| {
            histogram
                .buckets
                .iter()
                .find(|bucket| bucket.low <= value && bucket.high.is_none_or(|high| value < high))
                .unwrap_or_else(|| panic!("no bucket holds {value}: {:?}", histogram.buckets))
        };
        assert_eq!(bucket(1_000).count, 100);
        assert_eq!(bucket(1_000_000).count, 2);

        let mean = histogram.sum / histogram.count as f64;
        assert_eq!(
            bucket(mean as u64).count,
            0,
            "the mean is {mean}, and nothing was ever measured near it"
        );

        // The gap is rendered rather than skipped: between the two modes the
        // buckets are present and empty, which is what makes the shape visible.
        let between = histogram
            .buckets
            .iter()
            .filter(|bucket| {
                bucket.low > 1_000 && bucket.high.is_some_and(|high| high <= 1_000_000)
            })
            .count();
        assert!(between > 0, "{:?}", histogram.buckets);
    }

    /// There is no bucket below zero, and putting a negative value in `[0, 1)`
    /// would be reporting it as a value it is not.
    #[test]
    fn a_value_no_bucket_can_hold_is_counted_rather_than_placed() {
        let mut negative = event(0, 1, 0);
        negative.attrs = vec![Attr::int("bytes", -5)];
        let events = vec![event(0, 1, 8), negative];

        let series = hist(&events, "bytes", BucketLayout::Log2).unwrap();
        assert_eq!(series[0].count, 1);
        assert_eq!(series[0].unbucketable, 1);
        assert_eq!(
            series[0].buckets.iter().map(|b| b.count).sum::<usize>(),
            1,
            "the negative value is nowhere in the buckets"
        );
    }

    /// A linear width far below the spread would need millions of buckets. The
    /// tail becomes one open-ended bucket: unresolved, and visibly so.
    #[test]
    fn a_histogram_too_wide_to_render_folds_its_tail_rather_than_dropping_it() {
        let events = vec![event(0, 1, 0), event(0, 1, 10_000_000)];
        let series = hist(&events, "bytes", BucketLayout::Linear { width: 1 }).unwrap();
        let buckets = &series[0].buckets;
        assert_eq!(buckets.len() as u64, BucketLayout::MAX_BUCKETS);
        let last = buckets.last().unwrap();
        assert_eq!(last.high, None, "the folded tail is open-ended");
        assert_eq!(
            buckets.iter().map(|b| b.count).sum::<usize>(),
            2,
            "nothing was dropped and nothing was counted twice"
        );
    }

    /// `hist:linear:0` parses, and every value divides into one bucket of zero
    /// width. That is not a histogram of anything, so it is refused rather than
    /// rendered.
    #[test]
    fn a_linear_histogram_of_zero_width_is_refused_rather_than_bucketed() {
        let events = vec![event(0, 1, 10)];
        assert_eq!(
            hist(&events, "bytes", BucketLayout::Linear { width: 0 }),
            Err(EmitError::ZeroBucketWidth)
        );
    }

    /// `topk` ranks series, and what it leaves out is reported: the top two of
    /// five is a true statement about two and says nothing about the other
    /// three unless it says what they came to.
    #[test]
    fn topk_ranks_series_and_accounts_for_the_ones_it_left_out() {
        let events = vec![
            event(0, 1, 10),
            event(0, 2, 300),
            event(0, 3, 50),
            event(0, 3, 50),
            event(0, 4, 1),
        ];
        let Output::TopK { series, rest, .. } = topk(&events, "bytes", 2).unwrap() else {
            panic!("topk produces a topk");
        };
        assert_eq!(series[0].key, "task:2");
        assert_eq!(series[0].total, 300.0);
        assert_eq!(series[1].key, "task:3");
        assert_eq!(
            (series[1].total, series[1].events),
            (100.0, 2),
            "a series is the sum of its events, not its last one"
        );
        assert_eq!((rest.series, rest.events, rest.total), (2, 2, 11.0));
    }

    /// Two series with the same total must come back in the same order every
    /// run, or a ranking is not reproducible.
    #[test]
    fn a_tie_is_broken_by_key_rather_than_by_arrival() {
        let events = vec![event(0, 9, 5), event(0, 2, 5)];
        let Output::TopK { series, .. } = topk(&events, "bytes", 10).unwrap() else {
            panic!("topk produces a topk");
        };
        assert_eq!(
            series.iter().map(|s| s.key.to_string()).collect::<Vec<_>>(),
            ["task:2", "task:9"]
        );
    }

    fn sampled(tid: i32, stack_id: u64, frames: Option<&str>, value: u64) -> Event {
        let mut event = event(0, tid, value);
        event.attrs.push(Attr::uint(STACK_ID_ATTR, stack_id));
        if let Some(frames) = frames {
            event.attrs.push(Attr::text(STACK_ATTR, frames));
        }
        event
    }

    /// Aggregation is by identity: the frames are compared once, when the table
    /// is built, and never again.
    #[test]
    fn stacks_aggregate_by_identity_and_the_frames_are_stored_once() {
        let mut events = vec![sampled(1, 7, Some("main;work;read"), 10)];
        for _ in 0..9 {
            events.push(sampled(1, 7, None, 10));
        }
        events.push(sampled(1, 8, Some("main;idle"), 1));

        let Output::Stack {
            stacks,
            unattributed,
        } = stack(&events, "bytes").unwrap()
        else {
            panic!("stack produces a stack");
        };
        assert_eq!(unattributed, 0);
        assert_eq!(stacks[0].stack, 7);
        assert_eq!((stacks[0].events, stacks[0].total), (10, 100.0));
        assert_eq!(stacks[1].stack, 8);

        let table = stack_table(&events);
        assert_eq!(table.len(), 2, "one entry per identity, not per event");
        assert_eq!(table[&7], ["main", "work", "read"]);
    }

    /// A profile assembled from a tenth of its samples is not a profile, and
    /// the count of what carried no stack is the only thing that would say so.
    #[test]
    fn events_without_a_stack_are_counted_rather_than_silently_absent() {
        let events = vec![sampled(1, 7, Some("main"), 10), event(0, 1, 10)];
        let Output::Stack { unattributed, .. } = stack(&events, "bytes").unwrap() else {
            panic!("stack produces a stack");
        };
        assert_eq!(unattributed, 1);
    }
}
