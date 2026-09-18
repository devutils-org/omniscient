//! The request model.
//!
//! A `Request` is the single object every input surface builds: the CLI, a
//! session file in any supported format, and later the API. Nothing downstream
//! knows which surface produced it, which is what keeps the surfaces from
//! drifting apart.
//!
//! A `Request` says what the user asked for. It deliberately says nothing about
//! how it will be collected: the backend, the filter placement and the
//! degradations belong to `ResolvedRequest`, produced by the resolver once the
//! machine has been introspected.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{one_of, ParseError};
use crate::filter::FilterExpr;
use crate::format::OutputFormat;
use crate::scalar::{Duration, Frequency};

/// A catalog source identifier, `<family>.<metric>`.
///
/// The metric part may itself contain dots (`net.tcp.connect`, `mem.heap.live`);
/// the family is the first segment and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(String);

impl SourceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn family(&self) -> &str {
        self.0.split('.').next().unwrap_or(&self.0)
    }

    pub fn metric(&self) -> &str {
        self.0
            .split_once('.')
            .map(|(_, metric)| metric)
            .unwrap_or("")
    }
}

impl FromStr for SourceId {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if !s.contains('.') {
            return Err(ParseError::new(format!(
                "`{s}` is not a source: a source is `<family>.<metric>`, for instance `io.latency`"
            )));
        }
        if s.split('.').any(|segment| segment.is_empty()) {
            return Err(ParseError::new(format!(
                "source `{s}` has an empty segment"
            )));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_')
        {
            return Err(ParseError::new(format!(
                "source `{s}` may only contain lowercase letters, digits, `_` and `.`"
            )));
        }
        Ok(Self(s.to_owned()))
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

serde_as_string!(SourceId);

/// What to observe.
///
/// Targets are not interchangeable: a system-wide source must refuse `pid`
/// rather than silently return a system-wide number dressed up as per-process
/// attribution. The catalog declares support per target kind and the validator
/// enforces it.
///
/// In a session file a target is a single-key map, `{pid: [4242]}`, or the bare
/// word `system`. Serialization is written by hand rather than derived because
/// the derived form is a YAML tag, which TOML cannot express and nobody wants
/// to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Launch the program and follow its process tree.
    Launch(Vec<String>),
    Pid(Vec<i32>),
    Tid(Vec<i32>),
    /// A cgroup v2 path.
    Cgroup(PathBuf),
    Cpu(CpuSet),
    Iface(String),
    /// A container name or id, resolved to a cgroup plus a pid namespace.
    Container(String),
    System,
}

impl Target {
    /// The process to observe, when the target names one.
    ///
    /// A target that names no process is not an error: an interface or the
    /// whole machine is observed without one.
    pub fn first_pid(&self) -> Option<i32> {
        match self {
            Target::Pid(pids) | Target::Tid(pids) => pids.first().copied(),
            _ => None,
        }
    }

    /// The interface to observe, when the target names one.
    pub fn interface(&self) -> Option<&str> {
        match self {
            Target::Iface(name) => Some(name),
            _ => None,
        }
    }

