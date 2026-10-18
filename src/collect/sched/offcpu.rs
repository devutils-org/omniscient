//! `sched.offcpu` over a raw tracepoint, counted in the kernel.
//!
//! The measurement is the time a task spent blocked: it left the cpu because
//! it was waiting for something, and this is how long until it came back. That
//! is the number no cpu profiler can produce, because a blocked thread burns
//! no cycles and appears nowhere in a profile of the ones that do.
//!
//! What makes this source different from every other one in the family is
//! where the arithmetic happens. `sched_switch` fires a few million times a
//! minute on a busy machine, and one event per switch is a measurement whose
//! cost grows with the workload. The program in `src/bpf/offcpu.rs` keeps a
//! hash map keyed by `(task, state)` instead, and this file drains it: a few
//! hundred entries per interval rather than a few hundred thousand events.
//!
//! Each event is therefore a total over the interval between two drains, not
//! one block. The interval is on the event as `window_ns`, because a number
//! that covers a window and does not say which window is not a measurement.
//!
//! A block is credited in full to the interval in which it *ended*, since that
//! is when its duration becomes known. `total_ns` can therefore exceed the
//! `window_ns` beside it -- a 400 ms wait drained on a 100 ms tick reports
//! 400 ms against a 100 ms window -- and the two are a total and the interval
//! it was collected over, never a share of it. Splitting a block across the
//! intervals it spans would need the kernel to walk its in-flight map on every
//! drain, which is the cost this whole shape exists to avoid.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use crate::bpf::{OffCpu, OffCpuProgram, Renames, Whose};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::emit::STACK_ID_ATTR;
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};
use crate::stack::unwind::{Debuginfod, NameOrigin};
use crate::stack::{Frame, NamedBy, PerfMapFile, RawStack, Symbolizer};

/// How often the totals are drained.
///
/// The cost of a drain is one syscall per `(task, state)` pair that blocked in
/// the interval, not per switch, which is the whole point of counting in the
/// kernel: a workload switching two million times still drains a few hundred
/// entries. Draining often also keeps the map far from the capacity at which
/// it would have to start refusing entries.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// Whether anything downstream will read the frames.
    ///
    /// Symbolising a stack means reading a process's `/proc/<pid>/maps` and
    /// then its binaries' symbol tables, and a machine-wide run meets every
    /// process on the machine. The emit layer keeps the resolved table only
    /// when the request asked for `stack`, and throws it away otherwise -- so
    /// without this the work is not merely expensive, it is expensive and
    /// discarded. Measured on this machine: `sched.offcpu --system --duration
    /// 1s --emit summary` did not finish in ninety seconds, and finishes in
    /// 1.2 with this.
    pub wants_stacks: bool,
    /// The request's predicate, applied here per drained row: the resolver
    /// credits a `bpf` backend with filtering in the kernel and then leaves its
    /// events alone, so a predicate this collector ignored would run nowhere
    /// (stream S's finding, `docs/fixes/silent.md`).
    pub filter: Option<FilterExpr>,
    /// Whether `--symbols debuginfod` asked for a stripped file's names to be
    /// fetched by build id. The kernel walks these stacks by frame pointer, so
    /// where the names come from is the only part of the unwind there is to
    /// choose (`stack::NAMED_BY_CHOICE`).
    pub debuginfod: bool,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot measure blocked time: a block has a beginning and an end, and \
                 a sample that catches one without the other has no duration to report",
                self.mode
            ));
        }
        let filter = super::whose(target, "blocked time is attributed per task")?;
        let mut collector = OffCpuCollector::new(filter, self.wants_stacks);
        collector.predicate = self.filter;
        // Refused here, before the target has run an instruction, rather than
        // discovered at the first stack: the same refusal `cpu.profile` gives.
        if self.debuginfod {
            collector.debuginfod = Some(Arc::new(Debuginfod::from_env()?));
        }
        Ok(Box::new(collector))
    }
}

