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

use crate::bpf::{LockDrops, LockProgram, LockWait, Whose};
use crate::clock::monotonic_ns;
use crate::collect::kernel::{KernelCounts, Lost, UserFilter};
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
        let filter = super::whose(target, "a lock wait is attributed per thread")?;
        Ok(Box::new(LockCollector::new(
            filter,
            self.wants_stacks,
            self.filter,
        )))
    }
}

pub struct LockCollector {
    /// Whose waits are kept.
    filter: Whose,
    program: Option<LockProgram>,
    losses: Losses,
    /// The program\'s loss counters, as last read (`collect::kernel`).
    counts: KernelCounts,
    notes: Vec<String>,
    /// Every stack the run resolved, by the id the kernel gave it.
    stacks: BTreeMap<u64, Vec<String>>,
    symbolizers: HashMap<i32, Symbolizer>,
    /// Kernel symbols, for the caller of each wait's lock primitive, and the
    /// caller found for each kernel stack id.
    kernel_symbols: Option<Symbolizer>,
    callers: HashMap<u32, Option<String>>,
    /// `/proc/kallsyms` gave this process no addresses, so no caller was named.
    callers_unnamed: bool,
    /// The callers frameless leaves left in the user link register.
    leaf: super::LeafCallers,
    /// Processes whose symbolizer outlived their place in the followed set
    /// at the last tick, dropped at the next one; see `retire_symbolizers`.
    leaving: Vec<i32>,
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
    predicate: UserFilter,
    /// Each waiting process's parent, for `ppid`.
    parents: super::Parents,
}

