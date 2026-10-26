//! The catalog: the tool's vocabulary, loaded as data.
//!
//! `docs/CATALOG.md` describes the entries and `catalog/*.yaml` holds them. The CLI,
//! the help text, `omn list` and the validator are projections of these files;
//! adding a source means adding an entry, not touching code.
//!
//! An entry never says a source *is* available. It declares an ordered chain of
//! backends and what each one needs; the resolver probes the machine and keeps
//! the first viable one. Availability lives in `Capabilities`, which arrives
//! with the resolver.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{one_of, ParseError};
use crate::request::{Emit, EmitKind, KeyKind, Mode, ModeKind, SourceId, StackMode, TargetKind};
use crate::scalar::Duration;

/// The catalog files compiled into the binary.
///
/// A build always carries a complete, consistent catalog; `Catalog::load_dir`
/// exists to iterate on entries without rebuilding.
const EMBEDDED: &[(&str, &str)] = &[
    ("cpu.yaml", include_str!("../catalog/cpu.yaml")),
    ("mem.yaml", include_str!("../catalog/mem.yaml")),
    ("io.yaml", include_str!("../catalog/io.yaml")),
    ("net.yaml", include_str!("../catalog/net.yaml")),
    ("sched.yaml", include_str!("../catalog/sched.yaml")),
    ("power.yaml", include_str!("../catalog/power.yaml")),
    ("gpu.yaml", include_str!("../catalog/gpu.yaml")),
    ("proc.yaml", include_str!("../catalog/proc.yaml")),
    ("runtime.yaml", include_str!("../catalog/runtime.yaml")),
];

/// The file of the catalog directory that holds the unwind modes rather than
/// entries: `catalog/unwind.yaml`, read by [`crate::stack::unwind`].
pub const UNWIND_TABLE: &str = "unwind.yaml";

/// One source: what it measures, and every way it might be measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: SourceId,
    pub family: String,
    pub summary: String,
    pub unit: Unit,
    /// False when the source is computed from others rather than measured.
    #[serde(default = "yes")]
    pub primitive: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived_from: Vec<SourceId>,
    /// Ordered: the resolver keeps the first viable one.
    pub backends: Vec<Backend>,
    pub modes: Vec<ModeKind>,
    pub emits: Vec<EmitKind>,
    /// Target kinds this source supports. An absent kind means `no`.
    ///
    /// A kind written `no` says why: `targets: {launch: native, pid: "no:
    /// <reason>"}`. The reason is moved into [`Entry::refused`] as the file
    /// is read, so this map holds only the support.
    #[serde(default)]
    pub targets: BTreeMap<TargetKind, TargetSupport>,
    /// Why each target the `targets:` line writes `no` is refused: the
    /// reason a refusal quotes, read from that line and from nowhere else,
    /// so it sits on the axis it explains (`docs/DECISIONS.md`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub refused: BTreeMap<TargetKind, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<KeyKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filter_fields: Vec<String>,
    pub determinism: Determinism,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
    /// Whose stacks the `stack` emit carries: absent, the host's, unwound on
    /// this machine in the mode `--stack` chooses; `device`, the device's
    /// own call paths (`gpu.profile`'s kernel and its source lines), which no
    /// host unwind mode applies to, so `--stack` has nothing to choose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stacks: Option<Stacks>,
}

/// Whose stacks a source's `stack` emit carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stacks {
    /// The device's own: a GPU kernel's call paths, read by the vendor's
    /// library, not unwound on the host.
    Device,
}

impl Entry {
    /// Whether the `stack` emit is the device's own call paths rather than
    /// host stacks a `--stack` mode unwinds.
    pub fn device_stacks(&self) -> bool {
        self.stacks == Some(Stacks::Device)
    }

    /// How this source supports a target kind, following the documented
    /// fallbacks: launching a program is following a pid tree, a container is a
    /// cgroup, a thread is a narrower pid.
    pub fn target_support(&self, kind: TargetKind) -> TargetSupport {
        support_in(&self.targets, kind)
    }

    /// Why this source refuses a target kind, when its `targets:` line says,
    /// following the same fallbacks as [`Entry::target_support`]: a thread
    /// refused because its process is.
    pub fn refusal(&self, kind: TargetKind) -> Option<&str> {
        if self.targets.contains_key(&kind) {
            return self.refused.get(&kind).map(String::as_str);
        }
        self.refusal(kind.fallback()?)
    }

    /// How one backend of this chain supports a target kind.
    ///
    /// A backend that declares its own `targets` speaks only for itself and is
    /// never widened by the entry's: the entry describes the union of what the
    /// chain can do, and the whole point of the per-backend map is that a
    /// backend which only attributes per cgroup must not be handed a
    /// system-wide request because a sibling could serve one.
    pub fn backend_target_support(&self, backend: &Backend, kind: TargetKind) -> TargetSupport {
        match &backend.targets {
            Some(targets) => support_in(targets, kind),
            None => self.target_support(kind),
        }
    }

    /// The filterable fields one backend of this chain actually carries.
    ///
    /// A backend that declares none carries the entry's, which is the common
    /// case: the per-backend list exists for the fields that are real through
    /// one mechanism and impossible through another. `comm` on `net.bytes` is
    /// the example -- the eBPF backend knows which process sent a byte and
    /// `/proc/net/dev` has no idea.
    pub fn backend_filter_fields<'a>(&'a self, backend: &'a Backend) -> &'a [String] {
        backend
            .filter_fields
            .as_deref()
            .unwrap_or(&self.filter_fields)
    }

    /// The modes one backend of this chain serves: its own list when it
    /// declares one, the entry's otherwise.
    pub fn backend_modes<'a>(&'a self, backend: &'a Backend) -> &'a [ModeKind] {
        backend.modes.as_deref().unwrap_or(&self.modes)
    }

    /// The emits one backend of this chain produces: its own list when it
    /// declares one, the entry's otherwise.
    pub fn backend_emits<'a>(&'a self, backend: &'a Backend) -> &'a [EmitKind] {
        backend.emits.as_deref().unwrap_or(&self.emits)
    }

    /// The keys one backend of this chain attaches to its events: its own
    /// list when it declares one, the entry's otherwise.
    pub fn backend_keys<'a>(&'a self, backend: &'a Backend) -> &'a [KeyKind] {
        backend.keys.as_deref().unwrap_or(&self.keys)
    }

    /// The unit one backend of this chain reports in: its own when it
    /// declares one, the entry's otherwise. An id that is not in the chain
    /// gets the entry's.
    pub fn backend_unit(&self, backend: &str) -> Unit {
        self.backends
            .iter()
            .find(|candidate| candidate.id == backend)
            .and_then(|candidate| candidate.unit)
            .unwrap_or(self.unit)
    }

    /// The backends of this chain that attach a key, in chain order. What a
    /// rejection names when the selected one does not.
    pub fn backends_carrying_key(&self, key: KeyKind) -> Vec<&str> {
        self.backends
            .iter()
            .filter(|backend| self.backend_keys(backend).contains(&key))
            .map(|backend| backend.id.as_str())
            .collect()
    }

    /// The backends of this chain that carry a filterable field, in chain
    /// order. What a rejection names when the selected one does not.
    pub fn backends_carrying(&self, field: &str) -> Vec<&str> {
        self.backends
            .iter()
            .filter(|backend| {
                self.backend_filter_fields(backend)
                    .iter()
                    .any(|known| known == field)
            })
            .map(|backend| backend.id.as_str())
            .collect()
    }

    /// The target kinds this source actually accepts.
    pub fn supported_targets(&self) -> Vec<TargetKind> {
        TargetKind::ALL
            .into_iter()
            .filter(|kind| self.target_support(*kind) != TargetSupport::No)
            .collect()
    }

    /// The cheapest cost across the backend chain, which is the best case once
    /// the resolver has picked one.
    pub fn best_cost(&self) -> Cost {
        self.backends
            .iter()
            .map(|backend| backend.cost)
            .min()
            .unwrap_or(Cost::Free)
    }

    pub fn backend(&self, id: &str) -> Option<&Backend> {
        self.backends.iter().find(|backend| backend.id == id)
    }

    /// Whether every backend of this chain delivers drained totals, so that
    /// whichever the resolver picks, one event stands for many occurrences.
    ///
    /// Every rather than any: a chain where one backend streams an event per
    /// occurrence can still serve a join that needs one, and which backend a
    /// run gets is the resolver's to decide after validation.
    pub fn always_drained(&self) -> bool {
        !self.backends.is_empty() && self.backends.iter().all(|backend| backend.drained)
    }
}