    /// The catalog key this target is looked up under.
    pub fn kind(&self) -> TargetKind {
        match self {
            Target::Launch(_) => TargetKind::Launch,
            Target::Pid(_) => TargetKind::Pid,
            Target::Tid(_) => TargetKind::Tid,
            Target::Cgroup(_) => TargetKind::Cgroup,
            Target::Cpu(_) => TargetKind::Cpu,
            Target::Iface(_) => TargetKind::Iface,
            Target::Container(_) => TargetKind::Container,
            Target::System => TargetKind::System,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Launch(argv) => write!(f, "launch {}", argv.join(" ")),
            Target::Pid(pids) => write!(f, "pid {}", join_ints(pids)),
            Target::Tid(tids) => write!(f, "tid {}", join_ints(tids)),
            Target::Cgroup(path) => write!(f, "cgroup {}", path.display()),
            Target::Cpu(set) => write!(f, "cpu {set}"),
            Target::Iface(name) => write!(f, "iface {name}"),
            Target::Container(name) => write!(f, "container {name}"),
            Target::System => f.write_str("system"),
        }
    }
}

/// The shape a target takes in a session file: one key, or the word `system`.
#[derive(Deserialize)]
#[serde(untagged)]
enum TargetRepr {
    Word(String),
    Map(Box<TargetMap>),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct TargetMap {
    #[serde(default)]
    launch: Option<Vec<String>>,
    #[serde(default)]
    pid: Option<Ids>,
    #[serde(default)]
    tid: Option<Ids>,
    #[serde(default)]
    cgroup: Option<PathBuf>,
    #[serde(default)]
    cpu: Option<CpuSet>,
    #[serde(default)]
    iface: Option<String>,
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    system: Option<bool>,
}

/// One id or several, so `pid: 4242` and `pid: [4242, 4243]` both work.
#[derive(Deserialize)]
#[serde(untagged)]
enum Ids {
    One(i32),
    Many(Vec<i32>),
}

impl From<Ids> for Vec<i32> {
    fn from(ids: Ids) -> Self {
        match ids {
            Ids::One(id) => vec![id],
            Ids::Many(ids) => ids,
        }
    }
}

impl Serialize for Target {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        if let Target::System = self {
            return serializer.serialize_str("system");
        }
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Target::Launch(argv) => map.serialize_entry("launch", argv)?,
            Target::Pid(pids) => map.serialize_entry("pid", pids)?,
            Target::Tid(tids) => map.serialize_entry("tid", tids)?,
            Target::Cgroup(path) => map.serialize_entry("cgroup", path)?,
            Target::Cpu(set) => map.serialize_entry("cpu", set)?,
            Target::Iface(name) => map.serialize_entry("iface", name)?,
            Target::Container(name) => map.serialize_entry("container", name)?,
            Target::System => unreachable!("handled above"),
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        match TargetRepr::deserialize(deserializer)? {
            TargetRepr::Word(word) if word == "system" => Ok(Target::System),
            TargetRepr::Word(word) => Err(D::Error::custom(format!(
                "`{word}` is not a target; write `system`, or a single-key map such as `{{pid: [4242]}}`"
            ))),
            TargetRepr::Map(map) => {
                let mut targets = Vec::new();
                if let Some(argv) = map.launch {
                    targets.push(Target::Launch(argv));
                }
                if let Some(pids) = map.pid {
                    targets.push(Target::Pid(pids.into()));
                }
                if let Some(tids) = map.tid {
                    targets.push(Target::Tid(tids.into()));
                }
                if let Some(path) = map.cgroup {
                    targets.push(Target::Cgroup(path));
                }
                if let Some(set) = map.cpu {
                    targets.push(Target::Cpu(set));
                }
                if let Some(name) = map.iface {
                    targets.push(Target::Iface(name));
                }
                if let Some(name) = map.container {
                    targets.push(Target::Container(name));
                }
                if map.system == Some(true) {
                    targets.push(Target::System);
                }
                match targets.len() {
                    1 => Ok(targets.pop().expect("length checked")),
                    0 => Err(D::Error::custom(
                        "a target names one of launch, pid, tid, cgroup, cpu, iface, container, system",
                    )),
                    _ => Err(D::Error::custom(
                        "a request observes one target: name a single one, and use several requests for several targets",
                    )),
                }
            }
        }
    }
}

fn join_ints(values: &[i32]) -> String {
    values
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// The target kinds the catalog declares support for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Launch,
    Pid,
    Tid,
    Cgroup,
    Cpu,
    Iface,
    Container,
    System,
}

impl TargetKind {
    pub const ALL: [TargetKind; 8] = [
        TargetKind::Launch,
        TargetKind::Pid,
        TargetKind::Tid,
        TargetKind::Cgroup,
        TargetKind::Cpu,
        TargetKind::Iface,
        TargetKind::Container,
        TargetKind::System,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::Launch => "launch",
            TargetKind::Pid => "pid",
            TargetKind::Tid => "tid",
            TargetKind::Cgroup => "cgroup",
            TargetKind::Cpu => "cpu",
            TargetKind::Iface => "iface",
            TargetKind::Container => "container",
            TargetKind::System => "system",
        }
    }

