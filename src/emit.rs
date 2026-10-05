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
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::catalog::Unit;
use crate::collect::{Collection, Filtered, Losses};
use crate::event::{Event, KeyValue};
use crate::request::{AttrName, Emit, EmitKind, KeyKind};
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

/// The attribute that says how many measurements an event stands for. Absent
/// means one, which is what every event of a streaming collector is.
///
/// A collector that aggregates in the kernel does not hand up one event per
/// measurement -- that is the cost it exists to avoid. `sched.runqlat` drains
/// `(task, bucket) -> count` from a BPF map and publishes one event per row, so
/// a row standing for four hundred waits must count as four hundred and not as
/// one. Weighing every event as one made the buckets come out in the right
/// places, each one unit high, which is a histogram of the *shape* of the
/// kernel's map rather than of the workload.
///
/// Deliberately not `count`. That name is already the measured value of eleven
/// sources, and for a `pmu` counter it holds billions: reading it as a weight
/// would not be a rounding error. A name no collector used before this is also
/// what makes the change invisible to every source that does not want it.
pub const WEIGHT_ATTR: &str = "weight";

/// The attribute that says an event's value is the *sum* over that many
/// measurements, rather than one measurement repeated that many times.
///
/// The distinction is the whole difference between a histogram and a lie, and
/// [`WEIGHT_ATTR`] cannot express it. A `sched.runqlat` row is
/// `(task, bucket) -> count`: every wait behind it had a value inside that
/// bucket, so weighing the row by its count puts each of them where it belongs.
/// A `sched.offcpu` row is `(task, state, stack) -> total_ns` drained on a tick:
/// the individual blocks are added up in the kernel and their values are gone.
/// Weighing that row by its block count would not recover them -- it would put
/// four hundred blocks in the bucket of their *sum*, which is a range no single
/// one of them was in.
///
/// So a collector that carries this is saying the values behind the number do
/// not exist up here. `raw`, `rate`, `timeline` and `topk` are unaffected, since
/// a total is exactly what they add up or plot. `hist` and `summary` are of the
/// rows rather than of the workload, and [`aggregate`] says so with the results
/// -- which is what lets a reader tell the two shapes apart, `runqlat`'s
/// buckets from `offcpu`'s drain windows, when the output does not otherwise.
///
/// The fix is always to move the bucketing into the kernel, as `runqlat` does,
/// and then the row carries [`WEIGHT_ATTR`] and this attribute goes away.
pub const TOTAL_OF_ATTR: &str = "total_of";

/// The attribute a weighted row built in the kernel carries beside its
/// bucketed value: the exact sum, in nanoseconds, of the measurements behind
/// the row.
///
/// The row's value is its bucket's low bound -- the one value the log2
/// layout maps back to that bucket -- so `value * weight` puts every
/// measurement at the bottom of its bucket, up to a factor of two short. A
/// sum, a mean, a rate or a ranking built from it is a lower bound that says
/// nothing of being one. Every emit that adds values up -- `hist`'s sum,
/// `summary`'s mean, `rate`, `topk` and `stack` -- takes this instead when the
/// row carries it (see `weighed`). The buckets and `min` stay the bucket
/// bounds they are; `max` is raised to a row's exact mean where that is above
/// its low bound, so it is never below the mean beside it (see `highest`).
pub const SUM_NS_ATTR: &str = "total_ns";

/// What a collector counted about the run, by name: a count a caveat or a
/// fact would otherwise only say in a sentence (`net.request`'s
/// `never_ended`, `unborn`, `replaced`, `filtered_out`). Counts, so whole
/// numbers; ordered, so a file written twice reads the same.
pub type Fields = BTreeMap<String, u64>;

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
    /// When this request was watching, inside the run: its own window when the
    /// request carried one, the run's otherwise.
    ///
    /// `observed_ns` says for how long, and is what every rate is over; this
    /// says *where*, which is what a reader comparing two requests needs --
    /// two seconds of a ten-second run at its start and at its end are
    /// different measurements of the same length -- and what a join across a
    /// detached source is decided by.
    #[serde(default)]
    pub window: crate::collect::Watched,
    pub stopped_because: String,
    pub losses: Losses,
    /// What the request's filter removed after the fact, when the resolver
    /// could not push it down. Absent when the request carried no filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered: Option<Filtered>,
    /// When the run's launched program ran, from the gate to its exit. Absent
    /// when nothing was launched.
    ///
    /// It is the *whole* a decomposition's parts are parts of, which is why it
    /// belongs beside the numbers rather than in the resolved file: a residue
    /// is a statement about a total, and a total nobody carries cannot be
    /// checked. The exit is bracketed rather than exact -- `exited_err_ns` is
    /// how long before `exited_ns` the program was last seen alive -- and both
    /// travel, because a reader given only the first would over-trust a
    /// residue smaller than the bracket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime: Option<crate::collect::Lifetime>,
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
    /// What stating the facts of this measurement took that is not a
    /// concession: a count another party kept of the same thing, a window the
    /// request asked for. Beside `degradations` rather than in it, because a
    /// collector saying "the kernel counted 400 handler runs and here are all
    /// of them" is the strongest thing it can say about its own completeness,
    /// and under `degraded:` it read as an apology (`open-requests 3892`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// What this measurement cost, measured rather than declared: the tool's
    /// cpu and what the collector spent inside the target, as `cost.json` has
    /// it, so a pipeline reading the results alone can tell a 0.1 %
    /// measurement from a 9 % one (`open-requests 2316`).
    #[serde(default)]
    pub overhead: crate::collect::Overhead,
    /// The backends that produced these results, in order: longer than one
    /// when a budget moved the collection mid-run, and then `backend` is the
    /// last of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub served_by: Vec<String>,
    /// The interval between samples, for a sampling mode; absent for every
    /// other, which has no period. What `pprof` writes as `period`
    /// (`open-requests 2127`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_ns: Option<u64>,
    /// The part of `losses` each ring lost, for a collector that reads one
    /// ring per key: what [`Shortfalls`] places on each series.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_ring: Vec<crate::collect::RingLoss>,
    /// The attribute each event's value was read from (`latency_ns`, `bytes`,
    /// ...): what every emit above reduced, named so a reader reads a source's
    /// value by name from the run rather than from a table of its own.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value_attr: String,
    /// How the collector stamped a row it read from a table the kernel
    /// filled between drains, when it says: a reader places such a row's
    /// window by this rather than by a table of its own. Absent says nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_stamp: Option<crate::collect::RowStamp>,
    /// The kernel's drops placed in time: each drain that found the count
    /// grown, with what it had grown by. They sum to `losses.dropped` for a
    /// run (a recording's document holds the drains read in its window).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drains: Vec<crate::collect::DrainDrop>,
    /// What the collector counted about the run, as numbers a program reads
    /// by name ([`Fields`], `Collector::fields`). Absent when it counted
    /// nothing, which is most collectors.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: Fields,
    /// What `stacks`' labels cannot say -- each frame's `named_by`, the
    /// `perf_maps` that named frames, each task's `executables` -- written
    /// beside them, each only when not empty.
    #[serde(flatten)]
    pub provenance: crate::stack::Provenance,
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

    /// The series an event belongs to when the request names what to group
    /// by: one part per requested key, in the order asked, joined by a space.
    /// An event that carries none of a requested kind is `<kind>:?` in that
    /// part rather than folded into a neighbour's series. With no keys asked
    /// for this is [`SeriesKey::of`].
    pub fn by(event: &Event, axes: &[crate::request::KeyKind]) -> Self {
        if axes.is_empty() {
            return Self::of(event);
        }
        let parts: Vec<String> = axes
            .iter()
            .map(|axis| {
                event
                    .keys
                    .iter()
                    .find(|key| key.kind() == *axis)
                    .map(KeyValue::to_string)
                    .unwrap_or_else(|| format!("{}:?", axis.as_str()))
            })
            .collect();
        Self(parts.join(" "))
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
        /// The moment every point's `offset_ns` is measured from, on
        /// `CLOCK_MONOTONIC`: the result's first event. Without it two
        /// timelines cannot be laid on one axis, since each starts at its own
        /// first event and not at the run's opening.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_ts_ns: Option<u64>,
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
        /// What the run did not see, when it lost anything.
        ///
        /// Beside `unattributed` and not folded into it, because the two are
        /// different facts: `unattributed` counts events that arrived carrying
        /// no stack, and this counts records that never arrived. One says the
        /// profile is missing its labels and the other says it is missing its
        /// samples, and a reader given one number for both could not tell which
        /// profile they were holding.
        ///
        /// Once per table rather than once per stack: a ring drops without
        /// regard to which stack the record would have joined.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        short: Option<Shortfall>,
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
    /// Set when `window_ns` is the counter's own enabled time rather than the
    /// run's: a counted source puts `enabled_ns` on its event, which is the
    /// window the counter measured -- for a per-task counter, the time the
    /// task was on a cpu -- and the wall clock also holds the time between
    /// the last read and the run noticing it was over (`open-requests 168`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub counted_window: bool,
    /// What the run did not see, when it lost anything.
    ///
    /// A rate from a lossy run is a **lower bound**, and it is biased the one
    /// way: the records a ring drops are the ones from the busiest moment, so
    /// the rate is understated exactly where it is highest. Finding 37 scoped
    /// itself to distributions; this is the same statement about a quotient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
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

/// What a distribution built from a lossy run is missing, and which way that
/// biases it.
///
/// A histogram of a run that dropped events renders exactly like a histogram
/// of a complete one, and the events it is missing are **the slow ones**:
/// whatever stalled the machine is what stopped the measuring. So a percentile
/// taken off it is a lower bound and not an estimate, and nothing in the
/// numbers themselves says so -- the loss line is printed, and is correct, and
/// is several lines away and phrased as a count of events rather than as a
/// statement about the shape.
///
/// This is that statement, carried on every series the bias reaches so that it
/// arrives beside the value rather than above it. It is derived and adds no
/// counter: `dropped` and `throttled` are the two of the five counted losses
/// that bias a distribution, and both were already counted.
///
/// Two of the five are deliberately not here. `late_ticks` and `failed_reads`
/// belong to `poll`, and `poll` cannot produce `hist` at all -- `CATALOG.md`
/// §12 refuses it, because a histogram of a polled gauge is a histogram of
/// when the poller happened to look, which is the same argument under another
/// name. `undecoded` is a record shape this build does not parse, and there is
/// no reason to think the shapes it cannot parse are the slow ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shortfall {
    /// Records the ring reported losing. A count, so it yields a fraction.
    pub dropped: u64,
    /// Windows in which the kernel declined to deliver, which is not a count
    /// of events: how many happened inside one is not recorded anywhere, and
    /// `DECISIONS.md` chose to say so rather than to estimate it.
    pub throttled: u64,
    /// Records the distribution was actually built from. Run-wide, like the
    /// losses it is weighed against: a ring drops without regard to which
    /// series the record would have joined, so splitting the fraction per
    /// series would be attributing the drops to one.
    pub records: u64,
    /// `dropped / (dropped + records)`. Absent when nothing was dropped, which
    /// is the throttled case: a fraction there would be an invention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f64>,
    /// Whether a record here stood for more than one measurement, which makes
    /// the fraction a fraction of *records* and not of the distribution.
    ///
    /// This is the line between [`WEIGHT_ATTR`] and a loss, and the two must
    /// never be conflated: a bucket weighted four hundred is complete, and a
    /// run that dropped four hundred records is not. When rows are weighted
    /// both are true at once and the second number is unknowable -- the
    /// dropped rows carried weights nobody up here ever saw.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub per_record: bool,
    /// Events a recording read and evicted for its retention bound before
    /// this window opened, and put in no document.
    ///
    /// The one count here that does *not* make a value a lower bound: the
    /// window's events are all present, and what was evicted is older than
    /// it. It is here because it is what the run saw and let go, and a
    /// distribution cut out of a longer run says so beside its numbers.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub evicted: u64,
    /// The whole statement, in one sentence, so that a consumer of the
    /// structured output has it without deriving it from the fields above.
    pub note: String,
}

/// What each series of one collection is short by.
///
/// The run's shortfall, on every series alike, is the answer whenever a
/// loss cannot be placed: a single ring drops without regard to which series
/// the record would have joined. A collector that reads **one ring per key**
/// can place it -- a profile that follows each thread opens a ring for each --
/// and when every loss the run counted belongs to one of those rings, and the
/// series are keyed by that key, a series is short by its own ring's losses
/// and by nothing else. The thread that overflowed its ring is marked; the
/// thread beside it, which lost nothing, is not.
pub(crate) struct Shortfalls {
    /// The run's, for the emits that are not per series (`stack`) and for
    /// every series when the losses cannot be placed.
    pub(crate) run: Option<Shortfall>,
    /// Per ring: the ring's key as a series-key part, its losses, and how many
    /// events carry that key. `None` when the losses are the run's alone.
    rings: Option<Vec<(String, Losses, u64)>>,
    per_record: bool,
}

impl Shortfalls {
    /// The run's shortfall on every series, as before rings were told apart.
    #[cfg(test)]
    pub(crate) fn run_wide(short: Option<&Shortfall>) -> Self {
        Self {
            run: short.cloned(),
            rings: None,
            per_record: false,
        }
    }