/// The mechanism a backend works by, read once from its id.
///
/// What the resolver and the runner decide from a backend's mechanism --
/// whether it can narrow a target or a predicate inside a program, how what it
/// spends inside the target is weighed -- they ask of this, never of the id's
/// spelling: a new mechanism is a variant here and a compile error wherever it
/// must be decided, not one more `starts_with` somebody forgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// `bpf` and `bpf:*`: loads eBPF programs.
    Bpf,
    /// `pmu` and `pmu:*`: a perf counter or sampler.
    Pmu,
    /// `perf:*`: a perf event that is not a counter, such as a kprobe event.
    Perf,
    /// `fs:*`: reads a file under `/proc`, `/sys` or a cgroup.
    Fs,
    /// `netlink` and `netlink:*`.
    Netlink,
    /// `preload`: a library loaded into the target.
    Preload,
    /// `af_packet`: a capture socket.
    AfPacket,
    /// `so_timestamping`: a socket option on the target's own sockets.
    SoTimestamping,
    /// `vendor:*`: a vendor's library, named after the colon.
    Vendor,
    /// Any other id: a catalog written for a test, or a mechanism no code
    /// decides anything about yet.
    Other,
}

impl BackendKind {
    /// The kind of a backend id.
    pub fn of(id: &str) -> Self {
        match id.split(':').next().unwrap_or(id) {
            "bpf" => BackendKind::Bpf,
            "pmu" => BackendKind::Pmu,
            "perf" => BackendKind::Perf,
            "fs" => BackendKind::Fs,
            "netlink" => BackendKind::Netlink,
            "preload" => BackendKind::Preload,
            "af_packet" => BackendKind::AfPacket,
            "so_timestamping" => BackendKind::SoTimestamping,
            "vendor" => BackendKind::Vendor,
            _ => BackendKind::Other,
        }
    }

    /// Whether the backend loads eBPF programs, whose run time is then the
    /// kernel's to report, and whose task ids are the kernel's numbering.
    pub fn loads_programs(self) -> bool {
        self == BackendKind::Bpf
    }

    /// Whether the backend can narrow inside the kernel what a userspace
    /// reader could only discard after delivery: a `bpf_only` target, the
    /// fields of a predicate.
    pub fn narrows_in_program(self) -> bool {
        self == BackendKind::Bpf
    }
}

impl Backend {
    /// The mechanism this backend works by.
    pub fn kind(&self) -> BackendKind {
        BackendKind::of(&self.id)
    }
}

/// One way of obtaining a source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    /// A backend tag from `docs/CATALOG.md`: `pmu`, `bpf:tp`, `fs:proc`, and so on.
    pub id: String,
    /// What the machine must provide: tracepoints, PMU events, files, BTF.
    /// The resolver probes each one; nothing here is assumed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// The worst regime's band: the one the bench gates, the resolver ranks
    /// by and a budget is promised against.
    pub cost: Cost,
    /// The band in each regime, where they are orders of magnitude apart:
    /// `gpu.kernel` through CUPTI costs +64.5 % of a launch-bound program and
    /// +0.15 % of a memory-bound one. `cost` stays the worst of them; this
    /// says which programs pay it, for the budget's note and the run's cost
    /// line, and the bench checks each regime's rows against its band
    /// (`docs/DECISIONS.md`, settled 2026-10-19).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cost_by_regime: BTreeMap<String, Cost>,
    /// What is lost relative to the backend above it in the chain. Required on
    /// every fallback that is not an `equivalent`: a degradation the user is
    /// not told about is a wrong number that looks right.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
    /// A different mechanism producing the same measurement, rather than a
    /// fallback that loses something: the vendor path for a closed GPU, netlink
    /// instead of procfs for the same counter. Marking it says the resolver may
    /// pick either without the result changing meaning.
    #[serde(default, skip_serializing_if = "is_false")]
    pub equivalent: bool,
    /// One event stands for many occurrences, added up in the kernel and
    /// drained on a tick: `sched.offcpu`'s rows are `(task, state, stack) ->
    /// total_ns` over a hundred milliseconds, not one block each.
    ///
    /// Declared rather than inferred because the join engine has to know it
    /// before the run. A `wakeup` join credits each downstream event to one
    /// wakeup, and a drained total holds a hundred blocks that a hundred
    /// different wakeups released: it matches, reports WAKEUP, and every block
    /// in it goes to whichever wakeup came last before the drain.
    #[serde(default, skip_serializing_if = "is_false")]
    pub drained: bool,
    /// The join this backend needs to produce a value at all, when it derives
    /// one from two separate events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<JoinRule>,
    /// Which targets *this* backend serves, when the chain does not agree.
    ///
    /// Absent means the backend serves whatever the entry declares, which is
    /// the common case. Present, it replaces the entry's map rather than
    /// patching it: a kind it does not mention is one this backend cannot
    /// serve, and writing `no` for every other kind would be noise nobody
    /// maintains. The loader checks it claims nothing the entry refuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<BTreeMap<TargetKind, TargetSupport>>,
    /// Which of the entry's filterable fields *this* backend carries.
    ///
    /// Absent means all of them. Present, it is a subset: a predicate over a
    /// field the selected backend never emits is false everywhere, which
    /// empties a series rather than narrowing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_fields: Option<Vec<String>>,
    /// The fields whose comparison the backend's program carries, so that an
    /// event the predicate rejects is dropped in the kernel and never paid for.
    ///
    /// Empty means none: whatever the backend applies, it applies after the
    /// event has been delivered. What a field here promises is the shape a
    /// program compares against a constant -- a conjunction of `==` with one
    /// value, one comparison per field -- and nothing wider: `syscall ==
    /// 'read'` runs in the program, `syscall in (read, write)` and `syscall !=
    /// 'read'` do not. The resolver says `kernel` for a predicate only when
    /// every part of it is that shape over these fields. The loader checks the
    /// list is a subset of what the backend carries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filter_in_program: Vec<String>,
    /// The modes *this* backend serves, when the chain does not agree.
    ///
    /// Absent means every mode the entry declares. Present, it replaces the
    /// entry's list for this backend: `/proc/[pid]/stat` holds a total and can
    /// be polled, and cannot be traced, so a `trace` request must not be
    /// handed to it because a sibling could serve one. The resolver passes
    /// over a backend whose modes leave the request's out, with that reason in
    /// the chain, and the loader checks the list is a subset of the entry's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<Vec<ModeKind>>,
    /// Which of the entry's keys *this* backend attaches to an event.
    ///
    /// Absent means all of them, as for `filter_fields`. Present, it is a
    /// subset: a key the selected backend never attaches makes every join
    /// over it attribute nothing, so the resolver refuses a request keyed on
    /// one (`ResolveError::KeyNotCarried`) rather than recording events that
    /// join to nothing. `/proc/diskstats` knows the device and not the task
    /// that asked for the I/O: `io.bytes` through `fs:proc` attaches `dev`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<KeyKind>>,
    /// The unit *this* backend's values are in, when it is not the entry's.
    ///
    /// Absent means the entry's, which is the common case. Present, it is
    /// what a fallback that measures something else in its own unit reports
    /// in: `cpu.cycles` through `pmu:sw` is the cpu clock, nanoseconds of cpu
    /// and not a count of cycles, and a result that labelled them `count`
    /// would let a reader add them to the hardware counter's. The loader
    /// refuses one that repeats the entry's, one on the head of the chain
    /// (whose unit is the entry's), one on a backend declared `equivalent`
    /// (a different unit is not the same measurement), and one that does not
    /// add up where the entry's does (its `rate` and `stack` would mean
    /// nothing). [`Entry::backend_unit`] reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<Unit>,
    /// The emits *this* backend can produce, when the chain does not agree.
    ///
    /// Absent means every emit the entry declares. Present, it replaces the
    /// entry's list for this backend, as `modes` does: the preload shim
    /// counts every allocation and captures no stack, so `mem.heap.alloc -e
    /// stack` must not be handed to it because a sibling could unwind. The
    /// resolver passes over a backend whose emits leave one of the request's
    /// out, with that reason in the chain, so the refusal comes before the
    /// run rather than after it; the loader checks the list is a non-empty
    /// subset of the entry's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emits: Option<Vec<EmitKind>>,
}

/// How two events are tied together into one measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinRule {
    /// The typed key, written as in `docs/CATALOG.md`: `blk:rq@open_ts`.
    pub key: String,
    #[serde(rename = "match")]
    pub match_kind: MatchKind,
    /// How long an open state is kept before it is evicted and counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<Duration>,
}

