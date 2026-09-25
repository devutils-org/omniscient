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
    /// Whether this names part of the machine rather than a workload: its
    /// collectors do not act on a program and do not decide when a run ends.
    ///
    /// That is what lets any number of machine targets sit beside one workload
    /// target in a run -- the shape of every confounder check, where the
    /// machine's state is read at `system` or `cpu` and the thing it might
    /// explain is measured on the program.
    pub fn is_machine(&self) -> bool {
        matches!(self, Target::System | Target::Cpu(_) | Target::Iface(_))
    }

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
    /// `dev:major+minor`, the block device an event belongs to.
    ///
    /// Without it the diskstats sources have nowhere to put the device they
    /// read, so they sum every one of them into a single number: a machine
    /// with an NVMe and a spinning disk reports one figure for both, and the
    /// `topk` over devices those entries declare cannot be expressed at all.
    Dev,
    /// `cpu:n`, the logical cpu an event belongs to.
    ///
    /// A frequency or an idle residency is per core and the mean across
    /// twenty-four of them describes no core. This is what makes a per-cpu
    /// series possible rather than a mean with a range beside it.
    Cpu,
}

impl KeyKind {
    pub const ALL: [KeyKind; 12] = [
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
        KeyKind::Dev,
        KeyKind::Cpu,
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
            KeyKind::Dev => "dev",
            KeyKind::Cpu => "cpu",
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

/// When a request is attached and detached, relative to the run.
///
/// Written `1s..3s`: attached one second after the launch gate opens, detached
/// three seconds after it, while every other request of the session keeps
/// running. Either end may be left open -- `1s..` is attached late and kept to
/// the end, `..2s` is attached with the rest and detached early -- and a
/// request with no window at all is watched for the whole run, as every
/// request was before hot attach.
///
/// One string rather than a map, so that it reads the same in the three
/// session formats and on a command line, and so that a dumped request reads
/// back as the text that was written: `0s..2s` and `..2s` watch the same span
/// and are kept apart, because the file is a record of what was asked.
///
/// **A window the run decides.** `on lock` is a window whose ends are not
/// times: the request is prepared before the run and held, attached when the
/// session's escalation names the `lock` layer from the cheap pass's own
/// numbers, and detached when it names another. The session says which
/// objective the escalation decides for, in its `escalate:` block, and that
/// objective is copied here by [`Session::apply_defaults`] so that the runner,
/// which is handed requests and not sessions, can say what a decision was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// When the request is attached. `None` is with the rest, before the gate.
    pub start: Option<Duration>,
    /// When it is detached. `None` is at the end of the run.
    pub stop: Option<Duration>,
    /// Held for the escalation: attached when it names this layer. Neither
    /// end is a time when this is set.
    pub on: Option<Held>,
}

/// A request held for the escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    /// The layer whose detailed source this is.
    pub layer: Layer,
    /// What the escalation decides for: the session's, copied down. `None`
    /// until [`Session::apply_defaults`] has run, and refused there when the
    /// session names none.
    pub objective: Option<crate::triage::Objective>,
}

/// Where a task's wall clock can go, as the escalation names it: the four
/// states thread state analysis splits a wall clock into, each with the
/// detailed sources that say *why* rather than *how long*.
///
/// The cheap pass tells these apart and nothing finer: which lock, which
/// function, which device is the detailed layer's question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    /// On a cpu, doing its own work: the profile says which function.
    Cpu,
    /// Runnable and waiting for a cpu: the run queue says how long, per wakeup.
    Queue,
    /// Blocked uninterruptibly, which is a device and usually a disk.
    Disk,
    /// Asleep waiting for something another task will do: a lock, a
    /// condition, a timer. The lock sources say which, and whether it was one.
    Lock,
}