    /// The target kind this one falls back to when the catalog does not mention
    /// it explicitly.
    ///
    /// Launching a program is following a pid tree, a container resolves to a
    /// cgroup, and a thread is a narrower pid. Everything else stands alone.
    pub fn fallback(self) -> Option<TargetKind> {
        match self {
            TargetKind::Launch => Some(TargetKind::Pid),
            TargetKind::Container => Some(TargetKind::Cgroup),
            TargetKind::Tid => Some(TargetKind::Pid),
            _ => None,
        }
    }
}

impl fmt::Display for TargetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of CPUs, written the way the kernel writes them: `0-3,8,12-15`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuSet(Vec<u32>);

impl CpuSet {
    pub fn cpus(&self) -> &[u32] {
        &self.0
    }
}

impl FromStr for CpuSet {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut cpus = Vec::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part.split_once('-') {
                Some((start, end)) => {
                    let start: u32 = start.trim().parse().map_err(|_| bad_cpu(part))?;
                    let end: u32 = end.trim().parse().map_err(|_| bad_cpu(part))?;
                    if end < start {
                        return Err(ParseError::new(format!(
                            "cpu range `{part}` runs backwards"
                        )));
                    }
                    cpus.extend(start..=end);
                }
                None => cpus.push(part.parse().map_err(|_| bad_cpu(part))?),
            }
        }
        if cpus.is_empty() {
            return Err(ParseError::new("empty cpu set"));
        }
        cpus.sort_unstable();
        cpus.dedup();
        Ok(Self(cpus))
    }
}

fn bad_cpu(part: &str) -> ParseError {
    ParseError::new(format!(
        "`{part}` is not a cpu or a cpu range (expected `0-3,8`)"
    ))
}

impl fmt::Display for CpuSet {
    /// Re-folds consecutive cpus into ranges so the set round-trips.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        let mut iter = self.0.iter().copied().peekable();
        while let Some(start) = iter.next() {
            let mut end = start;
            while iter.peek() == Some(&(end + 1)) {
                end = iter.next().expect("peeked");
            }
            if start == end {
                parts.push(start.to_string());
            } else {
                parts.push(format!("{start}-{end}"));
            }
        }
        f.write_str(&parts.join(","))
    }
}

serde_as_string!(CpuSet);

/// How to observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Aggregate a counter over the session.
    Count,
    /// Sample periodically at a frequency.
    Sample { freq: Frequency },
    /// Record every event.
    Trace,
    /// Re-read a value at an interval.
    Poll { interval: Duration },
    /// Read a value once.
    Snapshot,
}

impl Mode {
    pub const DEFAULT_SAMPLE_FREQ: Frequency = Frequency::from_hz(99);
    pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_nanos(1_000_000_000);

    pub fn kind(self) -> ModeKind {
        match self {
            Mode::Count => ModeKind::Count,
            Mode::Sample { .. } => ModeKind::Sample,
            Mode::Trace => ModeKind::Trace,
            Mode::Poll { .. } => ModeKind::Poll,
            Mode::Snapshot => ModeKind::Snapshot,
        }
    }
}

impl FromStr for Mode {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (name, argument) = match s.split_once('@') {
            Some((name, argument)) => (name.trim(), Some(argument.trim())),
            None => (s, None),
        };
        let mode = match (name, argument) {
            ("count", None) => Mode::Count,
            ("trace", None) => Mode::Trace,
            ("snapshot", None) => Mode::Snapshot,
            ("sample", argument) => Mode::Sample {
                freq: match argument {
                    Some(text) => text.parse()?,
                    None => Mode::DEFAULT_SAMPLE_FREQ,
                },
            },
            ("poll", argument) => Mode::Poll {
                interval: match argument {
                    Some(text) => text.parse()?,
                    None => Mode::DEFAULT_POLL_INTERVAL,
                },
            },
            (name @ ("count" | "trace" | "snapshot"), Some(argument)) => {
                return Err(ParseError::new(format!(
                    "mode `{name}` takes no argument, but `@{argument}` was given \
                     (only `sample@<freq>` and `poll@<interval>` do)"
                )))
            }
            (other, _) => {
                return Err(ParseError::new(format!(
                    "unknown mode `{other}` (expected {})",
                    one_of(ModeKind::ALL.iter().map(|kind| kind.as_str()))
                )))
            }
        };
        Ok(mode)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mode::Count => f.write_str("count"),
            Mode::Trace => f.write_str("trace"),
            Mode::Snapshot => f.write_str("snapshot"),
            Mode::Sample { freq } => write!(f, "sample@{freq}"),
            Mode::Poll { interval } => write!(f, "poll@{interval}"),
        }
    }
}