/// The only ways two events may be matched.
///
/// Keeping this set closed is what stops the join engine from growing a special
/// case per subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// Equal keys.
    Eq,
    /// Equal keys within a scope, because the key is a recycled pointer.
    ScopedEq,
    /// The downstream key falls inside the upstream interval.
    Interval,
    /// The wakeup chain ties them: the only join that crosses a block.
    Wakeup,
    /// A polled total's change between two readings, credited to the states
    /// that opened or closed in between. Nothing ties the two sides but the
    /// clock, so it is the one match whose evidence is TEMPORAL by
    /// construction.
    Between,
    /// A close paired with the next wakeup of its task: a completion, and the
    /// wakeup that released the task waiting on it. The other way round from
    /// `wakeup`, which opens on the wakeup and credits what follows it.
    Woke,
    /// A downstream event's window overlapping an upstream reading's, on the
    /// same key, credited by the share of it that overlaps. TEMPORAL by
    /// construction, like `between`: nothing ties the two but the clock.
    Overlap,
}

impl MatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchKind::Eq => "eq",
            MatchKind::ScopedEq => "scoped_eq",
            MatchKind::Interval => "interval",
            MatchKind::Wakeup => "wakeup",
            MatchKind::Between => "between",
            MatchKind::Woke => "woke",
            MatchKind::Overlap => "overlap",
        }
    }
}

impl fmt::Display for MatchKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a source accepts a target kind, and where the filtering happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetSupport {
    /// The backend itself filters, at no extra cost.
    Native,
    /// The eBPF program filters, so the backend chain is limited to eBPF.
    BpfOnly,
    /// Invalid: the combination must fail validation, never degrade.
    No,
}

impl TargetSupport {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetSupport::Native => "native",
            TargetSupport::BpfOnly => "bpf_only",
            TargetSupport::No => "no",
        }
    }
}

impl fmt::Display for TargetSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Order-of-magnitude overhead, as declared. The real cost depends on event
/// rate and is measured by the phase 8 harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cost {
    /// A counter read, about zero.
    Free,
    /// Under 1 %.
    Low,
    /// 1 to 5 %.
    Med,
    /// 5 to 20 %.
    High,
    /// Over 20 %, with no bound above: the one band past `High`. What the
    /// bench measured in it is a program the collector runs inside -- the
    /// preload shim on an allocation- or lock-bound program, about +600 % to
    /// +1000 %, and `proc.syscall`'s program on every system call, +296 % --
    /// and it is also where kernel replay would sit, as a GPU profiler does it
    /// to read more counters than the hardware has. Not dynamic binary
    /// instrumentation, which `docs/DECISIONS.md` rules out as a backend.
    Extreme,
}

impl Cost {
    /// The top of the overhead band this cost stands for, as a percentage of
    /// the target's own cpu. `extreme` is "over 20 %, with no bound above",
    /// and no percentage bounds it.
    ///
    /// `free` is documented as "a counter read, about zero", which as a number
    /// has to be *something*: at exactly zero the claim could never be met by
    /// a real measurement, so every `free` backend would read as having broken
    /// its own promise the moment the promise became checkable. A tenth of a
    /// percent is the reading that makes "about zero" an assertion rather than
    /// a figure of speech. Held here, once, for the runner's budget and for
    /// `omn-mcp`, which each had a copy.
    pub fn worst_case_percent(self) -> f64 {
        match self {
            Cost::Free => 0.1,
            Cost::Low => 1.0,
            Cost::Med => 5.0,
            Cost::High => 20.0,
            Cost::Extreme => f64::INFINITY,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Cost::Free => "free",
            Cost::Low => "low",
            Cost::Med => "med",
            Cost::High => "high",
            Cost::Extreme => "extreme",
        }
    }
}

impl fmt::Display for Cost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How reproducible a source is between two runs of the same workload.
///
/// Never `exact`: bit-exact determinism is outside this tool's boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Determinism {
    Low,
    Medium,
    High,
}

impl Determinism {
    pub fn as_str(self) -> &'static str {
        match self {
            Determinism::Low => "low",
            Determinism::Medium => "medium",
            Determinism::High => "high",
        }
    }
}

impl fmt::Display for Determinism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The unit a source reports in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Ns,
    Bytes,
    Count,
    Ratio,
    Hz,
    Joules,
    /// A power, as the hardware reported it: an instantaneous draw or a mean
    /// over the device's own window, never an energy. Kept apart from
    /// `joules` because the two answer different questions -- how much was
    /// spent, and how fast -- and a rate over a level is not one over a count.
    Watts,
    Celsius,
    /// Core clock cycles, as the hardware counted them. Kept apart from `ns`
    /// on purpose: converting goes through a clock that moves with load and
    /// temperature, which is a number the hardware did not measure.
    Cycles,
    /// A throughput the hardware reported as one, bytes over its own window:
    /// a level, like watts, that a sum over readings makes nothing of
    /// (`gpu.pcie`, whose cumulative counters wrap in a third of a second).
    BytesPerSecond,
}

