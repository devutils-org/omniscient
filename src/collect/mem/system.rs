//! `mem.system` over `/proc/meminfo`, and the machine's per-node allocation
//! counters for `mem.numa --system` over `/sys/devices/system/node`.
//!
//! What `free` and `vmstat`'s memory columns print, as events: the machine's
//! memory as the kernel accounts it, one event per poll. The value is **used**
//! the way `free` has computed it since procps 4: total minus available, where
//! *available* is the kernel's own estimate of what could be handed to a new
//! program without swapping -- free pages, plus the page cache and reclaimable
//! slab it could drop. Total minus free would call every cached page "used",
//! and on a machine that has been up an hour that is most of them.
//!
//! The slab is carried in the same event (`slab_bytes`, split into its
//! reclaimable and unreclaimable halves), because what `slabtop` adds -- the
//! split per cache -- lives in `/proc/slabinfo`, which only root can read.
//!
//! `/proc/meminfo` is in kibibytes whatever it writes after the number
//! (`kB`), and a line it does not know is skipped rather than refused: the
//! file grows a field every few kernel releases.
//!
//! The per-node counters are a different measurement from the per-process
//! page placement `mem.numa` reads through `numa_maps`: they count
//! *allocations*, each filed on the node the page came from, as a hit when
//! that was the node asked for and a miss when it was not. A delta per poll,
//! one event per node, never summed across nodes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use super::cadence::Cadence;
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

const MEMINFO: &str = "/proc/meminfo";
const NODES: &str = "/sys/devices/system/node";

/// The `/proc/meminfo` lines carried, and the attribute each becomes.
const FIELDS: &[(&str, &str)] = &[
    ("MemTotal", "total_bytes"),
    ("MemFree", "free_bytes"),
    ("MemAvailable", "available_bytes"),
    ("Buffers", "buffers_bytes"),
    ("Cached", "cached_bytes"),
    ("SwapCached", "swap_cached_bytes"),
    ("Active", "active_bytes"),
    ("Inactive", "inactive_bytes"),
    ("AnonPages", "anon_bytes"),
    ("Mapped", "mapped_bytes"),
    ("Shmem", "shmem_bytes"),
    ("Slab", "slab_bytes"),
    ("SReclaimable", "slab_reclaimable_bytes"),
    ("SUnreclaim", "slab_unreclaimable_bytes"),
    ("KernelStack", "kernel_stack_bytes"),
    ("PageTables", "page_tables_bytes"),
    ("Dirty", "dirty_bytes"),
    ("Writeback", "writeback_bytes"),
    ("SwapTotal", "swap_total_bytes"),
    ("SwapFree", "swap_free_bytes"),
    ("Committed_AS", "committed_bytes"),
    ("CommitLimit", "commit_limit_bytes"),
    ("AnonHugePages", "anon_huge_bytes"),
    ("HugePages_Total", "huge_pages_total"),
    ("HugePages_Free", "huge_pages_free"),
];

fn interval_of(mode: Mode) -> Duration {
    match mode {
        Mode::Poll { interval } => Duration::from_nanos(interval.as_nanos()),
        Mode::Snapshot => Duration::ZERO,
        _ => Duration::from_nanos(Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
    }
}

fn machine_only(target: &Target, what: &str) -> Result<(), String> {
    match target {
        Target::System => Ok(()),
        other => Err(format!(
            "{what} is the machine's, and a {} target is not the machine: a process's \
             footprint is `mem.rss`, a cgroup's charge is `mem.cgroup`",
            other.kind()
        )),
    }
}

/// `mem.system`: holds the mode until the target is known.
pub struct Prepare(Mode);

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } | Mode::Snapshot => Ok(Self(mode)),
            other => Err(format!(
                "{MEMINFO} is a set of levels that is read, not a stream of events: `{}` cannot \
                 be served from it; poll it, or take one snapshot",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        machine_only(target, MEMINFO)?;
        Ok(Box::new(MemInfo {
            id: CollectorId::new("mem.system/fs:proc"),
            path: PathBuf::from(MEMINFO),
            cadence: Cadence::new(interval_of(self.0)),
        }))
    }
}

