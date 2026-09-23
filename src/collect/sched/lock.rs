//! `sched.lock` over a pair of raw tracepoints, bucketed in the kernel.
//!
//! The measurement is the time a thread spent waiting for a *kernel* lock: a
//! mutex, a semaphore, a spinlock, a per-cpu rwsem. It is the one kind of
//! stall a cpu profile is least able to show -- a thread spinning on a
//! contended spinlock burns cycles and looks busy, and a thread sleeping on a
//! contended mutex burns none and looks absent.
//!
//! `lock:contention_begin` and `lock:contention_end` are the kernel's own
//! account of it, and they are why `DECISIONS.md` set the floor at 6.1: below
//! that there is no such account, and the alternative is a kprobe on every
//! lock primitive, covering a different set on every kernel.
//!
//! Each drained row is one log2 bucket of one `(task, lock kind, stack)`, with
//! how many waits fell into it and their exact total. That is the same shape
//! `sched.runqlat` produces and for the same reason: a contended lock fires
//! these tracepoints as fast as the machine can take them, and one event per
//! wait would be a measurement whose cost grows with the contention.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration as StdDuration;

use crate::bpf::{LockDrops, LockProgram, LockWait};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::emit::{STACK_ID_ATTR, WEIGHT_ATTR};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};
use crate::stack::{Frame, RawStack, Symbolizer};

/// How often the rows are drained.
///
/// One syscall per `(task, kind, stack, bucket)` row that saw a wait in the
/// interval, not per wait. Draining often also keeps the map far from the
/// capacity at which it would have to start refusing rows.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// The request's predicate, which this collector applies itself.
    ///
    /// The resolver credits any backend whose id begins with `bpf` with
    /// pushing a filter into the kernel, and then leaves its events alone.
    /// That credit is given from the backend's *name*, so a collector that
    /// does nothing with the predicate returns every event and nothing
    /// anywhere says the filter did not run -- which is how a run asking for
    /// one process's lock waits quietly reports the machine's. Applying it
    /// here is what makes the resolver's claim true of the numbers; what it is
    /// not yet true of is the cost, which `docs/open-findings.md` records.
    pub filter: Option<FilterExpr>,
    /// Symbolising a stack means reading a process's `/proc/<pid>/maps` and
    /// its binaries' symbol tables, which is the expensive half of this
    /// source. The frames are resolved only when the request asked for
    /// `stack`; the kernel walks them either way, because the walk happens in
    /// the program.
    pub wants_stacks: bool,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot measure lock contention: a wait is the gap between two \
                 events, and a sample that catches one of them without the other has no \
                 duration to report",
                self.mode
            ));
        }
        let filter = match target {
            Target::System => None,
            other => Some(other.first_pid().ok_or_else(|| {
                format!(
                    "a {} target names no process, and a lock wait is attributed per thread; \
                     use --system to watch the whole machine",
                    other.kind()
                )
            })? as u32),
        };
        Ok(Box::new(LockCollector::new(
            filter,
            self.wants_stacks,
            self.filter,
        )))
    }
}

pub struct LockCollector {
    id: CollectorId,
    /// The thread group whose waits are kept, or none for the machine.
    filter: Option<u32>,
    program: Option<LockProgram>,
    losses: Losses,
    notes: Vec<String>,
    /// Every stack the run resolved, by the id the kernel gave it.
    stacks: BTreeMap<u64, Vec<String>>,
    symbolizers: HashMap<i32, Symbolizer>,
    /// Whether the frames behind a stack id will be read by anyone.
    wants_stacks: bool,
    /// Waits whose stack the kernel could not walk, and waits whose id it had
    /// already evicted. Counted in waits rather than rows, because a row
    /// stands for as many waits as its count says.
    unwalkable: u64,
    evicted_stacks: u64,
    /// The programs' counters are cumulative, so what is new is the difference.
    dropped: LockDrops,
    /// Waits of the per-cpu idle task the program declined to pair. Not a
    /// loss -- see `LockDrops::skipped_idle` -- so it is reported as a
    /// degradation rather than counted against the run.
    skipped_idle: u64,
    /// The end of the last interval, which is the start of this one.
    window_opened_ns: u64,
    /// The request's predicate, applied to each row as it is drained.
    predicate: Option<FilterExpr>,
    /// Waits the predicate removed, and waits it kept. Counted in waits rather
    /// than in rows, because a row is not one measurement here.
    filtered_out: u64,
    filtered_in: u64,
}

