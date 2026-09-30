//! `mem.numa` over `/proc/[pid]/numa_maps`.
//!
//! On a machine with more than one memory controller, *where* a page lives is
//! as much a performance fact as how many pages there are: a load served from
//! the node the thread is running on costs a fraction of one served across the
//! interconnect. The catalog's first backend for this source samples the
//! accesses themselves through the PMU's data-source information, which is the
//! question people actually ask. This one is the fallback, and it answers a
//! neighbouring question honestly rather than the same one badly: **page
//! placement, not accesses.** A page on the wrong node that is never touched
//! costs nothing, and this backend cannot tell the two apart.
//!
//! One event per node per tick, never a total and never a "locality ratio"
//! computed from placement. A process with two thirds of its pages on node 0
//! may be doing all of its work from node 1, and a single number would hide
//! exactly the thing the source is read to find.
//!
//! The file reports a count per node per mapping, in units of that mapping's
//! own page size -- `kernelpagesize_kB=2048` for a mapping the kernel gave huge
//! pages. Counting every `N<node>=` as a small page would under-report a
//! transparently promoted region by a factor of five hundred, which on this
//! family's own subject matter is the kind of mistake that looks plausible.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::cadence::Cadence;
use crate::clock::monotonic_ns;
use crate::collect::task::Comm;
use crate::collect::{Collector, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

/// Holds the mode until the target exists.
pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "page placement is read per process out of /proc/[pid]/numa_maps, and a {} \
                 target names none: the per-node view of a whole machine is \
                 /sys/devices/system/node, which is a different measurement",
                target.kind()
            )
        })?;
        let interval = match self.0 {
            Mode::Poll { interval } => std::time::Duration::from_nanos(interval.as_nanos()),
            _ => std::time::Duration::ZERO,
        };
        Ok(Box::new(NumaCollector::new(pid, interval)))
    }
}

/// Reads one process's page placement.
pub struct NumaCollector {
    id: CollectorId,
    pid: i32,
    path: PathBuf,
    comm: Comm,
    cadence: Cadence,
}

impl NumaCollector {
    pub fn new(pid: i32, interval: std::time::Duration) -> Self {
        Self {
            id: CollectorId::new("mem.numa/fs:proc"),
            pid,
            path: PathBuf::from(format!("/proc/{pid}/numa_maps")),
            comm: Comm::of(pid),
            cadence: Cadence::new(interval),
        }
    }

    fn read(&self) -> Result<BTreeMap<u32, Residency>, SampleError> {
        let text = std::fs::read_to_string(&self.path).map_err(|error| {
            let gone = error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH);
            if gone {
                SampleError::TargetGone(format!("process {} exited", self.pid))
            } else if error.kind() == std::io::ErrorKind::PermissionDenied {
                SampleError::TargetGone(format!(
                    "{} is no longer readable: {error}",
                    self.path.display()
                ))
            } else {
                SampleError::Transient(format!("{}: {error}", self.path.display()))
            }
        })?;
        Ok(residency(&text))
    }
}

impl Collector for NumaCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "pages"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if !self.cadence.due(monotonic_ns()) {
            return Ok(Vec::new());
        }
        let by_node = self.read()?;
        let ts_ns = monotonic_ns();
        let comm = self.comm.get().map(str::to_owned);
        Ok(by_node
            .into_iter()
            .map(|(node, residency)| {
                let mut attrs = vec![
                    Attr::uint("pages", residency.pages),
                    Attr::uint("bytes", residency.bytes),
                    Attr::uint("node", u64::from(node)),
                    Attr::text("read_from", "numa_maps"),
                ];
                if let Some(comm) = comm.as_deref() {
                    attrs.push(Attr::text("comm", comm));
                }
                Event {
                    ts_ns,
                    ts_err_ns: 0,
                    kind: "mem.numa".to_owned(),
                    // The node first: one event per node is the source's
                    // shape (its first caveat), and a series keyed by the
                    // task alone folded every node of a process into one.
                    keys: vec![KeyValue::node(node), KeyValue::task(self.pid)],
                    attrs,
                    collector_id: self.id.clone(),
                    seq,
                }
            })
            .collect())
    }
}

/// What one node holds of one address space.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Residency {
    /// Pages as the file counts them, which for a huge mapping are huge pages.
    pages: u64,
    bytes: u64,
}

/// Adds up the per-node counts of every mapping.
///
/// A line is `<address> <policy>` followed by `key=value` fields, of which
/// `N<node>=<pages>` and `kernelpagesize_kB=<size>` are the two that matter.
/// The page size is per line, and a line that does not state one is read at the
/// system page size, which is what the kernel omits it for.
fn residency(text: &str) -> BTreeMap<u32, Residency> {
    let mut by_node: BTreeMap<u32, Residency> = BTreeMap::new();
    for line in text.lines() {
        let mut nodes: Vec<(u32, u64)> = Vec::new();
        let mut page_bytes = page_size();
        for field in line.split_whitespace().skip(2) {
            let Some((name, value)) = field.split_once('=') else {
                continue;
            };
            if name == "kernelpagesize_kB" {
                if let Ok(kilobytes) = value.parse::<u64>() {
                    page_bytes = kilobytes * 1024;
                }
            } else if let Some(node) = name.strip_prefix('N') {
                if let (Ok(node), Ok(pages)) = (node.parse::<u32>(), value.parse::<u64>()) {
                    nodes.push((node, pages));
                }
            }
        }
        for (node, pages) in nodes {
            let entry = by_node.entry(node).or_default();
            entry.pages += pages;
            entry.bytes += pages * page_bytes;
        }
    }
    by_node
}

