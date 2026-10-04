//! What a view is given to draw: one run, as `omn` wrote it.
//!
//! The types here mirror the JSON `omn` writes (`results.json`, a flight
//! recorder's `trigger-<n>.json`), field for field, and nothing else. They are
//! a mirror rather than `omn`'s own types because `omn` links this crate: the
//! dependency can only go one way. The mirror is held to the real thing by
//! `tests/view_model.rs`, which reads every fixture through both and fails on
//! a field this side does not know.
//!
//! A live run is the same shape. `omn top` serializes what its runner holds
//! into these types as it arrives ([`Data::live`] says it is still running),
//! so a view never has two code paths: what it draws from a recording is what
//! it would have drawn live.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Everything one run produced, as `results.json` holds it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Run {
    #[serde(default)]
    pub results: Vec<Results>,
    /// The joins, as `omn` wrote them. Kept as JSON: the join engine's output
    /// is large and only the request view reads it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub joins: Vec<Value>,
    /// The decompositions (`omn latency`'s session carries one), as written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decompositions: Vec<Value>,
}

/// What one request measured.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Results {
    pub label: String,
    pub source: String,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub outputs: Vec<Output>,
    #[serde(default)]
    pub events_seen: u64,
    #[serde(default)]
    pub observed_ns: u64,
    #[serde(default)]
    pub window: Window,
    #[serde(default)]
    pub stopped_because: String,
    #[serde(default)]
    pub losses: Losses,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unverified: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degradations: Vec<String>,
    /// The frames behind each stack id, root first.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stacks: BTreeMap<u64, Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default)]
    pub overhead: Overhead,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub served_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_ring: Vec<Value>,
    /// The attribute each event's value was read from (`latency_ns`,
    /// `bytes`, ...): what a view reads a source's value by. Absent from a
    /// recording made before `omn` wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_attr: Option<String>,
    /// How a drained source stamped a row it read from a table the kernel
    /// filled between drains: `drain_close` or `first_event`. Absent when the
    /// run did not say, or from a recording made before it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_stamp: Option<String>,
    /// The kernel's drops placed in time: each drain that found the count
    /// grown, with what it had grown by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drains: Vec<Drain>,
    /// The worst time error of the events this result was made from, when
    /// something saw them before they were aggregated: `omn top` writes it
    /// in `view.json`, and a live run's events carry it. Not part of
    /// `results.json`, whose summaries, timelines and histograms do not say
    /// it; `None` is unknown, never zero.
    #[serde(skip)]
    pub ts_err_ns: Option<u64>,
}

/// One drain that found the kernel had dropped events since the drain before
/// it: when it read, on `CLOCK_MONOTONIC`, and how many.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drain {
    pub ts_ns: u64,
    pub dropped: u64,
}

impl Results {
    /// Every event this result carries, from its `raw` emit; empty when it
    /// was asked for no `raw`.
    pub fn events(&self) -> &[Event] {
        self.outputs
            .iter()
            .find_map(|output| match output {
                Output::Raw { events } => Some(events.as_slice()),
                _ => None,
            })
            .unwrap_or(&[])
    }

    /// The output of one emit, by its name (`summary`, `hist`, `stack`...).
    pub fn output(&self, emit: &str) -> Option<&Output> {
        self.outputs.iter().find(|output| output.emit() == emit)
    }

    /// The largest time error any of its events carries: zero when every
    /// timestamp was taken on `CLOCK_MONOTONIC`. `None` when nothing says --
    /// a result with no `raw` events, from a recording `omn top` did not
    /// write (`view.json` carries it) -- which is unknown, never zero.
    pub fn max_ts_err_ns(&self) -> Option<u64> {
        let raw = self.outputs.iter().find_map(|output| match output {
            Output::Raw { events } => Some(events.iter().map(|event| event.ts_err_ns).max()),
            _ => None,
        });
        match (raw, self.ts_err_ns) {
            (Some(raw), held) => Some(raw.unwrap_or(0).max(held.unwrap_or(0))),
            (None, held) => held,
        }
    }