impl Unit {
    pub fn as_str(self) -> &'static str {
        match self {
            Unit::Ns => "ns",
            Unit::Bytes => "bytes",
            Unit::Count => "count",
            Unit::Ratio => "ratio",
            Unit::Hz => "hz",
            Unit::Joules => "joules",
            Unit::Watts => "watts",
            Unit::Celsius => "celsius",
            Unit::Cycles => "cycles",
            Unit::BytesPerSecond => "bytes_per_second",
        }
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Refuses a requirement written as a pattern.
///
/// Nothing expands one: a probe asked about `tracepoint:ext4:*` looks for a
/// directory literally named `*`, and `pmu:topdown-*` for an event of that
/// name, so the answer is missing for a machine that has every one of them --
/// or available by an accident of the probe. A chain is resolved against
/// requirements, and "any of them" is not one; the entry names the events its
/// collector attaches to. Finding 25 of `docs/open-findings.md`.
fn check_requirement_globs(entry: &Entry) -> Result<(), ParseError> {
    for backend in &entry.backends {
        if let Some(pattern) = backend
            .requires
            .iter()
            // A raw tracepoint's prototype follows its name in parentheses,
            // and a pointer is written with a `*` there.
            .find(|needs| {
                needs
                    .split('(')
                    .next()
                    .unwrap_or(needs)
                    .contains(['*', '?'])
            })
        {
            return Err(ParseError::new(format!(
                "catalog entry `{}`: backend `{}` requires `{pattern}`, which is a pattern: no \
                 probe expands one, so name each event or file the collector attaches to",
                entry.id, backend.id
            )));
        }
    }
    Ok(())
}

/// The per-backend `targets` maps have to stay consistent with the entry's.
///
/// Two ways they can drift, and both are the kind of mistake that reads as
/// correct. A backend claiming a target the entry refuses is unreachable: the
/// validator turns the request away before the resolver ever walks the chain.
/// A target the entry promises and no backend serves is worse, because it is
/// accepted and then resolves to nothing at all.
fn check_backend_targets(entry: &Entry) -> Result<(), ParseError> {
    for backend in &entry.backends {
        let Some(targets) = &backend.targets else {
            continue;
        };
        for kind in TargetKind::ALL {
            if support_in(targets, kind) != TargetSupport::No
                && entry.target_support(kind) == TargetSupport::No
            {
                return Err(ParseError::new(format!(
                    "catalog entry `{}`: backend `{}` claims a `{kind}` target the entry itself \
                     refuses, so no request could ever reach it",
                    entry.id, backend.id
                )));
            }
        }
    }

    for kind in entry.supported_targets() {
        if entry
            .backends
            .iter()
            .any(|backend| entry.backend_target_support(backend, kind) != TargetSupport::No)
        {
            continue;
        }
        return Err(ParseError::new(format!(
            "catalog entry `{}` declares `{kind}` support, but no backend in its chain serves \
             that target: the request would be accepted and then resolve onto nothing",
            entry.id
        )));
    }
    Ok(())
}

/// A backend declared `equivalent` declares no cheaper band than a backend
/// above it in the chain serving a target it serves. A budget degrades down
/// the chain to pay less; an equivalent that claimed to cost less than what it
/// stands in for, for the same targets, would be stepped onto and keep paying
/// the same -- or the band above it is wrong. Either way one of the two lines
/// is false, and the loader is where a chain's other invariants are held
/// (moved here from `tests/bench_bands.rs`, which held it of the embedded
/// catalog alone).
fn check_equivalent_costs(entry: &Entry) -> Result<(), ParseError> {
    let served = |backend: &Backend| -> BTreeSet<TargetKind> {
        TargetKind::ALL
            .into_iter()
            .filter(|kind| entry.backend_target_support(backend, *kind) != TargetSupport::No)
            .collect()
    };
    for (at, fallback) in entry.backends.iter().enumerate() {
        if !fallback.equivalent {
            continue;
        }
        let theirs = served(fallback);
        for above in &entry.backends[..at] {
            if fallback.cost < above.cost && !served(above).is_disjoint(&theirs) {
                return Err(ParseError::new(format!(
                    "catalog entry `{}`: `{}` is declared equivalent and declares `{}`, under \
                     `{}`'s `{}` for a target both serve: a budget would degrade onto it and \
                     keep paying the same",
                    entry.id, fallback.id, fallback.cost, above.id, above.cost
                )));
            }
        }
    }
    Ok(())
}

/// The same two directions for `filter_fields`: a backend cannot carry a field
/// the entry never offers, and a field the entry offers has to be carried by
/// some backend or it is a predicate that can only ever empty a series.
fn check_backend_filter_fields(entry: &Entry) -> Result<(), ParseError> {
    for backend in &entry.backends {
        let Some(fields) = &backend.filter_fields else {
            continue;
        };
        for field in fields {
            if !entry.filter_fields.contains(field) {
                return Err(ParseError::new(format!(
                    "catalog entry `{}`: backend `{}` carries the filter field `{field}`, which \
                     the entry does not offer, so nobody could ever ask for it",
                    entry.id, backend.id
                )));
            }
        }
    }

    for backend in &entry.backends {
        for field in &backend.filter_in_program {
            if !entry.backend_filter_fields(backend).contains(field) {
                return Err(ParseError::new(format!(
                    "catalog entry `{}`: backend `{}` says its program filters on `{field}`, \
                     which it does not carry, so no predicate could ever be pushed down on it",
                    entry.id, backend.id
                )));
            }
        }
    }

    for field in &entry.filter_fields {
        if entry.backends_carrying(field).is_empty() {
            return Err(ParseError::new(format!(
                "catalog entry `{}` offers the filter field `{field}`, which no backend in its \
                 chain carries: a predicate over it would be false on every event and empty the \
                 series instead of narrowing it",
                entry.id
            )));
        }
    }
    Ok(())
}

/// The same two directions for `keys`: a backend cannot attach a key the entry
/// never offers, and a key the entry offers has to be attached by some backend,
/// or a request keyed on it is accepted and refused by every resolution.
/// A backend's own `emits`: a non-empty subset of the entry's, and every emit
/// the entry declares produced by some backend of the chain.
fn check_backend_emits(entry: &Entry) -> Result<(), ParseError> {
    for backend in &entry.backends {
        let Some(emits) = &backend.emits else {
            continue;
        };
        if emits.is_empty() {
            return Err(ParseError::new(format!(
                "catalog entry `{}`: backend `{}` declares an empty `emits` list, so no request \
                 could ever reach it; leave the key out to mean the entry's emits",
                entry.id, backend.id
            )));
        }
        for emit in emits {
            if !entry.emits.contains(emit) {
                return Err(ParseError::new(format!(
                    "catalog entry `{}`: backend `{}` emits `{}`, which the entry itself does \
                     not declare, so no request could ever ask for it",
                    entry.id,
                    backend.id,
                    emit.as_str()
                )));
            }
        }
    }
    for emit in &entry.emits {
        if !entry
            .backends
            .iter()
            .any(|backend| entry.backend_emits(backend).contains(emit))
        {
            return Err(ParseError::new(format!(
                "catalog entry `{}` declares the emit `{}`, which no backend in its chain \
                 produces: a request for it would be accepted and then refused wherever it \
                 resolved",
                entry.id,
                emit.as_str()
            )));
        }
    }
    Ok(())
}

fn check_backend_keys(entry: &Entry) -> Result<(), ParseError> {
    for backend in &entry.backends {
        let Some(keys) = &backend.keys else {
            continue;
        };
        for key in keys {
            if !entry.keys.contains(key) {
                return Err(ParseError::new(format!(
                    "catalog entry `{}`: backend `{}` attaches the key `{key}`, which the entry \
                     does not offer, so nobody could ever ask for it",
                    entry.id, backend.id
                )));
            }
        }
    }
    for key in &entry.keys {
        if entry.backends_carrying_key(*key).is_empty() {
            return Err(ParseError::new(format!(
                "catalog entry `{}` offers the key `{key}`, which no backend in its chain \
                 attaches: a request keyed on it would be accepted and then refused wherever \
                 it resolved",
                entry.id
            )));
        }
    }
    Ok(())
}

/// A backend's own `unit`: a different unit from the entry's, on a fallback
/// that says what it degrades, and one a sum means something of wherever the
/// entry's does.
fn check_backend_units(entry: &Entry) -> Result<(), ParseError> {
    // The same split as `validate`'s, kept exhaustive so that a unit added
    // tomorrow has to answer it.
    fn sums(unit: Unit) -> bool {
        match unit {
            Unit::Ns | Unit::Bytes | Unit::Count | Unit::Joules | Unit::Cycles => true,
            Unit::Ratio | Unit::Hz | Unit::Watts | Unit::Celsius | Unit::BytesPerSecond => false,
        }
    }
    for (rank, backend) in entry.backends.iter().enumerate() {
        let Some(unit) = backend.unit else {
            continue;
        };
        let complaint = if unit == entry.unit {
            Some(format!(
                "declares the unit `{unit}`, which is the entry's; leave the key out to mean it"
            ))
        } else if rank == 0 {
            Some(format!(
                "is the head of the chain and declares the unit `{unit}`, where the entry says \
                 `{}`: the entry's unit is its head's, so one of the two is wrong",
                entry.unit
            ))
        } else if backend.equivalent {
            Some(format!(
                "is declared equivalent and reports in `{unit}`, where the entry says `{}`: a \
                 value in another unit is not the same measurement",
                entry.unit
            ))
        } else if sums(entry.unit) && !sums(unit) {
            Some(format!(
                "reports in `{unit}`, which does not add up where the entry's `{}` does: its \
                 `rate` and `stack` would be sums of a level",
                entry.unit
            ))
        } else {
            None
        };
        if let Some(complaint) = complaint {
            return Err(ParseError::new(format!(
                "catalog entry `{}`: backend `{}` {complaint}",
                entry.id, backend.id
            )));
        }
    }
    Ok(())
}

/// The regimes a band may be given for: the GPU bench's three, a program
/// that launches many short kernels, one that computes, one that moves memory.
pub const REGIMES: [&str; 3] = ["launch", "compute", "memory"];

/// A backend's `cost_by_regime`: regimes the bench measures, no band above
/// `cost`, and `cost` the worst of them -- `cost` is a promise, and a regime
/// past it, or a `cost` past every regime, makes one of the two wrong.
fn check_backend_regimes(entry: &Entry) -> Result<(), ParseError> {
    for backend in &entry.backends {
        if backend.cost_by_regime.is_empty() {
            continue;
        }
        let complain = |what: String| {
            Err(ParseError::new(format!(
                "catalog entry `{}`: backend `{}` {what}",
                entry.id, backend.id
            )))
        };
        if let Some(unknown) = backend
            .cost_by_regime
            .keys()
            .find(|regime| !REGIMES.contains(&regime.as_str()))
        {
            return complain(format!(
                "gives a band for the regime `{unknown}`, which the bench does not measure (one of \
                 {})",
                REGIMES.join(", ")
            ));
        }
        let worst = backend.cost_by_regime.values().max().copied();
        if worst != Some(backend.cost) {
            return complain(format!(
                "declares `cost: {}` and its worst regime is `{}`: `cost` is the worst regime's band",
                backend.cost,
                worst.map_or("none", Cost::as_str)
            ));
        }
    }
    Ok(())
}

/// The per-backend `modes` lists, in the same two directions as the targets:
/// a backend cannot serve a mode the entry refuses, and a mode the entry
/// declares has to be served by some backend, or a request the validator
/// accepts resolves onto nothing.
fn check_backend_modes(entry: &Entry) -> Result<(), ParseError> {
    for backend in &entry.backends {
        let Some(modes) = &backend.modes else {
            continue;
        };
        if modes.is_empty() {
            return Err(ParseError::new(format!(
                "catalog entry `{}`: backend `{}` declares an empty `modes` list, so no request \
                 could ever reach it; leave the key out to mean the entry's modes",
                entry.id, backend.id
            )));
        }
        for mode in modes {
            if !entry.modes.contains(mode) {
                return Err(ParseError::new(format!(
                    "catalog entry `{}`: backend `{}` serves `{mode}`, which the entry itself \
                     does not declare, so no request could ever reach it",
                    entry.id, backend.id
                )));
            }
        }
    }
    for mode in &entry.modes {
        if entry
            .backends
            .iter()
            .any(|backend| entry.backend_modes(backend).contains(mode))
        {
            continue;
        }
        return Err(ParseError::new(format!(
            "catalog entry `{}` declares `{mode}` mode, but no backend in its chain serves it: \
             the request would be accepted and then resolve onto nothing",
            entry.id
        )));
    }
    Ok(())
}

/// Walks the documented target fallbacks over one map: launching a program is
/// following a pid tree, a container is a cgroup, a thread is a narrower pid.
fn support_in(map: &BTreeMap<TargetKind, TargetSupport>, kind: TargetKind) -> TargetSupport {
    if let Some(support) = map.get(&kind) {
        return *support;
    }
    match kind.fallback() {
        Some(fallback) => support_in(map, fallback),
        None => TargetSupport::No,
    }
}

fn yes() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Every known source, indexed by id.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    entries: Vec<Entry>,
    /// Each family's default, the requests `omn trace <family>` composes:
    /// read from the `- default:` item of the family's file.
    defaults: Vec<FamilyDefault>,
}