pub struct OffCpuCollector {
    id: CollectorId,
    /// Whose blocks are kept.
    filter: Whose,
    program: Option<OffCpuProgram>,
    /// Each process's parent, for the rows' `ppid`.
    parents: super::Parents,
    losses: Losses,
    /// The kernel's skipped runs of the program, cumulative, as last read.
    skipped: u64,
    notes: Vec<String>,
    /// The names tasks took at exec, applied to the rows of the window.
    renames: Option<Renames>,
    /// The counters are cumulative, so what is new is the difference.
    dropped_in_flight: u64,
    dropped_totals: u64,
    /// The end of the last interval, which is the start of this one.
    window_opened_ns: u64,
    /// Every stack the run resolved, by the id the kernel gave it.
    ///
    /// Resolved as the drains come in rather than at the end, because a
    /// symbolizer reads the target's `/proc/<pid>/maps` and those are gone the
    /// moment it exits -- and a program that blocks is often a program that
    /// then finishes.
    stacks: BTreeMap<u64, Vec<String>>,
    /// Who named each frame of `stacks`, root first, under the same ids, so
    /// the flame and the triage charge off-cpu stacks as they charge
    /// `cpu.profile`'s.
    named_by: BTreeMap<u64, Vec<Option<NamedBy>>>,
    /// Each frame's object, under the same ids, for `--keys object`.
    objects: BTreeMap<u64, Vec<Option<String>>>,
    /// The process each stack's task belonged to, by the `task:<tid>` key a
    /// result writes, for the file each executed.
    task_processes: BTreeMap<String, i32>,
    /// What the retired symbolizers knew of their perf maps and executables.
    left_perf_maps: Vec<PerfMapFile>,
    left_executables: BTreeMap<i32, String>,
    /// One symbolizer per process seen, built lazily. A system-wide run sees
    /// many, and each carries that process's own mappings.
    symbolizers: HashMap<i32, Symbolizer>,
    /// The callers frameless leaves left in the user link register.
    leaf: super::LeafCallers,
    /// Processes the target started that were no longer followed at the last
    /// tick: their symbolizers are dropped at this one, after the drain that
    /// held their last blocks has named them.
    leaving: BTreeSet<i32>,
    /// Where the names of the dropped symbolizers' modules came from, kept for
    /// the facts line.
    left_origins: BTreeMap<PathBuf, NameOrigin>,
    /// Blocks whose stack the kernel could not walk, and blocks whose id it had
    /// evicted by the time it was asked. Different failures, counted apart.
    unwalkable: u64,
    evicted_stacks: u64,
    /// Blocks whose stack the kernel walked and had no room to keep: the
    /// bucket it hashed to already held another stack.
    crowded_out: u64,
    /// Blocks whose releasing wakeup this run did not see, so their
    /// `wake_to_run_ns` is unknown rather than zero.
    blocks_without_a_wakeup: u64,
    /// Blocks the kernel held no queue stamp for.
    unstamped: u64,
    /// Where a stripped file's names are fetched from, when asked.
    debuginfod: Option<Arc<Debuginfod>>,
    /// Whether the frames behind a stack id will be read by anyone.
    wants_stacks: bool,
    /// The request's predicate, and the blocks it kept and removed.
    predicate: Option<FilterExpr>,
    filtered_in: u64,
    filtered_out: u64,
}

