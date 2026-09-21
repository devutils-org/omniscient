//! `sched.numa_balance` over a raw tracepoint, counted in the kernel.
//!
//! Automatic NUMA balancing moves a task towards the memory it touches.
//! `sched_stick_numa` is the event for the decision *not* to: the balancer
//! found a better node for a task and left it where it was. A machine
//! producing these in quantity has tasks the balancer keeps wanting to move
//! and keeps not moving, which is a cause of slowness that appears in no cpu
//! measurement -- the task runs at full speed and every memory access takes
//! longer than it should.
//!
//! **The node is added here, not in the kernel.** The tracepoint carries cpus;
//! a reader thinks in nodes, and the catalog offers `node` as a filter field.
//! The mapping from one to the other is in `/sys/devices/system/node`, which
//! a program cannot read and this can. It is read once, at attach: cpus do not
//! change nodes while a machine is running.
//!
//! **On a machine with one NUMA node this source cannot fire**, and that is
//! not a failure of anything: there is no balancing to do when there is
//! nowhere else to be. The run says so rather than reporting an empty series
//! that looks like a machine with nothing to balance.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration as StdDuration;

use crate::bpf::{NumaDrops, NumaProgram, NumaStick};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the counts are drained.
///
/// Coarse on purpose: a balancing decision is a count rather than a duration,
/// so a slow interval costs resolution in the `rate` series and nothing else.
/// The balancer itself runs at most a few times a second per task.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(250);

/// Where the kernel publishes which cpus belong to which NUMA node.
const NODE_ROOT: &str = "/sys/devices/system/node";

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot count balancing decisions: a decision is an instant, and \
                 sampling instants at a frequency counts the samples rather than the decisions",
                self.mode
            ));
        }
        if !matches!(target, Target::System) {
            return Err(format!(
                "a {} target cannot be given to `sched.numa_balance`: the balancer's decisions \
                 are the machine's, and the catalog declares this source system-wide only",
                target.kind()
            ));
        }
        Ok(Box::new(NumaCollector::new(self.mode, self.filter)))
    }
}

/// Which node each cpu belongs to, read from sysfs.
///
/// A cpu whose node cannot be read is absent rather than zero: node zero is a
/// real node and reporting an unknown one as it would be a wrong answer that
/// reads like a right one.
#[derive(Debug, Default, Clone)]
pub struct NodeMap {
    by_cpu: BTreeMap<u32, u32>,
    nodes: usize,
}

impl NodeMap {
    pub fn read() -> Self {
        Self::read_from(Path::new(NODE_ROOT))
    }

    fn read_from(root: &Path) -> Self {
        let mut map = Self::default();
        let Ok(entries) = std::fs::read_dir(root) else {
            return map;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(node) = name
                .strip_prefix("node")
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(list) = std::fs::read_to_string(entry.path().join("cpulist")) else {
                continue;
            };
            map.nodes += 1;
            for cpu in parse_cpulist(&list) {
                map.by_cpu.insert(cpu, node);
            }
        }
        map
    }

    pub fn node_of(&self, cpu: u32) -> Option<u32> {
        self.by_cpu.get(&cpu).copied()
    }

    /// How many NUMA nodes this machine has. One means this source has nothing
    /// it could ever report.
    pub fn nodes(&self) -> usize {
        self.nodes
    }
}

/// `0-3,8,12-13` as sysfs writes it.
fn parse_cpulist(text: &str) -> Vec<u32> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        match part.split_once('-') {
            Some((start, end)) => {
                if let (Ok(start), Ok(end)) = (start.trim().parse(), end.trim().parse::<u32>()) {
                    cpus.extend(start..=end);
                }
            }
            None => {
                if let Ok(cpu) = part.trim().parse() {
                    cpus.push(cpu);
                }
            }
        }
    }
    cpus
}

pub struct NumaCollector {
    id: CollectorId,
    mode: Mode,
    program: Option<NumaProgram>,
    nodes: NodeMap,
    losses: Losses,
    dropped: NumaDrops,
    window_opened_ns: u64,
    run_opened_ns: u64,
    counted: BTreeMap<(u32, u32, u32), NumaStick>,
    predicate: Option<FilterExpr>,
    filtered_out: u64,
    filtered_in: u64,
}