    /// The frames of a stack id, root first; empty when the id is unknown.
    pub fn frames(&self, stack: u64) -> &[String] {
        self.stacks.get(&stack).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// One emit's worth of results, tagged by `emit` as `omn` writes it.
///
/// An emit this build does not know is kept as [`Output::Other`] with its
/// JSON, rather than failing the whole file: a recording from a later `omn`
/// still draws every view that does not need it.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Raw {
        events: Vec<Event>,
    },
    Timeline {
        series: Vec<Series>,
        /// The moment every point's offset is from, on `CLOCK_MONOTONIC`:
        /// the result's first event. Absent from a recording made before
        /// `omn` wrote it.
        first_ts_ns: Option<u64>,
    },
    Summary {
        series: Vec<SeriesSummary>,
    },
    Rate {
        series: Vec<SeriesRate>,
    },
    Hist {
        layout: Value,
        series: Vec<SeriesHist>,
    },
    TopK {
        k: u32,
        series: Vec<SeriesTotal>,
        rest: Rest,
    },
    Stack {
        stacks: Vec<StackCount>,
        unattributed: u64,
        short: Option<Shortfall>,
    },
    Other(Value),
}

impl Output {
    /// The emit's name, as the session spells it.
    pub fn emit(&self) -> &str {
        match self {
            Output::Raw { .. } => "raw",
            Output::Timeline { .. } => "timeline",
            Output::Summary { .. } => "summary",
            Output::Rate { .. } => "rate",
            Output::Hist { .. } => "hist",
            Output::TopK { .. } => "topk",
            Output::Stack { .. } => "stack",
            Output::Other(value) => value.get("emit").and_then(Value::as_str).unwrap_or("?"),
        }
    }
}

impl Serialize for Output {
    /// As `omn` writes it: the fields of the emit, tagged by `emit`.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error as _;
        let fields = match self {
            Output::Raw { events } => serde_json::json!({ "events": events }),
            Output::Timeline {
                series,
                first_ts_ns,
            } => match first_ts_ns {
                Some(first) => serde_json::json!({ "series": series, "first_ts_ns": first }),
                None => serde_json::json!({ "series": series }),
            },
            Output::Summary { series } => serde_json::json!({ "series": series }),
            Output::Rate { series } => serde_json::json!({ "series": series }),
            Output::Hist { layout, series } => {
                serde_json::json!({ "layout": layout, "series": series })
            }
            Output::TopK { k, series, rest } => {
                serde_json::json!({ "k": k, "series": series, "rest": rest })
            }
            Output::Stack {
                stacks,
                unattributed,
                short,
            } => {
                let mut fields =
                    serde_json::json!({ "stacks": stacks, "unattributed": unattributed });
                if let Some(short) = short {
                    fields["short"] = serde_json::to_value(short).map_err(S::Error::custom)?;
                }
                fields
            }
            Output::Other(value) => return value.serialize(serializer),
        };
        let mut tagged = serde_json::Map::new();
        tagged.insert("emit".to_owned(), Value::String(self.emit().to_owned()));
        if let Value::Object(fields) = fields {
            tagged.extend(fields);
        }
        Value::Object(tagged).serialize(serializer)
    }
}

#[derive(Deserialize)]
struct RawOut {
    events: Vec<Event>,
}
#[derive(Deserialize)]
struct SeriesOut<T> {
    series: Vec<T>,
}
#[derive(Deserialize)]
struct HistOut {
    #[serde(default)]
    layout: Value,
    series: Vec<SeriesHist>,
}
#[derive(Deserialize)]
struct TopKOut {
    k: u32,
    series: Vec<SeriesTotal>,
    #[serde(default)]
    rest: Rest,
}
#[derive(Deserialize)]
struct StackOut {
    stacks: Vec<StackCount>,
    #[serde(default)]
    unattributed: u64,
    #[serde(default)]
    short: Option<Shortfall>,
}

impl<'de> Deserialize<'de> for Output {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let emit = value
            .get("emit")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let typed = |value: Value| -> Result<Output, serde_json::Error> {
            Ok(match emit.as_str() {
                "raw" => Output::Raw {
                    events: serde_json::from_value::<RawOut>(value)?.events,
                },
                "timeline" => Output::Timeline {
                    first_ts_ns: value.get("first_ts_ns").and_then(Value::as_u64),
                    series: serde_json::from_value::<SeriesOut<Series>>(value)?.series,
                },
                "summary" => Output::Summary {
                    series: serde_json::from_value::<SeriesOut<SeriesSummary>>(value)?.series,
                },
                "rate" => Output::Rate {
                    series: serde_json::from_value::<SeriesOut<SeriesRate>>(value)?.series,
                },
                "hist" => {
                    let hist: HistOut = serde_json::from_value(value)?;
                    Output::Hist {
                        layout: hist.layout,
                        series: hist.series,
                    }
                }
                "topk" => {
                    let topk: TopKOut = serde_json::from_value(value)?;
                    Output::TopK {
                        k: topk.k,
                        series: topk.series,
                        rest: topk.rest,
                    }
                }
                "stack" => {
                    let stack: StackOut = serde_json::from_value(value)?;
                    Output::Stack {
                        stacks: stack.stacks,
                        unattributed: stack.unattributed,
                        short: stack.short,
                    }
                }
                _ => Output::Other(value),
            })
        };
        // An emit this side knows by name but cannot read is an error, not an
        // `Other`: the mirror drifted from `omn`, and the test that holds the
        // two together has to see it.
        typed(value).map_err(serde::de::Error::custom)
    }
}