/// Reads `/proc/meminfo`.
pub struct MemInfo {
    id: CollectorId,
    path: PathBuf,
    cadence: Cadence,
}

/// The `/proc/meminfo` lines, in bytes, or in pages for the two counts of
/// huge pages, which the file writes without a unit.
fn parse_meminfo(text: &str) -> BTreeMap<&str, u64> {
    text.lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let mut words = rest.split_whitespace();
            let value: u64 = words.next()?.parse().ok()?;
            Some(match words.next() {
                Some("kB") => (name, value * 1024),
                _ => (name, value),
            })
        })
        .collect()
}

/// The event's attributes; `None` when the file lacks the two lines `used`
/// is computed from.
fn meminfo_attrs(fields: &BTreeMap<&str, u64>) -> Option<Vec<Attr>> {
    let total = *fields.get("MemTotal")?;
    let available = *fields.get("MemAvailable")?;
    let mut attrs = vec![Attr::uint("used_bytes", total.saturating_sub(available))];
    for (line, attr) in FIELDS {
        if let Some(value) = fields.get(line) {
            attrs.push(Attr::uint(*attr, *value));
        }
    }
    // `free`'s buff/cache column: the reclaimable slab is counted with the
    // cache because it is dropped the same way.
    let cache = ["Buffers", "Cached", "SReclaimable"]
        .iter()
        .filter_map(|line| fields.get(line))
        .sum::<u64>();
    attrs.push(Attr::uint("buff_cache_bytes", cache));
    if let (Some(total), Some(free)) = (fields.get("SwapTotal"), fields.get("SwapFree")) {
        attrs.push(Attr::uint("swap_used_bytes", total.saturating_sub(*free)));
    }
    attrs.push(Attr::text("read_from", MEMINFO));
    Some(attrs)
}

impl Collector for MemInfo {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "used_bytes"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if !self.cadence.due(monotonic_ns()) {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.path)
            .map_err(|error| SampleError::Transient(format!("{MEMINFO}: {error}")))?;
        let ts_ns = monotonic_ns();
        let attrs = meminfo_attrs(&parse_meminfo(&text)).ok_or_else(|| {
            SampleError::Transient(format!("{MEMINFO} has no MemTotal or MemAvailable line"))
        })?;
        Ok(vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.system".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        }])
    }
}

/// The counters of one node's `numastat`, in pages.
const NODE_FIELDS: &[&str] = &[
    "numa_hit",
    "numa_miss",
    "numa_foreign",
    "interleave_hit",
    "local_node",
    "other_node",
];

/// `mem.numa --system`: holds the mode until the target is known.
pub struct NodePrepare(Mode);

impl NodePrepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } => Ok(Self(mode)),
            other => Err(format!(
                "the per-node allocation counters are counters that are read: `{}` cannot be \
                 served from {NODES}; poll them",
                other.kind()
            )),
        }
    }
}

impl Prepared for NodePrepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        machine_only(target, "the per-node allocation count")?;
        let nodes = list_nodes(std::path::Path::new(NODES))?;
        Ok(Box::new(NodeStat {
            id: CollectorId::new("mem.numa/fs:sys"),
            nodes,
            cadence: Cadence::new(interval_of(self.0)),
            previous: BTreeMap::new(),
            page_size: page_size(),
            losses: Losses::default(),
        }))
    }
}

fn list_nodes(root: &std::path::Path) -> Result<Vec<u32>, String> {
    let entries = std::fs::read_dir(root)
        .map_err(|error| format!("cannot list {}: {error}", root.display()))?;
    let mut nodes: Vec<u32> = entries
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_prefix("node")?
                .parse()
                .ok()
        })
        .filter(|node| root.join(format!("node{node}/numastat")).exists())
        .collect();
    nodes.sort_unstable();
    if nodes.is_empty() {
        return Err(format!("{} lists no node with a numastat", root.display()));
    }
    Ok(nodes)
}

