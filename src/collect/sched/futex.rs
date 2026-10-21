//! `sched.futex` over `sys_enter` and `sys_exit`, bucketed in the kernel.
//!
//! The measurement is the time a thread spent asleep inside a userspace lock.
//! Every mutex, condition variable, channel and monitor in userspace is a
//! futex underneath: the fast path is an atomic in the program's own memory
//! and never reaches the kernel, and the slow path -- the one that happens
//! when somebody else holds the lock -- is a `futex()` call that does not
//! return until they let go. That call is what this source times.
//!
//! It is the companion of `sched.lock`, which is the same question one layer
//! down: `sched.lock` is the kernel's own locks, this is the program's.
//! Between them they cover why a thread that is not running is not running,
//! with `sched.offcpu` saying how long and `sched.runqlat` saying how long
//! after that it waited for a cpu.
//!
//! **This source has no `--system` target and the catalog says so.** Both
//! tracepoints fire on every syscall of every process on the machine, and a
//! machine-wide futex histogram would be dominated by whatever thread pool
//! happens to be idle -- an idle pool waits forever, by design, and it is not
//! a problem. The narrowing is in the program, where the catalog's `bpf`
//! backend promises the filtering happens.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration as StdDuration;

use crate::bpf::{FutexDrops, FutexProgram, FutexWait, Whose};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::emit::{STACK_ID_ATTR, WEIGHT_ATTR};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};
use crate::stack::{Frame, RawStack, Symbolizer};

/// How often the rows are drained.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// Whether the frames behind a stack id will be read by anyone; see
    /// `lock::Prepare`.
    pub wants_stacks: bool,
    /// The request's predicate, applied here because the resolver credits a
    /// `bpf` backend with pushing it down; see `lock::Prepare`.
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot measure futex waits: a wait is the gap between a syscall and \
                 its return, and a sample that catches one of them without the other has no \
                 duration to report",
                self.mode
            ));
        }
        // A thread and a process are different filters here, and the catalog
        // offers both: a `--tid` run that compared thread groups would report
        // every thread of the process instead of the one that was named.
        let whose = match target {
            Target::System => {
                return Err("a system target names every thread on the machine, and this \
                            source has no machine-wide form: `sys_enter` fires on every syscall on the \
                            box, and a machine's futex histogram is mostly idle thread pools \
                            waiting on purpose; name a process with --pid, a thread with \
                            --tid, or a cgroup with --cgroup"
                    .to_owned())
            }
            other => super::whose(other, "a futex wait belongs to a thread of one")?,
        };
        Ok(Box::new(FutexCollector::new(
            whose,
            self.wants_stacks,
            self.filter,
        )))
    }
}

pub struct FutexCollector {
    whose: Whose,
    program: Option<FutexProgram>,
    /// Each process's parent, for the rows' `ppid`.
    parents: super::Parents,
    losses: Losses,
    /// The kernel's skipped runs of the program, cumulative, as last read.
    skipped: u64,
    notes: Vec<String>,
    stacks: BTreeMap<u64, Vec<String>>,
    symbolizers: HashMap<i32, Symbolizer>,
    /// The callers frameless leaves left in the user link register.
    leaf: super::LeafCallers,
    /// Processes whose symbolizer outlived their place in the followed set
    /// at the last tick, dropped at the next one; see `retire_symbolizers`.
    leaving: Vec<i32>,
    wants_stacks: bool,
    unwalkable: u64,
    evicted_stacks: u64,
    dropped: FutexDrops,
    window_opened_ns: u64,
    predicate: Option<FilterExpr>,
    filtered_out: u64,
    filtered_in: u64,
}

impl FutexCollector {
    pub fn new(whose: Whose, wants_stacks: bool, predicate: Option<FilterExpr>) -> Self {
        Self {
            whose,
            program: None,
            parents: super::Parents::default(),
            losses: Losses::default(),
            skipped: 0,
            notes: Vec::new(),
            stacks: BTreeMap::new(),
            symbolizers: HashMap::new(),
            leaf: super::LeafCallers::default(),
            leaving: Vec::new(),
            wants_stacks,
            unwalkable: 0,
            evicted_stacks: 0,
            dropped: FutexDrops::default(),
            window_opened_ns: 0,
            predicate,
            filtered_out: 0,
            filtered_in: 0,
        }
    }