/// One series of a `timeline`: its points, offset from the collection's
/// first event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Series {
    pub key: String,
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub offset_ns: u64,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesSummary {
    pub key: String,
    pub count: u64,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub last: f64,
    #[serde(default)]
    pub unmeasured: u64,
    #[serde(default)]
    pub short: Option<Shortfall>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesRate {
    pub key: String,
    pub events: u64,
    pub total: f64,
    pub per_second: Option<f64>,
    pub window_ns: u64,
    #[serde(default)]
    pub counted_window: bool,
    #[serde(default)]
    pub short: Option<Shortfall>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesHist {
    pub key: String,
    pub count: u64,
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    #[serde(default)]
    pub short: Option<Shortfall>,
    pub buckets: Vec<Bucket>,
    #[serde(default)]
    pub unbucketable: u64,
    #[serde(default)]
    pub unmeasured: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    pub low: u64,
    #[serde(default)]
    pub high: Option<u64>,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesTotal {
    pub key: String,
    pub events: u64,
    pub total: f64,
    #[serde(default)]
    pub unmeasured: u64,
    #[serde(default)]
    pub short: Option<Shortfall>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rest {
    pub series: u64,
    pub events: u64,
    pub total: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackCount {
    pub stack: u64,
    pub events: u64,
    pub total: f64,
}

/// What a distribution built from a lossy run is missing, as `omn` says it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shortfall {
    #[serde(default)]
    pub dropped: u64,
    #[serde(default)]
    pub throttled: u64,
    #[serde(default)]
    pub records: u64,
    #[serde(default)]
    pub fraction: Option<f64>,
    #[serde(default)]
    pub per_record: bool,
    #[serde(default)]
    pub evicted: u64,
    #[serde(default)]
    pub note: String,
}

/// What a collection lost, as `omn` counted it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Losses {
    #[serde(default)]
    pub dropped: u64,
    #[serde(default)]
    pub failed_reads: u64,
    #[serde(default)]
    pub late_ticks: u64,
    #[serde(default)]
    pub undecoded: u64,
    #[serde(default)]
    pub throttled: u64,
    #[serde(default)]
    pub evicted: u64,
    #[serde(default)]
    pub reasons: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrun: Option<Value>,
}

impl Losses {
    /// The same test `omn`'s `Losses::is_clean` makes.
    pub fn is_clean(&self) -> bool {
        self.dropped == 0
            && self.failed_reads == 0
            && self.late_ticks == 0
            && self.undecoded == 0
            && self.throttled == 0
            && self.evicted == 0
    }

    /// Records that never arrived or could not be read: what a figure is
    /// short of. Late ticks are not in it -- a late read is a read.
    pub fn missing(&self) -> u64 {
        self.dropped + self.failed_reads + self.undecoded + self.throttled + self.evicted
    }
}

/// When a request was watching, on `CLOCK_MONOTONIC`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    #[serde(default)]
    pub run_opened_ns: u64,
    #[serde(default)]
    pub opened_ns: u64,
    #[serde(default)]
    pub closed_ns: u64,
    #[serde(default)]
    pub attached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_ns: Option<u64>,
}

/// What a measurement cost, as `cost.json` has it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Overhead {
    #[serde(default)]
    pub collector_cpu_ns: u64,
    #[serde(default)]
    pub attributed_cpu_ns: u64,
    #[serde(default)]
    pub observed_cpu_ns: Option<u64>,
    #[serde(default)]
    pub observed_from: String,
    #[serde(default)]
    pub wall_ns: u64,
    #[serde(default)]
    pub measurement_ns: u64,
    #[serde(default)]
    pub measurements: u64,
    #[serde(default)]
    pub in_target_cpu_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_target_runs: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub in_target_left_out: u64,
    #[serde(default)]
    pub in_target_from: String,
    #[serde(default)]
    pub before_window_cpu_ns: u64,
    #[serde(default)]
    pub after_window_cpu_ns: u64,
}

impl Overhead {
    /// The tool's cpu as a share of the target's, as `omn`'s
    /// `Overhead::percent` computes it.
    pub fn percent(&self) -> Option<f64> {
        let observed = self.observed_cpu_ns.filter(|observed| *observed > 0)?;
        let inside = self.in_target_cpu_ns.unwrap_or(0);
        let work = match observed.checked_sub(inside) {
            Some(work) if work > 0 => work,
            _ => observed,
        };
        Some((self.attributed_cpu_ns + inside) as f64 * 100.0 / work as f64)
    }
}

/// One event, as `omn`'s `raw` emit writes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub ts_ns: u64,
    #[serde(default)]
    pub ts_err_ns: u64,
    pub kind: String,
    #[serde(default)]
    pub keys: Vec<Key>,
    #[serde(default)]
    pub attrs: Vec<Attr>,
    #[serde(default)]
    pub collector_id: String,
    #[serde(default)]
    pub seq: u64,
}