serde_as_string!(Mode);

/// A mode without its argument, which is what the catalog declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeKind {
    Count,
    Sample,
    Trace,
    Poll,
    Snapshot,
}

impl ModeKind {
    pub const ALL: [ModeKind; 5] = [
        ModeKind::Count,
        ModeKind::Sample,
        ModeKind::Trace,
        ModeKind::Poll,
        ModeKind::Snapshot,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ModeKind::Count => "count",
            ModeKind::Sample => "sample",
            ModeKind::Trace => "trace",
            ModeKind::Poll => "poll",
            ModeKind::Snapshot => "snapshot",
        }
    }

    /// The emits this mode can produce, from the mode x emit matrix.
    ///
    /// Sampling a procfs file, counting a tracepoint without tracing it, or
    /// asking for stacks on a poll are all rejected here rather than quietly
    /// producing something else.
    pub fn valid_emits(self) -> &'static [EmitKind] {
        match self {
            // `raw` belongs here because a counted window *is* one event: the
            // counter, its value, and the window the hardware says it was
            // enabled for. Without it there is no way to see a derived metric's
            // terms, or the counter's own window, which is the honest
            // denominator for a rate.
            ModeKind::Count => &[EmitKind::Rate, EmitKind::Summary, EmitKind::Raw],
            ModeKind::Sample => &[
                EmitKind::Stack,
                EmitKind::TopK,
                EmitKind::Hist,
                EmitKind::Raw,
            ],
            ModeKind::Trace => &[
                EmitKind::Raw,
                EmitKind::Hist,
                EmitKind::TopK,
                EmitKind::Timeline,
                EmitKind::Stack,
                EmitKind::Rate,
                EmitKind::Summary,
            ],
            ModeKind::Poll => &[EmitKind::Raw, EmitKind::Timeline, EmitKind::Summary],
            ModeKind::Snapshot => &[EmitKind::Summary, EmitKind::Raw],
        }
    }
}

impl fmt::Display for ModeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What comes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emit {
    /// Every event, unaggregated.
    Raw,
    /// A histogram, log2 by default.
    Hist(HistScale),
    /// A per-second rate.
    Rate,
    /// The N heaviest contributors.
    TopK(u32),
    /// Aggregated call stacks.
    Stack,
    /// Values over time.
    Timeline,
    /// A single aggregate per series.
    Summary,
}

impl Emit {
    pub const DEFAULT_TOPK: u32 = 10;

    pub fn kind(self) -> EmitKind {
        match self {
            Emit::Raw => EmitKind::Raw,
            Emit::Hist(_) => EmitKind::Hist,
            Emit::Rate => EmitKind::Rate,
            Emit::TopK(_) => EmitKind::TopK,
            Emit::Stack => EmitKind::Stack,
            Emit::Timeline => EmitKind::Timeline,
            Emit::Summary => EmitKind::Summary,
        }
    }
}