fn page_size() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as u64
    } else {
        4096
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::mem::testing::Toucher;

    /// A mapping the kernel gave huge pages counts huge pages, and the file
    /// says so on the line. Reading every `N0=` at the system page size
    /// under-reports such a mapping by the ratio of the two sizes.
    #[test]
    fn a_mapping_is_counted_in_its_own_page_size() {
        let text = "\
7f0000000000 default anon=3 dirty=3 N0=2 N1=1 kernelpagesize_kB=4
7f0100000000 default anon=8 dirty=8 N0=8 kernelpagesize_kB=2048
";
        let by_node = residency(text);
        assert_eq!(by_node[&0].pages, 10);
        assert_eq!(by_node[&1].pages, 1);
        assert_eq!(by_node[&0].bytes, 2 * 4096 + 8 * 2 * 1024 * 1024);
        assert_eq!(by_node[&1].bytes, 4096);
    }

    /// A file mapping that is not resident has no `N<node>=` field at all, and
    /// must contribute nothing rather than a zero that makes a node exist.
    #[test]
    fn a_mapping_that_is_not_resident_creates_no_node() {
        let by_node = residency("7f0000000000 default file=/usr/lib/libc.so.6\n");
        assert!(by_node.is_empty());
    }

    /// `mapped=` and `mapmax=` are not node counts, and a reader that took
    /// every `N`-looking field would invent nodes out of nothing.
    #[test]
    fn only_the_per_node_fields_are_counted() {
        let text = "7f0000000000 default file=/x mapped=7 mapmax=3 active=2 N0=7 \
                    kernelpagesize_kB=4\n";
        let by_node = residency(text);
        assert_eq!(by_node.len(), 1);
        assert_eq!(by_node[&0].pages, 7);
    }

    /// Ground truth, twice over, on a process that holds still.
    ///
    /// A forked child makes a known number of bytes resident and then waits,
    /// which is what makes this measurable at all: the test binary itself runs
    /// its other tests in parallel threads of the same process, allocating and
    /// freeing megabytes between any two reads of its own `numa_maps`. The
    /// first version of this test measured the suite rather than the
    /// allocation, and came up 22 MiB short.
    ///
    /// Two things are then checked, and neither is this collector's own
    /// arithmetic:
    ///
    /// - the child made `SIZE` bytes resident, so the per-node residency has
    ///   to account for at least that much.
    /// - every resident page is on exactly one node, so the per-node residency
    ///   summed over the nodes *is* the resident set -- which the kernel
    ///   publishes independently, by another route, as `Rss:` in
    ///   `smaps_rollup`.
    #[test]
    fn the_per_node_residency_is_the_resident_set_and_holds_what_was_touched() {
        const SIZE: u64 = 64 * 1024 * 1024;

        let mut child = Toucher::start(None, SIZE);
        let mut collector = NumaCollector::new(child.pid(), std::time::Duration::ZERO);
        let events = match collector.sample(0) {
            Ok(events) => events,
            Err(error) => {
                child.stop();
                eprintln!("skipped: numa_maps is not readable here ({error:?})");
                return;
            }
        };
        let resident = resident_bytes(child.pid());
        child.stop();

        assert!(!events.is_empty(), "a running process holds resident pages");
        let total: f64 = events
            .iter()
            .map(|event| event.attr("bytes").unwrap().as_f64().unwrap())
            .sum();
        assert!(
            total >= SIZE as f64,
            "the child made {SIZE} bytes resident and its per-node residency adds up to {total}"
        );

        let Some(resident) = resident else {
            eprintln!("skipped: smaps_rollup of the child is not readable here");
            return;
        };
        let ratio = total / resident as f64;
        assert!(
            (0.95..1.05).contains(&ratio),
            "numa_maps adds up to {total} bytes across its nodes and smaps_rollup says the \
             resident set is {resident}"
        );
    }

    /// Every node the file names is reported apart, and the node is on the
    /// event: a single total would hide the one thing this source is read to
    /// find.
    #[test]
    fn each_node_is_its_own_event() {
        let mut child = Toucher::start(None, 8 * 1024 * 1024);
        let mut collector = NumaCollector::new(child.pid(), std::time::Duration::ZERO);
        let events = match collector.sample(0) {
            Ok(events) => events,
            Err(_) => {
                child.stop();
                eprintln!("skipped: numa_maps is not readable here");
                return;
            }
        };
        child.stop();

        let nodes: Vec<f64> = events
            .iter()
            .map(|event| event.attr("node").unwrap().as_f64().unwrap())
            .collect();
        let mut sorted = nodes.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        sorted.dedup();
        assert_eq!(
            nodes.len(),
            sorted.len(),
            "a node appears twice, so two mappings on one node became two events"
        );
        let online = std::fs::read_to_string("/sys/devices/system/node/online").unwrap_or_default();
        assert!(
            !nodes.is_empty(),
            "a resident process is on at least one node (online: {online})"
        );
        // Keyed by the node, first, and the node is one the kernel says is
        // online -- the second account, from another file.
        let online = crate::capabilities::parse_cpu_list(online.trim()).unwrap_or_default();
        for event in &events {
            let node = event.attr("node").unwrap().as_f64().unwrap() as u32;
            assert_eq!(
                event.keys.first(),
                Some(&KeyValue::node(node)),
                "an event per node is keyed by its node: {:?}",
                event.keys
            );
            assert!(
                online.contains(&node),
                "node {node} is not one /sys/devices/system/node/online names ({online:?})"
            );
        }
    }

    fn resident_bytes(pid: i32) -> Option<u64> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
        text.lines()
            .find_map(|line| line.strip_prefix("Rss:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|kilobytes| kilobytes * 1024)
    }
}