fn page_size() -> u64 {
    // SAFETY: sysconf reads a constant of the running system.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as u64
    } else {
        4096
    }
}

fn parse_numastat(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next()?;
            let value = words.next()?.parse().ok()?;
            Some((name.to_owned(), value))
        })
        .collect()
}

/// Reads every node's `numastat` and reports what each counter moved.
pub struct NodeStat {
    id: CollectorId,
    nodes: Vec<u32>,
    cadence: Cadence,
    previous: BTreeMap<u32, (u64, BTreeMap<String, u64>)>,
    page_size: u64,
    losses: Losses,
}

impl NodeStat {
    fn delta_event(
        &mut self,
        node: u32,
        before: &(u64, BTreeMap<String, u64>),
        after: &(u64, BTreeMap<String, u64>),
        seq: u64,
    ) -> Option<Event> {
        let mut attrs = Vec::with_capacity(NODE_FIELDS.len() + 3);
        for field in NODE_FIELDS {
            let (Some(now), Some(then)) = (after.1.get(*field), before.1.get(*field)) else {
                continue;
            };
            let Some(moved) = now.checked_sub(*then) else {
                self.losses.undecoded += 1;
                self.losses.note(format!(
                    "node {node}'s {field} went backwards, so that interval is not reported"
                ));
                return None;
            };
            attrs.push(Attr::uint(*field, moved));
        }
        let hit = attrs
            .iter()
            .find(|attr| attr.name == "numa_hit")
            .and_then(|attr| attr.value.as_f64())
            .unwrap_or(0.0) as u64;
        attrs.push(Attr::uint("allocated_bytes", hit * self.page_size));
        attrs.push(Attr::uint("interval_ns", after.0.saturating_sub(before.0)));
        attrs.push(Attr::text("read_from", "numastat"));
        Some(Event {
            ts_ns: after.0,
            ts_err_ns: 0,
            kind: "mem.numa".to_owned(),
            keys: vec![KeyValue::Node { node }],
            attrs,
            collector_id: self.id.clone(),
            seq,
        })
    }
}