impl FromStr for Emit {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let mut parts = s.split(':');
        let name = parts.next().unwrap_or_default().trim();
        let argument = parts.next().map(str::trim);
        let extra = parts.next().map(str::trim);
        let emit = match (name, argument, extra) {
            ("raw", None, _) => Emit::Raw,
            ("rate", None, _) => Emit::Rate,
            ("stack", None, _) => Emit::Stack,
            ("timeline", None, _) => Emit::Timeline,
            ("summary", None, _) => Emit::Summary,
            ("hist", None, _) => Emit::Hist(HistScale::Log2),
            ("hist", Some("log2"), None) => Emit::Hist(HistScale::Log2),
            ("hist", Some("linear"), Some(width)) => Emit::Hist(HistScale::Linear {
                width: crate::scalar::parse_scalar(width)?
                    .try_into()
                    .map_err(|_| ParseError::new("linear histogram width must be positive"))?,
            }),
            ("hist", Some("linear"), None) => {
                return Err(ParseError::new(
                    "a linear histogram needs a bucket width, as in `hist:linear:1ms`",
                ))
            }
            ("hist", Some(other), _) => {
                return Err(ParseError::new(format!(
                    "unknown histogram scale `{other}` (expected log2 or linear)"
                )))
            }
            ("topk", None, _) => Emit::TopK(Emit::DEFAULT_TOPK),
            ("topk", Some(count), None) => Emit::TopK(
                count
                    .parse()
                    .map_err(|_| ParseError::new(format!("`{count}` is not a top-k count")))?,
            ),
            (name @ ("raw" | "rate" | "stack" | "timeline" | "summary"), Some(argument), _) => {
                return Err(ParseError::new(format!(
                    "emit `{name}` takes no argument, but `:{argument}` was given"
                )))
            }
            (other, _, _) => {
                return Err(ParseError::new(format!(
                    "unknown emit `{other}` (expected {})",
                    one_of(EmitKind::ALL.iter().map(|kind| kind.as_str()))
                )))
            }
        };
        Ok(emit)
    }
}

impl fmt::Display for Emit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Emit::Raw => f.write_str("raw"),
            Emit::Rate => f.write_str("rate"),
            Emit::Stack => f.write_str("stack"),
            Emit::Timeline => f.write_str("timeline"),
            Emit::Summary => f.write_str("summary"),
            Emit::Hist(HistScale::Log2) => f.write_str("hist:log2"),
            Emit::Hist(HistScale::Linear { width }) => write!(f, "hist:linear:{width}"),
            Emit::TopK(count) => write!(f, "topk:{count}"),
        }
    }
}

serde_as_string!(Emit);

/// How a histogram lays out its buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistScale {
    /// Power-of-two buckets: what in-kernel aggregation produces cheaply.
    Log2,
    /// Fixed-width buckets, for a known range.
    Linear { width: u64 },
}

/// An emit without its argument, which is what the catalog declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmitKind {
    Raw,
    Hist,
    Rate,
    #[serde(rename = "topk")]
    TopK,
    Stack,
    Timeline,
    Summary,
}

impl EmitKind {
    pub const ALL: [EmitKind; 7] = [
        EmitKind::Raw,
        EmitKind::Hist,
        EmitKind::Rate,
        EmitKind::TopK,
        EmitKind::Stack,
        EmitKind::Timeline,
        EmitKind::Summary,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EmitKind::Raw => "raw",
            EmitKind::Hist => "hist",
            EmitKind::Rate => "rate",
            EmitKind::TopK => "topk",
            EmitKind::Stack => "stack",
            EmitKind::Timeline => "timeline",
            EmitKind::Summary => "summary",
        }
    }
}

impl fmt::Display for EmitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed correlation key.
///
/// Each key names the boundary it crosses. They are the only things a join rule
/// is allowed to match on, which is what keeps the join engine from growing
/// per-subsystem special cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    /// `req:<id>`, a propagated request identity.
    Req,
    /// `task:tid`.
    Task,
    /// `proc:pidns+pid`, unambiguous across containers.
    Proc,
    /// `file:dev+ino`.
    File,
    /// `sock:cookie`, the key that survives softirq processing.
    Sock,
    /// `blk:rq@open_ts`, scoped because request pointers get recycled.
    Blk,
    /// `mem:[base,len)`, matched by interval.
    Mem,
    /// `gpu:fence_ctx+seqno`.
    Gpu,
    /// `cgroup:id`.
    Cgroup,
    /// `iface:name`, the network interface an event belongs to.
    ///
    /// Not only a series label: a qdisc delay, a driver drop and a link error
    /// are three sources whose only shared identity is the interface.
    Iface,
}