impl Event {
    pub fn attr(&self, name: &str) -> Option<&AttrValue> {
        self.attrs
            .iter()
            .find(|attr| attr.name == name)
            .map(|attr| &attr.value)
    }

    /// An attribute as a number, whatever its JSON spelling.
    pub fn number(&self, name: &str) -> Option<f64> {
        self.attr(name).and_then(AttrValue::as_f64)
    }

    /// The thread the event is keyed on, if any.
    pub fn tid(&self) -> Option<i32> {
        self.keys.iter().find_map(|key| match key {
            Key::Task { tid } => Some(*tid),
            _ => None,
        })
    }

    /// The process the event is keyed on, if any.
    pub fn pid(&self) -> Option<i32> {
        self.keys.iter().find_map(|key| match key {
            Key::Proc { pid, .. } => Some(*pid),
            _ => None,
        })
    }
}

/// A typed correlation key, as `omn` writes it: one variant per boundary.
///
/// A key this side does not know is kept as [`Key::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    Req {
        id: String,
    },
    Task {
        tid: i32,
    },
    Proc {
        #[serde(default)]
        pidns: u64,
        pid: i32,
    },
    File {
        dev: u64,
        ino: u64,
    },
    Sock {
        cookie: u64,
    },
    Blk {
        ptr: u64,
        open_ts: u64,
    },
    Mem {
        base: u64,
        len: u64,
    },
    Gpu {
        fence_ctx: u64,
        seqno: u64,
    },
    Cgroup {
        id: u64,
    },
    Iface {
        #[serde(default)]
        netns: u64,
        index: u32,
        name: String,
    },
    Dev {
        major: u32,
        minor: u32,
    },
    Cpu {
        cpu: u32,
    },
    Node {
        node: u32,
    },
    #[serde(untagged)]
    Other(Value),
}

impl fmt::Display for Key {
    /// The spelling `omn` gives a series key, so a key read off an event
    /// names the same series as the aggregated outputs' `key`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Req { id } => write!(f, "req:{id}"),
            Key::Task { tid } => write!(f, "task:{tid}"),
            Key::Proc { pidns, pid } => write!(f, "proc:{pidns}+{pid}"),
            Key::File { dev, ino } => write!(f, "file:{dev}+{ino}"),
            Key::Sock { cookie } => write!(f, "sock:{cookie}"),
            Key::Blk { ptr, open_ts } => write!(f, "blk:{ptr:#x}@{open_ts}"),
            Key::Mem { base, len } => write!(f, "mem:[{base:#x},{:#x})", base + len),
            Key::Gpu { fence_ctx, seqno } => write!(f, "gpu:{fence_ctx}+{seqno}"),
            Key::Cgroup { id } => write!(f, "cgroup:{id}"),
            // Another namespace's interface, judged as `omn` judges it:
            // against the namespace of the process printing.
            Key::Iface { netns, name, .. } if *netns != 0 && *netns != own_netns() => {
                write!(f, "iface:{name}@netns:{netns}")
            }
            Key::Iface { name, .. } => write!(f, "iface:{name}"),
            Key::Dev { major, minor } => write!(f, "dev:{major}+{minor}"),
            Key::Cpu { cpu } => write!(f, "cpu:{cpu}"),
            Key::Node { node } => write!(f, "node:{node}"),
            Key::Other(value) => write!(f, "{value}"),
        }
    }
}