impl LockCollector {
    pub fn new(filter: Whose, wants_stacks: bool, predicate: Option<FilterExpr>) -> Self {
        Self {
            filter,
            program: None,
            losses: Losses::default(),
            counts: KernelCounts::default(),
            notes: Vec::new(),
            stacks: BTreeMap::new(),
            symbolizers: HashMap::new(),
            kernel_symbols: None,
            callers: HashMap::new(),
            callers_unnamed: false,
            leaf: super::LeafCallers::default(),
            leaving: Vec::new(),
            wants_stacks,
            unwalkable: 0,
            evicted_stacks: 0,
            dropped: LockDrops::default(),
            skipped_idle: 0,
            window_opened_ns: 0,
            predicate: UserFilter::new(predicate, "lock waits"),
            parents: super::Parents::default(),
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
                self.losses.note_times(reason, 1);
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
            let mut event = Event {
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
                collector_id: CollectorId::unstamped(),
                seq: seq + index as u64,
            };
            if let Some(ppid) = self.parents.of(row.tgid) {
                event.attrs.push(Attr::uint("ppid", u64::from(ppid)));
            }
            if let Some(caller) = self.caller(&row) {
                event.attrs.push(Attr::text("caller", caller));
            }
            // The predicate runs before the stack is symbolized: resolving the
            // frames of a row about to be thrown away costs a read of a
            // process's mappings for nothing.
            // `kind` is the lock's, which `topk:N@kind` ranks by, as
            // `perf lock contention` groups by type; see `super::matches_as`.
            let kept = self
                .predicate
                .expression()
                .is_none_or(|predicate| super::matches_as(predicate, &event, &row.kind()));
            self.predicate
                .tally(u64::from(kept) * row.count, u64::from(!kept) * row.count);
            if !kept {
                continue;
            }
            if let Some(id) = self.resolve_stack(&row) {
                event.attrs.push(Attr::uint(STACK_ID_ATTR, id));
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
        // A thread's `/proc/<tid>/maps` is its process's, so a thread target
        // is symbolised through its own id. A process target is followed into
        // the processes it starts, and each of those is read too.
        let mut followed = match self.filter {
            Whose::Process(id) | Whose::Thread(id) => vec![id],
            Whose::Any | Whose::Cgroup(_) => return,
        };
        if let Some(program) = self.program.as_ref() {
            followed.extend(program.processes());
        }
        followed.sort_unstable();
        followed.dedup();
        super::refresh_followed_symbols(&mut self.symbolizers, &mut self.leaving, &followed);
        for &tgid in &followed {
            self.leaf.refresh(tgid);
        }
        let symbolizers = &self.symbolizers;
        self.leaf.retain(|tgid| symbolizers.contains_key(&tgid));
    }

    /// The function that called the lock primitive: the first frame of the
    /// wait's kernel stack that is neither the tracing machinery nor a lock
    /// or scheduler function -- what `perf lock contention` prints as the
    /// caller. Once per kernel stack id; `None` where the kernel walked no
    /// stack or evicted it.
    fn caller(&mut self, row: &LockWait) -> Option<String> {
        let id = row.kernel_stack?;
        if let Some(known) = self.callers.get(&id) {
            return known.clone();
        }
        let addresses = self.program.as_ref()?.kernel_stack(id)?;
        let symbols = self
            .kernel_symbols
            .get_or_insert_with(|| Symbolizer::for_process(std::process::id() as i32));
        // Without the kernel's names the tracing and the lock's own frames
        // cannot be told from the caller, and the first address is this
        // tool's own program: no caller rather than a wrong one.
        if !symbols.kernel_symbols_available() {
            self.callers_unnamed = true;
            return None;
        }
        let stack = symbols.symbolize(&RawStack {
            kernel: addresses,
            user: Vec::new(),
            elsewhere: Vec::new(),
        });
        let caller = stack
            .frames
            .iter()
            .map(Frame::label)
            .find(|label| !is_lock_machinery(label));
        self.callers.insert(id, caller.clone());
        caller
    }

    /// Symbolizes the stack behind a row, once per distinct id.
    ///
    /// Returns the id to put on the event, or `None` when there is nothing to
    /// point at. Neither case is an empty stack, which would read as a thread
    /// that waited at no call site at all.
    fn resolve_stack(&mut self, row: &LockWait) -> Option<u64> {
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
        if let Some(resolved) = self.leaf.cached(id, row.lr, row.count) {
            return Some(resolved);
        }
        let program = self.program.as_ref()?;
        let Some(mut addresses) = program.stack(id) else {
            self.evicted_stacks += row.count;
            return None;
        };
        let truncated = addresses.len() >= program.stack_depth();
        let (resolved, recovered) =
            self.leaf
                .place(row.tgid, id, row.lr, row.count, &mut addresses);
        if self.stacks.contains_key(&resolved) {
            return Some(resolved);
        }
        let symbolizer = self
            .symbolizers
            .entry(row.tgid as i32)
            .or_insert_with(|| Symbolizer::for_kernel_process(row.tgid as i32));
        // User frames only: `BPF_F_USER_STACK` asked for the thread's own, and
        // putting them through kernel symbols would name them after whatever
        // function lives at that offset in the kernel's tables.
        let mut stack = symbolizer.symbolize(&RawStack {
            kernel: Vec::new(),
            user: addresses,
            elsewhere: Vec::new(),
        });
        if recovered {
            stack.frames[1].recovered = true;
        }
        let mut frames: Vec<String> = stack.frames.iter().rev().map(Frame::label).collect();
        if truncated {
            // Root first, so the missing frames are the ones before the first
            // one here.
            frames.insert(0, "[truncated]".to_owned());
        }
        self.stacks.insert(resolved, frames);
        Some(resolved)
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
        // The kernel's own count of runs it skipped, which no counter of the
        // program's can hold. It is the machine's -- the program runs on every
        // occurrence and narrows to the target after -- so it is an upper
        // bound on what the target lost, and the reason says so.
        self.counts
            .absorb(&mut self.losses, [Lost::skipped_runs(program.skipped())]);
        let unfollowed = program.unfollowed();
        if unfollowed > 0 {
            self.losses.note(format!(
                "{unfollowed} process{} the target started {} not followed: the map of the \
                 processes it started was full, so {} waits are in no row",
                if unfollowed == 1 { "" } else { "es" },
                if unfollowed == 1 { "was" } else { "were" },
                if unfollowed == 1 { "its" } else { "their" },
            ));
        }
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
                self.losses.note_times(reason, fresh - before);
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
/// Whether a kernel frame is the lock's own machinery rather than its caller:
/// the tracing that recorded the wait, the lock primitives, and the scheduler
/// a sleeping lock blocks in.
fn is_lock_machinery(label: &str) -> bool {
    const PREFIXES: [&str; 22] = [
        "bpf_",
        "__bpf",
        "__traceiter",
        "trace_",
        "__mutex_lock",
        "mutex_lock",
        "__rwsem",
        "rwsem_down",
        "down_read",
        "down_write",
        "_raw_spin",
        "_raw_read",
        "_raw_write",
        "queued_spin",
        "queued_read",
        "queued_write",
        "native_queued",
        "__pv_queued",
        "rt_mutex",
        "rt_spin",
        "osq_lock",
        "percpu_down",
    ];
    let name = label.trim_start_matches('[');
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || name.starts_with("schedule")
        || name.starts_with("__schedule")
        || name.starts_with("preempt_schedule")
        || name.starts_with("io_schedule")
        || name.starts_with("__percpu_down")
        || name.contains("contention")
}

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
        // The lock's address, as `perf lock contention -l` prints it: valid
        // while the lock exists, so an attribute and never a key.
        Attr::text("addr", format!("{:#x}", row.addr)),
    ]
}

impl Collector for LockCollector {
    fn value_attr(&self) -> &'static str {
        "wait_ns"
    }

    fn row_stamp(&self) -> Option<crate::collect::RowStamp> {
        use crate::collect::RowStamp;
        Some(RowStamp::FirstEvent)
    }

    fn start(&mut self) -> Result<(), String> {
        let program = LockProgram::load(self.filter)?;
        self.notes
            .extend(program.leaf_lr_missing().map(str::to_owned));
        self.leaf.enable(program.leaf_lr_missing().is_none());
        self.program = Some(program);
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
        if self.callers_unnamed {
            notes.push(
                "/proc/kallsyms gives no addresses to this process (kptr_restrict, or \
                 perf_event_paranoid above 1 without CAP_SYSLOG), so no wait carries a `caller`: \
                 the lock's own frames cannot be told from the function that took it"
                    .to_owned(),
            );
        }
        notes.extend(self.predicate.note());
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
        notes.extend(self.leaf.note("lock waits"));
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mutex wait's kernel stack, leaf first, as `contention_end` sees it:
    /// the caller is the first frame past the tracing and the lock.
    #[test]
    fn the_caller_is_the_first_frame_past_the_lock_and_the_tracing() {
        let stack = [
            "bpf_prog_omn_lock_end",
            "__bpf_trace_contention_end",
            "__traceiter_contention_end",
            "__mutex_lock.constprop.0",
            "__mutex_lock_slowpath",
            "mutex_lock",
            "pipe_write",
            "vfs_write",
        ];
        assert_eq!(
            stack.iter().find(|label| !is_lock_machinery(label)),
            Some(&"pipe_write")
        );
        assert!(is_lock_machinery("rwsem_down_write_slowpath"));
        assert!(is_lock_machinery("_raw_spin_lock_irqsave"));
        assert!(!is_lock_machinery("ext4_buffered_write_iter"));
    }

    fn row() -> LockWait {
        LockWait {
            tid: 11,
            tgid: 10,
            flags: 1 << 5,
            bucket: 12,
            stack: None,
            kernel_stack: None,
            addr: 0xffff_8881_0000_1000,
            lr: 0,
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
        // `ppid` and `caller` are put on the event by the collector, from the
        // process's parent and the kernel stack, not by the row's attributes.
        for field in declared
            .iter()
            .filter(|field| *field != "ppid" && *field != "caller")
        {
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
        let collector = LockCollector::new(Whose::Any, false, None);
        let name = collector.value_attr();
        assert!(attributes(&row(), 1).iter().any(|attr| attr.name == name));
    }
}