impl NumaCollector {
    pub fn new(mode: Mode, predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("sched.numa_balance/bpf:raw_tp"),
            mode,
            program: None,
            nodes: NodeMap::default(),
            losses: Losses::default(),
            dropped: NumaDrops::default(),
            window_opened_ns: 0,
            run_opened_ns: 0,
            counted: BTreeMap::new(),
            predicate,
            filtered_out: 0,
            filtered_in: 0,
        }
    }

    fn counting(&self) -> bool {
        self.mode == Mode::Count
    }

    fn event(&self, row: &NumaStick, closed_ns: u64, window_ns: u64, seq: u64) -> Event {
        Event {
            ts_ns: closed_ns,
            ts_err_ns: 0,
            kind: "sched.numa_balance".to_owned(),
            // The task is the series: the decision was made about it, and the
            // cpus are where it was and where it did not go.
            keys: vec![KeyValue::task(row.tid as i32)],
            attrs: attributes(row, &self.nodes, window_ns),
            collector_id: self.id.clone(),
            seq,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.window_opened_ns);
        let (rows, drain_losses) = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                self.losses.failed_reads += 1;
                self.losses.note(reason);
                return;
            }
        };
        self.window_opened_ns = closed_ns;

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("a NUMA balancing count was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the NUMA balancing map held more rows than one drain can walk");
        }

        let counting = self.counting();
        let run_window_ns = closed_ns.saturating_sub(self.run_opened_ns);
        for (index, row) in rows.into_iter().enumerate() {
            let event = self.event(
                &row,
                closed_ns,
                if counting { run_window_ns } else { window_ns },
                seq + index as u64,
            );
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += row.count;
                    continue;
                }
                self.filtered_in += row.count;
            }
            if counting {
                self.counted
                    .entry((row.tid, row.src_cpu, row.dst_cpu))
                    .and_modify(|held| {
                        held.count += row.count;
                        held.comm.clone_from(&row.comm);
                    })
                    .or_insert(row);
                continue;
            }
            events.push(event);
        }
    }

    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let now = program.dropped();
        for (fresh, before, reason) in [
            (
                now.rows_full,
                self.dropped.rows_full,
                "more distinct (task, source cpu, destination cpu) triples appeared in one \
                 interval than the map holds, so those decisions were never counted",
            ),
            (
                now.rows_busy,
                self.dropped.rows_busy,
                "the kernel declined to add a row because the map's own bucket was already \
                 being updated on this cpu, so those decisions were never counted",
            ),
        ] {
            if fresh > before {
                self.losses.dropped += fresh - before;
                self.losses.note(reason);
            }
        }
        self.dropped = now;
    }
}

/// What an event carries beside its value.
///
/// `comm`, `pid` and `node` are here because the catalog entry declares them
/// as filter fields: a predicate over a field no event carries is false
/// everywhere, which empties the series and looks exactly like a balancer that
/// never gave up on anything.
fn attributes(row: &NumaStick, nodes: &NodeMap, window_ns: u64) -> Vec<Attr> {
    let mut attrs = vec![
        Attr::uint("decisions", row.count),
        // The interval the count covers. A count without its window cannot be
        // compared with another count.
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", u64::from(row.tid)),
        Attr::uint("pid", u64::from(row.tgid)),
        Attr::text("comm", row.comm.clone()),
        Attr::uint("src_cpu", u64::from(row.src_cpu)),
        Attr::uint("dst_cpu", u64::from(row.dst_cpu)),
    ];
    // The node the balancer wanted the task on, which is what `node` means for
    // this source: the one it did not go to. A cpu whose node sysfs does not
    // name carries no node at all rather than node zero, which is a real node.
    if let Some(node) = nodes.node_of(row.dst_cpu) {
        attrs.push(Attr::uint("node", u64::from(node)));
    }
    if let Some(node) = nodes.node_of(row.src_cpu) {
        attrs.push(Attr::uint("src_node", u64::from(node)));
    }
    attrs
}