    pub(crate) fn of(collection: &Collection, axes: &[KeyKind]) -> Self {
        let per_record = per_record(&collection.events);
        let run = Shortfall::of(
            &collection.losses,
            collection.events.len() as u64,
            per_record,
        );
        let placed = |losses: &Losses| {
            let rings = &collection.by_ring;
            let dropped: u64 = rings.iter().map(|ring| ring.losses.dropped).sum();
            let throttled: u64 = rings.iter().map(|ring| ring.losses.throttled).sum();
            !rings.is_empty()
                && dropped == losses.dropped
                && throttled == losses.throttled
                && losses.evicted == 0
        };
        // A series is one ring's only when its key names the ring's key: the
        // grouping asked for includes that kind, or -- with none asked for --
        // every event's primary key is of it.
        let keyed_by_ring = collection.by_ring.iter().all(|ring| {
            let kind = ring.key.kind();
            if axes.is_empty() {
                collection
                    .events
                    .iter()
                    .all(|event| event.keys.first().is_some_and(|key| key.kind() == kind))
            } else {
                axes.contains(&kind)
            }
        });
        let rings = (placed(&collection.losses) && keyed_by_ring).then(|| {
            collection
                .by_ring
                .iter()
                .map(|ring| {
                    let records = collection
                        .events
                        .iter()
                        .filter(|event| event.keys.contains(&ring.key))
                        .count() as u64;
                    (ring.key.to_string(), ring.losses.clone(), records)
                })
                .collect()
        });
        Self {
            run,
            rings,
            per_record,
        }
    }

    /// What the series `key` is short by.
    pub(crate) fn for_series(&self, key: &SeriesKey) -> Option<Shortfall> {
        let Some(rings) = &self.rings else {
            return self.run.clone();
        };
        let parts: Vec<&str> = key.as_str().split(' ').collect();
        let mut losses = Losses::default();
        let mut records = 0;
        for (ring, lost, carried) in rings {
            if parts.contains(&ring.as_str()) {
                losses.dropped += lost.dropped;
                losses.throttled += lost.throttled;
                records += carried;
            }
        }
        Shortfall::of(&losses, records, self.per_record)
    }
}

impl Shortfall {
    /// What goes beside a value this shortfall makes a lower bound.
    ///
    /// One token, and it is an inequality rather than a symbol a reader has to
    /// look up: the number printed after it is the smallest the measurement
    /// could have been, and the true one is somewhere above.
    pub const MARK: &'static str = ">=";

    /// The shortfall of a run, or nothing when the run lost nothing that
    /// biases a distribution.
    ///
    /// A clean run gains nothing at all: no field, no mark, no line. That is
    /// not a convenience -- a mark that appears on every run is a mark readers
    /// learn to skip, and then the one run that needed it is the one nobody
    /// reads it on.
    pub fn of(losses: &Losses, records: u64, per_record: bool) -> Option<Self> {
        if losses.dropped == 0 && losses.throttled == 0 && losses.evicted == 0 {
            return None;
        }
        let fraction =
            (losses.dropped > 0).then(|| losses.dropped as f64 / (losses.dropped + records) as f64);
        let mut note = if losses.dropped == 0 && losses.throttled == 0 {
            String::new()
        } else {
            Self::describe(
                losses.dropped,
                losses.throttled,
                records,
                fraction,
                per_record,
            )
        };
        if losses.evicted > 0 {
            if !note.is_empty() {
                note.push_str(". And ");
            }
            let _ = write!(
                note,
                "{} earlier events were read and evicted by the recording's retention before \
                 this window opened, and are in no document: this is the distribution of the \
                 window, which holds every event it saw, and not of the run",
                losses.evicted
            );
        }
        Some(Self {
            dropped: losses.dropped,
            throttled: losses.throttled,
            records,
            fraction,
            per_record,
            evicted: losses.evicted,
            note,
        })
    }

    /// Whether this shortfall makes a value a lower bound: something inside
    /// the span was lost. An eviction alone does not -- it is older than the
    /// window -- and carries its sentence without the mark.
    pub fn bounds_below(&self) -> bool {
        self.dropped > 0 || self.throttled > 0
    }

    fn describe(
        dropped: u64,
        throttled: u64,
        records: u64,
        fraction: Option<f64>,
        per_record: bool,
    ) -> String {
        let thing = if per_record { "rows" } else { "events" };
        let mut note = String::new();

        if let Some(fraction) = fraction {
            let _ = write!(
                note,
                "short by at least {dropped} of {} {thing} ({:.1} % unaccounted for)",
                dropped + records,
                fraction * 100.0,
            );
        } else {
            let _ = write!(note, "short by an unknown number of {thing}");
        }

        if throttled > 0 {
            let times = if throttled == 1 { "once" } else { "times" };
            let count = if throttled == 1 {
                String::new()
            } else {
                format!("{throttled} ")
            };
            let and_by = if fraction.is_some() {
                ", and by an unknown number more"
            } else {
                ""
            };
            let _ = write!(
                note,
                "{and_by}: the kernel throttled this event {count}{times} and how many events \
                 happened inside those windows is not recorded anywhere"
            );
        } else {
            let _ = write!(note, ": the ring had nowhere to put them");
        }

        if per_record {
            let _ = write!(
                note,
                ". Each row stands for more than one measurement, so the fraction is of the \
                 rows this backend handed up and not of the distribution: what the dropped rows \
                 weighed is not knowable up here"
            );
        }

        let _ = write!(
            note,
            ". What stalls a machine is what stops the measuring, so the {thing} it is missing \
             are the slow ones, and every count, mean and maximum marked `{}` is a lower bound \
             rather than an estimate",
            Self::MARK
        );
        note
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// One key's values reduced to the shape of the whole run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesSummary {
    pub key: SeriesKey,
    pub count: usize,
    /// The smallest value seen; over rows bucketed in the kernel, the lowest
    /// occupied bucket's low bound, which no measurement is below.
    pub min: f64,
    /// The largest value seen; over rows bucketed in the kernel, a lower
    /// bound on the largest measurement, as [`SeriesHist::max`] is, and like
    /// it never below `mean`.
    pub max: f64,
    /// Exact over rows bucketed in the kernel when they carry
    /// [`SUM_NS_ATTR`]; see [`SeriesHist::sum`].
    pub mean: f64,
    pub last: f64,
    /// What the measurements add up to: `count * mean`, exactly, so that a
    /// total -- the bytes a program allocated, a pid's cpu over the run, a
    /// counter's packets -- is read rather than recomputed (heap 4, top 1).
    /// Absent where the values are levels rather than increments, whose sum
    /// means nothing: `mem.rss` polled fifty times is not fifty footprints.
    /// Which is which is the catalog's, [`values_add_up`]. A lower bound when
    /// `short` is present, as `count` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// Events that carried no value at all: measured by the collector as
    /// having happened, with nothing for this emit to aggregate -- an L1 hit
    /// that AMD's IBS samples by level and does not time. Counted rather than
    /// refused, and left out of every other number here.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub unmeasured: usize,
    /// What this run lost, when it lost something that biases the shape.
    ///
    /// Beside the numbers rather than above them: `count`, `mean` and `max`
    /// are lower bounds when it is present, and a reader taking one of them
    /// off a result file has it in the same object. `min` and `last` are not
    /// marked -- the events a lossy run is missing are the slow ones, so the
    /// smallest value measured is the one part of the shape the loss does not
    /// pull.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
}

/// One key's total, which is what `topk` ranks on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesTotal {
    pub key: SeriesKey,
    pub events: usize,
    pub total: f64,
    /// Events that carried no value at all: measured by the collector as
    /// having happened, with nothing for this emit to aggregate -- an L1 hit
    /// that AMD's IBS samples by level and does not time. Counted rather than
    /// refused, and left out of every other number here.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub unmeasured: usize,
    /// What the run did not see, when it lost anything.
    ///
    /// Deliberately not folded into [`Rest`], which is a different fact:
    /// `Rest` is what the ranking *left out and measured*, and this is what
    /// nothing measured at all. A reader deciding what to look at next is
    /// making the same decision a percentile is read for, and a ranking whose
    /// totals are lower bounds can be a ranking in the wrong order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
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
/// says how to read itself: the request's own [`HistScale`], under the name the
/// results give it.
pub use crate::request::HistScale as BucketLayout;

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
    /// The smallest and largest value seen. Over rows bucketed in the kernel
    /// the measurements themselves are not up here: `min` is the lowest
    /// occupied bucket's low bound, and `max` the highest of the rows' low
    /// bounds and, where a row carries [`SUM_NS_ATTR`], of its measurements'
    /// exact mean. Both are lower bounds; the largest measurement may be up to
    /// the next bucket's low bound. `max` is never below `sum / count`.
    pub min: f64,
    pub max: f64,
    /// What the values add up to: exact, including over rows bucketed in the
    /// kernel when they carry [`SUM_NS_ATTR`].
    pub sum: f64,
    /// What this run lost, when it lost something that biases the shape. See
    /// [`SeriesSummary::short`]; every bucket count here is a lower bound too,
    /// and the tail is the part that is short.
    ///
    /// Declared before `buckets` on purpose. TOML writes a table after the
    /// scalars and an array of tables after that, so a table declared behind
    /// `buckets` would be serialized into the last bucket instead of the
    /// series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
    pub buckets: Vec<Bucket>,
    /// Values no bucket can hold: negative, or not a number. There is no bucket
    /// below zero and inventing one would put a value in a range it is not in,
    /// so they are counted here instead and left out of `count`, `min`, `max`
    /// and `sum`.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub unbucketable: usize,
    /// Events that carried no value at all: measured by the collector as
    /// having happened, with nothing for this emit to aggregate -- an L1 hit
    /// that AMD's IBS samples by level and does not time. Counted rather than
    /// refused, and left out of every other number here.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub unmeasured: usize,
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

/// One stack identity and what was measured on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackCount {
    pub stack: u64,
    /// The series this row belongs to, when the request named keys to group
    /// by (`--keys task`): one row per series and stack, so a followed
    /// task's own stacks read apart from its siblings' without `raw` beside
    /// them. Absent when the request named none, and then a row is the
    /// stack's across the whole result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<SeriesKey>,
    /// The events on this stack, as `topk`'s `events` are: a row bucketed in
    /// the kernel is one event however many measurements it stands for.
    pub events: usize,
    /// What the measurements on this stack add up to, every row weighed (see
    /// `weighed`): exact where a kernel-built row carries [`SUM_NS_ATTR`].
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

    // One shortfall for the run, not one per series: a ring drops without
    // regard to which series the record would have joined, so a fraction
    // computed per series would be attributing every drop to each of them.
    //
    // `per_record` is the guard between a weight and a loss. A row that stands
    // for many measurements makes the fraction a fraction of rows, because
    // what the dropped rows would have weighed is a number nobody up here ever
    // saw -- see [`Shortfall::per_record`], and findings 17 and 18.
    //
    // Per series, when the collector read one ring per key and every loss it
    // counted belongs to one of them: then a series keyed by that key is short
    // by its own ring's losses and by nothing else ([`Shortfalls`]).
    let shorts = Shortfalls::of(&collection, &request.keys);
    let short = shorts.run.clone();

    // `--keys` regroups every reducing emit; the default is the collector's
    // primary key, which is what `SeriesKey` documents.
    let axes = request.keys.as_slice();
    let adds_up = values_add_up(request.source.as_str());
    let polled = matches!(request.mode, crate::request::Mode::Poll { .. });
    let ticks = if request
        .emit
        .iter()
        .any(|emit| matches!(emit, Emit::Summary | Emit::Hist(_)))
    {
        groups_for(&collection.events, axes, value_attr, polled)?
    } else {
        None
    };
    let mut outputs = Vec::new();
    for emit in &request.emit {
        outputs.push(match emit {
            Emit::Raw => Output::Raw {
                events: collection.events.clone(),
            },
            Emit::Timeline => Output::Timeline {
                series: timeline_by(&collection.events, axes, first_ts_ns, value_attr)?,
                first_ts_ns: collection.events.first().map(|event| event.ts_ns),
            },
            Emit::Summary => Output::Summary {
                series: match &ticks {
                    Some(groups) => summary_of(borrowed(groups), value_attr, adds_up, |key| {
                        shorts.for_series(key)
                    })?,
                    None => summary_by(&collection.events, axes, value_attr, adds_up, &shorts)?,
                },
            },
            Emit::Rate => Output::Rate {
                series: rate_by(
                    &collection.events,
                    axes,
                    collection.observed_ns,
                    value_attr,
                    &shorts,
                )?,
            },
            Emit::Hist(scale) => {
                let layout = *scale;
                Output::Hist {
                    layout,
                    series: match &ticks {
                        Some(groups) => hist_of(borrowed(groups), value_attr, layout, |key| {
                            shorts.for_series(key)
                        })?,
                        None => hist_by(&collection.events, axes, value_attr, layout, &shorts)?,
                    },
                }
            }
            Emit::SummaryBy(by) => {
                uncarried(&collection.events, *by, "summary", &mut degradations);
                Output::Summary {
                    series: summary_over(
                        &collection.events,
                        by.as_str(),
                        value_attr,
                        adds_up,
                        &shorts,
                    )?,
                }
            }
            Emit::HistBy(scale, by) => {
                uncarried(&collection.events, *by, "histogram", &mut degradations);
                Output::Hist {
                    layout: *scale,
                    series: hist_over(
                        &collection.events,
                        by.as_str(),
                        value_attr,
                        *scale,
                        &shorts,
                    )?,
                }
            }
            Emit::TopK { k, by: None } => nothing_counted(
                topk_by(&collection.events, axes, value_attr, *k, &shorts)?,
                request,
                collection.observed_ns,
                &shorts,
            ),
            Emit::TopK { k, by: Some(by) } => {
                uncarried(&collection.events, *by, "ranking", &mut degradations);
                nothing_counted(
                    topk_over(&collection.events, by.as_str(), value_attr, *k, &shorts)?,
                    request,
                    collection.observed_ns,
                    &shorts,
                )
            }
            Emit::Stack => {
                let (output, unmeasured) =
                    stack_counting(&collection.events, axes, value_attr, short.as_ref())?;
                if unmeasured > 0 {
                    degradations.push(format!(
                        "{unmeasured} of {} events carried no `{value_attr}`: the collector saw \
                         them happen and could not measure them, so they are in no stack's \
                         events or total -- counted here rather than refused",
                        collection.events.len()
                    ));
                }
                output
            }
        });
    }