impl KeyKind {
    pub const ALL: [KeyKind; 10] = [
        KeyKind::Req,
        KeyKind::Task,
        KeyKind::Proc,
        KeyKind::File,
        KeyKind::Sock,
        KeyKind::Blk,
        KeyKind::Mem,
        KeyKind::Gpu,
        KeyKind::Cgroup,
        KeyKind::Iface,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            KeyKind::Req => "req",
            KeyKind::Task => "task",
            KeyKind::Proc => "proc",
            KeyKind::File => "file",
            KeyKind::Sock => "sock",
            KeyKind::Blk => "blk",
            KeyKind::Mem => "mem",
            KeyKind::Gpu => "gpu",
            KeyKind::Cgroup => "cgroup",
            KeyKind::Iface => "iface",
        }
    }
}

impl FromStr for KeyKind {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        KeyKind::ALL
            .into_iter()
            .find(|key| key.as_str() == s.trim())
            .ok_or_else(|| {
                ParseError::new(format!(
                    "unknown correlation key `{s}` (expected {})",
                    one_of(KeyKind::ALL.iter().map(|key| key.as_str()))
                ))
            })
    }
}

impl fmt::Display for KeyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How call stacks are unwound.
///
/// The mode is part of the request rather than an implementation detail: `dwarf`
/// copies the stack on every sample, so a profile taken with it is not
/// comparable in cost to one taken with frame pointers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackMode {
    /// Frame pointers: cheap, needs a build that keeps them.
    Fp,
    /// DWARF unwinding from a copied stack: portable, expensive.
    Dwarf,
    /// Last Branch Record: cheap and shallow, hardware-limited depth.
    Lbr,
    /// No stacks.
    None,
}

impl StackMode {
    pub const ALL: [StackMode; 4] = [
        StackMode::Fp,
        StackMode::Dwarf,
        StackMode::Lbr,
        StackMode::None,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            StackMode::Fp => "fp",
            StackMode::Dwarf => "dwarf",
            StackMode::Lbr => "lbr",
            StackMode::None => "none",
        }
    }
}

impl FromStr for StackMode {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        StackMode::ALL
            .into_iter()
            .find(|mode| mode.as_str() == s.trim())
            .ok_or_else(|| {
                ParseError::new(format!(
                    "unknown stack mode `{s}` (expected {})",
                    one_of(StackMode::ALL.iter().map(|mode| mode.as_str()))
                ))
            })
    }
}

impl fmt::Display for StackMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An overhead ceiling, as a percentage of the observed workload.
///
/// The resolver degrades to stay under it (`trace` to in-kernel `hist` to
/// `sample`) and reports what it degraded. It never silently drops events to
/// fit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    percent: f64,
}

impl Budget {
    pub fn percent(self) -> f64 {
        self.percent
    }
}

impl FromStr for Budget {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = s.trim().strip_suffix('%').unwrap_or(s.trim());
        let percent: f64 = text
            .parse()
            .map_err(|_| ParseError::new(format!("budget `{s}` is not a percentage (try `2%`)")))?;
        if !percent.is_finite() || percent <= 0.0 {
            return Err(ParseError::new("budget must be a positive percentage"));
        }
        Ok(Self { percent })
    }
}

impl fmt::Display for Budget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}%", self.percent)
    }
}

serde_as_string!(Budget);

/// A pinned backend, bypassing the resolver.
///
/// This exists for the self-benchmark of phase 8, which forces every backend in
/// a chain to measure them against each other. Using it outside that context
/// means measuring a backend the resolver would not have chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendOverride(String);

impl BackendOverride {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for BackendOverride {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = s.trim();
        let id = text.strip_prefix("force:").unwrap_or(text);
        if id.is_empty() {
            return Err(ParseError::new(
                "empty backend override (expected `force:bpf:tp`)",
            ));
        }
        Ok(Self(id.to_owned()))
    }
}

impl fmt::Display for BackendOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "force:{}", self.0)
    }
}

serde_as_string!(BackendOverride);