/// This process's network namespace inode, as `/proc/self/ns/net` names it;
/// 0 where it cannot be read.
fn own_netns() -> u64 {
    static OWN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *OWN.get_or_init(|| {
        std::fs::read_link("/proc/self/ns/net")
            .ok()
            .and_then(|link| {
                let link = link.to_string_lossy().into_owned();
                link.strip_prefix("net:[")?.strip_suffix(']')?.parse().ok()
            })
            .unwrap_or(0)
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attr {
    pub name: String,
    pub value: AttrValue,
}

/// An attribute's value: an integer, a float or a text, untagged in JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttrValue {
    Uint(u64),
    Int(i64),
    Float(f64),
    Text(String),
}

impl AttrValue {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            AttrValue::Uint(value) => Some(*value as f64),
            AttrValue::Int(value) => Some(*value as f64),
            AttrValue::Float(value) => Some(*value),
            AttrValue::Text(_) => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            AttrValue::Text(text) => Some(text),
            _ => None,
        }
    }
}

impl fmt::Display for AttrValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttrValue::Uint(value) => write!(f, "{value}"),
            AttrValue::Int(value) => write!(f, "{value}"),
            AttrValue::Float(value) => write!(f, "{value}"),
            AttrValue::Text(text) => f.write_str(text),
        }
    }
}

/// What the catalog says about a source, which a result file does not carry:
/// handed in by `omn`, which holds the catalog, and written beside a
/// recording by `omn top -o`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceFacts {
    /// `low`, `medium` or `high`: how reproducible the source is between two
    /// runs of the same workload.
    pub determinism: String,
    /// The catalog's one-line summary.
    #[serde(default)]
    pub summary: String,
}

/// A request that was asked for and not served, with the reason in the
/// resolver's or the collector's own words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// The request's label, which names its source when it has none.
    pub label: String,
    pub source: String,
    pub reason: String,
}

/// What `omn top -o` writes beside `results.json`: what the result file
/// itself does not hold.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Beside {
    /// Where the run came from, as `omn top` described it.
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub facts: BTreeMap<String, SourceFacts>,
    #[serde(default)]
    pub refused: Vec<Refusal>,
    /// Each result's worst time error, by label, from its events before they
    /// were aggregated: `results.json` does not carry it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ts_err_ns: BTreeMap<String, u64>,
    /// What the view of the live run spent, drawing and handing events over:
    /// taken out of every collector's figure in `results.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_cpu_ns: Option<u64>,
}

impl Beside {
    /// Gives each result of `data` the time error this file holds for it.
    pub fn apply_ts_err(&self, data: &mut Data) {
        for results in &mut data.run.results {
            if let Some(ts_err) = self.ts_err_ns.get(&results.label) {
                results.ts_err_ns = Some(*ts_err);
            }
        }
    }
}

/// The name of the file `omn top -o` writes beside `results.json`.
pub const BESIDE: &str = "view.json";

/// Everything a view is given.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Data {
    /// What this is: a live target, or the path of a recording.
    pub origin: String,
    pub run: Run,
    /// What the catalog says about each source, by source name.
    pub facts: BTreeMap<String, SourceFacts>,
    /// The requests asked for and not served, with why.
    pub refused: Vec<Refusal>,
    /// A flight recorder document's trigger, when the run is one.
    pub trigger: Option<Value>,
    /// Set while a live run is still going: every figure is provisional --
    /// losses, overhead and stack names are only known at the run's end.
    pub live: Option<Live>,
}

/// Where a live run is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Live {
    /// When the run opened, and the last moment its runner reported, on
    /// `CLOCK_MONOTONIC`.
    pub opened_ns: u64,
    pub now_ns: u64,
}

/// Why a recording could not be read.
#[derive(Debug)]
pub struct LoadError(pub String);

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoadError {}