impl Catalog {
    /// Loads the catalog compiled into the binary.
    ///
    /// Parsed once per process and copied after: the text cannot change under
    /// a running binary, and the run, the emit layer and the collectors each
    /// asked for it -- a second parse in `emit::values_add_up` was 45 % of
    /// `omn trace`'s startup. A catalog that fails to parse is not kept, and
    /// says why every time.
    pub fn embedded() -> Result<Self, ParseError> {
        static PARSED: std::sync::OnceLock<Catalog> = std::sync::OnceLock::new();
        if let Some(catalog) = PARSED.get() {
            return Ok(catalog.clone());
        }
        let catalog = Self::parse_embedded()?;
        Ok(PARSED.get_or_init(|| catalog).clone())
    }

    fn parse_embedded() -> Result<Self, ParseError> {
        let mut entries = Vec::new();
        let mut defaults = Vec::new();
        for (name, text) in EMBEDDED {
            let (file_entries, file_defaults) = parse_file_with_defaults(name, text)?;
            entries.extend(file_entries);
            defaults.extend(file_defaults);
        }
        Self::from_entries(entries)?.with_defaults(defaults)
    }

    /// Loads a catalog from a directory of YAML files, for iterating on entries
    /// without rebuilding.
    pub fn load_dir(dir: &Path) -> Result<Self, ParseError> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .map_err(|error| {
                ParseError::new(format!(
                    "cannot read catalog directory {}: {error}",
                    dir.display()
                ))
            })?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext == "yaml" || ext == "yml")
            })
            // The unwind modes live beside the entries and are not entries:
            // `crate::stack::unwind` reads that table (`UNWIND_TABLE`).
            .filter(|path| path.file_name().is_none_or(|name| name != UNWIND_TABLE))
            .collect();
        files.sort();
        if files.is_empty() {
            return Err(ParseError::new(format!(
                "no catalog file in {}",
                dir.display()
            )));
        }
        let mut entries = Vec::new();
        let mut defaults = Vec::new();
        for path in files {
            let text = std::fs::read_to_string(&path).map_err(|error| {
                ParseError::new(format!("cannot read {}: {error}", path.display()))
            })?;
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("catalog")
                .to_owned();
            let (file_entries, file_defaults) = parse_file_with_defaults(&name, &text)?;
            entries.extend(file_entries);
            defaults.extend(file_defaults);
        }
        Self::from_entries(entries)?.with_defaults(defaults)
    }

    /// Builds a catalog from entries, checking the invariants the rest of the
    /// code relies on.
    pub fn from_entries(entries: Vec<Entry>) -> Result<Self, ParseError> {
        let mut seen = BTreeSet::new();
        for entry in &entries {
            if !seen.insert(entry.id.clone()) {
                return Err(ParseError::new(format!(
                    "duplicate catalog entry `{}`",
                    entry.id
                )));
            }
            if entry.id.family() != entry.family {
                return Err(ParseError::new(format!(
                    "catalog entry `{}` declares family `{}`, which does not match its id",
                    entry.id, entry.family
                )));
            }
            if entry.backends.is_empty() {
                return Err(ParseError::new(format!(
                    "catalog entry `{}` has no backend",
                    entry.id
                )));
            }
            if entry.modes.is_empty() {
                return Err(ParseError::new(format!(
                    "catalog entry `{}` has no mode",
                    entry.id
                )));
            }
            if entry.emits.is_empty() {
                return Err(ParseError::new(format!(
                    "catalog entry `{}` has no emit",
                    entry.id
                )));
            }
            let mut backend_ids = BTreeSet::new();
            for backend in &entry.backends {
                for requirement in &backend.requires {
                    if let Err(reason) = requirement.parse::<crate::need::Need>() {
                        return Err(ParseError::new(format!(
                            "catalog entry `{}`, backend `{}`: {reason}",
                            entry.id, backend.id
                        )));
                    }
                }
                if !backend_ids.insert(backend.id.as_str()) {
                    return Err(ParseError::new(format!(
                        "catalog entry `{}` declares the backend id `{}` twice: ids must be unique \
                         within a chain, or `--backend force:` cannot name one and a collector \
                         cannot tell which it serves",
                        entry.id, backend.id
                    )));
                }
            }
            for backend in entry.backends.iter().skip(1) {
                if backend.degraded.is_none() && !backend.equivalent {
                    return Err(ParseError::new(format!(
                        "catalog entry `{}`: fallback backend `{}` neither states what it degrades \
                         nor declares itself equivalent",
                        entry.id, backend.id
                    )));
                }
            }
            if entry.supported_targets().is_empty() {
                return Err(ParseError::new(format!(
                    "catalog entry `{}` supports no target: it could never be requested",
                    entry.id
                )));
            }
            check_backend_targets(entry)?;
            check_backend_filter_fields(entry)?;
            check_backend_modes(entry)?;
            check_backend_emits(entry)?;
            check_backend_keys(entry)?;
            check_backend_units(entry)?;
            check_backend_regimes(entry)?;
            check_equivalent_costs(entry)?;
            check_requirement_globs(entry)?;
        }
        let catalog = Self {
            entries,
            defaults: Vec::new(),
        };
        // What no build could keep, refused where the catalog is read rather
        // than found by a test of the embedded one: `--catalog <dir>` loads
        // through here too (`docs/open-findings.md`, "To catalog.rs").
        let complaints = crate::validate::check_catalog(&catalog);
        if !complaints.is_empty() {
            return Err(ParseError::new(complaints.join("\n")));
        }
        for entry in &catalog.entries {
            for derived in &entry.derived_from {
                if catalog.get(derived).is_none() {
                    return Err(ParseError::new(format!(
                        "catalog entry `{}` derives from `{derived}`, which does not exist",
                        entry.id
                    )));
                }
            }
        }
        Ok(catalog)
    }

    pub fn get(&self, id: &SourceId) -> Option<&Entry> {
        self.entries.iter().find(|entry| &entry.id == id)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Families in catalog order, deduplicated.
    pub fn families(&self) -> Vec<&str> {
        let mut families: Vec<&str> = Vec::new();
        for entry in &self.entries {
            if !families.contains(&entry.family.as_str()) {
                families.push(&entry.family);
            }
        }
        families
    }

    pub fn family(&self, family: &str) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.family == family)
            .collect()
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.id.as_str())
    }

    /// The default of one family, what `omn trace <family>` composes, when
    /// its file declares one.
    pub fn default_of(&self, family: &str) -> Option<&FamilyDefault> {
        self.defaults
            .iter()
            .find(|default| default.family == family)
    }

    /// Every family's default, in catalog order.
    pub fn defaults(&self) -> &[FamilyDefault] {
        &self.defaults
    }

    /// Adds the families' defaults, refusing one that names a source, a mode,
    /// an emit, a key or a target its family does not have, or a band its
    /// cheapest backend cannot keep: a default is a request written down in
    /// advance, and is held to what a request would be.
    pub fn with_defaults(mut self, defaults: Vec<FamilyDefault>) -> Result<Self, ParseError> {
        for (at, default) in defaults.iter().enumerate() {
            if defaults[..at]
                .iter()
                .any(|earlier| earlier.family == default.family)
            {
                return Err(ParseError::new(format!(
                    "family `{}` declares two defaults: `omn trace {}` could only run one",
                    default.family, default.family
                )));
            }
            check_default(&self, default)?;
        }
        self.defaults = defaults;
        Ok(self)
    }
}

/// One family's default: what `omn trace <family>` composes, per cost band.
///
/// Either the requests of each band the family can keep, or `none`, the
/// reason the family has no default that is safe to run without choosing.
/// A family owns its default the way it owns its sources, so it is data in
/// the family's own file, never code (`docs/DECISIONS.md`, "the lazy mode").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FamilyDefault {
    pub family: String,
    /// The family's first question, the one the default answers, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// Why this family has no safe default: the refusal says it, with the
    /// sources to pick from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub none: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub low: Vec<DefaultRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub med: Vec<DefaultRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub high: Vec<DefaultRequest>,
}

impl FamilyDefault {
    /// The bands a default may name: the cost words of the catalog's chains,
    /// without the two that bound nothing a person would choose -- `free` is
    /// a counter read, and `extreme` has no ceiling at all.
    pub const BANDS: [Cost; 3] = [Cost::Low, Cost::Med, Cost::High];