/// One measurement request: what the user asked for, before any resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub source: SourceId,
    pub target: Target,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emit: Vec<Emit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<KeyKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<FilterExpr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<StackMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendOverride>,
    /// A label, used to name this request's output when a session has several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A session: the requests to run together, plus what they share.
///
/// A session file and a command line build the same thing; a command line is
/// simply a session with one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Session-wide defaults, applied to every request that does not set them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Where results and the resolved file are written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out: Option<PathBuf>,
    #[serde(default)]
    pub format: OutputFormat,
    pub requests: Vec<Request>,
    /// Boundaries to join the requests' events across.
    ///
    /// A join is declared next to the sources it joins, because the two are one
    /// measurement: running the sources without the rule gives two unrelated
    /// numbers, and the rule without the sources gives nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub joins: Vec<crate::join::Rule>,
}

impl Session {
    /// Builds a single-request session, the shape a command line produces.
    pub fn single(request: Request, format: OutputFormat, out: Option<PathBuf>) -> Self {
        Self {
            name: None,
            duration: request.duration,
            budget: request.budget,
            out,
            format,
            requests: vec![request],
            joins: Vec::new(),
        }
    }

    /// Pushes session-wide defaults down into the requests that omit them, so
    /// that every request is self-contained from here on.
    pub fn apply_defaults(&mut self) {
        for request in &mut self.requests {
            if request.duration.is_none() {
                request.duration = self.duration;
            }
            if request.budget.is_none() {
                request.budget = self.budget;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_splits_into_family_and_metric() {
        let source: SourceId = "net.tcp.connect".parse().unwrap();
        assert_eq!(source.family(), "net");
        assert_eq!(source.metric(), "tcp.connect");
    }

    #[test]
    fn a_source_without_a_family_is_refused() {
        assert!("latency".parse::<SourceId>().is_err());
        assert!("io..latency".parse::<SourceId>().is_err());
        assert!("IO.latency".parse::<SourceId>().is_err());
    }

    #[test]
    fn modes_round_trip_with_their_arguments() {
        for text in ["count", "trace", "snapshot", "sample@997hz", "poll@100ms"] {
            let mode: Mode = text.parse().unwrap();
            assert_eq!(mode.to_string(), text);
        }
    }

    #[test]
    fn a_mode_argument_on_a_mode_that_takes_none_is_refused() {
        let error = "count@10ms".parse::<Mode>().unwrap_err();
        assert!(error.message().contains("takes no argument"), "{error}");
    }

    #[test]
    fn emits_round_trip_with_their_arguments() {
        for text in [
            "raw",
            "rate",
            "stack",
            "timeline",
            "summary",
            "hist:log2",
            "hist:linear:1000000",
            "topk:25",
        ] {
            let emit: Emit = text.parse().unwrap();
            assert_eq!(emit.to_string(), text);
        }
        assert_eq!("hist".parse::<Emit>().unwrap(), Emit::Hist(HistScale::Log2));
        assert_eq!(
            "topk".parse::<Emit>().unwrap(),
            Emit::TopK(Emit::DEFAULT_TOPK)
        );
    }

    #[test]
    fn a_linear_histogram_accepts_a_duration_as_its_width() {
        assert_eq!(
            "hist:linear:1ms".parse::<Emit>().unwrap(),
            Emit::Hist(HistScale::Linear { width: 1_000_000 })
        );
    }

    #[test]
    fn cpu_sets_fold_back_into_ranges() {
        let set: CpuSet = "0-3,8,12-13".parse().unwrap();
        assert_eq!(set.cpus(), &[0, 1, 2, 3, 8, 12, 13]);
        assert_eq!(set.to_string(), "0-3,8,12-13");
        assert!("3-1".parse::<CpuSet>().is_err());
    }

    #[test]
    fn a_backend_override_accepts_the_force_prefix_or_a_bare_id() {
        assert_eq!(
            "force:bpf:tp".parse::<BackendOverride>().unwrap().as_str(),
            "bpf:tp"
        );
        assert_eq!(
            "bpf:tp".parse::<BackendOverride>().unwrap().as_str(),
            "bpf:tp"
        );
    }
}