impl Collector for NumaCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "decisions"
    }

    fn start(&mut self) -> Result<(), String> {
        // Read once: a cpu does not change node while a machine is running.
        self.nodes = NodeMap::read();
        self.program = Some(NumaProgram::load()?);
        self.window_opened_ns = monotonic_ns();
        self.run_opened_ns = self.window_opened_ns;
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_drops();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        self.absorb_drops();
        self.program = None;
        if self.counting() {
            let closed_ns = monotonic_ns();
            let window_ns = closed_ns.saturating_sub(self.run_opened_ns);
            let seq = u64::MAX / 2;
            let rows: Vec<NumaStick> = std::mem::take(&mut self.counted).into_values().collect();
            for (index, row) in rows.iter().enumerate() {
                trailing.push(self.event(row, closed_ns, window_ns, seq + index as u64));
            }
        }
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        // Said with the run rather than left to documentation: an empty series
        // from a machine that cannot produce one and an empty series from a
        // balancer with nothing to do look identical, and only the second is
        // about the workload.
        if self.nodes.nodes() <= 1 {
            notes.push(format!(
                "this machine has {} NUMA node, so the balancer has nowhere to move anything to \
                 and this source can never report a decision here: an empty series is the \
                 machine, not the workload",
                self.nodes.nodes().max(1)
            ));
        }
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} decisions and removed {}; it is applied to drained rows \
                 rather than in the program, so the cost of the ones it removed was still paid",
                self.filtered_in, self.filtered_out
            ));
        }
        if self.counting() {
            notes.push(
                "`count` reports one total per (task, source, destination) over the whole \
                 window, so there is no series here; `--mode trace` is the one that reports per \
                 drain"
                    .to_owned(),
            );
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> NumaStick {
        NumaStick {
            tid: 91,
            tgid: 90,
            src_cpu: 3,
            dst_cpu: 19,
            count: 5,
            comm: "walker".to_owned(),
        }
    }

    fn two_nodes() -> NodeMap {
        let by_cpu = (0..24u32).map(|cpu| (cpu, u32::from(cpu >= 12))).collect();
        NodeMap { by_cpu, nodes: 2 }
    }

    fn prepare(mode: Mode) -> Box<Prepare> {
        Box::new(Prepare { mode, filter: None })
    }

    #[test]
    fn every_mode_the_catalog_declares_is_accepted_and_the_rest_are_refused() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let entry = catalog
            .get(&"sched.numa_balance".parse().expect("a source id"))
            .expect("sched.numa_balance is in the catalog");
        for mode in [Mode::Trace, Mode::Count] {
            assert!(entry.modes.iter().any(|declared| *declared == mode.kind()));
            assert!(prepare(mode).attach(&Target::System).is_ok(), "`{mode}`");
        }
        let sampled = Mode::Sample {
            freq: "997hz".parse().expect("a frequency"),
        };
        assert!(prepare(sampled).attach(&Target::System).is_err());
    }

    #[test]
    fn a_target_the_catalog_does_not_declare_is_refused_by_name() {
        let Err(error) = prepare(Mode::Trace).attach(&Target::Pid(vec![1])) else {
            panic!("this source is system-wide only");
        };
        assert!(error.contains("pid"), "{error}");
    }

    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&row(), &two_nodes(), 250_000_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"sched.numa_balance".parse().expect("a source id"))
            .expect("sched.numa_balance is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty());
        for field in declared {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// `node` is the node the balancer wanted the task on, and `src_node` the
    /// one it stayed on. Getting these the wrong way round would reverse every
    /// answer this source gives.
    #[test]
    fn the_node_on_an_event_is_the_one_the_task_did_not_move_to() {
        let attributes = attributes(&row(), &two_nodes(), 1);
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
        };
        // cpu 19 is on node 1 and cpu 3 on node 0.
        assert_eq!(of("node"), Some(1.0));
        assert_eq!(of("src_node"), Some(0.0));
        assert_eq!(of("dst_cpu"), Some(19.0));
        assert_eq!(of("src_cpu"), Some(3.0));
    }

    /// Node zero is a real node. A cpu whose node sysfs does not name must
    /// carry no node rather than the one that happens to sort first.
    #[test]
    fn a_cpu_with_no_node_carries_no_node_rather_than_node_zero() {
        let empty = NodeMap::default();
        assert_eq!(empty.node_of(3), None);
        let attributes = attributes(&row(), &empty, 1);
        assert!(!attributes.iter().any(|attr| attr.name == "node"));
        assert!(!attributes.iter().any(|attr| attr.name == "src_node"));
    }

    /// An empty series from a machine that cannot produce one and an empty
    /// series from a balancer with nothing to do look identical. Only the
    /// second is about the workload, so the first has to say so.
    #[test]
    fn a_single_node_machine_says_the_series_is_empty_because_of_the_machine() {
        let collector = NumaCollector::new(Mode::Trace, None);
        assert!(
            collector
                .notes()
                .iter()
                .any(|note| note.contains("NUMA node") && note.contains("never")),
            "{:?}",
            collector.notes()
        );
    }

    #[test]
    fn a_cpulist_is_read_the_way_sysfs_writes_it() {
        assert_eq!(parse_cpulist("0-3\n"), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpulist("0,2,4\n"), vec![0, 2, 4]);
        assert_eq!(parse_cpulist("0-1,6,8-9\n"), vec![0, 1, 6, 8, 9]);
        assert!(parse_cpulist("\n").is_empty());
    }

    #[test]
    fn the_value_attribute_is_one_the_events_carry() {
        let collector = NumaCollector::new(Mode::Trace, None);
        let name = collector.value_attr();
        assert!(attributes(&row(), &two_nodes(), 1)
            .iter()
            .any(|attr| attr.name == name));
    }
}