    /// The requests of one band, empty when the family declares none there.
    pub fn band(&self, cost: Cost) -> &[DefaultRequest] {
        match cost {
            Cost::Low => &self.low,
            Cost::Med => &self.med,
            Cost::High => &self.high,
            Cost::Free | Cost::Extreme => &[],
        }
    }

    /// The bands this default declares, cheapest first.
    pub fn bands(&self) -> impl Iterator<Item = (Cost, &[DefaultRequest])> + '_ {
        Self::BANDS
            .into_iter()
            .map(|cost| (cost, self.band(cost)))
            .filter(|(_, requests)| !requests.is_empty())
    }
}

/// One request of a family's default: a request without its target, which
/// the person gives, unless it is the machine's own (`target: system`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultRequest {
    pub source: SourceId,
    pub mode: Mode,
    pub emit: Vec<Emit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<KeyKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<StackMode>,
    /// `system` for a request that reads the machine during the run whatever
    /// the person targets; absent, the request takes the person's target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// What the loader holds a default to: a request it composes has to be one
/// the validator could accept, for the targets a person gives.
fn check_default(catalog: &Catalog, default: &FamilyDefault) -> Result<(), ParseError> {
    let family = default.family.as_str();
    let refuse = |why: String| ParseError::new(format!("the default of family `{family}` {why}"));
    if catalog.family(family).is_empty() {
        return Err(refuse(
            "names a family with no source in the catalog".to_owned(),
        ));
    }
    let declared = default.bands().count();
    match (&default.none, declared) {
        (Some(reason), 0) if !reason.trim().is_empty() => return Ok(()),
        (Some(_), 0) => {
            return Err(refuse(
                "says it has none and gives no reason: the refusal would say nothing".to_owned(),
            ))
        }
        (Some(_), _) => {
            return Err(refuse(
                "both declares requests and says it has none: one of the two is wrong".to_owned(),
            ))
        }
        (None, 0) => {
            return Err(refuse(
                "declares no band: give `low`, `med` or `high`, or `none` with the reason"
                    .to_owned(),
            ))
        }
        (None, _) => {}
    }
    for (band, requests) in default.bands() {
        let mut machine_only = true;
        for request in requests {
            let source = &request.source;
            let Some(entry) = catalog.get(source).filter(|entry| entry.family == family) else {
                return Err(refuse(format!(
                    "names `{source}` in its `{band}` band, which is not a source of the family"
                )));
            };
            let mode = request.mode.kind();
            if !entry.modes.contains(&mode) {
                return Err(refuse(format!(
                    "asks `{source}` for `{mode}` in its `{band}` band, a mode it does not have \
                     (it has {})",
                    one_of(entry.modes.iter().map(|mode| mode.as_str()))
                )));
            }
            if request.emit.is_empty() {
                return Err(refuse(format!(
                    "asks `{source}` for no emit in its `{band}` band: the run would print nothing"
                )));
            }
            for emit in &request.emit {
                let kind = emit.kind();
                if !entry.emits.contains(&kind) || !mode.valid_emits().contains(&kind) {
                    return Err(refuse(format!(
                        "asks `{source}` for `{emit}` in `{mode}` in its `{band}` band, an emit \
                         it does not produce there"
                    )));
                }
            }
            if let Some(key) = request.keys.iter().find(|key| !entry.keys.contains(key)) {
                return Err(refuse(format!(
                    "keys `{source}` on `{key}` in its `{band}` band, a key it does not attach"
                )));
            }
            if entry.best_cost() > band {
                return Err(refuse(format!(
                    "puts `{source}` in its `{band}` band, and its cheapest backend declares \
                     `{}`: no machine could keep that band with it",
                    entry.best_cost()
                )));
            }
            match request.target {
                Some(TargetKind::System) => {
                    if entry.target_support(TargetKind::System) == TargetSupport::No {
                        return Err(refuse(format!(
                            "reads `{source}` for the whole machine in its `{band}` band, which \
                             it does not serve"
                        )));
                    }
                }
                Some(other) => {
                    return Err(refuse(format!(
                        "fixes `{source}`'s target to `{other}` in its `{band}` band: a default \
                         fixes only `system`, and takes every other target from the person"
                    )))
                }
                None => {
                    machine_only = false;
                    for kind in [TargetKind::Launch, TargetKind::Pid] {
                        if entry.target_support(kind) == TargetSupport::No {
                            return Err(refuse(format!(
                                "gives `{source}` the person's target in its `{band}` band, and \
                                 it refuses a `{kind}`: `omn trace {family} -- PROGRAM` and                                  `--pid` must both mean something"
                            )));
                        }
                    }
                }
            }
        }
        // A band of machine-wide requests is a default too (`power`), but
        // it cannot be beside one that follows a program: the run is one
        // shape or the other, and a person cannot tell which it chose.
        if machine_only
            != default
                .bands()
                .all(|(_, all)| all.iter().all(|r| r.target.is_some()))
        {
            return Err(refuse(format!(
                "follows the person's target in some bands and only the machine in others \
                 (`{band}`): `omn trace {family}` would change what it measures with the cost"
            )));
        }
    }
    Ok(())
}

/// The entries of one catalog file, its `default:` item set aside.
#[cfg(test)]
fn parse_file(name: &str, text: &str) -> Result<Vec<Entry>, ParseError> {
    parse_file_with_defaults(name, text).map(|(entries, _)| entries)
}

fn parse_file_with_defaults(
    name: &str,
    text: &str,
) -> Result<(Vec<Entry>, Vec<FamilyDefault>), ParseError> {
    let invalid = |error: serde_norway::Error| {
        ParseError::new(format!("catalog file {name} is invalid: {error}"))
    };
    let mut document: serde_norway::Value = serde_norway::from_str(text).map_err(invalid)?;
    let mut defaults = Vec::new();
    if let serde_norway::Value::Sequence(entries) = &mut document {
        // A family's default is an item of its file, `- default: {...}`,
        // beside the entries rather than among them.
        let mut kept = Vec::with_capacity(entries.len());
        for item in entries.drain(..) {
            match item.get("default") {
                Some(default) => defaults.push(
                    serde_norway::from_value::<FamilyDefault>(default.clone()).map_err(
                        |error| {
                            ParseError::new(format!(
                                "catalog file {name} has an invalid `default:` block: {error}"
                            ))
                        },
                    )?,
                ),
                None => kept.push(item),
            }
        }
        *entries = kept;
        for entry in entries {
            move_refusals(entry);
        }
    }
    let entries = serde_norway::from_value(document).map_err(invalid)?;
    Ok((entries, defaults))
}

/// Splits each `no: <reason>` of an entry's `targets:` line into the `no` the
/// support map holds and the reason [`Entry::refused`] holds. A bare `no`
/// is left as it is, for the loader to refuse; anything else is left for
/// serde to refuse in its own words.
fn move_refusals(entry: &mut serde_norway::Value) {
    use serde_norway::Value;
    let Some(Value::Mapping(targets)) = entry.get_mut("targets") else {
        return;
    };
    let mut refused = serde_norway::Mapping::new();
    for (kind, support) in targets.iter_mut() {
        let Value::String(text) = support else {
            continue;
        };
        let Some(reason) = text.strip_prefix("no:") else {
            continue;
        };
        refused.insert(kind.clone(), Value::String(reason.trim().to_owned()));
        *support = Value::String("no".to_owned());
    }
    if !refused.is_empty() {
        if let Value::Mapping(entry) = entry {
            entry.insert(Value::String("refused".to_owned()), Value::Mapping(refused));
        }
    }
}