impl OffCpuCollector {
    pub fn new(filter: Whose, wants_stacks: bool) -> Self {
        Self {
            id: CollectorId::new("sched.offcpu/bpf:raw_tp"),
            filter,
            program: None,
            parents: super::Parents::default(),
            losses: Losses::default(),
            skipped: 0,
            notes: Vec::new(),
            renames: None,
            dropped_in_flight: 0,
            dropped_totals: 0,
            window_opened_ns: 0,
            stacks: BTreeMap::new(),
            symbolizers: HashMap::new(),
            leaf: super::LeafCallers::default(),
            leaving: BTreeSet::new(),
            named_by: BTreeMap::new(),
            objects: BTreeMap::new(),
            task_processes: BTreeMap::new(),
            left_perf_maps: Vec::new(),
            left_executables: BTreeMap::new(),
            left_origins: BTreeMap::new(),
            unwalkable: 0,
            evicted_stacks: 0,
            crowded_out: 0,
            blocks_without_a_wakeup: 0,
            unstamped: 0,
            debuginfod: None,
            wants_stacks,
            predicate: None,
            filtered_in: 0,
            filtered_out: 0,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.window_opened_ns);
        let (mut entries, drain_losses) = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                // A drain that failed is a window whose totals are still in
                // the map: they are not lost, they arrive with the next one.
                // What is lost is knowing which window they belonged to.
                self.losses.failed_reads += 1;
                self.losses.note(reason);
                return;
            }
        };
        self.window_opened_ns = closed_ns;
        self.parents.absorb(program.parents());
        // A task that exec'd in this window is named what it became, not what
        // it was when its row was made; see `bpf::Renames`.
        if let Some(renames) = self.renames.as_mut() {
            let renamed = renames.take();
            for row in entries.iter_mut() {
                if let Some(name) = renamed.get(&row.tid) {
                    row.comm.clone_from(name);
                }
            }
        }

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("an off-cpu total was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the off-cpu map held more entries than one drain can walk");
        }

        // A process the target started may have exec'd since its symbolizer
        // was built, and may exit before the next tick: its maps are read
        // again now, once per drain, for every process a new stack in this
        // drain belongs to.
        if self.wants_stacks {
            let unresolved: BTreeSet<u32> = entries
                .iter()
                .filter(|entry| {
                    entry
                        .stack
                        .is_some_and(|id| !self.leaf.placed(id, entry.lr))
                })
                .map(|entry| entry.tgid)
                .collect();
            for tgid in unresolved {
                self.symbolizer(tgid).refresh();
                self.leaf.refresh(tgid);
            }
        }

        for (index, entry) in entries.into_iter().enumerate() {
            self.blocks_without_a_wakeup += entry.unwoken;
            self.unstamped += entry.unstamped;
            let mut attrs = attributes(&entry, window_ns);
            if let Some(ppid) = self.parents.of(entry.tgid) {
                attrs.push(Attr::uint("ppid", u64::from(ppid)));
            }
            if let Some(id) = self.resolve_stack(&entry) {
                attrs.push(Attr::uint(STACK_ID_ATTR, id));
            }
            let event = Event {
                ts_ns: closed_ns,
                ts_err_ns: 0,
                kind: "sched.offcpu".to_owned(),
                // The blocked task is the series: this is its time, not the
                // machine's, and not the time of whatever it was waiting for.
                keys: vec![KeyValue::task(entry.tid as i32)],
                attrs,
                collector_id: self.id.clone(),
                seq: seq + index as u64,
            };
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += entry.blocks;
                    continue;
                }
                self.filtered_in += entry.blocks;
            }
            events.push(event);
        }
    }

    /// Reads the target's mappings while it still has some, every tick.
    ///
    /// Not once, and not when a stack first needs resolving. A symbolizer
    /// snapshots `/proc/<pid>/maps` when it is built, and for a launched
    /// program *every* single moment is the wrong one: at `start` the child has
    /// not execed, at the first tick it is inside `execve` with its old image
    /// gone and its new one not yet mapped, and by the time the block that
    /// matters is drained -- which for a program that sleeps and then exits is
    /// at `stop` -- there are no maps at all. Each of those reads a launched
    /// program's stack back as twelve bare addresses.
    ///
    /// There is no single right moment, so this takes them all: one read per
    /// tick of a file of a few dozen lines, keeping the last one that had
    /// anything in it. The snapshot a stack is symbolized against is then the
    /// most recent one taken while the target was alive, which is the best
    /// answer that exists.
    ///
    /// Processes this run did not launch are still resolved lazily, because
    /// there is no list of them to walk until one of them blocks.
    fn open_target_symbols(&mut self) {
        if !self.wants_stacks {
            return;
        }
        // A thread's `/proc/<tid>/maps` is its process's, so a thread target
        // is symbolised through its own id. A process target is followed into
        // the processes it starts, and each of those is read too.
        let mut processes = match self.filter {
            Whose::Process(id) | Whose::Thread(id) => vec![id],
            Whose::Any | Whose::Cgroup(_) => return,
        };
        if let Some(program) = self.program.as_ref() {
            processes.extend(program.processes());
        }
        processes.sort_unstable();
        processes.dedup();
        self.retire_symbolizers(&processes);
        for tgid in processes {
            let fresh = !self.symbolizers.contains_key(&(tgid as i32));
            let symbolizer = self.symbolizer(tgid);
            if !fresh {
                symbolizer.refresh();
            }
            self.leaf.refresh(tgid);
        }
    }

    /// Drops the symbolizers of processes that have left the followed set.
    ///
    /// A build starts thousands of short-lived processes, and each symbolizer
    /// holds the parsed symbol tables of every file its process mapped, so
    /// keeping them all grows with every process the run ever followed. A
    /// process leaves the set when its leader is freed, after its last switch,
    /// so its last blocks are in the drain of the tick that first sees it gone;
    /// its symbolizer is dropped at the tick after, and where its names came
    /// from is kept for the facts line.
    fn retire_symbolizers(&mut self, followed: &[u32]) {
        for tgid in std::mem::take(&mut self.leaving) {
            if followed.contains(&(tgid as u32)) {
                continue;
            }
            self.leaf.retain(|kept| kept != tgid);
            if let Some(symbolizer) = self.symbolizers.remove(&tgid) {
                self.left_perf_maps.extend(symbolizer.perf_map_file());
                if let Some(file) = symbolizer.executable() {
                    self.left_executables.insert(tgid, file.to_owned());
                }
                for (path, origin) in symbolizer.origins() {
                    self.left_origins
                        .entry(path.clone())
                        .or_insert_with(|| origin.clone());
                }
            }
        }
        self.leaving = self
            .symbolizers
            .keys()
            .copied()
            .filter(|tgid| !followed.contains(&(*tgid as u32)))
            .collect();
    }

    /// The symbolizer of one process, built the first time it is asked for.
    fn symbolizer(&mut self, tgid: u32) -> &mut Symbolizer {
        let debuginfod = self.debuginfod.clone();
        self.symbolizers
            .entry(tgid as i32)
            .or_insert_with(|| Symbolizer::with_debuginfod_kernel(tgid as i32, debuginfod))
    }

    /// Symbolizes the stack behind an entry, once per distinct id.
    ///
    /// Returns the id to put on the event, or `None` when there is nothing to
    /// point at: the kernel could not walk the task's stack, or the stack it
    /// walked is no longer in the map. Both are counted and reported; neither
    /// is an empty stack, which would read as a task that blocked at no call
    /// site at all.
    fn resolve_stack(&mut self, entry: &OffCpu) -> Option<u64> {
        // Nothing downstream will read the frames, so there is no id worth
        // putting on the event either: an id whose table is empty points at
        // nothing. The kernel still walked the stack -- that happens in the
        // program, at the switch -- and what is skipped here is reading a
        // process's mappings and its binaries' symbols.
        if !self.wants_stacks {
            return None;
        }
        // Every task with a stack, for the file it executed: a stack two
        // tasks share is resolved once, and both tasks are named.
        self.task_processes.insert(
            KeyValue::task(entry.tid as i32).to_string(),
            entry.tgid as i32,
        );
        let id = match entry.stack {
            Some(id) => id,
            None if entry.stack_error == Some(libc::EEXIST) => {
                self.crowded_out += entry.blocks;
                return None;
            }
            None => {
                self.unwalkable += entry.blocks;
                return None;
            }
        };
        if let Some(resolved) = self.leaf.cached(id, entry.lr, entry.blocks) {
            return Some(resolved);
        }
        let program = self.program.as_ref()?;
        let Some(mut addresses) = program.stack(id) else {
            self.evicted_stacks += entry.blocks;
            return None;
        };
        let truncated = addresses.len() >= program.stack_depth();
        let (resolved, recovered) =
            self.leaf
                .place(entry.tgid, id, entry.lr, entry.blocks, &mut addresses);
        if self.stacks.contains_key(&resolved) {
            return Some(resolved);
        }
        let symbolizer = self.symbolizer(entry.tgid);
        // User frames only: `BPF_F_USER_STACK` asked for the task's own, and
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
        let mut named_by = stack.named_by_root_first();
        let mut objects = stack.objects_root_first();
        if truncated {
            // Root first, so the missing frames are the ones before the first
            // one here. Said rather than left to be inferred from a stack that
            // begins in the middle of a program.
            frames.insert(0, "[truncated]".to_owned());
            named_by.insert(0, None);
            objects.insert(0, None);
        }
        self.stacks.insert(resolved, frames);
        self.named_by.insert(resolved, named_by);
        self.objects.insert(resolved, objects);
        Some(resolved)
    }

    /// Reads the program's own counters of what it could not record.
    ///
    /// A full map is the only way this source loses a block, and nothing else
    /// anywhere would record that it happened: the helper returns an error in
    /// the kernel and the block is simply never counted.
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
            self.losses.note(
                "the kernel skipped a run of this program because one was already running on \
                 that cpu, so an occurrence was never seen; the count is the machine's, an \
                 upper bound on the target's share, and every total here is a lower bound",
            );
            self.skipped = skipped;
        }
        // A process the target started that the map of them had no room for
        // is a process none of whose blocks is here, and nothing else says so.
        let unfollowed = program.unfollowed();
        if unfollowed > 0 {
            self.losses.note(format!(
                "{unfollowed} process{} the target started {} not followed: the map of the \
                 processes it started was full, so {} blocks are in no total",
                if unfollowed == 1 { "" } else { "es" },
                if unfollowed == 1 { "was" } else { "were" },
                if unfollowed == 1 { "its" } else { "their" },
            ));
        }
        let (in_flight, totals) = program.dropped();
        if in_flight > self.dropped_in_flight {
            self.losses.dropped += in_flight - self.dropped_in_flight;
            self.losses.note(
                "more tasks were off cpu at once than the in-flight map holds, so their blocks \
                 were never timed",
            );
            self.dropped_in_flight = in_flight;
        }
        if totals > self.dropped_totals {
            self.losses.dropped += totals - self.dropped_totals;
            self.losses.note(
                "more distinct tasks blocked in one interval than the total map holds, so their \
                 time was never added",
            );
            self.dropped_totals = totals;
        }
    }
}