impl LockCollector {
    pub fn new(filter: Option<u32>, wants_stacks: bool, predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("sched.lock/bpf:raw_tp"),
            filter,
            program: None,
            losses: Losses::default(),
            notes: Vec::new(),
            stacks: BTreeMap::new(),
            symbolizers: HashMap::new(),
            wants_stacks,
            unwalkable: 0,
            evicted_stacks: 0,
            dropped: LockDrops::default(),
            skipped_idle: 0,
            window_opened_ns: 0,
            predicate,
            filtered_out: 0,
            filtered_in: 0,
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
                // A drain that failed is a window whose rows are still in the
                // map: they are not lost, they arrive with the next one. What
                // is lost is knowing which window they belonged to.
                self.losses.failed_reads += 1;
                self.losses.note(reason);
                return;
            }
        };
        self.window_opened_ns = closed_ns;

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("a lock wait row was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the lock histogram held more rows than one drain can walk");
        }

        for (index, row) in rows.into_iter().enumerate() {
            let event = Event {
                // Stamped where the row's window began rather than at the
                // drain that read it, which is finding 44: a join that opens on
                // a bucketed source would otherwise open at the window's close.
                // A zero is the kernel having recorded no beginning, and falls
                // back to the drain rather than to the beginning of time.
                ts_ns: if row.first_ns == 0 {
                    closed_ns
                } else {
                    row.first_ns
                },
                ts_err_ns: 0,
                kind: "sched.lock".to_owned(),
                // The waiting thread is the series: this is its wait, not the
                // machine's, and not the time the lock was held by whoever had
                // it.
                keys: vec![KeyValue::task(row.tid as i32)],
                attrs: attributes(&row, window_ns),
                collector_id: self.id.clone(),
                seq: seq + index as u64,
            };
            // The predicate runs before the stack is symbolized: resolving the
            // frames of a row about to be thrown away costs a read of a
            // process's mappings for nothing.
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += row.count;
                    continue;
                }
                self.filtered_in += row.count;
            }
            let mut event = event;
            if let Some(id) = self.resolve_stack(&row) {
                event.attrs.push(Attr::uint(STACK_ID_ATTR, u64::from(id)));
            }
            events.push(event);
        }
    }

    /// Reads the target's mappings while it still has some, every tick.
    ///
    /// The same reason `sched.offcpu` does it: a symbolizer snapshots
    /// `/proc/<pid>/maps` when it is built, and for a launched program every
    /// single obvious moment is the wrong one -- at `start` the child has not
    /// execed, and by the time its rows are drained it has often exited.
    fn open_target_symbols(&mut self) {
        if !self.wants_stacks {
            return;
        }
        let Some(tgid) = self.filter else { return };
        match self.symbolizers.entry(tgid as i32) {
            std::collections::hash_map::Entry::Occupied(mut held) => {
                held.get_mut().refresh();
            }
            std::collections::hash_map::Entry::Vacant(empty) => {
                empty.insert(Symbolizer::for_process(tgid as i32));
            }
        }
    }

    /// Symbolizes the stack behind a row, once per distinct id.
    ///
    /// Returns the id to put on the event, or `None` when there is nothing to
    /// point at. Neither case is an empty stack, which would read as a thread
    /// that waited at no call site at all.
    fn resolve_stack(&mut self, row: &LockWait) -> Option<u32> {
        if !self.wants_stacks {
            return None;
        }
        let id = match row.stack {
            Some(id) => id,
            None => {
                self.unwalkable += row.count;
                return None;
            }
        };
        if self.stacks.contains_key(&u64::from(id)) {
            return Some(id);
        }
        let program = self.program.as_ref()?;
        let Some(addresses) = program.stack(id) else {
            self.evicted_stacks += row.count;
            return None;
        };
        let truncated = addresses.len() >= program.stack_depth();
        let symbolizer = self
            .symbolizers
            .entry(row.tgid as i32)
            .or_insert_with(|| Symbolizer::for_process(row.tgid as i32));
        // User frames only: `BPF_F_USER_STACK` asked for the thread's own, and
        // putting them through kernel symbols would name them after whatever
        // function lives at that offset in the kernel's tables.
        let stack = symbolizer.symbolize(&RawStack {
            kernel: Vec::new(),
            user: addresses,
            elsewhere: Vec::new(),
        });
        let mut frames: Vec<String> = stack.frames.iter().rev().map(Frame::label).collect();
        if truncated {
            // Root first, so the missing frames are the ones before the first
            // one here.
            frames.insert(0, "[truncated]".to_owned());
        }
        self.stacks.insert(u64::from(id), frames);
        Some(id)
    }

    /// Reads the programs' own counters of what they could not record.
    ///
    /// A full map is the only way this source loses a wait, and nothing else
    /// anywhere would record that it happened: the helper returns an error in
    /// the kernel and the wait is simply never timed.
    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let now = program.dropped();
        if now.skipped_idle > self.skipped_idle {
            self.skipped_idle = now.skipped_idle;
        }
        for (fresh, before, reason) in [
            (
                now.waiting_full,
                self.dropped.waiting_full,
                "more threads were waiting for a lock at once than the in-flight map holds, so \
                 their waits were never timed",
            ),
            (
                now.waiting_nested,
                self.dropped.waiting_nested,
                "a thread began waiting for a second lock without ending its first: an \
                 interrupt taking a lock while the thread already spins on one. The outer wait \
                 is kept and the inner one was never timed",
            ),
            (
                now.backwards,
                self.dropped.backwards,
                "a lock wait ended at a moment before it began, which is not a duration; it was \
                 refused rather than bucketed",
            ),
            (
                now.waiting_busy,
                self.dropped.waiting_busy,
                "the kernel declined to open a wait because the in-flight map's own bucket was \
                 already being updated on this cpu: a lock tracepoint fires inside the code a \
                 map update locks, and those waits were never timed",
            ),
            (
                now.rows_full,
                self.dropped.rows_full,
                "more distinct (task, lock kind, stack, bucket) rows appeared in one interval \
                 than the histogram holds, so their waits were never counted",
            ),
            (
                now.rows_busy,
                self.dropped.rows_busy,
                "the kernel declined to add a row because the histogram's own bucket was \
                 already being updated on this cpu, so those waits were never counted",
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
/// `comm` and `pid` are here because the catalog entry declares them as filter
/// fields: a predicate over a field no event carries is false everywhere,
/// which empties the series and looks exactly like a workload that never
/// contended anything.
fn attributes(row: &LockWait, window_ns: u64) -> Vec<Attr> {
    vec![
        // The value. The low bound of the bucket rather than its mean, because
        // it is the one number that puts the row back in the same bucket when
        // the emit layer buckets it again.
        Attr::uint("wait_ns", row.bucket_low_ns()),
        // How many waits this event stands for. Every number derived from
        // `wait_ns` is this many times over, which is what the emit layer reads
        // out of `weight`; `count` carries the same number under the name a
        // reader and the filter grammar expect.
        Attr::uint("count", row.count),
        Attr::uint(WEIGHT_ATTR, row.count),
        // The exact sum of those waits, which a log2 bucket cannot give back.
        Attr::uint("total_ns", row.total_ns),
        Attr::uint("bucket", u64::from(row.bucket)),
        // The interval the row was collected over. Not a denominator: a wait is
        // credited to the interval it ended in, so a total can exceed it.
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", u64::from(row.tid)),
        Attr::uint("pid", u64::from(row.tgid)),
        Attr::text("comm", row.comm.clone()),
        // What kind of lock it was, in words and in the kernel's own bits. A
        // spinlock wait and a mutex wait are different problems with different
        // fixes, and the set of bits is open -- a machine whose kernel added
        // one must still be able to show it.
        Attr::text("kind", row.kind()),
        Attr::uint("flags", u64::from(row.flags)),
    ]
}

impl Collector for LockCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "wait_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(LockProgram::load(self.filter)?);
        self.window_opened_ns = monotonic_ns();
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        self.open_target_symbols();
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_drops();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        self.absorb_drops();
        // A thread still waiting when the run ended has real waiting time that
        // is in no bucket, because the wait has no end yet. That is a fact
        // about the window rather than something lost.
        if let Some(program) = self.program.as_ref() {
            let open = program.still_waiting();
            if open > 0 {
                self.notes.push(format!(
                    "{open} thread{} still waiting for a lock when the run ended; that wait has \
                     no end yet and is in no bucket",
                    if open == 1 { " was" } else { "s were" }
                ));
            }
        }
        self.program = None;
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn stacks(&self) -> BTreeMap<u64, Vec<String>> {
        self.stacks.clone()
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} lock waits and removed {}; it is applied to drained rows \
                 rather than in the program, so the cost of the ones it removed was still paid",
                self.filtered_in, self.filtered_out
            ));
        }
        if self.skipped_idle > 0 {
            notes.push(format!(
                "{} lock waits of the per-cpu idle task were not measured: every cpu's idle task \
                 is numbered thread zero, so pairing their begins and ends by thread id would \
                 close one cpu's wait with another cpu's end and report the gap between two \
                 unrelated instants",
                self.skipped_idle
            ));
        }
        if self.unwalkable > 0 {
            notes.push(format!(
                "{} lock waits had no user stack to walk: a kernel thread has none, and a task \
                 built without frame pointers may have none the kernel can follow",
                self.unwalkable
            ));
        }
        if self.evicted_stacks > 0 {
            notes.push(format!(
                "{} lock waits named a stack the kernel had already evicted from its stack map, \
                 so they are counted and cannot be placed",
                self.evicted_stacks
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> LockWait {
        LockWait {
            tid: 11,
            tgid: 10,
            flags: 1 << 5,
            bucket: 12,
            stack: None,
            count: 7,
            total_ns: 30_000,
            first_ns: 0,
            comm: "contender".to_owned(),
        }
    }

    /// A wait is the gap between two events. A sampled mode catches one of
    /// them and has nothing to subtract from, and a counting mode would report
    /// how many waits there were under a unit of nanoseconds.
    #[test]
    fn every_mode_but_the_one_the_catalog_declares_is_refused_with_the_reason() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let entry = catalog
            .get(&"sched.lock".parse().expect("a source id"))
            .expect("sched.lock is in the catalog");
        assert_eq!(
            entry.modes.len(),
            1,
            "this test is written against the entry"
        );
        assert!(entry.modes.iter().any(|mode| *mode == Mode::Trace.kind()));

        let prepared = Box::new(Prepare {
            mode: Mode::Count,
            wants_stacks: false,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("a counting mode must be refused");
        };
        assert!(error.contains("cannot measure lock contention"), "{error}");
    }

    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            wants_stacks: false,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Iface("eth0".to_owned())) else {
            panic!("an interface names no thread that can wait for a lock");
        };
        assert!(
            error.contains("iface") && error.contains("--system"),
            "{error}"
        );
    }

    /// Every filter field the catalog offers must be on the events, or a
    /// predicate over it is false everywhere and silently empties the series.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&row(), 100_000_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"sched.lock".parse().expect("a source id"))
            .expect("sched.lock is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty(), "the entry declares filter fields");
        for field in declared {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// The value on the event is the bucket's low bound, and the weight is how
    /// many waits it stands for. A histogram that read the row as one
    /// measurement would report a lock contended seven thousand times and one
    /// contended seven as the same height.
    #[test]
    fn a_row_carries_its_own_weight_and_the_buckets_low_bound() {
        let attributes = attributes(&row(), 1);
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
        };
        assert_eq!(of("wait_ns"), Some(2048.0));
        assert_eq!(of(WEIGHT_ATTR), Some(7.0));
        assert_eq!(of("count"), of(WEIGHT_ATTR));
        // And the exact total is not the bucket bound times the count: the
        // bucket is a factor of two wide and this number is not.
        assert_eq!(of("total_ns"), Some(30_000.0));
    }

    /// The value attribute has to be one the events actually carry, or every
    /// emit reads zero from a run that measured something.
    #[test]
    fn the_value_attribute_is_one_the_events_carry() {
        let collector = LockCollector::new(None, false, None);
        let name = collector.value_attr();
        assert!(attributes(&row(), 1).iter().any(|attr| attr.name == name));
    }
}