impl Collector for NodeStat {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    /// The pages filed on a node other than the one asked for: the imbalance
    /// the counters exist to show.
    fn value_attr(&self) -> &'static str {
        "numa_miss"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if !self.cadence.due(monotonic_ns()) {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        for node in self.nodes.clone() {
            let path = format!("{NODES}/node{node}/numastat");
            let text = std::fs::read_to_string(&path)
                .map_err(|error| SampleError::Transient(format!("{path}: {error}")))?;
            let now = (monotonic_ns(), parse_numastat(&text));
            if let Some(before) = self.previous.insert(node, now.clone()) {
                if let Some(event) = self.delta_event(node, &before, &now, seq) {
                    events.push(event);
                }
            }
        }
        Ok(events)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO_TEXT: &str = "MemTotal:       65768868 kB\n\
        MemFree:        40000000 kB\n\
        MemAvailable:   60000000 kB\n\
        Buffers:          100000 kB\n\
        Cached:          9000000 kB\n\
        SReclaimable:     500000 kB\n\
        SUnreclaim:       200000 kB\n\
        Slab:             700000 kB\n\
        SwapTotal:       8000000 kB\n\
        SwapFree:        7000000 kB\n\
        HugePages_Total:       4\n\
        Unknownfield:        12 kB\n";

    #[test]
    fn used_is_total_minus_available_and_the_lines_are_in_kibibytes() {
        let fields = parse_meminfo(MEMINFO_TEXT);
        let attrs = meminfo_attrs(&fields).expect("both lines");
        let get = |name: &str| {
            attrs
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
                .unwrap_or_else(|| panic!("{name}")) as u64
        };
        assert_eq!(get("used_bytes"), (65768868 - 60000000) * 1024);
        assert_eq!(get("buff_cache_bytes"), (100000 + 9000000 + 500000) * 1024);
        assert_eq!(get("slab_bytes"), 700000 * 1024);
        assert_eq!(get("swap_used_bytes"), 1000000 * 1024);
        assert_eq!(get("huge_pages_total"), 4, "a count, not kibibytes");
    }

    #[test]
    fn a_meminfo_without_available_is_refused_rather_than_guessed() {
        assert!(meminfo_attrs(&parse_meminfo("MemTotal: 10 kB\nMemFree: 5 kB\n")).is_none());
    }

    /// Against `free -b`, read the moment after: the same file, an independent
    /// reader. Totals match to the byte; used moves with the machine, so it is
    /// held to a percent of the total.
    #[test]
    fn the_machine_reads_as_free_prints_it() {
        let mut collector = MemInfo {
            id: CollectorId::new("mem.system/fs:proc"),
            path: PathBuf::from(MEMINFO),
            cadence: Cadence::new(Duration::ZERO),
        };
        let events = collector.sample(0).expect("meminfo");
        let Ok(output) = std::process::Command::new("free").arg("-b").output() else {
            eprintln!("skipped: no `free` to compare with");
            return;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let mem: Vec<u64> = text
            .lines()
            .find(|line| line.starts_with("Mem:"))
            .expect("free's Mem line")
            .split_whitespace()
            .skip(1)
            .filter_map(|word| word.parse().ok())
            .collect();
        let get = |name: &str| events[0].attr(name).and_then(|v| v.as_f64()).expect(name);
        assert_eq!(get("total_bytes") as u64, mem[0]);
        let used = get("used_bytes");
        assert!(
            (used - mem[1] as f64).abs() < 0.01 * mem[0] as f64,
            "used {used} against free's {}",
            mem[1]
        );
    }

    #[test]
    fn a_process_target_is_refused_with_where_to_look() {
        let refusal = Box::new(Prepare::new(Mode::Snapshot).unwrap())
            .attach(&Target::Pid(vec![1]))
            .err()
            .expect("a pid is not the machine");
        assert!(refusal.contains("mem.rss"), "{refusal}");
    }

    #[test]
    fn numastat_moves_are_reported_per_node() {
        let before = (
            0,
            parse_numastat("numa_hit 100\nnuma_miss 1\nother_node 3\n"),
        );
        let after = (
            1_000,
            parse_numastat("numa_hit 150\nnuma_miss 4\nother_node 3\n"),
        );
        let mut collector = NodeStat {
            id: CollectorId::new("mem.numa/fs:sys"),
            nodes: vec![0],
            cadence: Cadence::new(Duration::ZERO),
            previous: BTreeMap::new(),
            page_size: 4096,
            losses: Losses::default(),
        };
        let event = collector
            .delta_event(0, &before, &after, 0)
            .expect("an event");
        assert_eq!(event.keys, vec![KeyValue::Node { node: 0 }]);
        assert_eq!(event.attr("numa_hit").and_then(|v| v.as_f64()), Some(50.0));
        assert_eq!(event.attr("numa_miss").and_then(|v| v.as_f64()), Some(3.0));
        assert_eq!(
            event.attr("allocated_bytes").and_then(|v| v.as_f64()),
            Some(50.0 * 4096.0)
        );
        let backwards = (2_000, parse_numastat("numa_hit 10\n"));
        assert!(collector.delta_event(0, &after, &backwards, 1).is_none());
        assert_eq!(collector.losses.undecoded, 1);
    }

    #[test]
    fn this_machine_lists_its_nodes() {
        if !std::path::Path::new(NODES).exists() {
            eprintln!("skipped: no {NODES}");
            return;
        }
        let nodes = list_nodes(std::path::Path::new(NODES)).expect("nodes");
        assert!(nodes.contains(&0), "{nodes:?}");
    }
}