/// What an event carries beside its value.
///
/// `comm`, `pid` and `state` are here because the catalog entry declares them
/// as filter fields: a predicate over a field no event carries is false
/// everywhere, which empties the series and looks exactly like a workload that
/// never blocked.
fn attributes(entry: &OffCpu, window_ns: u64) -> Vec<Attr> {
    vec![
        // The value: the mean of the blocks behind this row. Every one of them
        // fell in the row's log2 bucket, so their mean did too, and weighed by
        // their count it puts each block where a histogram of blocks puts it --
        // and adds up, in `rate`, `topk` and `stack`, to exactly `total_ns`.
        // The total was the value before the bucket was in the key, and a
        // histogram of totals is a histogram of drain windows.
        Attr::float(
            "block_ns",
            entry.total_ns as f64 / entry.blocks.max(1) as f64,
        ),
        Attr::uint(crate::emit::WEIGHT_ATTR, entry.blocks),
        Attr::uint("bucket", u64::from(entry.bucket)),
        Attr::uint("total_ns", entry.total_ns),
        // The part of the total that was runnable rather than blocked, which
        // `sched.runqlat` reports too: a decomposition that adds this source to
        // that one counts it twice unless it subtracts this. Carried beside the
        // total rather than taken out of it, so that nothing already written
        // against `total_ns` changes meaning.
        Attr::uint("queued_ns", entry.queued_ns),
        Attr::uint("blocked_ns", entry.total_ns.saturating_sub(entry.queued_ns)),
        Attr::uint("wake_to_run_ns", entry.wake_to_run_ns),
        Attr::uint("blocks", entry.blocks),
        // The interval this total was collected over. Not a denominator: a
        // block is credited where it ended, so a wait longer than the tick
        // reports more nanoseconds than the window holds.
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", entry.tid as u64),
        Attr::uint("pid", entry.tgid as u64),
        Attr::text("comm", entry.comm.clone()),
        Attr::text("state", OffCpu::state_name(entry.state)),
        // The kernel's own value, beside the name. The set of states is open,
        // and a state this build does not recognise must still be visible.
        Attr::uint("state_bits", entry.state as u64),
    ]
}