impl Layer {
    pub const ALL: [Layer; 4] = [Layer::Cpu, Layer::Queue, Layer::Disk, Layer::Lock];

    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Cpu => "cpu",
            Layer::Queue => "queue",
            Layer::Disk => "disk",
            Layer::Lock => "lock",
        }
    }

    /// The state of a task this layer explains, in the words the cheap pass
    /// reports it in.
    pub fn state(self) -> &'static str {
        match self {
            Layer::Cpu => "on a cpu",
            Layer::Queue => "runnable, waiting for a cpu",
            Layer::Disk => "blocked uninterruptibly",
            Layer::Lock => "blocked sleeping",
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Layer {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        Layer::ALL
            .into_iter()
            .find(|layer| layer.as_str() == s)
            .ok_or_else(|| {
                ParseError::new(format!(
                    "`{s}` is not a layer the escalation names: {}",
                    Layer::ALL
                        .iter()
                        .map(|layer| format!("`{layer}` ({})", layer.state()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }
}

impl Window {
    /// A window the escalation decides, for the `layer` it names.
    pub fn on(layer: Layer) -> Self {
        Window {
            start: None,
            stop: None,
            on: Some(Held {
                layer,
                objective: None,
            }),
        }
    }

    /// When the request is attached, in nanoseconds after the gate.
    pub fn start_ns(&self) -> u64 {
        self.start.map_or(0, Duration::as_nanos)
    }

    /// Whether the request is attached after the others, once the program is
    /// already running -- which is what a collector given at exec cannot do.
    /// A held request always is.
    pub fn attaches_late(&self) -> bool {
        self.start_ns() > 0 || self.on.is_some()
    }

    /// The layer this request is held for, when the escalation decides its
    /// window.
    pub fn layer(&self) -> Option<Layer> {
        self.on.map(|held| held.layer)
    }
}

impl FromStr for Window {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(layer) = s.strip_prefix("on ") {
            return Ok(Window::on(layer.parse()?));
        }
        let Some((start, stop)) = s.split_once("..") else {
            return Err(ParseError::new(format!(
                "`{s}` is not a window: write `<start>..<stop>` relative to the run, for \
                 instance `1s..3s`, `1s..` or `..2s`, or `on <layer>` for one the escalation \
                 decides, such as `on lock`"
            )));
        };
        let end = |text: &str| -> Result<Option<Duration>, ParseError> {
            let text = text.trim();
            if text.is_empty() {
                Ok(None)
            } else {
                text.parse().map(Some)
            }
        };
        let window = Window {
            start: end(start)?,
            stop: end(stop)?,
            on: None,
        };
        if window.start.is_none() && window.stop.is_none() {
            return Err(ParseError::new(
                "the window `..` is the run's own: leave `window` out to watch the whole run",
            ));
        }
        if let (Some(start), Some(stop)) = (window.start, window.stop) {
            if stop <= start {
                return Err(ParseError::new(format!(
                    "the window `{s}` closes before it opens: it is attached at {start} and \
                     would be detached at {stop}"
                )));
            }
        }
        Ok(window)
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(held) = self.on {
            return write!(f, "on {}", held.layer);
        }
        if let Some(start) = self.start {
            write!(f, "{start}")?;
        }
        f.write_str("..")?;
        if let Some(stop) = self.stop {
            write!(f, "{stop}")?;
        }
        Ok(())
    }
}

serde_as_string!(Window);

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
    /// When, inside the run, this request is attached and detached. Absent is
    /// the whole run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Window>,
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
    /// Wholes to split into the parts the requests measured, with what is
    /// left over.
    ///
    /// Declared here for the reason a join is: the parts are rows of one
    /// table only because they were collected over one window against one
    /// target, which is what this session is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decompose: Vec<crate::join::Decomposition>,
    /// What the run decides for itself: which layer's detailed sources to
    /// attach, from the cheap pass's own numbers. Present exactly when some
    /// request's window is `on <layer>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate: Option<Escalation>,
    /// The flight recorder: a run whose duration is *until a predicate
    /// fires*, which keeps only what its retention bound holds and writes one
    /// document per firing. Absent is an ordinary run, with one document at
    /// its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<Recording>,
}

/// A session's recording: the trigger, and the three bounds that make a run
/// with no fixed end still a bounded one.
///
/// Every bound is `Option` so that its absence is refused in this project's
/// words by [`Session::check_recording`] rather than in serde's: a recording
/// with no retention would keep everything, one with no bound on documents
/// could write forever, and "missing field `documents`" says neither.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recording {
    /// When a document is written: the filter grammar, over the events one
    /// round of the `on` request read. `value` names that request's measured
    /// value, whatever its collector calls it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<FilterExpr>,
    /// The request whose rounds the trigger reads, by label or by source.
    /// May be left out when the session has one request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// How far back a document reaches when the trigger fires: what the
    /// run keeps, and everything older is evicted and counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain: Option<Duration>,
    /// How long the run goes on recording into a document after its trigger
    /// fired, before the document is written. Zero when absent: the document
    /// ends at the round that fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Duration>,
    /// The most documents this run writes; the run ends with the last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documents: Option<u32>,
}

impl Recording {
    /// The name the trigger reads a request's measured value by.
    pub const VALUE: &'static str = "value";

    /// The retention bound, in nanoseconds. Only meaningful once
    /// [`Session::check_recording`] has accepted the session.
    pub fn retain_ns(&self) -> u64 {
        self.retain.map_or(0, Duration::as_nanos)
    }

    pub fn after_ns(&self) -> u64 {
        self.after.map_or(0, Duration::as_nanos)
    }
}

/// A session's escalation: the objective its decisions are for.
///
/// Required rather than defaulted, for the reason every triage request carries
/// one: the layer that dominates a task's wall clock is the suspect *for its
/// latency*, and a run escalating for an objective nobody named would attach
/// a confident wrong layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Escalation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<crate::triage::Objective>,
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
            decompose: Vec::new(),
            escalate: None,
            record: None,
        }
    }

    /// Pushes session-wide defaults down into the requests that omit them, so
    /// that every request is self-contained from here on.
    pub fn apply_defaults(&mut self) {
        let objective = self.escalate.and_then(|escalation| escalation.objective);
        for request in &mut self.requests {
            if request.duration.is_none() {
                request.duration = self.duration;
            }
            if request.budget.is_none() {
                request.budget = self.budget;
            }
            if let Some(held) = request
                .window
                .as_mut()
                .and_then(|window| window.on.as_mut())
            {
                held.objective = held.objective.or(objective);
            }
        }
    }

    /// Refuses an escalation that cannot mean anything as written: requests
    /// held for one the session does not declare, one declared with nothing
    /// held for it, or one with no objective, or an objective it cannot
    /// decide for.
    ///
    /// Asked where a session is built, from a file or from a command line, so
    /// the refusal comes before anything is resolved or launched. What needs
    /// the machine -- whether the cheap pass's sources are there -- is asked
    /// by the runner, before the launch.
    pub fn check_escalation(&self) -> Result<(), ParseError> {
        let held: Vec<String> = self
            .requests
            .iter()
            .filter(|request| request.window.is_some_and(|window| window.on.is_some()))
            .map(|request| {
                format!(
                    "`{}`",
                    request
                        .label
                        .clone()
                        .unwrap_or_else(|| request.source.to_string())
                )
            })
            .collect();
        match (self.escalate, held.is_empty()) {
            (None, true) => Ok(()),
            (None, false) => Err(ParseError::new(format!(
                "{} {} held for an escalation (`window: on <layer>`), and this session declares \
                 none: add `escalate: {{objective: latency}}`, which says what the run decides \
                 for, beside cheap requests it decides from",
                held.join(", "),
                if held.len() == 1 { "is" } else { "are" }
            ))),
            (Some(_), true) => Err(ParseError::new(
                "this session declares an escalation and holds no request for it: give the \
                 detailed sources a `window: on <layer>`, such as `on lock` for `sched.futex`, \
                 or leave `escalate` out",
            )),
            (Some(escalation), false) => {
                crate::triage::escalate::decides_for(escalation.objective)?;
                self.nothing_reads_across_a_held_window()
            }
        }
    }

    /// Refuses a recording that cannot mean anything as written, before
    /// anything is resolved or launched.
    ///
    /// The bounds are required rather than defaulted. A run with no fixed end
    /// is only bounded by what it is told: without a retention it keeps
    /// everything, without a bound on documents it can write forever, and
    /// without a longest duration it can run forever -- a launched program's
    /// exit ends it too, but nothing says the program will exit. What needs
    /// the catalog, whether the trigger reads fields the source emits, is
    /// asked by [`Session::check_trigger_fields`].
    pub fn check_recording(&self) -> Result<(), ParseError> {
        let Some(record) = &self.record else {
            return Ok(());
        };
        let refuse = |reason: String| Err(ParseError::new(format!("record: {reason}")));
        if record.trigger.is_none() {
            return refuse(
                "a recording writes a document when its `trigger` fires, and this one names \
                 none: give it a predicate in the filter grammar over what one round of a \
                 request read, such as `trigger: \"value >= 64\"`"
                    .to_owned(),
            );
        }
        let Some(retain) = record.retain.filter(|retain| retain.as_nanos() > 0) else {
            return refuse(
                "a recording needs `retain`, how far back a document reaches: without it the \
                 run would keep everything it read for as long as it runs, which is not a \
                 bound. Such as `retain: 2s`"
                    .to_owned(),
            );
        };
        if record.documents.is_none_or(|documents| documents == 0) {
            return refuse(
                "a recording needs `documents`, the most it writes, at least one: without it a \
                 trigger that keeps firing writes for as long as the run lasts"
                    .to_owned(),
            );
        }
        let Some(longest) = self
            .requests
            .iter()
            .filter_map(|request| request.duration)
            .max()
        else {
            return refuse(
                "a recording needs `duration`, the longest it may run: its end is otherwise a \
                 trigger that may never fire, and a launched program's exit is not a bound, \
                 since nothing says the program exits"
                    .to_owned(),
            );
        };
        if retain >= longest {
            return refuse(format!(
                "`retain: {retain}` is not shorter than the run's longest, `{longest}`: nothing \
                 would ever be evicted, and a document would be the whole run so far -- which \
                 is an ordinary run with `duration: {longest}`"
            ));
        }
        if let Some(after) = record.after.filter(|after| *after >= longest) {
            return refuse(format!(
                "`after: {after}` is not shorter than the run's longest, `{longest}`: no \
                 document could be written after its trigger"
            ));
        }
        if self.escalate.is_some() {
            return refuse(
                "a session either records or escalates: an escalation attaches and detaches \
                 layers over the run, and a recording's documents are windows cut from it by \
                 time, so a document would hold a layer for part of its window and say nothing \
                 of which part"
                    .to_owned(),
            );
        }
        if !self.decompose.is_empty() {
            return refuse(
                "a decomposition splits the program's life into its parts, and a document is \
                 one window of that life, not all of it: the whole it would be weighed against \
                 is not in the document. Decompose an ordinary run"
                    .to_owned(),
            );
        }
        for request in &self.requests {
            let name = request
                .label
                .clone()
                .unwrap_or_else(|| request.source.to_string());
            if let Some(window) = request.window {
                return refuse(format!(
                    "`{name}` has `window: {window}`, and a request in a recording is watched \
                     for the whole run: a document is a window already, cut where the trigger \
                     fired, and a second window inside it would leave the document's span and \
                     the request's disagreeing"
                ));
            }
            if matches!(request.mode, Mode::Count | Mode::Snapshot) {
                return refuse(format!(
                    "`{name}` is in `{}` mode, which is one reading at the end of the run or at \
                     its start: a recording keeps a series and cuts documents out of it, and a \
                     single reading has no series to retain. Use `poll`, `sample` or `trace`",
                    request.mode.kind()
                ));
            }
        }
        self.recorded_index().map(|_| ())
    }

    /// Which request the recording's trigger reads, by its `on`.
    pub fn recorded_index(&self) -> Result<usize, ParseError> {
        let on = self.record.as_ref().and_then(|record| record.on.as_deref());
        let names = || {
            let names: Vec<String> = self
                .requests
                .iter()
                .map(|request| {
                    format!(
                        "`{}`",
                        request.label.as_deref().unwrap_or(request.source.as_str())
                    )
                })
                .collect();
            one_of(names.iter().map(String::as_str))
        };
        let Some(on) = on else {
            return match self.requests.len() {
                1 => Ok(0),
                _ => Err(ParseError::new(format!(
                    "record: this session has {} requests, and the trigger reads one of them: \
                     name it with `on`, one of {}",
                    self.requests.len(),
                    names()
                ))),
            };
        };
        let by_label: Vec<usize> = (0..self.requests.len())
            .filter(|index| self.requests[*index].label.as_deref() == Some(on))
            .collect();
        let found: Vec<usize> = if by_label.is_empty() {
            (0..self.requests.len())
                .filter(|index| {
                    self.requests[*index].label.is_none()
                        && self.requests[*index].source.as_str() == on
                })
                .collect()
        } else {
            by_label
        };
        match found.as_slice() {
            [one] => Ok(*one),
            [] => Err(ParseError::new(format!(
                "record: `on: {on}` names no request of this session: one of {}",
                names()
            ))),
            _ => Err(ParseError::new(format!(
                "record: `on: {on}` names {} requests of this session, and the trigger reads \
                 one: give them labels",
                found.len()
            ))),
        }
    }

    /// Refuses a trigger that reads a field its request does not emit.
    ///
    /// The fields a trigger may read are the ones the catalog declares for
    /// the source's filter, `kind`, and `value`, which is the request's
    /// measured value by whatever name its collector gives it. A field outside
    /// those is false on every event, so the trigger would never fire and the
    /// run would end at its longest with no document, looking exactly like a
    /// workload that never did the thing -- which is why it is refused.
    ///
    /// `backend` is the one the resolver chose, when it has: a field can be
    /// real through one mechanism and impossible through another, and the
    /// trigger reads what this run's backend carries.
    pub fn check_trigger_fields(
        &self,
        catalog: &crate::catalog::Catalog,
        backend: Option<&str>,
    ) -> Result<(), ParseError> {
        let Some(trigger) = self
            .record
            .as_ref()
            .and_then(|record| record.trigger.as_ref())
        else {
            return Ok(());
        };
        let request = &self.requests[self.recorded_index()?];
        let Some(entry) = catalog.get(&request.source) else {
            return Ok(());
        };
        let carried = match backend.and_then(|id| entry.backends.iter().find(|one| one.id == id)) {
            Some(backend) => entry.backend_filter_fields(backend),
            None => &entry.filter_fields,
        };
        let declared: Vec<&str> = carried.iter().map(String::as_str).collect();
        let unknown: Vec<&str> = trigger
            .fields()
            .into_iter()
            .filter(|field| {
                *field != Recording::VALUE && *field != "kind" && !declared.contains(field)
            })
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        let mut readable = vec![format!("`{}`", Recording::VALUE), "`kind`".to_owned()];
        readable.extend(declared.iter().map(|field| format!("`{field}`")));
        Err(ParseError::new(format!(
            "record: the trigger reads `{}`, which `{}`{} does not emit, so it could never fire \
             and the run would end at its longest with no document. It can read {}",
            unknown.join("`, `"),
            request.source,
            backend.map_or_else(String::new, |id| format!(" through `{id}`")),
            readable.join(", ")
        )))
    }

    /// Refuses a decomposition that reads a source the escalation attaches and
    /// detaches.
    ///
    /// A join across a held source is allowed since the eleventh wave's
    /// `join::apply_within`: an event outside the source's window is counted
    /// as `outside_window` and states still open when it is detached close
    /// there, so nothing is credited to a state the run had stopped watching.
    /// A decomposition is still refused, because it would weigh a part watched
    /// for half a second against a whole of two -- a number that looks right
    /// and is not, until parts are weighed against their own window.
    fn nothing_reads_across_a_held_window(&self) -> Result<(), ParseError> {
        let held: Vec<&str> = self
            .requests
            .iter()
            .filter(|request| request.window.is_some_and(|window| window.on.is_some()))
            .map(|request| request.source.as_str())
            .collect();
        let across = |source: &str| held.contains(&source);
        let readers = self
            .decompose
            .iter()
            .flat_map(|decomposition| {
                decomposition
                    .parts
                    .iter()
                    .filter(|part| across(&part.from))
                    .map(move |part| {
                        format!(
                            "decomposition `{}` reads `{}` for `{}`",
                            decomposition.id, part.from, part.state
                        )
                    })
            })
            .collect::<Vec<_>>();
        if readers.is_empty() {
            return Ok(());
        }
        Err(ParseError::new(format!(
            "{}, which the escalation attaches and detaches mid-run: a decomposition weighs every \
             part against one whole, and a part watched for part of the run would be weighed \
             against all of it. Decompose the cheap pass, which runs for the whole of it, or \
             leave the escalation out; a join across a held source is fine",
            readers.join("; ")
        )))
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
    fn a_window_round_trips_as_it_was_written() {
        for text in ["1s..3s", "1s..", "..2s", "0ns..2s", "250ms..1500ms"] {
            let window: Window = text.parse().unwrap();
            assert_eq!(window.to_string(), text);
        }
        let late: Window = "1s..".parse().unwrap();
        assert!(late.attaches_late() && late.stop.is_none());
        let early: Window = "..2s".parse().unwrap();
        assert!(!early.attaches_late());
        assert_eq!(early.stop.unwrap().as_nanos(), 2_000_000_000);
    }

    #[test]
    fn a_window_that_cannot_mean_anything_is_refused_with_the_reason() {
        let backwards = "3s..1s".parse::<Window>().unwrap_err();
        assert!(
            backwards.message().contains("closes before it opens"),
            "{backwards}"
        );
        let empty = "2s..2s".parse::<Window>().unwrap_err();
        assert!(
            empty.message().contains("closes before it opens"),
            "{empty}"
        );
        let whole = "..".parse::<Window>().unwrap_err();
        assert!(whole.message().contains("leave `window` out"), "{whole}");
        let no_range = "2s".parse::<Window>().unwrap_err();
        assert!(no_range.message().contains("<start>..<stop>"), "{no_range}");
        let no_unit = "1..2s".parse::<Window>().unwrap_err();
        assert!(no_unit.message().contains("no unit"), "{no_unit}");
    }

    #[test]
    fn a_window_the_escalation_decides_round_trips_and_names_its_layer() {
        for layer in Layer::ALL {
            let text = format!("on {layer}");
            let window: Window = text.parse().unwrap();
            assert_eq!(window.to_string(), text);
            assert_eq!(window.layer(), Some(layer));
            assert!(window.attaches_late() && window.start.is_none() && window.stop.is_none());
        }
        let unknown = "on memory".parse::<Window>().unwrap_err();
        for layer in Layer::ALL {
            assert!(unknown.message().contains(layer.as_str()), "{unknown}");
        }
    }

    fn escalating(yaml: &str) -> Result<Session, ParseError> {
        let mut session: Session = serde_norway::from_str(yaml).expect("a session");
        session.apply_defaults();
        session.check_escalation().map(|()| session)
    }

    const HELD: &str = r#"
requests:
  - {label: blocked, source: sched.offcpu, target: {launch: [prog]}, mode: trace}
  - {label: futex, source: sched.futex, target: {launch: [prog]}, mode: trace, window: on lock}
"#;

    #[test]
    fn the_session_s_objective_reaches_every_held_request() {
        let session = escalating(&format!("escalate: {{objective: latency}}\n{HELD}")).unwrap();
        let held = session.requests[1].window.unwrap().on.unwrap();
        assert_eq!(held.objective, Some(crate::triage::Objective::Latency));
        assert_eq!(session.requests[0].window, None);
    }

    #[test]
    fn an_escalation_that_cannot_mean_anything_is_refused_with_the_reason() {
        let no_block = escalating(HELD).unwrap_err();
        assert!(no_block.message().contains("`futex` is held"), "{no_block}");

        let nothing_held = escalating(
            "escalate: {objective: latency}\nrequests:\n  - {source: sched.offcpu, target: \
             {launch: [prog]}, mode: trace}\n",
        )
        .unwrap_err();
        assert!(
            nothing_held.message().contains("holds no request"),
            "{nothing_held}"
        );

        let no_objective = escalating(&format!("escalate: {{}}\n{HELD}")).unwrap_err();
        assert!(
            no_objective.message().contains("needs an objective"),
            "{no_objective}"
        );

        let throughput =
            escalating(&format!("escalate: {{objective: throughput}}\n{HELD}")).unwrap_err();
        assert!(
            throughput.message().contains("omn saturate"),
            "{throughput}"
        );
    }

    /// Since `join::apply_within` knows each source's window, a join across a
    /// held source is allowed; a decomposition reading one is still refused.
    #[test]
    fn a_join_across_a_held_source_is_allowed_and_a_decomposition_is_not() {
        let joined = escalating(&format!(
            "escalate: {{objective: latency}}\n{HELD}joins:\n  - {{id: across, from: \
             sched.offcpu, to: sched.futex, via: task, match: eq}}\n"
        ));
        assert!(joined.is_ok(), "{joined:?}");
        let decomposed = escalating(&format!(
            "escalate: {{objective: latency}}\n{HELD}decompose:\n  - {{id: across, whole: \
             lifetime, parts: [{{state: waiting, from: sched.futex, value: wait_ns}}]}}\n"
        ))
        .unwrap_err();
        assert!(
            decomposed
                .message()
                .contains("decomposition `across` reads `sched.futex`"),
            "{decomposed}"
        );
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