    /// The process whose mappings a stack is symbolized against, when the
    /// target names one; a cgroup's processes are found as their rows arrive.
    fn watched_process(&self) -> Option<u32> {
        match self.whose {
            Whose::Process(tgid) => Some(tgid),
            // A thread's own id is not its process's, but a thread's stack is
            // in its process's mappings; the row carries the thread group and
            // that is what `resolve_stack` uses. This is only the value read
            // eagerly each tick, and for a thread target the process is the
            // one that will appear on the rows anyway.
            Whose::Thread(tid) => Some(tid),
            Whose::Any | Whose::Cgroup(_) => None,
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
                self.losses.note_times(reason, 1);
                return;
            }
        };
        self.window_opened_ns = closed_ns;
        self.parents.absorb(program.parents());

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("a futex wait row was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the futex histogram held more rows than one drain can walk");
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
                kind: "sched.futex".to_owned(),
                // The waiting thread is the series. The futex address is an
                // attribute rather than a key: the model has no key for an
                // address in a process, and `mem` is an interval rather than a
                // point.
                keys: vec![KeyValue::task(row.tid as i32)],
                attrs: attributes(&row, window_ns),
                collector_id: CollectorId::unstamped(),
                seq: seq + index as u64,
            };
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += row.count;
                    continue;
                }
                self.filtered_in += row.count;
            }
            let mut event = event;
            if let Some(ppid) = self.parents.of(row.tgid) {
                event.attrs.push(Attr::uint("ppid", u64::from(ppid)));
            }
            if let Some(id) = self.resolve_stack(&row) {
                event.attrs.push(Attr::uint(STACK_ID_ATTR, id));
            }
            events.push(event);
        }
    }

    /// Reads the target's mappings while it still has some, every tick; see
    /// `lock::LockCollector::open_target_symbols` for why every other moment
    /// is the wrong one.
    fn open_target_symbols(&mut self) {
        if !self.wants_stacks {
            return;
        }
        let Some(watched) = self.watched_process() else {
            return;
        };
        // A process target is followed into the processes it starts, and each
        // of those is read too.
        let mut followed = vec![watched];
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

    fn resolve_stack(&mut self, row: &FutexWait) -> Option<u64> {
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
            frames.insert(0, "[truncated]".to_owned());
        }
        self.stacks.insert(resolved, frames);
        Some(resolved)
    }

    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        // The kernel's own count of runs it skipped, which no counter of the
        // program's can hold. It is the machine's -- the program runs on every
        // occurrence and narrows to the target after -- so it is an upper
        // bound on what the target lost, and the reason says so.
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note_times(
                "the kernel skipped a run of this program because one was already running on \
                 that cpu, so an occurrence was never seen; the count is the machine's, an \
                 upper bound on the target's share, and every total here is a lower bound",
                skipped - self.skipped,
            );
            self.skipped = skipped;
        }
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
        for (fresh, before, reason) in [
            (
                now.waiting_full,
                self.dropped.waiting_full,
                "more threads were inside a futex wait at once than the in-flight map holds, so \
                 their waits were never timed",
            ),
            (
                now.waiting_nested,
                self.dropped.waiting_nested,
                "a thread entered a futex wait while this run believed it was already in one, \
                 which it cannot be; the newer wait was not timed",
            ),
            (
                now.waiting_busy,
                self.dropped.waiting_busy,
                "the kernel declined to open a wait because the in-flight map's own bucket was \
                 already being updated on this cpu, so those waits were never timed",
            ),
            (
                now.backwards,
                self.dropped.backwards,
                "a futex wait returned at a moment before it was entered, which is not a \
                 duration; it was refused rather than bucketed",
            ),
            (
                now.rows_full,
                self.dropped.rows_full,
                "more distinct (address, task, command, stack, bucket) rows appeared in one \
                 interval than the histogram holds, so their waits were never counted",
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
/// `comm`, `pid` and `addr` are here because the catalog entry declares them
/// as filter fields: a predicate over a field no event carries is false
/// everywhere, which empties the series and looks exactly like a program whose
/// threads never contended anything.
fn attributes(row: &FutexWait, window_ns: u64) -> Vec<Attr> {
    vec![
        Attr::uint("wait_ns", row.bucket_low_ns()),
        Attr::uint("count", row.count),
        Attr::uint(WEIGHT_ATTR, row.count),
        Attr::uint("total_ns", row.total_ns),
        Attr::uint("bucket", u64::from(row.bucket)),
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", u64::from(row.tid)),
        Attr::uint("pid", u64::from(row.tgid)),
        Attr::text("comm", row.comm.clone()),
        // The futex word's address in the waiting process. This is the field
        // that makes contention visible as contention: two threads waiting on
        // one address are fighting over a lock, and two threads waiting on two
        // addresses are two idle threads.
        Attr::uint("addr", row.addr),
        // Which futex operation it was. `wait` and `wait_bitset` are an
        // ordinary mutex and a condition variable; the `lock_pi` family is
        // priority inheritance.
        Attr::text("op", row.command_name()),
    ]
}

impl Collector for FutexCollector {
    fn value_attr(&self) -> &'static str {
        "wait_ns"
    }

    fn row_stamp(&self) -> Option<crate::collect::RowStamp> {
        use crate::collect::RowStamp;
        Some(RowStamp::FirstEvent)
    }

    fn start(&mut self) -> Result<(), String> {
        let program = FutexProgram::load(self.whose)?;
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
        if let Some(program) = self.program.as_ref() {
            let open = program.still_waiting();
            if open > 0 {
                self.notes.push(format!(
                    "{open} thread{} inside a futex wait when the run ended; that wait has no \
                     end yet and is in no bucket. For a thread pool this is the ordinary state \
                     rather than a problem: an idle worker waits on purpose",
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
                "the filter kept {} futex waits and removed {}; it is applied to drained rows \
                 rather than in the program, so the cost of the ones it removed was still paid",
                self.filtered_in, self.filtered_out
            ));
        }
        if self.unwalkable > 0 {
            notes.push(format!(
                "{} futex waits had no user stack to walk, which is what a program built without \
                 frame pointers looks like from here",
                self.unwalkable
            ));
        }
        if self.evicted_stacks > 0 {
            notes.push(format!(
                "{} futex waits named a stack the kernel had already evicted from its stack map, \
                 so they are counted and cannot be placed",
                self.evicted_stacks
            ));
        }
        notes.extend(self.leaf.note("futex waits"));
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> FutexWait {
        FutexWait {
            addr: 0x7fff_0000_1000,
            tid: 12,
            tgid: 10,
            command: 9,
            bucket: 20,
            stack: None,
            lr: 0,
            count: 4,
            total_ns: 3_000_000,
            first_ns: 0,
            comm: "worker".to_owned(),
        }
    }

    fn prepare(mode: Mode) -> Box<Prepare> {
        Box::new(Prepare {
            mode,
            wants_stacks: false,
            filter: None,
        })
    }

    #[test]
    fn every_mode_but_the_one_the_catalog_declares_is_refused_with_the_reason() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let entry = catalog
            .get(&"sched.futex".parse().expect("a source id"))
            .expect("sched.futex is in the catalog");
        assert_eq!(
            entry.modes.len(),
            1,
            "this test is written against the entry"
        );
        let Err(error) = prepare(Mode::Count).attach(&Target::Pid(vec![1])) else {
            panic!("a counting mode must be refused");
        };
        assert!(error.contains("cannot measure futex waits"), "{error}");
    }

    /// The catalog offers this source a thread and a process, and they mean
    /// different things. Collapsing them would answer a `--tid` run with every
    /// thread of the process.
    #[test]
    fn a_thread_target_is_not_turned_into_its_process() {
        let _by_thread = prepare(Mode::Trace)
            // This thread: a tid nobody has is refused inside a container,
            // where it is translated to the kernel's.
            .attach(&Target::Tid(vec![unsafe { libc::gettid() }]))
            .expect("a tid target is declared");
        // The shape is checked through the program's own tests; here the point
        // is only that both targets are accepted and neither is refused.
        assert!(prepare(Mode::Trace)
            .attach(&Target::Pid(vec![std::process::id() as i32]))
            .is_ok());
    }

    /// This source has no machine-wide form, and the refusal has to say why
    /// rather than leaving a reader to wonder whether it is a missing feature.
    #[test]
    fn a_system_target_is_refused_with_the_reason_it_does_not_exist() {
        let Err(error) = prepare(Mode::Trace).attach(&Target::System) else {
            panic!("`sched.futex` declares no system target");
        };
        assert!(error.contains("system"), "{error}");
        assert!(error.contains("every syscall"), "{error}");
    }

    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&row(), 100_000_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"sched.futex".parse().expect("a source id"))
            .expect("sched.futex is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty());
        // `ppid` is put on the event by the collector, from the process's
        // parent (`super::Parents`), not by the row's attributes.
        for field in declared.iter().filter(|field| *field != "ppid") {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    #[test]
    fn a_row_carries_its_own_weight_and_the_buckets_low_bound() {
        let attributes = attributes(&row(), 1);
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
        };
        assert_eq!(of("wait_ns"), Some(524_288.0));
        assert_eq!(of(WEIGHT_ATTR), Some(4.0));
        assert_eq!(of("count"), of(WEIGHT_ATTR));
        assert_eq!(of("total_ns"), Some(3_000_000.0));
        assert_eq!(of("addr"), Some(0x7fff_0000_1000u64 as f64));
    }

    #[test]
    fn the_value_attribute_is_one_the_events_carry() {
        let collector = FutexCollector::new(Whose::Process(1), false, None);
        let name = collector.value_attr();
        assert!(attributes(&row(), 1).iter().any(|attr| attr.name == name));
    }
}