impl Collector for OffCpuCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "block_ns"
    }

    fn row_stamp(&self) -> Option<crate::collect::RowStamp> {
        use crate::collect::RowStamp;
        Some(RowStamp::DrainClose)
    }

    fn start(&mut self) -> Result<(), String> {
        let program = OffCpuProgram::load(self.filter)?;
        // Said at the start, because `stop` drops the program before the
        // notes are read.
        self.notes
            .extend(program.clock_missing().map(str::to_owned));
        self.notes
            .extend(program.entries_missing().map(str::to_owned));
        self.notes
            .extend(program.leaf_lr_missing().map(str::to_owned));
        self.leaf.enable(program.leaf_lr_missing().is_none());
        self.program = Some(program);
        // Optional: without it a row keeps the name its task had when the row
        // was made, which for a launched program's first rows is this tool's.
        // A process target is followed into the processes it starts, and
        // those exec: the renames are the machine's then, applied by thread
        // id to the rows of the window, which are the target's alone.
        let renamed = match self.filter {
            Whose::Process(_) => Whose::Any,
            other => other,
        };
        self.renames = match Renames::load(renamed) {
            Ok(renames) => Some(renames),
            Err(reason) => {
                self.notes.push(format!(
                    "a task that exec'd during a window is named what it was when its first \
                     row was made, because the exec program could not be attached: {reason}"
                ));
                None
            }
        };
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
        // A task that was still blocked when the run ended has real blocked
        // time that is in no total, because it has no end yet. That is a fact
        // about the window rather than something lost, and a run of a program
        // that spends its life waiting would otherwise report almost nothing
        // with nothing saying why.
        if let Some(program) = self.program.as_ref() {
            let open = program.still_blocked();
            if open > 0 {
                self.notes.push(format!(
                    "{open} task{} still off cpu when the run ended; the block it was in has no \
                     end yet and is in no total",
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

    /// Who named each frame, the perf maps that named any, and the file each
    /// task executed: what `cpu.profile` says beside its stacks.
    fn provenance(&self) -> crate::stack::Provenance {
        let mut perf_maps: Vec<PerfMapFile> = self
            .symbolizers
            .values()
            .filter_map(Symbolizer::perf_map_file)
            .chain(self.left_perf_maps.iter().cloned())
            .collect();
        perf_maps.sort();
        perf_maps.dedup();
        let executables = self
            .task_processes
            .iter()
            .filter_map(|(task, pid)| {
                let file = self
                    .symbolizers
                    .get(pid)
                    .and_then(Symbolizer::executable)
                    .map(str::to_owned)
                    .or_else(|| self.left_executables.get(pid).cloned())?;
                Some((task.clone(), file))
            })
            .collect();
        crate::stack::Provenance {
            named_by: self.named_by.clone(),
            perf_maps,
            executables,
            objects: self.objects.clone(),
        }
    }

    /// Where the stacks' names came from, in the words `cpu.profile` uses.
    fn facts(&self) -> Vec<String> {
        if !self.wants_stacks {
            return Vec::new();
        }
        let origins: BTreeMap<_, _> = self
            .left_origins
            .iter()
            .chain(
                self.symbolizers
                    .values()
                    .flat_map(|symbolizer| symbolizer.origins()),
            )
            .collect();
        crate::stack::names_fact(origins, self.debuginfod.is_some())
            .into_iter()
            .collect()
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        // A stack that could not be walked is the ordinary case for a kernel
        // worker, which has no user stack at all, and it must not be confused
        // with one the kernel lost. Reported as a degradation because a flame
        // graph of the blocks that *did* resolve looks complete either way.
        if self.unwalkable > 0 {
            notes.push(format!(
                "{} blocks had no user stack to walk: a kernel thread has none, and a task built \
                 without frame pointers may have none the kernel can follow",
                self.unwalkable
            ));
        }
        if self.blocks_without_a_wakeup > 0 {
            notes.push(format!(
                "{} blocks ended without this run having seen the wakeup that released them, so \
                 their `wake_to_run_ns` is zero because it is unknown rather than because there \
                 was no wait: the task was woken before the programs attached, or by a path that \
                 does not raise `sched_wakeup`",
                self.blocks_without_a_wakeup
            ));
        }
        if self.unstamped > 0 {
            notes.push(format!(
                "{} blocks ended with no queue stamp in the kernel's `sched_info`, so their \
                 `queued_ns` is zero as the kernel counts it: the wait after their wakeup is in \
                 `blocked_ns`, as it is in the task's `schedstat`",
                self.unstamped
            ));
        }

        if self.crowded_out > 0 {
            notes.push(format!(
                "{} blocks were walked and their stack not kept: the kernel's stack map holds one \
                 stack per bucket over {} buckets and refuses a second, so a stack that hashed \
                 onto one already taken is not kept -- the blocks are counted, and cannot be \
                 placed",
                self.crowded_out,
                OffCpuProgram::STACK_SLOTS,
            ));
        }
        if self.filtered_in > 0 || self.filtered_out > 0 {
            notes.push(format!(
                "the filter kept {} blocks and removed {}; it ran here, over the drained rows, \
                 not in the kernel",
                self.filtered_in, self.filtered_out,
            ));
        }
        if self.evicted_stacks > 0 {
            notes.push(format!(
                "{} blocks named a stack the kernel had already evicted from its stack map, so \
                 they are counted and cannot be placed",
                self.evicted_stacks
            ));
        }
        notes.extend(self.leaf.note("blocks"));
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process that leaves the followed set keeps its symbolizer for one
    /// more tick, for the drain that holds its last blocks, and loses it at
    /// the next; the target itself never does.
    #[test]
    fn a_process_that_left_is_forgotten_a_tick_after_it_left() {
        let mut collector = OffCpuCollector::new(Whose::Process(1), true);
        for tgid in [1, 2, 3] {
            collector.symbolizer(tgid);
        }
        collector.retire_symbolizers(&[1, 2]);
        assert_eq!(collector.symbolizers.len(), 3, "kept for this tick's drain");
        collector.retire_symbolizers(&[1, 2]);
        let mut kept: Vec<i32> = collector.symbolizers.keys().copied().collect();
        kept.sort_unstable();
        assert_eq!(kept, vec![1, 2]);
        collector.retire_symbolizers(&[1]);
        collector.retire_symbolizers(&[1]);
        assert_eq!(
            collector.symbolizers.keys().copied().collect::<Vec<_>>(),
            vec![1]
        );
    }

    /// A mode that cannot mean anything here is refused with the reason. A
    /// sampled block is a block whose beginning or end was not seen, and a
    /// duration built from one of the two is not a short duration, it is not
    /// a duration.
    #[test]
    fn a_sampled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Sample {
                freq: "997hz".parse().expect("a frequency"),
            },
            wants_stacks: false,
            filter: None,
            debuginfod: false,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("a sampled mode must be refused");
        };
        assert!(error.contains("cannot measure blocked time"), "{error}");
    }

    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            wants_stacks: false,
            filter: None,
            debuginfod: false,
        });
        let Err(error) = prepared.attach(&Target::Iface("eth0".to_owned())) else {
            panic!("an interface names no task to attribute blocked time to");
        };
        assert!(error.contains("iface"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// Every filter field the catalog offers must be on the events, or a
    /// predicate over it is false everywhere and silently empties the series.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let entry = OffCpu {
            tid: 7,
            tgid: 7,
            state: 1,
            bucket: 10,
            total_ns: 1_000,
            queued_ns: 0,
            wake_to_run_ns: 0,
            unstamped: 0,
            unwoken: 0,
            blocks: 2,
            comm: "sleep".to_owned(),
            stack: None,
            stack_error: None,
            lr: 0,
        };
        let attributes = attributes(&entry, 100_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"sched.offcpu".parse().expect("a source id"))
            .expect("sched.offcpu is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty(), "the entry declares filter fields");
        // `ppid` is put on the event by the collector, from the process's
        // parent (`super::Parents`), not by the row's attributes.
        for field in declared.iter().filter(|field| *field != "ppid") {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// The state reaches the user as a word, because a filter written against
    /// this source is written by a person: `state == 'uninterruptible'` is the
    /// question someone asks, and `state_bits == 2` is not.
    #[test]
    fn the_state_is_carried_as_a_name_and_as_the_kernels_own_value() {
        let entry = OffCpu {
            tid: 7,
            tgid: 7,
            // TASK_UNINTERRUPTIBLE | TASK_NOLOAD, which is an idle kernel
            // thread and not a program stuck on a disk.
            state: 0x402,
            bucket: 1,
            total_ns: 1,
            queued_ns: 0,
            wake_to_run_ns: 0,
            unstamped: 0,
            unwoken: 0,
            blocks: 1,
            comm: "kworker".to_owned(),
            stack: None,
            stack_error: None,
            lr: 0,
        };
        let attributes = attributes(&entry, 1);
        let named = attributes.iter().find(|attr| attr.name == "state");
        assert_eq!(
            named.map(|attr| attr.value.clone()),
            Some(crate::event::AttrValue::Text("idle".to_owned()))
        );
        let bits = attributes.iter().find(|attr| attr.name == "state_bits");
        assert_eq!(
            bits.and_then(|attr| attr.value.as_f64()),
            Some(0x402 as f64)
        );
    }

    /// Whether this machine lets the tests load a program at all. Without the
    /// capabilities there is nothing here to test, and saying so beats a test
    /// that passes because it did nothing.
    fn skip_without_bpf(collector: &mut OffCpuCollector) -> bool {
        match collector.start() {
            Ok(()) => false,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                true
            }
        }
    }

    /// Ground truth: a thread that sleeps for a known time must be reported
    /// off cpu for that time. The sleep is the independent quantity -- the
    /// kernel's timer, not anything this code computed -- and the tolerance is
    /// the one the brief sets.
    #[test]
    fn a_thread_that_sleeps_a_known_time_is_off_cpu_for_it() {
        const SLEEP: StdDuration = StdDuration::from_millis(300);
        let mut collector = OffCpuCollector::new(
            Whose::Process(
                crate::bpf::pidns::kernel_tgid(std::process::id()).expect("this process"),
            ),
            false,
        );
        if skip_without_bpf(&mut collector) {
            return;
        }

        // The collector's events carry the kernel's id for the thread, which
        // in a container is not the one gettid returns.
        let tid = crate::bpf::pidns::kernel_tid(unsafe { libc::syscall(libc::SYS_gettid) } as u32)
            .expect("this thread");
        std::thread::sleep(SLEEP);

        let mut events = collector.sample(0).expect("a drain");
        events.extend(collector.stop());

        let slept_ns: u64 = events
            .iter()
            .filter(|event| event.attr("tid").and_then(|value| value.as_f64()) == Some(tid as f64))
            .filter_map(|event| event.attr("total_ns").and_then(|value| value.as_f64()))
            .sum::<f64>() as u64;

        let expected = SLEEP.as_nanos() as u64;
        let margin = expected / 20;
        assert!(
            slept_ns.abs_diff(expected) <= margin,
            "a {} ms sleep must show as {} ms off cpu within 5 %, saw {} ms",
            expected / 1_000_000,
            expected / 1_000_000,
            slept_ns / 1_000_000
        );
        assert!(collector.losses().is_clean(), "{:?}", collector.losses());
    }

    /// The state must be the one the task was actually in. A `sleep` waits on
    /// a timer, which is interruptible; reporting it as uninterruptible would
    /// turn a program doing nothing into a program stuck on a device.
    #[test]
    fn a_timed_sleep_is_reported_as_interruptible() {
        let mut collector = OffCpuCollector::new(
            Whose::Process(
                crate::bpf::pidns::kernel_tgid(std::process::id()).expect("this process"),
            ),
            false,
        );
        if skip_without_bpf(&mut collector) {
            return;
        }
        // The collector's events carry the kernel's id for the thread, which
        // in a container is not the one gettid returns.
        let tid = crate::bpf::pidns::kernel_tid(unsafe { libc::syscall(libc::SYS_gettid) } as u32)
            .expect("this thread");
        std::thread::sleep(StdDuration::from_millis(50));
        let mut events = collector.sample(0).expect("a drain");
        events.extend(collector.stop());

        let event = events
            .iter()
            .find(|event| event.attr("tid").and_then(|value| value.as_f64()) == Some(tid as f64))
            .expect("the sleeping thread is in the totals");
        assert_eq!(
            event.attr("state").and_then(|value| match value {
                crate::event::AttrValue::Text(text) => Some(text.as_str()),
                _ => None,
            }),
            Some("sleeping"),
            "a thread waiting on a timer is in interruptible sleep"
        );
        assert_eq!(
            event.key(crate::request::KeyKind::Task),
            Some(&KeyValue::task(tid as i32)),
            "the series is the task that was blocked"
        );
    }

    /// Preemption is not blocked time. A thread that never sleeps but is
    /// pushed off the cpu by the scheduler must not appear with a duration:
    /// its wait is run queue latency, which is a different source.
    #[test]
    fn a_thread_that_only_spins_is_not_reported_as_blocked() {
        let mut collector = OffCpuCollector::new(
            Whose::Process(
                crate::bpf::pidns::kernel_tgid(std::process::id()).expect("this process"),
            ),
            false,
        );
        if skip_without_bpf(&mut collector) {
            return;
        }
        // The collector's events carry the kernel's id for the thread, which
        // in a container is not the one gettid returns.
        let tid = crate::bpf::pidns::kernel_tid(unsafe { libc::syscall(libc::SYS_gettid) } as u32)
            .expect("this thread");
        let until = std::time::Instant::now() + StdDuration::from_millis(200);
        let mut spun = 0u64;
        while std::time::Instant::now() < until {
            spun = spun.wrapping_add(1);
        }

        let mut events = collector.sample(0).expect("a drain");
        events.extend(collector.stop());
        let blocked_ns: u64 = events
            .iter()
            .filter(|event| event.attr("tid").and_then(|value| value.as_f64()) == Some(tid as f64))
            .filter_map(|event| event.attr("total_ns").and_then(|value| value.as_f64()))
            .sum::<f64>() as u64;
        assert!(
            blocked_ns < 50_000_000,
            "a thread that spun for 200 ms without waiting for anything reported {} ms blocked; \
             preemption is being counted as a block",
            blocked_ns / 1_000_000
        );
        let _ = spun;
    }
}