impl Data {
    /// Reads a recorded run: a `results.json`, a flight recorder's
    /// `trigger-<n>.json`, or a directory holding either (`results.json`
    /// first, then the first document). A `view.json` beside it is read too.
    pub fn load(path: &Path) -> Result<Data, LoadError> {
        let file = Self::locate(path)?;
        let text = std::fs::read_to_string(&file)
            .map_err(|error| LoadError(format!("cannot read {}: {error}", file.display())))?;
        let mut data = Self::from_json_text(&text).map_err(|error| {
            LoadError(format!(
                "{} is not a run omn wrote: {error}",
                file.display()
            ))
        })?;
        data.origin = format!("replay of {}", file.display());
        if let Some(beside) = file.parent().map(|dir| dir.join(BESIDE)) {
            if beside.is_file() {
                let text = std::fs::read_to_string(&beside).map_err(|error| {
                    LoadError(format!("cannot read {}: {error}", beside.display()))
                })?;
                let beside: Beside = serde_json::from_str(&text).map_err(|error| {
                    LoadError(format!(
                        "{} is not what omn top writes: {error}",
                        beside.display()
                    ))
                })?;
                beside.apply_ts_err(&mut data);
                data.facts = beside.facts;
                data.refused = beside.refused;
                if !beside.origin.is_empty() {
                    data.origin = format!("replay of {} ({})", file.display(), beside.origin);
                }
            }
        }
        Ok(data)
    }

    /// Which file a path names.
    fn locate(path: &Path) -> Result<PathBuf, LoadError> {
        if path.is_file() {
            return Ok(path.to_owned());
        }
        if !path.is_dir() {
            return Err(LoadError(format!("{} does not exist", path.display())));
        }
        let results = path.join("results.json");
        if results.is_file() {
            return Ok(results);
        }
        let first = path.join("trigger-1.json");
        if first.is_file() {
            return Ok(first);
        }
        Err(LoadError(format!(
            "{} holds neither a results.json nor a flight recorder's trigger-1.json; `omn trace \
             --format json -o DIR` and `omn top -o DIR` write the first",
            path.display()
        )))
    }

    /// Reads one JSON document `omn` wrote: a run, or a recorder document
    /// (a run with its `trigger` beside `results`).
    pub fn from_json_text(text: &str) -> Result<Data, serde_json::Error> {
        let value: Value = serde_json::from_str(text)?;
        Self::from_json(value)
    }

    /// The same, from a parsed document.
    pub fn from_json(mut value: Value) -> Result<Data, serde_json::Error> {
        let trigger = value
            .as_object_mut()
            .and_then(|object| object.remove("trigger"));
        let run: Run = serde_json::from_value(value)?;
        Ok(Data {
            run,
            trigger,
            ..Data::default()
        })
    }

    /// The results of one source, in the order the run holds them.
    pub fn of_source<'a>(&'a self, source: &'a str) -> impl Iterator<Item = &'a Results> + 'a {
        self.run
            .results
            .iter()
            .filter(move |results| results.source == source)
    }

    /// The first result of a source, if the run has one.
    pub fn first(&self, source: &str) -> Option<&Results> {
        self.run
            .results
            .iter()
            .find(|results| results.source == source)
    }

    /// The results a view's need is served by: the request `omn top` labelled
    /// for it, or -- in a run some other session made, `omn latency`'s, `omn
    /// trace`'s -- every request of its source.
    pub fn for_need(&self, label: &str, source: &str) -> Vec<&Results> {
        let labelled: Vec<&Results> = self
            .run
            .results
            .iter()
            .filter(|results| results.label == label)
            .collect();
        if !labelled.is_empty() {
            return labelled;
        }
        self.run
            .results
            .iter()
            .filter(|results| results.source == source)
            .collect()
    }

    /// Why a source the run asked for is not in it, if it was refused.
    pub fn refusal(&self, source: &str) -> Option<&Refusal> {
        self.refused.iter().find(|refusal| refusal.source == source)
    }

    /// Whether the run had a target of its own -- a launched program -- rather
    /// than the machine: what decides the first screen (`process` or
    /// `system`). Read from the results' `lifetime`, which only a launched
    /// program has; a live run says it by opening `process` itself.
    pub fn has_target(&self) -> bool {
        self.run
            .results
            .iter()
            .any(|results| results.lifetime.is_some())
    }

    /// Where the run's time starts, on `CLOCK_MONOTONIC`: the earliest window
    /// any result opened.
    pub fn opened_ns(&self) -> u64 {
        if let Some(live) = &self.live {
            return live.opened_ns;
        }
        self.run
            .results
            .iter()
            .map(|results| results.window.run_opened_ns)
            .filter(|opened| *opened > 0)
            .min()
            .unwrap_or(0)
    }

    /// The latest moment the run holds: the last window's close, or now for
    /// a live run.
    pub fn closed_ns(&self) -> u64 {
        if let Some(live) = &self.live {
            return live.now_ns;
        }
        self.run
            .results
            .iter()
            .map(|results| results.window.closed_ns)
            .filter(|closed| *closed != u64::MAX)
            .max()
            .unwrap_or(0)
    }
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}
