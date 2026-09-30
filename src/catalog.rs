//! The catalog: the tool's vocabulary, loaded as data.
//!
//! `CATALOG.md` describes the entries and `catalog/*.yaml` holds them. The CLI,
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
use crate::request::{EmitKind, KeyKind, ModeKind, SourceId, TargetKind};
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
    #[serde(default)]
    pub targets: BTreeMap<TargetKind, TargetSupport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<KeyKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filter_fields: Vec<String>,
    pub determinism: Determinism,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
}

impl Entry {
    /// How this source supports a target kind, following the documented
    /// fallbacks: launching a program is following a pid tree, a container is a
    /// cgroup, a thread is a narrower pid.
    pub fn target_support(&self, kind: TargetKind) -> TargetSupport {
        support_in(&self.targets, kind)
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

    /// The keys one backend of this chain attaches to its events: its own
    /// list when it declares one, the entry's otherwise.
    pub fn backend_keys<'a>(&'a self, backend: &'a Backend) -> &'a [KeyKind] {
        backend.keys.as_deref().unwrap_or(&self.keys)
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

/// One way of obtaining a source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    /// A backend tag from `CATALOG.md`: `pmu`, `bpf:tp`, `fs:proc`, and so on.
    pub id: String,
    /// What the machine must provide: tracepoints, PMU events, files, BTF.
    /// The resolver probes each one; nothing here is assumed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    pub cost: Cost,
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
}

/// How two events are tied together into one measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinRule {
    /// The typed key, written as in `CATALOG.md`: `blk:rq@open_ts`.
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
    /// instrumentation, which `DECISIONS.md` rules out as a backend.
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
}

impl Catalog {
    /// Loads the catalog compiled into the binary.
    pub fn embedded() -> Result<Self, ParseError> {
        let mut entries = Vec::new();
        for (name, text) in EMBEDDED {
            entries.extend(parse_file(name, text)?);
        }
        Self::from_entries(entries)
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
        for path in files {
            let text = std::fs::read_to_string(&path).map_err(|error| {
                ParseError::new(format!("cannot read {}: {error}", path.display()))
            })?;
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("catalog")
                .to_owned();
            entries.extend(parse_file(&name, &text)?);
        }
        Self::from_entries(entries)
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
            check_backend_keys(entry)?;
            check_requirement_globs(entry)?;
        }
        let catalog = Self { entries };
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
}

fn parse_file(name: &str, text: &str) -> Result<Vec<Entry>, ParseError> {
    serde_norway::from_str(text)
        .map_err(|error| ParseError::new(format!("catalog file {name} is invalid: {error}")))
}

impl FromStr for Unit {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const ALL: [Unit; 9] = [
            Unit::Ns,
            Unit::Bytes,
            Unit::Count,
            Unit::Ratio,
            Unit::Hz,
            Unit::Joules,
            Unit::Watts,
            Unit::Celsius,
            Unit::Cycles,
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

    #[test]
    fn a_power_is_a_unit_of_its_own() {
        assert_eq!("watts".parse::<Unit>().unwrap(), Unit::Watts);
        assert_eq!(Unit::Watts.to_string(), "watts");
        assert_ne!(Unit::Watts, Unit::Joules);
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