impl FromStr for Unit {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const ALL: [Unit; 10] = [
            Unit::Ns,
            Unit::Bytes,
            Unit::Count,
            Unit::Ratio,
            Unit::Hz,
            Unit::Joules,
            Unit::Watts,
            Unit::Celsius,
            Unit::Cycles,
            Unit::BytesPerSecond,
        ];
        ALL.into_iter()
            .find(|unit| unit.as_str() == s.trim())
            .ok_or_else(|| {
                ParseError::new(format!(
                    "unknown unit `{s}` (expected {})",
                    one_of(ALL.iter().map(|unit| unit.as_str()))
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::embedded().expect("the embedded catalog must load")
    }

    /// A minimal entry with the `targets:` line given, as a catalog file
    /// writes it.
    fn entry_with_targets(targets: &str) -> String {
        format!(
            "- id: cpu.example\n  family: cpu\n  summary: \"x\"\n  unit: count\n  backends:\n    \
             - {{id: \"fs:proc\", requires: [], cost: free}}\n  modes: [poll]\n  emits: [summary]\n  \
             targets:\n{targets}\n  determinism: high\n"
        )
    }

    /// A refused target's reason is read from its `targets:` line, quoted by
    /// the refusal of that target and of one that falls back to it, and a
    /// bare `no` -- a refusal with nothing to quote -- is refused by the
    /// loader.
    #[test]
    fn a_refused_target_carries_its_reason_on_the_targets_line() {
        let text = entry_with_targets(
            "    launch: native\n    pid: \"no: the counter lives in the launched process\"",
        );
        let entries = parse_file("test.yaml", &text).unwrap();
        let entry = &entries[0];
        assert_eq!(entry.target_support(TargetKind::Pid), TargetSupport::No);
        assert_eq!(
            entry.target_support(TargetKind::Launch),
            TargetSupport::Native
        );
        assert_eq!(
            entry.refusal(TargetKind::Pid),
            Some("the counter lives in the launched process")
        );
        // A thread is a narrower pid: refused for the pid's reason.
        assert_eq!(
            entry.refusal(TargetKind::Tid),
            Some("the counter lives in the launched process")
        );
        assert_eq!(entry.refusal(TargetKind::Launch), None);
        assert_eq!(entry.refusal(TargetKind::System), None);
        Catalog::from_entries(entries.clone()).expect("a reasoned `no` loads");

        let bare = parse_file(
            "test.yaml",
            &entry_with_targets("    launch: native\n    pid: \"no\""),
        )
        .unwrap();
        let refusal = Catalog::from_entries(bare).unwrap_err().to_string();
        assert!(refusal.contains("`pid: no` without a reason"), "{refusal}");

        // Every `no` the embedded catalog writes says why.
        for entry in catalog().entries() {
            for (kind, support) in &entry.targets {
                if *support == TargetSupport::No {
                    assert!(entry.refusal(*kind).is_some(), "`{}` {kind}", entry.id);
                }
            }
        }
        let kernel = catalog()
            .get(&"gpu.kernel".parse().unwrap())
            .unwrap()
            .clone();
        assert!(kernel
            .refusal(TargetKind::Pid)
            .unwrap()
            .contains("CUDA_INJECTION64_PATH"));
    }

    #[test]
    fn a_power_is_a_unit_of_its_own() {
        assert_eq!("watts".parse::<Unit>().unwrap(), Unit::Watts);
        assert_eq!(Unit::Watts.to_string(), "watts");
        assert_ne!(Unit::Watts, Unit::Joules);
    }

    /// Every backend the shipped catalog declares is of a kind the code
    /// decides about: an id that classified as `Other` would be weighed and
    /// placed by nobody's rule.
    #[test]
    fn every_shipped_backend_is_of_a_known_kind() {
        let catalog = catalog();
        let unknown: Vec<String> = catalog
            .entries()
            .iter()
            .flat_map(|entry| {
                entry
                    .backends
                    .iter()
                    .filter(|backend| backend.kind() == BackendKind::Other)
                    .map(move |backend| format!("{}: {}", entry.id, backend.id))
            })
            .collect();
        assert!(unknown.is_empty(), "{unknown:?}");
        assert!(BackendKind::of("bpf:raw_tp").narrows_in_program());
        assert!(BackendKind::of("bpf").loads_programs());
        assert!(!BackendKind::of("fs:proc").narrows_in_program());
    }

    #[test]
    fn the_embedded_catalog_loads_and_covers_every_family() {
        let catalog = catalog();
        assert!(
            catalog.len() >= 90,
            "catalog shrank unexpectedly: {}",
            catalog.len()
        );
        let families = catalog.families();
        for expected in [
            "cpu", "mem", "io", "net", "sched", "power", "gpu", "proc", "runtime",
        ] {
            assert!(families.contains(&expected), "missing family {expected}");
        }
    }

    /// A backend's own `emits` narrows the entry's and is read through
    /// [`Entry::backend_emits`]; one wider than the entry, an empty one, and
    /// an entry emit no backend produces are refused by the loader.
    #[test]
    fn a_backend_emits_list_is_a_non_empty_subset_the_chain_covers() {
        let base = catalog()
            .get(&"mem.heap.alloc".parse().unwrap())
            .unwrap()
            .clone();
        let mut narrowed = base.clone();
        narrowed.backends[0].emits = Some(vec![EmitKind::TopK, EmitKind::Summary]);
        let loaded = Catalog::from_entries(vec![narrowed.clone()]).expect("a subset loads");
        let entry = loaded.get(&base.id).unwrap();
        assert_eq!(
            entry.backend_emits(&entry.backends[0]),
            &[EmitKind::TopK, EmitKind::Summary]
        );
        assert_eq!(entry.backend_emits(&entry.backends[1]), &entry.emits[..]);

        let mut wider = base.clone();
        wider.emits.retain(|emit| *emit != EmitKind::Raw);
        wider.backends[0].emits = Some(vec![EmitKind::Raw]);
        let error = Catalog::from_entries(vec![wider]).unwrap_err().to_string();
        assert!(
            error.contains("which the entry itself does not declare"),
            "{error}"
        );

        let mut empty = base.clone();
        empty.backends[0].emits = Some(Vec::new());
        let error = Catalog::from_entries(vec![empty]).unwrap_err().to_string();
        assert!(error.contains("an empty `emits` list"), "{error}");

        let mut uncovered = base;
        for backend in &mut uncovered.backends {
            backend.emits = Some(vec![EmitKind::TopK]);
        }
        let error = Catalog::from_entries(vec![uncovered])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("which no backend in its chain produces"),
            "{error}"
        );
    }

    #[test]
    fn a_chain_with_two_identically_named_backends_is_refused() {
        let mut entry = catalog().get(&"mem.rss".parse().unwrap()).unwrap().clone();
        entry.backends[1].id = entry.backends[0].id.clone();
        let error = Catalog::from_entries(vec![entry]).unwrap_err();
        assert!(error.message().contains("twice"), "{error}");
    }

    #[test]
    fn a_fallback_that_neither_degrades_nor_declares_equivalence_is_refused() {
        let mut entry = catalog()
            .get(&"io.latency".parse().unwrap())
            .unwrap()
            .clone();
        // Both fields, not only `degraded`: the rule is that a fallback declares
        // one or the other, so a test that clears one of them is testing the
        // chain's current shape rather than the rule. This one did, and stopped
        // failing the day `io.latency` gained a `bpf:raw_tp` ahead of a
        // `bpf:tp` that declares itself equivalent.
        entry.backends[1].degraded = None;
        entry.backends[1].equivalent = false;
        let error = Catalog::from_entries(vec![entry]).unwrap_err();
        assert!(
            error.message().contains("neither states what it degrades"),
            "{error}"
        );
    }

    /// A backend's own unit is what its values are in, and the loader holds
    /// it to a fallback that says what it degrades: `cpu.cycles` through
    /// `pmu:sw` is the cpu clock, in nanoseconds, beside a head that counts.
    #[test]
    fn a_backend_reports_in_its_own_unit_only_where_it_is_a_fallback_that_says_so() {
        let embedded = catalog();
        let cycles = embedded.get(&"cpu.cycles".parse().unwrap()).unwrap();
        assert_eq!(cycles.unit, Unit::Count);
        assert_eq!(cycles.backend_unit("pmu"), Unit::Count);
        assert_eq!(cycles.backend_unit("pmu:sw"), Unit::Ns);
        assert_eq!(cycles.backend_unit("fs:proc"), Unit::Count);
        let base = cycles.clone();

        let refused = |edit: &dyn Fn(&mut Entry)| -> String {
            let mut entry = base.clone();
            edit(&mut entry);
            let error = Catalog::from_entries(vec![entry]).unwrap_err().to_string();
            // A continuation that lost its `\` prints its indentation inside
            // the message; a sentence never holds two spaces in a row.
            assert!(!error.contains("  "), "a run of spaces in: {error:?}");
            error
        };
        let error = refused(&|entry| entry.backends[1].unit = Some(Unit::Count));
        assert!(error.contains("which is the entry's"), "{error}");
        let error = refused(&|entry| entry.backends[0].unit = Some(Unit::Ns));
        assert!(error.contains("head of the chain"), "{error}");
        let error = refused(&|entry| {
            entry.backends[1].degraded = None;
            entry.backends[1].equivalent = true;
        });
        assert!(error.contains("not the same measurement"), "{error}");
        let error = refused(&|entry| entry.backends[1].unit = Some(Unit::Hz));
        assert!(error.contains("does not add up"), "{error}");

        // And it reads back from the catalog's own spelling.
        let yaml = serde_norway::to_string(&base.backends[1]).unwrap();
        assert!(yaml.contains("unit: ns"), "{yaml}");
    }

    /// A band per regime holds to `cost`: the GPU tracers declare theirs, a
    /// regime the bench does not measure, a band past `cost` or a `cost` past
    /// every regime is refused.
    #[test]
    fn a_band_per_regime_holds_to_the_worst_one() {
        let kernel = catalog()
            .get(&"gpu.kernel".parse().unwrap())
            .unwrap()
            .clone();
        let cupti = kernel.backend("vendor:cupti").unwrap();
        assert_eq!(cupti.cost_by_regime.get("memory"), Some(&Cost::Low));
        assert_eq!(cupti.cost_by_regime.get("launch"), Some(&Cost::Extreme));
        let refused = |edit: &dyn Fn(&mut Entry)| -> String {
            let mut entry = kernel.clone();
            edit(&mut entry);
            Catalog::from_entries(vec![entry]).unwrap_err().to_string()
        };
        let error = refused(&|entry| {
            entry.backends[0]
                .cost_by_regime
                .insert("idle".into(), Cost::Low);
        });
        assert!(error.contains("the regime `idle`"), "{error}");
        let error = refused(&|entry| {
            entry.backends[0]
                .cost_by_regime
                .insert("launch".into(), Cost::High);
        });
        assert!(error.contains("worst regime is `high`"), "{error}");
        let yaml = serde_norway::to_string(cupti).unwrap();
        assert!(yaml.contains("cost_by_regime"), "{yaml}");
    }

    /// The rule's ground truth is the chain: `io.size`'s `bpf:tp` stands in
    /// for `bpf:raw_tp` as an equivalent. Made cheaper than its head on a
    /// target both serve, it is refused; as dear, dearer, degraded rather than
    /// equivalent, or on a disjoint target, it is not.
    #[test]
    fn an_equivalent_backend_cheaper_than_what_it_stands_in_for_is_refused() {
        let io_size = catalog().get(&"io.size".parse().unwrap()).unwrap().clone();
        assert_eq!(io_size.backends[0].id, "bpf:raw_tp");
        assert!(io_size.backends[1].equivalent);
        let load = |fallback: Cost, equivalent: bool, disjoint: bool| {
            let mut entry = io_size.clone();
            entry.backends[0].cost = Cost::High;
            entry.backends[1].cost = fallback;
            entry.backends[1].equivalent = equivalent;
            if !equivalent {
                entry.backends[1].degraded = Some("test".into());
            }
            if disjoint {
                // The head serves the system alone, the equivalent every
                // other target -- none of which falls back onto the system --
                // so the entry's targets are all still served.
                let (system, others): (Vec<_>, Vec<_>) = io_size
                    .supported_targets()
                    .into_iter()
                    .partition(|kind| *kind == TargetKind::System);
                assert_eq!(system.len(), 1, "`io.size` serves the system");
                let some = |kinds: &[TargetKind]| {
                    Some(
                        kinds
                            .iter()
                            .map(|kind| (*kind, io_size.target_support(*kind)))
                            .collect::<BTreeMap<_, _>>(),
                    )
                };
                entry.backends[0].targets = some(&system);
                entry.backends[1].targets = some(&others);
            }
            Catalog::from_entries(vec![entry]).map(|_| ())
        };
        let error = load(Cost::Low, true, false).unwrap_err().to_string();
        assert!(
            error.contains("`bpf:tp` is declared equivalent and declares `low`")
                && error.contains("`bpf:raw_tp`'s `high`"),
            "{error}"
        );
        assert!(load(Cost::High, true, false).is_ok());
        assert!(load(Cost::Extreme, true, false).is_ok());
        assert!(load(Cost::Low, false, false).is_ok());
        let disjoint = load(Cost::Low, true, true);
        assert!(disjoint.is_ok(), "{disjoint:?}");
    }

    #[test]
    fn system_wide_sources_refuse_per_process_targets() {
        let catalog = catalog();
        let bandwidth = catalog.get(&"mem.bandwidth".parse().unwrap()).unwrap();
        assert_eq!(bandwidth.target_support(TargetKind::Pid), TargetSupport::No);
        assert_eq!(
            bandwidth.target_support(TargetKind::System),
            TargetSupport::Native
        );

        let energy = catalog.get(&"power.energy".parse().unwrap()).unwrap();
        assert_eq!(energy.target_support(TargetKind::Pid), TargetSupport::No);
    }

    #[test]
    fn target_support_follows_the_documented_fallbacks() {
        let catalog = catalog();
        let offcpu = catalog.get(&"sched.offcpu".parse().unwrap()).unwrap();
        // `launch` is not declared, so it follows `pid`.
        assert_eq!(
            offcpu.target_support(TargetKind::Launch),
            offcpu.target_support(TargetKind::Pid)
        );
    }

    /// `filter_in_program` is a subset of what the backend carries, and a
    /// backend's `modes` a non-empty subset of the entry's that leaves no
    /// declared mode unserved: each is a promise nothing could ever keep
    /// otherwise.
    #[test]
    fn a_backend_promising_a_field_or_a_mode_its_entry_cannot_have_is_refused() {
        let embedded = catalog();
        let base = embedded
            .get(&"proc.syscall".parse().unwrap())
            .unwrap()
            .clone();

        let mut entry = base.clone();
        entry.backends[0].filter_in_program = vec!["filename".to_owned()];
        let error = Catalog::from_entries(vec![entry]).unwrap_err().to_string();
        assert!(error.contains("does not carry"), "{error}");

        let mut entry = base.clone();
        entry.backends[0].modes = Some(vec![ModeKind::Sample]);
        let error = Catalog::from_entries(vec![entry]).unwrap_err().to_string();
        assert!(error.contains("does not declare"), "{error}");

        let mut entry = base.clone();
        entry.backends[0].modes = Some(Vec::new());
        let error = Catalog::from_entries(vec![entry]).unwrap_err().to_string();
        assert!(error.contains("empty `modes`"), "{error}");

        let mut entry = base;
        for backend in &mut entry.backends {
            backend.modes = Some(vec![ModeKind::Trace]);
        }
        let error = Catalog::from_entries(vec![entry]).unwrap_err().to_string();
        assert!(
            error.contains("no backend in its chain serves it"),
            "{error}"
        );
    }

    /// A backend's `keys` is held to the entry's in both directions, the way
    /// its `filter_fields` is.
    #[test]
    fn a_backend_attaching_a_key_its_entry_does_not_offer_is_refused() {
        let base = catalog().get(&"io.bytes".parse().unwrap()).unwrap().clone();

        let mut entry = base.clone();
        entry.backends[0].keys = Some(vec![KeyKind::Req]);
        let error = Catalog::from_entries(vec![entry]).unwrap_err().to_string();
        assert!(error.contains("attaches the key `req`"), "{error}");

        let mut entry = base.clone();
        for backend in &mut entry.backends {
            backend.keys = Some(vec![KeyKind::Dev]);
        }
        let error = Catalog::from_entries(vec![entry]).unwrap_err().to_string();
        assert!(
            error.contains("no backend in its chain attaches"),
            "{error}"
        );

        let mut entry = base;
        let last = entry.backends.len() - 1;
        entry.backends[last].keys = Some(vec![KeyKind::Dev]);
        let catalog = Catalog::from_entries(vec![entry]).unwrap();
        let entry = catalog.get(&"io.bytes".parse().unwrap()).unwrap();
        assert_eq!(entry.backend_keys(&entry.backends[last]), &[KeyKind::Dev]);
        assert_eq!(
            entry.backend_keys(&entry.backends[0]),
            entry.keys.as_slice()
        );
        assert!(!entry
            .backends_carrying_key(KeyKind::Task)
            .contains(&entry.backends[last].id.as_str()));
    }

    /// The unwind table sits beside the entries and is not read as entries:
    /// a copy of the directory -- what every `--catalog` scenario makes --
    /// loads as the embedded catalog does.
    #[test]
    fn a_copy_of_the_catalog_directory_loads_without_reading_the_unwind_table_as_entries() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("catalog");
        assert!(
            source.join(UNWIND_TABLE).is_file(),
            "the unwind table is in the catalog directory"
        );
        let loaded = Catalog::load_dir(&source).expect("the tree's catalog directory loads");
        let embedded = catalog();
        assert_eq!(
            loaded.ids().collect::<BTreeSet<_>>(),
            embedded.ids().collect::<BTreeSet<_>>()
        );
    }

    /// The bands are ordered as the costs are, and `extreme` has no ceiling.
    #[test]
    fn a_cost_bands_worst_case_rises_with_the_cost() {
        let costs = [Cost::Free, Cost::Low, Cost::Med, Cost::High, Cost::Extreme];
        for pair in costs.windows(2) {
            assert!(pair[0].worst_case_percent() < pair[1].worst_case_percent());
        }
        assert!(Cost::Extreme.worst_case_percent().is_infinite());
    }

    #[test]
    fn a_duplicate_entry_is_refused() {
        let entry = catalog().entries()[0].clone();
        let error = Catalog::from_entries(vec![entry.clone(), entry]).unwrap_err();
        assert!(error.message().contains("duplicate"), "{error}");
    }
}
