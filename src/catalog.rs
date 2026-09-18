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
        if let Some(support) = self.targets.get(&kind) {
            return *support;
        }
        match kind.fallback() {
            Some(fallback) => self.target_support(fallback),
            None => TargetSupport::No,
        }
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
    /// The join this backend needs to produce a value at all, when it derives
    /// one from two separate events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<JoinRule>,
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
}

impl MatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchKind::Eq => "eq",
            MatchKind::ScopedEq => "scoped_eq",
            MatchKind::Interval => "interval",
            MatchKind::Wakeup => "wakeup",
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
    /// Over 10x: replay or dynamic binary instrumentation.
    Extreme,
}

impl Cost {
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
    Celsius,
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
            Unit::Celsius => "celsius",
        }
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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
        }
        let catalog = Self { entries };
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
        const ALL: [Unit; 7] = [
            Unit::Ns,
            Unit::Bytes,
            Unit::Count,
            Unit::Ratio,
            Unit::Hz,
            Unit::Joules,
            Unit::Celsius,
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
        entry.backends[1].degraded = None;
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

    #[test]
    fn a_duplicate_entry_is_refused() {
        let entry = catalog().entries()[0].clone();
        let error = Catalog::from_entries(vec![entry.clone(), entry]).unwrap_err();
        assert!(error.message().contains("duplicate"), "{error}");
    }
}