    if let Some(note) = pre_aggregated(&request.emit, &collection.events) {
        degradations.push(note);
    }

    // Only built when something asked for stacks: it is a scan of every event
    // for a table nothing else reads.
    //
    // A collector that interns its own stacks hands the table over whole, which
    // is the cheap path and the one a profiler takes. The scan is the fallback
    // for a collector that writes the frames onto each event instead, and it
    // never overwrites what the collector already said.
    let mut provenance = collection.provenance;
    let stacks = if request.emit.contains(&Emit::Stack) {
        let mut stacks = collection.stacks;
        for (id, frames) in stack_table(&collection.events) {
            stacks.entry(id).or_insert(frames);
        }
        stacks
    } else {
        BTreeMap::new()
    };
    // Who named the frames speaks of the stacks kept, and of nothing when
    // none were; what each task executed speaks of the keys, stacks or not.
    provenance.keep_stacks(&stacks);
    if stacks.is_empty() {
        provenance.perf_maps.clear();
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
        window: collection.window,
        stopped_because: collection.stopped_because,
        losses: collection.losses,
        filtered: collection.filtered,
        lifetime: collection.lifetime,
        unverified: resolved.unverified.clone(),
        degradations,
        stacks,
        notes: collection.facts,
        overhead: collection.overhead,
        served_by: collection.served_by,
        period_ns: match request.mode {
            crate::request::Mode::Sample { freq } if freq.as_hz() > 0 => {
                Some(1_000_000_000 / freq.as_hz())
            }
            _ => None,
        },
        by_ring: collection.by_ring,
        value_attr: value_attr.to_owned(),
        row_stamp: collection.row_stamp,
        drains: collection.drains,
        fields: collection.fields,
        provenance,
    })
}

/// Says how many events carried no `by`, for an `@<attr>` emit that holds
/// them in one `<by>=?` row.
fn uncarried(events: &[Event], by: AttrName, what: &str, degradations: &mut Vec<String>) {
    let carried = events
        .iter()
        .filter(|event| event.attr(by.as_str()).is_some())
        .count();
    if carried < events.len() {
        degradations.push(format!(
            "{} of {} events carried no `{by}`, so the {what} over it holds them as `{by}=?`",
            events.len() - carried,
            events.len()
        ));
    }
}

/// Groups events by their [`SeriesKey`], keeping the order they arrived in.
///
/// `axes` are the keys the request asked to group by (`--keys`); empty means
/// the collector's own primary key.
fn grouped<'a>(events: &'a [Event], axes: &[KeyKind]) -> BTreeMap<SeriesKey, Vec<&'a Event>> {
    let mut groups: BTreeMap<SeriesKey, Vec<&Event>> = BTreeMap::new();
    for event in events {
        groups
            .entry(SeriesKey::by(event, axes))
            .or_default()
            .push(event);
    }
    groups
}

/// Groups events by their value of one attribute, each group named
/// `<by>=<value>` and an event that carries none in `<by>=?`: what every
/// `@<attr>` emit reads.
fn grouped_over<'a>(events: &'a [Event], by: &str) -> BTreeMap<SeriesKey, Vec<&'a Event>> {
    let mut groups: BTreeMap<SeriesKey, Vec<&Event>> = BTreeMap::new();
    for event in events {
        let value = event
            .attr(by)
            .map_or_else(|| "?".to_owned(), |value| value.to_string());
        groups
            .entry(SeriesKey::from(format!("{by}={value}").as_str()))
            .or_default()
            .push(event);
    }
    groups
}

/// The five reducing emits as the unit tests call them: by the primary key.
#[cfg(test)]
fn timeline(events: &[Event], first_ts_ns: u64, attr: &str) -> Result<Vec<Series>, EmitError> {
    timeline_by(events, &[], first_ts_ns, attr)
}

#[cfg(test)]
fn rate(
    events: &[Event],
    window_ns: u64,
    attr: &str,
    short: Option<&Shortfall>,
) -> Result<Vec<SeriesRate>, EmitError> {
    rate_by(events, &[], window_ns, attr, &Shortfalls::run_wide(short))
}

#[cfg(test)]
fn summary(
    events: &[Event],
    attr: &str,
    short: Option<&Shortfall>,
) -> Result<Vec<SeriesSummary>, EmitError> {
    summary_by(events, &[], attr, true, &Shortfalls::run_wide(short))
}

#[cfg(test)]
fn hist(
    events: &[Event],
    attr: &str,
    layout: BucketLayout,
    short: Option<&Shortfall>,
) -> Result<Vec<SeriesHist>, EmitError> {
    hist_by(events, &[], attr, layout, &Shortfalls::run_wide(short))
}

#[cfg(test)]
fn topk(
    events: &[Event],
    attr: &str,
    k: u32,
    short: Option<&Shortfall>,
) -> Result<Output, EmitError> {
    topk_by(events, &[], attr, k, &Shortfalls::run_wide(short))
}

fn value_of(event: &Event, attr: &str) -> Result<f64, EmitError> {
    event
        .attr(attr)
        .and_then(|value| value.as_f64())
        .ok_or_else(|| EmitError::NotNumeric {
            attr: attr.to_owned(),
        })
}

/// An event's value, or `None` when it carries no value attribute at all.
///
/// Absent and malformed are different answers. An event without the attribute
/// is one the collector saw happen and could not time, and the emits that
/// reduce values count it as `unmeasured`; an attribute that is there and is
/// not a number is still a refusal, because that is a collector bug.
fn measured(event: &Event, attr: &str) -> Result<Option<f64>, EmitError> {
    match event.attr(attr) {
        None => Ok(None),
        Some(value) => value
            .as_f64()
            .map(Some)
            .ok_or_else(|| EmitError::NotNumeric {
                attr: attr.to_owned(),
            }),
    }
}

fn timeline_by(
    events: &[Event],
    axes: &[KeyKind],
    first_ts_ns: u64,
    attr: &str,
) -> Result<Vec<Series>, EmitError> {
    let mut series = Vec::new();
    for (key, events) in grouped(events, axes) {
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

fn rate_by(
    events: &[Event],
    axes: &[KeyKind],
    window_ns: u64,
    attr: &str,
    shorts: &Shortfalls,
) -> Result<Vec<SeriesRate>, EmitError> {
    let mut rates = Vec::new();
    for (key, events) in grouped(events, axes) {
        let total: f64 = events
            .iter()
            .map(|event| Ok(weighed(event, attr, value_of(event, attr)?)))
            .collect::<Result<Vec<_>, EmitError>>()?
            .iter()
            .sum();
        // A series of counted windows is over the time its counter was
        // enabled, when every one of them says; the run's window otherwise.
        let enabled: Option<u64> = events
            .iter()
            .map(|event| {
                event
                    .attr("enabled_ns")
                    .and_then(|value| value.as_f64())
                    .filter(|ns| *ns > 0.0)
                    .map(|ns| ns as u64)
            })
            .sum();
        let (window_ns, counted_window) = match enabled {
            Some(enabled) if !events.is_empty() => (enabled, true),
            _ => (window_ns, false),
        };
        let seconds = (window_ns > 0).then(|| window_ns as f64 / 1e9);
        rates.push(SeriesRate {
            short: shorts.for_series(&key),
            key,
            events: events.len(),
            total,
            per_second: seconds.map(|seconds| total / seconds),
            window_ns,
            counted_window,
        });
    }
    Ok(rates)
}

fn summary_by(
    events: &[Event],
    axes: &[KeyKind],
    attr: &str,
    adds_up: bool,
    shorts: &Shortfalls,
) -> Result<Vec<SeriesSummary>, EmitError> {
    summary_of(grouped(events, axes), attr, adds_up, |key| {
        shorts.for_series(key)
    })
}

/// A polled series regrouped by fewer keys than its events carry, with the
/// readings of one tick added into one: `io.bytes` through `/proc/diskstats`
/// reads every device at once, and `--keys dir` asks for a direction's bytes
/// per tick, not one measurement per device -- four devices made `count` 4
/// and `mean` a quarter of the bytes (io 5). Readings of one tick are the
/// events of the series that share a timestamp, which a poll gives them; a
/// traced event is a measurement of its own whatever its timestamp, so
/// nothing but a poll is folded. An event with no value is kept as it is,
/// counted as unmeasured.
fn folded_ticks(
    groups: BTreeMap<SeriesKey, Vec<&Event>>,
    attr: &str,
) -> Result<BTreeMap<SeriesKey, Vec<Event>>, EmitError> {
    let mut folded = BTreeMap::new();
    for (key, events) in groups {
        let mut ticks: Vec<Event> = Vec::with_capacity(events.len());
        let mut open: Option<(u64, Event, f64, usize)> = None;
        for event in events {
            let Some(value) = measured(event, attr)? else {
                ticks.push(event.clone());
                continue;
            };
            let value = weighed(event, attr, value);
            match &mut open {
                Some((ts, _, sum, readings)) if *ts == event.ts_ns => {
                    *sum += value;
                    *readings += 1;
                }
                _ => {
                    if let Some(done) = open.take() {
                        ticks.push(tick(done, attr));
                    }
                    open = Some((event.ts_ns, event.clone(), value, 1));
                }
            }
        }
        if let Some(done) = open.take() {
            ticks.push(tick(done, attr));
        }
        folded.insert(key, ticks);
    }
    Ok(folded)
}

/// One tick's readings as one event: the first reading with `attr` set to
/// their sum, and no weight -- the sum is one measurement. A tick of one
/// reading is that reading, untouched.
fn tick((_, mut event, sum, readings): (u64, Event, f64, usize), attr: &str) -> Event {
    if readings == 1 {
        return event;
    }
    event
        .attrs
        .retain(|one| one.name != attr && one.name != WEIGHT_ATTR && one.name != SUM_NS_ATTR);
    event.attrs.push(crate::event::Attr::float(attr, sum));
    event
}

/// The groups a reducing emit reads: by `axes`, and for a poll regrouped by
/// fewer keys than its events carry, one event per tick ([`folded_ticks`]).
fn groups_for(
    events: &[Event],
    axes: &[KeyKind],
    attr: &str,
    polled: bool,
) -> Result<Option<BTreeMap<SeriesKey, Vec<Event>>>, EmitError> {
    if !polled || axes.is_empty() {
        return Ok(None);
    }
    folded_ticks(grouped(events, axes), attr).map(Some)
}

fn borrowed(groups: &BTreeMap<SeriesKey, Vec<Event>>) -> BTreeMap<SeriesKey, Vec<&Event>> {
    groups
        .iter()
        .map(|(key, events)| (key.clone(), events.iter().collect()))
        .collect()
}

/// Whether a source's values are increments a sum means something of --
/// events, deltas, allocations -- rather than levels: its entry declares an
/// emit that sums them, `rate`, `topk` or `stack`. A source the embedded
/// catalog does not carry (one from `--catalog <dir>`) is given the total; a
/// sum the reader can ignore is better than a missing one.
pub fn values_add_up(source: &str) -> bool {
    use std::sync::OnceLock;
    static CATALOG: OnceLock<Option<crate::catalog::Catalog>> = OnceLock::new();
    let catalog = CATALOG.get_or_init(|| crate::catalog::Catalog::embedded().ok());
    let entry = catalog
        .as_ref()
        .zip(source.parse().ok())
        .and_then(|(catalog, id)| catalog.get(&id).cloned());
    entry.is_none_or(|entry| {
        entry
            .emits
            .iter()
            .any(|emit| matches!(emit, EmitKind::Rate | EmitKind::TopK | EmitKind::Stack))
    })
}

/// `summary@<attr>`: one row per distinct value of `by`, named
/// `<by>=<value>`, an event that carries none in `<by>=?`. The run's
/// shortfall is on every row, as on `topk:N@<attr>`'s: a ring drops without
/// regard to which value the record carried.
fn summary_over(
    events: &[Event],
    by: &str,
    attr: &str,
    adds_up: bool,
    shorts: &Shortfalls,
) -> Result<Vec<SeriesSummary>, EmitError> {
    summary_of(grouped_over(events, by), attr, adds_up, |_| {
        shorts.run.clone()
    })
}

fn summary_of(
    groups: BTreeMap<SeriesKey, Vec<&Event>>,
    attr: &str,
    adds_up: bool,
    short_of: impl Fn(&SeriesKey) -> Option<Shortfall>,
) -> Result<Vec<SeriesSummary>, EmitError> {
    let mut summaries = Vec::new();
    for (key, events) in groups {
        // (value, weight, what the measurements behind it add up to, the
        // largest of them it is known to hold)
        let mut values: Vec<(f64, f64, f64, f64)> = Vec::with_capacity(events.len());
        let mut unmeasured = 0;
        for event in &events {
            match measured(event, attr)? {
                Some(value) => values.push((
                    value,
                    weight_of(event),
                    weighed(event, attr, value),
                    highest(event, attr, value),
                )),
                None => unmeasured += 1,
            }
        }
        if values.is_empty() {
            if unmeasured > 0 {
                summaries.push(SeriesSummary {
                    count: 0,
                    min: 0.0,
                    max: 0.0,
                    mean: 0.0,
                    last: 0.0,
                    total: adds_up.then_some(0.0),
                    unmeasured,
                    short: short_of(&key),
                    key,
                });
            }
            continue;
        }
        // Weighted, because an event may stand for many measurements; see
        // [`WEIGHT_ATTR`]. `count` is the number of measurements and not the
        // number of events, which is the number a reader of a summary wants
        // and the number `n=` has always claimed to be.
        let weights: f64 = values.iter().map(|(_, weight, _, _)| *weight).sum();
        let count = weights as usize;
        let min = values
            .iter()
            .map(|(value, _, _, _)| *value)
            .fold(f64::INFINITY, f64::min);
        let max = values
            .iter()
            .map(|(_, _, _, highest)| *highest)
            .fold(f64::NEG_INFINITY, f64::max);
        let total = sum(values.iter().map(|(_, _, total, _)| *total));
        let mean = total / weights;
        let last = values.last().expect("checked above").0;
        summaries.push(SeriesSummary {
            short: short_of(&key),
            key,
            count,
            min,
            max,
            mean,
            last,
            total: adds_up.then_some(total),
            unmeasured,
        });
    }
    Ok(summaries)
}

fn hist_by(
    events: &[Event],
    axes: &[KeyKind],
    attr: &str,
    layout: BucketLayout,
    shorts: &Shortfalls,
) -> Result<Vec<SeriesHist>, EmitError> {
    hist_of(grouped(events, axes), attr, layout, |key| {
        shorts.for_series(key)
    })
}

/// `hist@<attr>`: one histogram per distinct value of `by`, as
/// [`summary_over`] does it.
fn hist_over(
    events: &[Event],
    by: &str,
    attr: &str,
    layout: BucketLayout,
    shorts: &Shortfalls,
) -> Result<Vec<SeriesHist>, EmitError> {
    hist_of(grouped_over(events, by), attr, layout, |_| {
        shorts.run.clone()
    })
}

fn hist_of(
    groups: BTreeMap<SeriesKey, Vec<&Event>>,
    attr: &str,
    layout: BucketLayout,
    short_of: impl Fn(&SeriesKey) -> Option<Shortfall>,
) -> Result<Vec<SeriesHist>, EmitError> {
    if layout == (BucketLayout::Linear { width: 0 }) {
        return Err(EmitError::ZeroBucketWidth);
    }
    let mut histograms = Vec::new();
    for (key, events) in groups {
        let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
        let mut unbucketable = 0;
        let mut unmeasured = 0;
        let mut count = 0;
        let mut sum = 0.0;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for event in events {
            let Some(value) = measured(event, attr)? else {
                unmeasured += 1;
                continue;
            };
            if !value.is_finite() || value < 0.0 {
                unbucketable += 1;
                continue;
            }
            // An event may stand for many measurements; see [`WEIGHT_ATTR`].
            // `min` and `max` need no weight -- every measurement behind a row
            // is inside its bucket, so the extremes are the row's own value,
            // raised for `max` to the row's exact mean when it has one (see
            // `highest`). The sum is the exact one when the row carries it;
            // see [`SUM_NS_ATTR`].
            let weight = weight_of(event);
            *counts.entry(layout.index(value)).or_insert(0) += weight as usize;
            count += weight as usize;
            sum += weighed(event, attr, value);
            min = min.min(value);
            max = max.max(highest(event, attr, value));
        }
        if count == 0 && unbucketable == 0 && unmeasured == 0 {
            continue;
        }
        histograms.push(SeriesHist {
            short: short_of(&key),
            key,
            count,
            min: if count == 0 { 0.0 } else { min },
            max: if count == 0 { 0.0 } else { max },
            sum,
            buckets: buckets(&counts, layout),
            unbucketable,
            unmeasured,
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

fn topk_by(
    events: &[Event],
    axes: &[KeyKind],
    attr: &str,
    k: u32,
    shorts: &Shortfalls,
) -> Result<Output, EmitError> {
    let mut totals = Vec::new();
    for (key, events) in grouped(events, axes) {
        let mut total = 0.0;
        let mut unmeasured = 0;
        for event in &events {
            match measured(event, attr)? {
                Some(value) => total += weighed(event, attr, value),
                None => unmeasured += 1,
            }
        }
        totals.push(SeriesTotal {
            short: shorts.for_series(&key),
            key,
            events: events.len() - unmeasured,
            total,
            unmeasured,
        });
    }

    Ok(ranked(totals, k))
}

/// The ranking over one attribute's distinct values: each value is a row,
/// named `<attr>=<value>`, and what it weighs is the value attribute summed
/// over the events that carried it. The run's shortfall is on every row: a
/// ring drops without regard to which value the record carried.
fn topk_over(
    events: &[Event],
    by: &str,
    attr: &str,
    k: u32,
    shorts: &Shortfalls,
) -> Result<Output, EmitError> {
    let mut totals = Vec::new();
    for (key, events) in grouped_over(events, by) {
        let mut total = 0.0;
        let mut unmeasured = 0;
        for event in &events {
            match measured(event, attr)? {
                Some(value) => total += weighed(event, attr, value),
                None => unmeasured += 1,
            }
        }
        totals.push(SeriesTotal {
            short: shorts.run.clone(),
            key,
            events: events.len() - unmeasured,
            total,
            unmeasured,
        });
    }
    Ok(ranked(totals, k))
}

/// A ranking of a run that counted nothing over a window it did measure
/// holds one zero row, named by the request's target (`launch`, `pid:1234`,
/// `system`), rather than no row at all: "none" and "not measured" are
/// different answers and an empty ranking reads as either (perfsched 5). A
/// run that measured no window keeps its empty ranking.
fn nothing_counted(
    output: Output,
    request: &crate::request::Request,
    observed_ns: u64,
    shorts: &Shortfalls,
) -> Output {
    let Output::TopK { k, series, rest } = output else {
        return output;
    };
    if !series.is_empty() || !rest.is_empty() || observed_ns == 0 || k == 0 {
        return Output::TopK { k, series, rest };
    }
    let key = match &request.target {
        crate::request::Target::Pid(pids) if pids.len() == 1 => format!("pid:{}", pids[0]),
        crate::request::Target::Tid(tids) if tids.len() == 1 => format!("tid:{}", tids[0]),
        other => other.kind().as_str().to_owned(),
    };
    Output::TopK {
        k,
        series: vec![SeriesTotal {
            key: SeriesKey::from(key.as_str()),
            events: 0,
            total: 0.0,
            unmeasured: 0,
            short: shorts.run.clone(),
        }],
        rest,
    }
}

/// A sum that is `0.0` when there is nothing to add: `f64`'s own `sum` of an
/// empty iterator is `-0.0`, which a result file then prints (perfsched 5).
fn sum(values: impl Iterator<Item = f64>) -> f64 {
    values.fold(0.0, |total, value| total + value)
}

/// Heaviest first, the first `k` kept and the rest summed.
fn ranked(mut totals: Vec<SeriesTotal>, k: u32) -> Output {
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
    Output::TopK {
        k,
        series: totals,
        rest: Rest {
            series: dropped.len(),
            events: dropped.iter().map(|series| series.events).sum(),
            total: sum(dropped.iter().map(|series| series.total)),
        },
    }
}

/// Aggregates by stack identity rather than by frame list.
///
/// A deep stack is a long string; the same stack seen ten thousand times is one
/// `u64` compared ten thousand times, and the frames are looked up once in
/// [`Results::stacks`].
#[cfg(test)]
fn stack(events: &[Event], attr: &str, short: Option<&Shortfall>) -> Result<Output, EmitError> {
    stack_counting(events, &[], attr, short).map(|(output, _)| output)
}

/// The same, with how many events carried no value for the weight.
///
/// An event without the attribute is one the collector saw and could not
/// measure -- an IBS op the cpu did not time has no `latency_cycles` -- and
/// it is counted and left out, as every other reducing emit leaves it out
/// through [`measured`]. It used to be a refusal, which made `mem.latency
/// -e stack` exit 1 about one run in ten, on the runs whose sample happened
/// to include one. An attribute that is there and not a number is still a
/// refusal: that is a collector bug.
fn stack_counting(
    events: &[Event],
    axes: &[KeyKind],
    attr: &str,
    short: Option<&Shortfall>,
) -> Result<(Output, usize), EmitError> {
    // Keyed by series first only when the request named keys: a row per
    // stack is what every reader without keys has always read.
    let mut by_stack: BTreeMap<(Option<SeriesKey>, u64), (usize, f64)> = BTreeMap::new();
    let mut unattributed = 0;
    let mut unmeasured = 0;
    for event in events {
        let Some(id) = stack_id(event) else {
            unattributed += 1;
            continue;
        };
        let Some(value) = measured(event, attr)? else {
            unmeasured += 1;
            continue;
        };
        // A row may stand for many measurements on one stack: the sum of
        // what they measured, exact when the row carries it; see [`weighed`].
        let series = (!axes.is_empty()).then(|| SeriesKey::by(event, axes));
        let entry = by_stack.entry((series, id)).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += weighed(event, attr, value);
    }

    let mut stacks: Vec<StackCount> = by_stack
        .into_iter()
        .map(|((series, stack), (events, total))| StackCount {
            stack,
            series,
            events,
            total,
        })
        .collect();
    stacks.sort_by(|left, right| {
        right
            .total
            .partial_cmp(&left.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.series.cmp(&right.series))
            .then_with(|| left.stack.cmp(&right.stack))
    });

    Ok((
        Output::Stack {
            short: short.cloned(),
            stacks,
            unattributed,
        },
        unmeasured,
    ))
}

/// Whether a row in this run stands for more than one measurement.
///
/// True for a backend that aggregated before the events reached here: a
/// weighted bucket from a BPF map, or a row whose value is a total over a
/// drain window. It is what turns a fraction of dropped records into a
/// statement about records rather than about the distribution -- see
/// [`Shortfall::per_record`].
fn per_record(events: &[Event]) -> bool {
    events
        .iter()
        .any(|event| weight_of(event) > 1.0 || total_of(event).is_some())
}

/// How many measurements an event stands for.
///
/// One when it says nothing, and one when what it says cannot be a weight: a
/// zero or a negative would silently delete a row from every total, which is a
/// worse answer than ignoring a malformed attribute.
fn weight_of(event: &Event) -> f64 {
    declared_weight(event).unwrap_or(1.0)
}

/// The weight an event declares, when it declares one that can be a weight.
fn declared_weight(event: &Event) -> Option<f64> {
    match event.attr(WEIGHT_ATTR) {
        Some(crate::event::AttrValue::Uint(weight)) if *weight > 0 => Some(*weight as f64),
        Some(crate::event::AttrValue::Int(weight)) if *weight > 0 => Some(*weight as f64),
        Some(crate::event::AttrValue::Float(weight)) if *weight > 0.0 && weight.is_finite() => {
            Some(*weight)
        }
        _ => None,
    }
}

/// What the measurements behind one event add up to, given its value of
/// `attr`.
///
/// `value * weight` for every event but one shape: a weighted row built in the
/// kernel that also carries [`SUM_NS_ATTR`], the exact sum the bucketing could
/// not give back. Its value is the bucket's low bound, so `value * weight`
/// is a lower bound on the sum that nothing would mark as one -- 6 512 640
/// against the stream's 7 913 727 ns for the same 320 reads. The exact sum is
/// taken only when the value is itself in nanoseconds and is not the total:
/// `total_ns` is the sum of the row's duration, and of nothing else it
/// carries.
fn weighed(event: &Event, attr: &str, value: f64) -> f64 {
    let Some(weight) = declared_weight(event) else {
        return value;
    };
    exact_sum(event, attr).unwrap_or(value * weight)
}

/// The exact sum a weighted row carries for `attr`, when it carries one: see
/// [`weighed`] for which rows and which attributes.
fn exact_sum(event: &Event, attr: &str) -> Option<f64> {
    declared_weight(event)?;
    if attr == SUM_NS_ATTR || !attr.ends_with("_ns") {
        return None;
    }
    let total = event.attr(SUM_NS_ATTR)?.as_f64()?;
    (total.is_finite() && total >= 0.0).then_some(total)
}

/// The largest measurement a row is known to hold, given its value of `attr`.
///
/// Its value for every row but a kernel-built one with an exact sum, whose
/// value is its bucket's low bound: there the measurements' own mean is a
/// tighter lower bound on the largest of them, and the one that keeps `max`
/// from falling below a `mean` computed from the same sum -- one row of 300
/// reads in the 16384 ns bucket that took 6 ms in all has a mean of 20 000 ns,
/// and a max of 16 384 beside it would read as a contradiction.
fn highest(event: &Event, attr: &str, value: f64) -> f64 {
    match (exact_sum(event, attr), declared_weight(event)) {
        (Some(total), Some(weight)) => value.max(total / weight),
        _ => value,
    }
}

/// How many measurements an event's value is the sum of, when it says so.
///
/// `None` for the ordinary event, whose value is a measurement. See
/// [`TOTAL_OF_ATTR`] for why this is not a weight.
fn total_of(event: &Event) -> Option<u64> {
    match event.attr(TOTAL_OF_ATTR)? {
        crate::event::AttrValue::Uint(count) => Some(*count),
        crate::event::AttrValue::Int(count) => u64::try_from(*count).ok(),
        _ => None,
    }
}

/// Says, beside the results, when the distribution the reader is looking at is
/// of the rows a backend handed up rather than of the workload.
///
/// `hist` and `summary` are the two emits that claim to describe individual
/// measurements: a bucket is a range values fell in, and `n=`, `min`, `mean`
/// and `max` are about the values themselves. A backend that adds its values up
/// before they reach this layer leaves both describing the totals, and there is
/// nothing in the output to tell that histogram from an honest one -- which is
/// what made `sched.offcpu --emit hist` show nine samples with a mean of
/// 15.55 ms for a workload of twenty thousand blocks of a few microseconds.
///
/// The numbers are not changed, because there is no honest number to change
/// them to: the values that went into a kernel-side total are gone. What is
/// added is the sentence that lets a reader tell the two apart, which is the
/// same thing `sched.runqlat` says about its own bucketing.
///
/// Silent when every row stands for a single measurement: a total of one is a
/// measurement, and a run that happened to drain one at a time is not degraded.
fn pre_aggregated(emits: &[Emit], events: &[Event]) -> Option<String> {
    let mut rows = 0_u64;
    let mut measurements = 0_u64;
    for event in events {
        let Some(count) = total_of(event) else {
            continue;
        };
        rows += 1;
        measurements = measurements.saturating_add(count);
    }
    if rows == 0 || measurements <= rows {
        return None;
    }

    let affected: Vec<&str> = emits
        .iter()
        .filter_map(|emit| match emit {
            Emit::Hist(_) | Emit::HistBy(..) => Some("`hist`"),
            Emit::Summary | Emit::SummaryBy(_) => Some("`summary`"),
            _ => None,
        })
        .collect();
    let (named, verb) = match affected.as_slice() {
        [] => return None,
        [one] => ((*one).to_owned(), "describes"),
        many => (many.join(" and "), "describe"),
    };

    let row_word = if rows == 1 { "row" } else { "rows" };
    Some(format!(
        "{named} {verb} the {rows} {row_word} this backend handed up and not the {measurements} \
         measurements behind them: the values are added together before they reach the emit \
         layer, so every bucket, mean and extreme here is of a total over a drain window \
         rather than of a measurement. `{TOTAL_OF_ATTR}` on each row says how many it stands \
         for"
    ))
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

    /// One event standing for four hundred measurements counts as four
    /// hundred, in every emit that counts.
    ///
    /// This is what a collector that aggregates in the kernel produces:
    /// `sched.runqlat` drains `(task, bucket) -> count` from a BPF map and
    /// publishes one event per row, because not doing so is the whole reason
    /// the map exists. Weighing each row as one put the buckets in the right
    /// places and made every one of them a single unit high -- a histogram of
    /// the shape of the kernel's map rather than of the workload.
    #[test]
    fn an_event_that_stands_for_many_measurements_counts_as_many() {
        let weighted = |value: u64, weight: u64| {
            let mut event = event(0, 7, value);
            event.attrs.push(Attr::uint(WEIGHT_ATTR, weight));
            event
        };
        // Three rows of a kernel-side histogram: 400 waits in one bucket, 100
        // in another, 1 in a third.
        let events = vec![weighted(8, 400), weighted(16, 100), weighted(64, 1)];
        let weighted_sum = 8.0 * 400.0 + 16.0 * 100.0 + 64.0;

        let hists = hist(&events, "bytes", BucketLayout::Log2, None).expect("a histogram");
        assert_eq!(hists.len(), 1);
        assert_eq!(
            hists[0].count, 501,
            "three rows stand for five hundred and one measurements"
        );
        assert_eq!(hists[0].sum, weighted_sum);
        let heaviest = hists[0]
            .buckets
            .iter()
            .max_by_key(|bucket| bucket.count)
            .expect("a bucket");
        assert_eq!(
            heaviest.count, 400,
            "the heaviest bucket holds the four hundred, not one row"
        );

        let summaries = summary(&events, "bytes", None).expect("a summary");
        assert_eq!(summaries[0].count, 501);
        assert!(
            (summaries[0].mean - weighted_sum / 501.0).abs() < 1e-9,
            "{}",
            summaries[0].mean
        );
        assert_eq!(summaries[0].min, 8.0, "the extremes need no weight");
        assert_eq!(summaries[0].max, 64.0);

        let Output::TopK { series, .. } = topk(&events, "bytes", 1, None).expect("a ranking")
        else {
            panic!("topk returns a TopK");
        };
        assert_eq!(series[0].total, weighted_sum);

        let rates = rate(&events, 1_000_000_000, "bytes", None).expect("a rate");
        assert_eq!(rates[0].total, weighted_sum);
    }

    /// A row bucketed in the kernel carries its bucket's low bound as the
    /// value and the exact sum as `total_ns`, and every emit that adds up takes
    /// the exact sum: `value * weight` puts each measurement at the bottom of
    /// its bucket, a lower bound up to a factor of two short that nothing would
    /// mark as one. The buckets, `min` and `max` stay the bucket bounds.
    ///
    /// And only the duration's sum: a value not in nanoseconds, or a row with
    /// no weight, is weighed as before.
    #[test]
    fn a_kernel_built_histogram_sums_what_its_rows_measured_not_their_low_bounds() {
        let row = |low: u64, weight: u64, total: u64| {
            let mut event = event(0, 7, 4096);
            event.attrs.push(Attr::uint("latency_ns", low));
            event.attrs.push(Attr::uint(WEIGHT_ATTR, weight));
            event.attrs.push(Attr::uint(SUM_NS_ATTR, total));
            event
        };
        // 320 reads: 300 between 16 and 32 us, 20 between 32 and 64 us.
        let events = vec![row(16_384, 300, 6_000_000), row(32_768, 20, 1_000_000)];
        let exact = 7_000_000.0;
        let low_bounds = 16_384.0 * 300.0 + 32_768.0 * 20.0;
        assert!(low_bounds < exact);

        let hists = hist(&events, "latency_ns", BucketLayout::Log2, None).expect("a histogram");
        assert_eq!(hists[0].count, 320);
        assert_eq!(hists[0].sum, exact, "the exact sum, not the low bounds'");
        assert_eq!(hists[0].min, 16_384.0, "the smallest is a bucket bound");
        assert_eq!(
            hists[0].max, 50_000.0,
            "the largest is at least the top row's exact mean, 1 ms over 20"
        );
        assert_eq!(
            hists[0].buckets.iter().map(|b| b.count).sum::<usize>(),
            320,
            "the buckets are unchanged"
        );

        let summaries = summary(&events, "latency_ns", None).expect("a summary");
        assert!(
            (summaries[0].mean - exact / 320.0).abs() < 1e-9,
            "{}",
            summaries[0].mean
        );
        let Output::TopK { series, .. } = topk(&events, "latency_ns", 1, None).expect("a ranking")
        else {
            panic!("topk returns a TopK");
        };
        assert_eq!(series[0].total, exact);
        let rates = rate(&events, 1_000_000_000, "latency_ns", None).expect("a rate");
        assert_eq!(rates[0].total, exact);

        // `bytes` is not what `total_ns` sums.
        let hists = hist(&events, "bytes", BucketLayout::Log2, None).expect("a histogram");
        assert_eq!(hists[0].sum, 4096.0 * 320.0);

        // Nor is a row without a weight a kernel's row: its `total_ns`, if
        // any, is the collector's own business.
        let mut streamed = event(0, 7, 4096);
        streamed.attrs.push(Attr::uint("latency_ns", 20_000));
        streamed.attrs.push(Attr::uint(SUM_NS_ATTR, 1));
        let hists = hist(&[streamed], "latency_ns", BucketLayout::Log2, None).expect("a histogram");
        assert_eq!(hists[0].sum, 20_000.0);
    }

    /// One kernel-built row: every measurement in the 16384 ns bucket, 6 ms
    /// over 300 of them. The mean is the exact 20 000 ns, and a `max` left at
    /// the bucket's low bound would sit below it.
    #[test]
    fn a_kernel_built_row_s_max_is_never_below_its_mean() {
        let mut row = event(0, 7, 4096);
        row.attrs.push(Attr::uint("latency_ns", 16_384));
        row.attrs.push(Attr::uint(WEIGHT_ATTR, 300));
        row.attrs.push(Attr::uint(SUM_NS_ATTR, 6_000_000));
        let events = [row];

        let summaries = summary(&events, "latency_ns", None).expect("a summary");
        assert_eq!(summaries[0].min, 16_384.0);
        assert!((summaries[0].mean - 20_000.0).abs() < 1e-9);
        assert_eq!(summaries[0].max, 20_000.0);
        assert!(summaries[0].min <= summaries[0].mean && summaries[0].mean <= summaries[0].max);

        let hists = hist(&events, "latency_ns", BucketLayout::Log2, None).expect("a histogram");
        assert_eq!(hists[0].max, 20_000.0);
        assert!(hists[0].sum / hists[0].count as f64 <= hists[0].max);

        // A row without an exact sum keeps its own value as its largest.
        let mut plain = event(0, 7, 4096);
        plain.attrs.push(Attr::uint("latency_ns", 16_384));
        plain.attrs.push(Attr::uint(WEIGHT_ATTR, 300));
        assert_eq!(
            summary(&[plain], "latency_ns", None).unwrap()[0].max,
            16_384.0
        );
    }

    /// `sched.futex`, `sched.lock` and `sched.offcpu` hand up weighted rows
    /// keyed by stack: a stack's total is what its measurements add up to, the
    /// exact sum where the row carries it and `value * weight` otherwise --
    /// never one value per row, which is short by the row's count.
    #[test]
    fn a_stack_total_weighs_its_rows() {
        let row = |stack: u64, low: u64, weight: u64, total: Option<u64>| {
            let mut event = event(0, 7, 4096);
            event.attrs.push(Attr::uint(STACK_ID_ATTR, stack));
            event.attrs.push(Attr::uint("wait_ns", low));
            event.attrs.push(Attr::uint(WEIGHT_ATTR, weight));
            if let Some(total) = total {
                event.attrs.push(Attr::uint(SUM_NS_ATTR, total));
            }
            event
        };
        let events = vec![
            row(1, 16_384, 300, Some(6_000_000)),
            row(1, 32_768, 20, Some(1_000_000)),
            row(2, 1_000, 50, None),
        ];
        let Output::Stack { stacks, .. } = stack(&events, "wait_ns", None).unwrap() else {
            panic!("stack returns a Stack");
        };
        assert_eq!(stacks[0].stack, 1);
        assert_eq!(stacks[0].total, 7_000_000.0, "the exact sum");
        assert_eq!(stacks[0].events, 2, "rows, as topk counts them");
        assert_eq!(stacks[1].stack, 2);
        assert_eq!(stacks[1].total, 50_000.0, "value * weight without a sum");
    }

    /// An event that says nothing about its weight is one measurement, which
    /// is what every streaming collector in this tool produces. The attribute
    /// was new when this landed and no source but `sched.runqlat` emits it:
    /// this is the test that keeps that true.
    #[test]
    fn an_event_without_a_weight_is_one_measurement() {
        let events = vec![event(0, 7, 10), event(1, 7, 20)];
        assert_eq!(
            summary(&events, "bytes", None).expect("a summary")[0].count,
            2
        );

        // And a weight that cannot be one is ignored rather than obeyed: zero
        // would delete the row from every total, which is a worse answer than
        // ignoring a malformed attribute.
        let mut zero = event(0, 7, 10);
        zero.attrs.push(Attr::uint(WEIGHT_ATTR, 0));
        let mut negative = event(1, 7, 20);
        negative.attrs.push(Attr::int(WEIGHT_ATTR, -5));
        let events = vec![zero, negative];
        assert_eq!(
            summary(&events, "bytes", None).expect("a summary")[0].count,
            2
        );
    }

    /// A row whose value is a *sum* over its measurements says so, and the two
    /// emits that claim to describe measurements say what they are describing.
    ///
    /// This is `sched.offcpu --emit hist` in miniature: the kernel keeps
    /// `(task, state, stack) -> total_ns` and a drain hands up one row per
    /// window, so a workload of twenty thousand blocks of a few microseconds
    /// rendered as nine samples with a mean of 15.55 ms -- buckets no block was
    /// ever in. `docs/scenarios/sched.md`, "Defects found" 1.
    ///
    /// The second half is why [`WEIGHT_ATTR`], which the defect was filed
    /// against, is not the answer: weighing the row by its block count is
    /// arithmetic on a number that is already a total, and it puts every one of
    /// those blocks in the bucket of their sum.
    #[test]
    fn a_row_that_is_a_total_over_its_measurements_says_what_it_is() {
        // One drain: four hundred blocks whose durations added up to 8ms.
        let mut row = event(0, 7, 8_000_000);
        row.attrs.push(Attr::uint(TOTAL_OF_ATTR, 400));
        let events = vec![row];

        let note = pre_aggregated(&[Emit::Hist(BucketLayout::Log2), Emit::Summary], &events)
            .expect("a distribution of drain windows is a degradation of itself");
        assert!(
            note.contains("`hist` and `summary` describe")
                && note.contains("the 1 row this backend")
                && note.contains("400 measurements"),
            "both counts have to be in it, or a reader cannot tell this histogram from one \
             whose rows are measurements: {note}"
        );

        // The emits that add or plot a total are not degraded by it: a total is
        // exactly what they were going to sum.
        assert!(pre_aggregated(
            &[Emit::Rate, Emit::TopK { k: 3, by: None }, Emit::Raw],
            &events
        )
        .is_none());
        // Nor is a row that stands for one measurement, which is one.
        let mut single = event(0, 7, 8_000_000);
        single.attrs.push(Attr::uint(TOTAL_OF_ATTR, 1));
        assert!(pre_aggregated(&[Emit::Summary], &[single]).is_none());

        // And the arithmetic that the weight attribute would do instead. The
        // four hundred blocks averaged 20us each; weighing the row by four
        // hundred reports four hundred measurements of 8ms, in a bucket two
        // hundred times too high.
        let mut weighted = event(0, 7, 8_000_000);
        weighted.attrs.push(Attr::uint(WEIGHT_ATTR, 400));
        let hists = hist(&[weighted], "bytes", BucketLayout::Log2, None).expect("a histogram");
        assert_eq!(hists[0].count, 400, "the count would be right");
        assert_eq!(
            hists[0].buckets.len(),
            1,
            "and all four hundred would land in one bucket"
        );
        assert!(
            hists[0].buckets[0].low >= 4_000_000,
            "which is the bucket of their sum, not of any block: {:?}",
            hists[0].buckets[0]
        );
    }

    fn lost(dropped: u64, throttled: u64) -> Losses {
        Losses {
            dropped,
            throttled,
            ..Default::default()
        }
    }

    /// A collection of two threads' events, with the losses given, one ring
    /// per thread when `by_ring` says so.
    fn two_threads(losses: Losses, by_ring: Vec<crate::collect::RingLoss>) -> Collection {
        let events: Vec<Event> = (0..90)
            .map(|i| event(i, if i % 3 == 0 { 7 } else { 8 }, 100))
            .collect();
        Collection {
            events,
            value_attr: "bytes".to_owned(),
            losses,
            observed_ns: 1_000,
            stopped_because: "the test".to_owned(),
            stacks: BTreeMap::new(),
            notes: Vec::new(),
            filtered: None,
            served_by: Vec::new(),
            overhead: crate::collect::Overhead::default(),
            lifetime: None,
            window: crate::collect::Watched::default(),
            detached: false,
            by_ring,
            facts: Vec::new(),
            row_stamp: None,
            drains: Vec::new(),
            failure: None,
            fields: Default::default(),
            provenance: Default::default(),
        }
    }

    fn ring(tid: i32, dropped: u64) -> crate::collect::RingLoss {
        crate::collect::RingLoss {
            key: KeyValue::task(tid),
            losses: lost(dropped, 0),
        }
    }

    /// How a drained source stamped its rows, and its drops placed at the
    /// drains that read them, reach `results.json` as the run said them; a
    /// collection that says neither writes neither.
    /// `mem.rss` on this process, polled, resolved on this machine with the
    /// emits and keys given: a request every machine serves.
    fn rss_request(emit: Vec<Emit>, keys: Vec<KeyKind>) -> ResolvedRequest {
        let catalog = crate::catalog::Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let request = crate::request::Request {
            source: "mem.rss".parse().unwrap(),
            target: crate::request::Target::Pid(vec![std::process::id() as i32]),
            mode: crate::request::Mode::Poll {
                interval: crate::scalar::Duration::from_nanos(10_000_000),
            },
            emit,
            keys,
            filter: None,
            duration: Some("1s".parse().unwrap()),
            budget: None,
            stack: None,
            symbols: None,
            backend: None,
            label: None,
            window: None,
        };
        crate::resolve::resolve(&catalog, &capabilities, &request).unwrap()
    }

    /// What a collector counted reaches the result by name, and a result
    /// whose collector counted nothing writes no `fields`.
    #[test]
    fn a_result_carries_the_fields_its_collector_counted() {
        let resolved = rss_request(vec![Emit::Summary], Vec::new());
        let mut counted = two_threads(lost(0, 0), Vec::new());
        counted.fields = Fields::from([("never_ended".to_owned(), 3), ("unborn".to_owned(), 0)]);
        let results = aggregate(&resolved, counted, Unit::Bytes, "bytes").unwrap();
        assert_eq!(results.fields["never_ended"], 3);
        let json = serde_json::to_value(&results).unwrap();
        assert_eq!(json["fields"]["never_ended"], 3);
        assert_eq!(json["fields"]["unborn"], 0);
        let back: Results = serde_json::from_value(json).unwrap();
        assert_eq!(back.fields, results.fields);

        let silent = aggregate(
            &resolved,
            two_threads(lost(0, 0), Vec::new()),
            Unit::Bytes,
            "bytes",
        )
        .unwrap();
        let json = serde_json::to_value(&silent).unwrap();
        assert!(json.get("fields").is_none(), "{json}");
    }

    /// A collector's provenance reaches the result beside `stacks`: its
    /// executables whatever the emit, its namers only for the stacks kept.
    #[test]
    fn a_result_carries_what_its_stacks_labels_cannot_say() {
        use crate::stack::{NamedBy, PerfMapFile, Provenance};
        let resolved = rss_request(vec![Emit::Summary], Vec::new());
        let mut said = two_threads(lost(0, 0), Vec::new());
        said.stacks = BTreeMap::from([(1, vec!["main".to_owned()])]);
        said.provenance = Provenance {
            named_by: BTreeMap::from([(1, vec![Some(NamedBy::Elf)])]),
            perf_maps: vec![PerfMapFile {
                path: "/tmp/perf-7.map".to_owned(),
                dev: 1,
                ino: 2,
            }],
            executables: BTreeMap::from([("task:7".to_owned(), "/usr/bin/python3.12".to_owned())]),
        };
        let results = aggregate(&resolved, said, Unit::Bytes, "bytes").unwrap();
        let json = serde_json::to_value(&results).unwrap();
        assert_eq!(json["executables"]["task:7"], "/usr/bin/python3.12");
        // No stack emit, so no stacks and nothing about who named them.
        assert!(
            json.get("named_by").is_none() && json.get("perf_maps").is_none(),
            "{json}"
        );
        let back: Results = serde_json::from_value(json).unwrap();
        assert_eq!(back.provenance, results.provenance);
        // With the namers kept: a flattened map keyed by number has to read
        // back from the strings JSON wrote its keys as.
        let mut kept = results.clone();
        kept.provenance.named_by = BTreeMap::from([(1, vec![Some(NamedBy::Elf), None])]);
        let text = serde_json::to_string(&kept).unwrap();
        let back: Results = serde_json::from_str(&text).unwrap();
        assert_eq!(back.provenance, kept.provenance);
        let silent = aggregate(
            &resolved,
            two_threads(lost(0, 0), Vec::new()),
            Unit::Bytes,
            "bytes",
        )
        .unwrap();
        let json = serde_json::to_value(&silent).unwrap();
        assert!(json.get("executables").is_none(), "{json}");
    }

    #[test]
    fn a_result_carries_its_row_stamp_and_its_drains() {
        use crate::collect::{DrainDrop, RowStamp};
        let resolved = rss_request(vec![Emit::Summary], Vec::new());
        let drains = vec![
            DrainDrop {
                ts_ns: 40,
                dropped: 15,
            },
            DrainDrop {
                ts_ns: 90,
                dropped: 5,
            },
        ];
        let mut said = two_threads(lost(20, 0), Vec::new());
        said.row_stamp = Some(RowStamp::FirstEvent);
        said.drains = drains.clone();
        let results = aggregate(&resolved, said, Unit::Bytes, "bytes").unwrap();
        assert_eq!(results.row_stamp, Some(RowStamp::FirstEvent));
        assert_eq!(results.drains, drains);
        let json = serde_json::to_value(&results).unwrap();
        assert_eq!(json["row_stamp"], "first_event");
        assert_eq!(json["drains"][1]["ts_ns"], 90);
        assert_eq!(json["drains"][1]["dropped"], 5);

        let silent = aggregate(
            &resolved,
            two_threads(lost(0, 0), Vec::new()),
            Unit::Bytes,
            "bytes",
        )
        .unwrap();
        let json = serde_json::to_value(&silent).unwrap();
        assert!(json.get("row_stamp").is_none() && json.get("drains").is_none());
    }

    /// Two threads, one ring each, and only one of them overflowed: the
    /// shortfall lands on that thread's series, weighed against that thread's
    /// events, and the other thread's series is complete.
    #[test]
    fn a_ring_s_losses_land_on_the_series_of_the_key_it_belongs_to() {
        let collection = two_threads(lost(20, 0), vec![ring(7, 20), ring(8, 0)]);
        let shorts = Shortfalls::of(&collection, &[]);
        let short = shorts
            .for_series(&SeriesKey::from("task:7"))
            .expect("the thread that overflowed is short");
        assert_eq!((short.dropped, short.records), (20, 30));
        assert!(shorts.for_series(&SeriesKey::from("task:8")).is_none());
        // The run's own shortfall, for what is not per series, is unchanged.
        assert_eq!(shorts.run.as_ref().map(|run| run.records), Some(90));
    }

    /// Losses that are not all a ring's, or series that are not keyed by the
    /// rings' key, cannot be placed: every series carries the run's.
    #[test]
    fn a_loss_no_ring_owns_is_the_run_s_on_every_series() {
        let unplaced = two_threads(lost(25, 0), vec![ring(7, 20)]);
        let shorts = Shortfalls::of(&unplaced, &[]);
        for key in ["task:7", "task:8"] {
            assert_eq!(
                shorts
                    .for_series(&SeriesKey::from(key))
                    .map(|short| short.dropped),
                Some(25),
                "{key}"
            );
        }
        let placed = two_threads(lost(20, 0), vec![ring(7, 20)]);
        let by_cpu = Shortfalls::of(&placed, &[KeyKind::Cpu]);
        assert_eq!(
            by_cpu
                .for_series(&SeriesKey::from("cpu:?"))
                .map(|short| short.dropped),
            Some(20),
            "grouped by what no ring belongs to, the loss is everyone's"
        );
    }

    /// A counted window is a rate over the counter's own enabled time; a
    /// series without it is over the run's window.
    #[test]
    fn a_counted_rate_is_over_the_time_its_counter_was_enabled() {
        let mut counted = event(0, 7, 3_000);
        counted.attrs.push(Attr::uint("enabled_ns", 500_000_000));
        let plain = event(0, 8, 3_000);
        let rates = rate(&[counted, plain], 1_000_000_000, "bytes", None).unwrap();
        let by = |key: &str| rates.iter().find(|rate| rate.key == key).unwrap();
        assert_eq!(by("task:7").per_second, Some(6_000.0));
        assert!(by("task:7").counted_window);
        assert_eq!(by("task:8").per_second, Some(3_000.0));
        assert!(!by("task:8").counted_window);
    }

    /// A clean run gains nothing: not a field, not a mark, not a character.
    ///
    /// This is the half of finding 37 that is as important as the mark itself.
    /// A mark that appears on every run is one readers learn to skip, and then
    /// the run that needed it is the one nobody reads it on.
    ///
    /// The three losses that are *not* a reason to mark are checked here too,
    /// and each for its own reason. `late_ticks` and `failed_reads` belong to
    /// `poll`, and `poll` cannot produce `hist` at all -- `CATALOG.md` §12
    /// refuses it because a histogram of a polled gauge is a histogram of when
    /// the poller looked, which is the coordinated-omission argument under
    /// another name. `undecoded` is a record shape this build cannot parse,
    /// and nothing suggests the shapes it cannot parse are the slow ones.
    #[test]
    fn a_run_that_lost_nothing_that_biases_a_distribution_is_not_marked() {
        assert_eq!(Shortfall::of(&Losses::default(), 1_000, false), None);

        let unbiasing = Losses {
            late_ticks: 40,
            failed_reads: 7,
            undecoded: 3,
            ..Default::default()
        };
        assert!(
            !unbiasing.is_clean(),
            "these are losses, and the loss line still prints them"
        );
        assert_eq!(
            Shortfall::of(&unbiasing, 1_000, false),
            None,
            "a late tick does not shorten a tail: a polled gauge has no histogram to shorten"
        );

        let events = vec![event(0, 7, 10), event(1, 7, 20)];
        let histograms = hist(&events, "bytes", BucketLayout::Log2, None).expect("a histogram");
        assert_eq!(histograms[0].short, None);
        let summaries = summary(&events, "bytes", None).expect("a summary");
        assert_eq!(summaries[0].short, None);
    }

    /// A run that dropped events says by how much it is short, and says that
    /// the numbers built on it are lower bounds rather than estimates.
    ///
    /// The fraction is the one this test chose -- three hundred of a thousand
    /// records never arrived -- and not a number the emit layer derived from
    /// itself. The direction is not a choice: whatever stalled the machine is
    /// what stopped the measuring, so the records a ring drops are the ones
    /// from the busiest moments.
    #[test]
    fn a_run_that_dropped_records_says_what_fraction_of_them_is_missing() {
        let short = Shortfall::of(&lost(300, 0), 700, false).expect("a dropped run is short");
        assert_eq!(
            (short.dropped, short.throttled, short.records),
            (300, 0, 700)
        );
        assert_eq!(
            short.fraction,
            Some(0.3),
            "three hundred of the thousand records that existed"
        );
        assert!(!short.per_record);
        assert!(
            short.note.contains("at least 300 of 1000 events")
                && short.note.contains("30.0 %")
                && short.note.contains("lower bound"),
            "{}",
            short.note
        );
        assert!(
            !short.note.contains("throttled"),
            "nothing was throttled, and naming a loss that did not happen is the defect \
             the loss headline was fixed for: {}",
            short.note
        );
    }

    /// The throttled case, where the honest answer is that the number is not
    /// knowable -- which is the answer `DECISIONS.md` already chose for the
    /// loss line.
    ///
    /// A throttle is the kernel declining to deliver for a while. How many
    /// events happened inside that while is recorded nowhere, so there is no
    /// numerator and any fraction printed here would be an invention. The
    /// distribution is still short and still a lower bound, and that much is
    /// said.
    #[test]
    fn a_throttled_run_says_it_cannot_tell_how_much_it_lost() {
        let short = Shortfall::of(&lost(0, 4), 700, false).expect("a throttled run is short");
        assert_eq!(short.fraction, None, "no fraction exists to report");
        assert!(
            short.note.contains("unknown number of events")
                && short.note.contains("throttled this event 4 times")
                && short.note.contains("not recorded anywhere")
                && short.note.contains("lower bound"),
            "{}",
            short.note
        );
        assert!(
            !short.note.contains('%'),
            "a percentage here would be invented: {}",
            short.note
        );

        // One throttle reads as one throttle, not as "1 times".
        let once = Shortfall::of(&lost(0, 1), 700, false).expect("a throttled run is short");
        assert!(
            once.note.contains("throttled this event once"),
            "{}",
            once.note
        );
    }

    /// Both at once: what is known is given as a fraction, and what is not is
    /// named as unknown rather than folded into it.
    #[test]
    fn a_run_that_both_dropped_and_was_throttled_keeps_the_two_apart() {
        let short = Shortfall::of(&lost(300, 4), 700, false).expect("short both ways");
        assert_eq!(short.fraction, Some(0.3));
        assert!(
            short.note.contains("at least 300 of 1000 events")
                && short.note.contains("and by an unknown number more"),
            "the fraction is a floor and the note has to say the floor is not the total: {}",
            short.note
        );
    }

    /// A weighted bucket and a lossy run are different facts, and conflating
    /// them is the one way this mark could make the output worse.
    ///
    /// `sched.runqlat` hands up `(task, bucket) -> count`: a row weighted four
    /// hundred stands for four hundred waits and is **complete**. A run that
    /// dropped four hundred rows is not. When both are true the fraction that
    /// is knowable is a fraction of *rows*, because what the dropped rows
    /// weighed is a number nobody up here ever saw -- so the note says rows,
    /// and does not claim a fraction of the distribution. Findings 17 and 18.
    #[test]
    fn a_weighted_row_is_complete_and_a_dropped_one_is_not() {
        let weighted = |value: u64, weight: u64| {
            let mut event = event(0, 7, value);
            event.attrs.push(Attr::uint(WEIGHT_ATTR, weight));
            event
        };
        let rows = vec![weighted(8, 400), weighted(16, 100), weighted(64, 1)];
        assert!(per_record(&rows), "these rows stand for more than one each");

        // Complete on its own: weighing is not losing, and a run that lost
        // nothing is not marked however heavily its rows are weighted.
        assert_eq!(
            Shortfall::of(&Losses::default(), rows.len() as u64, true),
            None
        );
        let whole = hist(&rows, "bytes", BucketLayout::Log2, None).expect("a histogram");
        assert_eq!(
            whole[0].count, 501,
            "five hundred and one measurements, all of them here"
        );

        // And with a loss on top, the fraction is of rows and says so.
        let short = Shortfall::of(&lost(1, 0), rows.len() as u64, true).expect("a dropped row");
        assert!(short.per_record);
        assert!(
            short.note.contains("at least 1 of 4 rows")
                && short.note.contains("not of the distribution"),
            "one row of four is 25 % of the rows and an unknown share of the waits: {}",
            short.note
        );
        assert!(
            !short.note.contains("of the 4 measurements"),
            "the measurements behind the dropped row were never counted: {}",
            short.note
        );

        // An unweighted run of the same shape counts events, because there a
        // record is a measurement.
        let plain = vec![event(0, 7, 8), event(0, 7, 16)];
        assert!(!per_record(&plain));
        let short = Shortfall::of(&lost(2, 0), plain.len() as u64, false).expect("a dropped event");
        assert!(
            short.note.contains("at least 2 of 4 events"),
            "{}",
            short.note
        );
    }

    /// The shortfall reaches every series of the two emits it biases, and none
    /// of the ones it does not.
    ///
    /// Every series, because a ring drops without regard to which series the
    /// record would have joined: a run with four devices lost events from some
    /// unknown mixture of them, so marking one and not the others would be a
    /// claim about where the drops fell. `min` is left unmarked in the text
    /// for the opposite reason -- the missing values are the large ones, so
    /// the smallest measured is the one statistic the loss does not pull.
    #[test]
    fn the_mark_reaches_every_series_of_a_lossy_run() {
        let short = Shortfall::of(&lost(10, 0), 90, false).expect("a dropped run is short");
        let events = vec![event(0, 1, 10), event(0, 2, 30), event(1, 1, 20)];

        let histograms = hist(&events, "bytes", BucketLayout::Log2, Some(&short)).unwrap();
        assert_eq!(histograms.len(), 2);
        for series in &histograms {
            assert_eq!(series.short.as_ref(), Some(&short), "{}", series.key);
        }

        let summaries = summary(&events, "bytes", Some(&short)).unwrap();
        assert_eq!(summaries.len(), 2);
        for series in &summaries {
            assert_eq!(series.short.as_ref(), Some(&short), "{}", series.key);
        }
    }

    /// The three emits finding 37 scoped itself away from are short the same
    /// way, and say so the same way.
    ///
    /// A rate, a ranking's totals and a profile's weights are all understated
    /// by exactly the records the ring never delivered, and all understated in
    /// the one direction: what a ring drops is what arrived at the busiest
    /// moment. Stream F marked the distributions and left these three with the
    /// argument written down rather than widening a scope three documents had
    /// agreed; this is that widening.
    #[test]
    fn a_rate_a_ranking_and_a_profile_are_short_the_same_way_a_distribution_is() {
        let short = Shortfall::of(&lost(10, 0), 90, false).expect("a dropped run is short");
        let events = vec![event(0, 1, 10), event(0, 2, 30), event(1, 1, 20)];

        let rates = rate(&events, 1_000_000_000, "bytes", Some(&short)).unwrap();
        assert_eq!(rates.len(), 2);
        for series in &rates {
            assert_eq!(series.short.as_ref(), Some(&short), "{}", series.key);
        }

        let Output::TopK { series, rest, .. } = topk(&events, "bytes", 10, Some(&short)).unwrap()
        else {
            panic!("topk produces a ranking");
        };
        for total in &series {
            assert_eq!(total.short.as_ref(), Some(&short), "{}", total.key);
        }
        // `Rest` is what the ranking left out *and measured*; the mark is what
        // nothing measured at all. They must not be folded together.
        assert_eq!(rest.series, 0, "nothing was left out of a top ten of two");

        let mut stacked = events.clone();
        stacked[0].attrs.push(Attr::uint(STACK_ID_ATTR, 7));
        let Output::Stack {
            short: marked,
            unattributed,
            ..
        } = stack(&stacked, "bytes", Some(&short)).unwrap()
        else {
            panic!("stack produces a stack");
        };
        assert_eq!(marked.as_ref(), Some(&short));
        // And `unattributed` stays its own number: events that arrived without
        // a stack and records that never arrived are different facts.
        assert_eq!(unattributed, 2);
    }

    /// A clean run gains nothing, or the mark becomes noise and readers learn
    /// to ignore it. The negative matters as much as the positive.
    #[test]
    fn a_clean_run_carries_no_mark_on_any_of_the_three() {
        let events = vec![event(0, 1, 10), event(0, 2, 30)];
        assert!(rate(&events, 1_000_000_000, "bytes", None)
            .unwrap()
            .iter()
            .all(|series| series.short.is_none()));
        let Output::TopK { series, .. } = topk(&events, "bytes", 10, None).unwrap() else {
            panic!("topk produces a ranking");
        };
        assert!(series.iter().all(|total| total.short.is_none()));
        let Output::Stack { short, .. } = stack(&events, "bytes", None).unwrap() else {
            panic!("stack produces a stack");
        };
        assert!(short.is_none());
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
        let summaries = summary(&events, "bytes", None).unwrap();
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
        let rates = rate(&events, 0, "bytes", None).unwrap();
        assert_eq!(rates[0].total, 30.0, "the total is still true");
        assert_eq!(
            rates[0].per_second, None,
            "a window of zero has no rate, and no number should be invented for it"
        );

        // A window that exists, however short, does have one.
        let measured = rate(&events, 1_000_000, "bytes", None).unwrap();
        assert_eq!(measured[0].per_second, Some(30_000.0));
    }

    #[test]
    fn a_non_numeric_attribute_is_refused_rather_than_counted_as_zero() {
        let mut broken = event(0, 1, 10);
        broken.attrs = vec![Attr::text("bytes", "a lot")];
        assert!(summary(&[broken], "bytes", None).is_err());
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

        let series = hist(&events, "bytes", BucketLayout::Log2, None).unwrap();
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

    /// `--keys` was accepted, recorded, and ignored: every series was the
    /// collector's first key whatever the request asked for (sched 7). A
    /// request that names its axes is grouped by them, in the order asked.
    #[test]
    fn the_keys_a_request_names_are_what_its_series_are_grouped_by() {
        let with_file = |tid: i32, ino: u64, bytes: u64| {
            let mut event = event(0, tid, bytes);
            event.keys.push(KeyValue::File { dev: 28, ino });
            event
        };
        let events = vec![
            with_file(1, 10, 4),
            with_file(1, 11, 8),
            with_file(2, 10, 16),
        ];

        let by = |axes: &[KeyKind]| -> Vec<(String, f64)> {
            let Output::TopK { series, .. } =
                topk_by(&events, axes, "bytes", 10, &Shortfalls::run_wide(None)).unwrap()
            else {
                panic!("topk is a ranking");
            };
            series
                .into_iter()
                .map(|total| (total.key.to_string(), total.total))
                .collect()
        };
        assert_eq!(by(&[]), [("task:2".into(), 16.0), ("task:1".into(), 12.0)]);
        assert_eq!(
            by(&[KeyKind::File]),
            [("file:28+10".into(), 20.0), ("file:28+11".into(), 8.0)]
        );
        assert_eq!(
            by(&[KeyKind::File, KeyKind::Task]),
            [
                ("file:28+10 task:2".into(), 16.0),
                ("file:28+11 task:1".into(), 8.0),
                ("file:28+10 task:1".into(), 4.0),
            ]
        );
        // A key the event does not carry is its own part, not a neighbour's.
        assert_eq!(by(&[KeyKind::Cpu]), [("cpu:?".into(), 28.0)]);
    }

    fn quiet(events: Vec<Event>, observed_ns: u64) -> Collection {
        let mut collection = two_threads(Losses::default(), Vec::new());
        collection.events = events;
        collection.observed_ns = observed_ns;
        collection
    }

    /// A summary carries its total where the source's values add up (heap 4,
    /// top 1), as `count * mean` exactly, and none over a level: fifty
    /// readings of `mem.rss` are not fifty footprints.
    #[test]
    fn a_summary_totals_increments_and_not_levels() {
        assert!(values_add_up("mem.heap.alloc"));
        assert!(values_add_up("cpu.oncpu_time"));
        assert!(values_add_up("net.packets"));
        assert!(!values_add_up("mem.rss"));
        assert!(!values_add_up("mem.heap.live"));
        assert!(values_add_up("not.in.the.catalog"));

        let events = vec![event(0, 7, 100), event(1, 7, 300), event(2, 8, 5)];
        let series = summary_by(&events, &[], "bytes", true, &Shortfalls::run_wide(None)).unwrap();
        let totals: Vec<(String, Option<f64>, f64)> = series
            .iter()
            .map(|one| (one.key.to_string(), one.total, one.mean * one.count as f64))
            .collect();
        assert_eq!(
            totals,
            [
                ("task:7".into(), Some(400.0), 400.0),
                ("task:8".into(), Some(5.0), 5.0)
            ]
        );

        let resolved = rss_request(vec![Emit::Summary], Vec::new());
        let results = aggregate(&resolved, quiet(events, 1_000), Unit::Bytes, "bytes").unwrap();
        let Output::Summary { series } = &results.outputs[0] else {
            panic!("a summary");
        };
        assert!(series.iter().all(|one| one.total.is_none()), "{series:?}");
        let json = serde_json::to_string(series).unwrap();
        assert!(!json.contains("total"), "{json}");
    }

    /// `summary@<attr>` and `hist@<attr>` (gpu 2): one row per distinct value
    /// of the attribute, named as `topk:N@<attr>`'s rows are, an event that
    /// carries none in `<attr>=?` and said so.
    #[test]
    fn a_summary_and_a_histogram_per_attribute_value() {
        let with_op = |ts: u64, bytes: u64, op: Option<&str>| {
            let mut one = event(ts, 7, bytes);
            if let Some(op) = op {
                one.attrs.push(Attr::text("op", op));
            }
            one
        };
        let events = vec![
            with_op(0, 10, Some("copy")),
            with_op(1, 30, Some("copy")),
            with_op(2, 1000, Some("gemm")),
            with_op(3, 7, None),
        ];
        let by = AttrName::new("op").unwrap();
        let resolved = rss_request(
            vec![Emit::SummaryBy(by), Emit::HistBy(BucketLayout::Log2, by)],
            Vec::new(),
        );
        let results = aggregate(&resolved, quiet(events, 1_000), Unit::Bytes, "bytes").unwrap();
        let Output::Summary { series } = &results.outputs[0] else {
            panic!("a summary");
        };
        let rows: Vec<(String, usize, f64, f64, f64)> = series
            .iter()
            .map(|one| (one.key.to_string(), one.count, one.min, one.mean, one.max))
            .collect();
        assert_eq!(
            rows,
            [
                ("op=?".into(), 1, 7.0, 7.0, 7.0),
                ("op=copy".into(), 2, 10.0, 20.0, 30.0),
                ("op=gemm".into(), 1, 1000.0, 1000.0, 1000.0),
            ]
        );
        let Output::Hist { series, .. } = &results.outputs[1] else {
            panic!("a histogram");
        };
        let counts: Vec<(String, usize)> = series
            .iter()
            .map(|one| (one.key.to_string(), one.count))
            .collect();
        assert_eq!(
            counts,
            [
                ("op=?".into(), 1),
                ("op=copy".into(), 2),
                ("op=gemm".into(), 1)
            ]
        );
        assert!(
            results
                .degradations
                .iter()
                .any(|note| note.contains("1 of 4 events carried no `op`")),
            "{:?}",
            results.degradations
        );
    }

    /// A ranking of a run that measured a window and counted nothing holds
    /// one zero row named by the target, not none (perfsched 5); one that
    /// measured no window holds none. What it left out sums to `0`, never
    /// `-0`.
    #[test]
    fn a_ranking_of_nothing_counted_is_one_zero_row_and_no_negative_zero() {
        let resolved = rss_request(vec![Emit::TopK { k: 3, by: None }], Vec::new());
        let results = aggregate(&resolved, quiet(Vec::new(), 1_000), Unit::Bytes, "bytes").unwrap();
        let Output::TopK { series, rest, .. } = &results.outputs[0] else {
            panic!("a ranking");
        };
        assert_eq!(series.len(), 1, "{series:?}");
        assert_eq!(
            series[0].key.to_string(),
            format!("pid:{}", std::process::id())
        );
        assert_eq!((series[0].events, series[0].total), (0, 0.0));
        assert!(rest.total.is_sign_positive(), "{rest:?}");
        assert!(!serde_json::to_string(rest).unwrap().contains("-0"));

        let results = aggregate(&resolved, quiet(Vec::new(), 0), Unit::Bytes, "bytes").unwrap();
        let Output::TopK { series, .. } = &results.outputs[0] else {
            panic!("a ranking");
        };
        assert!(series.is_empty(), "{series:?}");

        let Output::TopK { rest, .. } = ranked(Vec::new(), 1) else {
            panic!("a ranking");
        };
        assert!(rest.total.is_sign_positive(), "{rest:?}");
    }

    /// A poll regrouped by fewer keys than its events carry adds the readings
    /// of one tick into one (io 5): two devices read at once are one reading
    /// of a direction, not two measurements of half of it.
    #[test]
    fn a_regrouped_poll_folds_the_readings_of_one_tick() {
        let with_file = |ts: u64, ino: u64, bytes: u64| {
            let mut one = event(ts, 1, bytes);
            one.keys.push(KeyValue::File { dev: 28, ino });
            one
        };
        let events = vec![
            with_file(0, 10, 4),
            with_file(0, 11, 8),
            with_file(5, 10, 2),
            with_file(5, 11, 0),
            with_file(9, 10, 6),
        ];
        let resolved = rss_request(
            vec![Emit::Summary, Emit::Hist(BucketLayout::Log2)],
            vec![KeyKind::Task],
        );
        let results = aggregate(
            &resolved,
            quiet(events.clone(), 1_000),
            Unit::Bytes,
            "bytes",
        )
        .unwrap();
        let Output::Summary { series } = &results.outputs[0] else {
            panic!("a summary");
        };
        let one = &series[0];
        assert_eq!(one.key.to_string(), "task:1");
        assert_eq!((one.count, one.min, one.max, one.last), (3, 2.0, 12.0, 6.0));
        assert!((one.mean - 20.0 / 3.0).abs() < 1e-9, "{one:?}");
        let Output::Hist { series, .. } = &results.outputs[1] else {
            panic!("a histogram");
        };
        assert_eq!(series[0].count, 3);
        assert_eq!(series[0].sum, 20.0);

        // Keyed as the events are, nothing is folded.
        let resolved = rss_request(vec![Emit::Summary], vec![KeyKind::Task, KeyKind::File]);
        let results = aggregate(&resolved, quiet(events, 1_000), Unit::Bytes, "bytes").unwrap();
        let Output::Summary { series } = &results.outputs[0] else {
            panic!("a summary");
        };
        let counts: Vec<usize> = series.iter().map(|one| one.count).collect();
        assert_eq!(counts, [3, 2]);
    }

    /// An event with no value attribute is one the collector saw and could not
    /// time -- an L1 hit under AMD's IBS. It is counted beside the numbers in
    /// every emit that reduces values, not refused, and it moves none of them.
    #[test]
    fn an_event_without_a_value_is_counted_as_unmeasured_and_moves_nothing() {
        let mut untimed = event(0, 1, 0);
        untimed.attrs.clear();
        let events = vec![event(0, 1, 8), event(0, 1, 24), untimed];

        let histogram = &hist(&events, "bytes", BucketLayout::Log2, None).unwrap()[0];
        assert_eq!((histogram.count, histogram.unmeasured), (2, 1));
        assert_eq!(histogram.sum, 32.0);

        let summary = &summary(&events, "bytes", None).unwrap()[0];
        assert_eq!((summary.count, summary.unmeasured), (2, 1));
        assert_eq!((summary.min, summary.mean, summary.max), (8.0, 16.0, 24.0));

        let Output::TopK { series, .. } = topk(&events, "bytes", 5, None).unwrap() else {
            panic!("topk is a ranking");
        };
        assert_eq!(
            (series[0].events, series[0].unmeasured, series[0].total),
            (2, 1, 32.0)
        );

        // A value that is there and is not a number is still a collector bug.
        let mut malformed = event(0, 1, 0);
        malformed.attrs = vec![Attr::text("bytes", "eight")];
        assert!(hist(&[malformed], "bytes", BucketLayout::Log2, None).is_err());
    }

    /// There is no bucket below zero, and putting a negative value in `[0, 1)`
    /// would be reporting it as a value it is not.
    #[test]
    fn a_value_no_bucket_can_hold_is_counted_rather_than_placed() {
        let mut negative = event(0, 1, 0);
        negative.attrs = vec![Attr::int("bytes", -5)];
        let events = vec![event(0, 1, 8), negative];

        let series = hist(&events, "bytes", BucketLayout::Log2, None).unwrap();
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
        let series = hist(&events, "bytes", BucketLayout::Linear { width: 1 }, None).unwrap();
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
            hist(&events, "bytes", BucketLayout::Linear { width: 0 }, None),
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
        let Output::TopK { series, rest, .. } = topk(&events, "bytes", 2, None).unwrap() else {
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
        let Output::TopK { series, .. } = topk(&events, "bytes", 10, None).unwrap() else {
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
            ..
        } = stack(&events, "bytes", None).unwrap()
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
        let Output::Stack { unattributed, .. } = stack(&events, "bytes", None).unwrap() else {
            panic!("stack produces a stack");
        };
        assert_eq!(unattributed, 1);
    }

    /// With keys asked for, a stack's row is per series: two threads on one
    /// stack are two rows, each its own thread's; with none, one row, and no
    /// `series` written.
    #[test]
    fn stack_rows_are_keyed_by_the_requests_keys() {
        let on = |tid: i32, stack: u64, ns: u64| Event {
            ts_ns: 1,
            ts_err_ns: 0,
            kind: "cpu.profile".to_owned(),
            keys: vec![KeyValue::task(tid)],
            attrs: vec![Attr::uint(STACK_ID_ATTR, stack), Attr::uint("ns", ns)],
            collector_id: CollectorId::new("profile"),
            seq: 0,
        };
        let events = vec![on(7, 1, 10), on(8, 1, 30), on(7, 1, 5), on(8, 2, 1)];
        let (output, _) = stack_counting(&events, &[KeyKind::Task], "ns", None).unwrap();
        let Output::Stack { stacks, .. } = output else {
            panic!("stack produces a stack");
        };
        let rows: Vec<(String, u64, f64)> = stacks
            .iter()
            .map(|row| {
                (
                    row.series.as_ref().unwrap().to_string(),
                    row.stack,
                    row.total,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("task:8".to_owned(), 1, 30.0),
                ("task:7".to_owned(), 1, 15.0),
                ("task:8".to_owned(), 2, 1.0),
            ]
        );

        let (output, _) = stack_counting(&events, &[], "ns", None).unwrap();
        let Output::Stack { stacks, .. } = output else {
            panic!("stack produces a stack");
        };
        assert_eq!(stacks.len(), 2);
        assert_eq!((stacks[0].stack, stacks[0].total), (1, 45.0));
        let json = serde_json::to_value(&stacks[0]).unwrap();
        assert!(json.get("series").is_none(), "{json}");
    }

    /// A stacked event the collector could not measure is counted and left
    /// out, not a refusal of the whole emit: an IBS op the cpu did not time
    /// carries a stack and no `latency_cycles`.
    #[test]
    fn a_stacked_event_without_its_weight_is_counted_and_left_out() {
        let with = |value: Option<u64>| {
            let mut attrs = vec![Attr::uint(STACK_ID_ATTR, 7)];
            if let Some(value) = value {
                attrs.push(Attr::uint("latency_cycles", value));
            }
            Event {
                ts_ns: 1,
                ts_err_ns: 0,
                kind: "mem.latency".to_owned(),
                keys: Vec::new(),
                attrs,
                collector_id: crate::event::CollectorId::new("ibs"),
                seq: 0,
            }
        };
        let events = vec![with(Some(40)), with(None), with(Some(2))];
        let (output, unmeasured) = stack_counting(&events, &[], "latency_cycles", None).unwrap();
        assert_eq!(unmeasured, 1);
        let Output::Stack { stacks, .. } = output else {
            panic!("stack produces a stack");
        };
        assert_eq!((stacks[0].events, stacks[0].total), (2, 42.0));

        let mut malformed = with(None);
        malformed.attrs.push(crate::event::Attr {
            name: "latency_cycles".to_owned(),
            value: crate::event::AttrValue::Text("x".to_owned()),
        });
        assert!(stack_counting(&[malformed], &[], "latency_cycles", None).is_err());
    }
}
