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

use std::collections::{BTreeMap, HashMap};
use std::time::Duration as StdDuration;

use crate::bpf::{OffCpu, OffCpuProgram};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::emit::STACK_ID_ATTR;
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};
use crate::stack::{Frame, RawStack, Symbolizer};

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
        let filter = match target {
            Target::System => None,
            other => Some(other.first_pid().ok_or_else(|| {
                format!(
                    "a {} target names no process, and blocked time is attributed per task; \
                     use --system to watch the whole machine",
                    other.kind()
                )
            })? as u32),
        };
        Ok(Box::new(OffCpuCollector::new(filter, self.wants_stacks)))
    }
}

pub struct OffCpuCollector {
    id: CollectorId,
    /// The thread group whose blocks are kept, or none for the machine.
    filter: Option<u32>,
    program: Option<OffCpuProgram>,
    losses: Losses,
    notes: Vec<String>,
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
    /// One symbolizer per process seen, built lazily. A system-wide run sees
    /// many, and each carries that process's own mappings.
    symbolizers: HashMap<i32, Symbolizer>,
    /// Blocks whose stack the kernel could not walk, and blocks whose id it had
    /// evicted by the time it was asked. Different failures, counted apart.
    unwalkable: u64,
    evicted_stacks: u64,
    /// Whether the frames behind a stack id will be read by anyone.
    wants_stacks: bool,
}

impl OffCpuCollector {
    pub fn new(filter: Option<u32>, wants_stacks: bool) -> Self {
        Self {
            id: CollectorId::new("sched.offcpu/bpf:raw_tp"),
            filter,
            program: None,
            losses: Losses::default(),
            notes: Vec::new(),
            dropped_in_flight: 0,
            dropped_totals: 0,
            window_opened_ns: 0,
            stacks: BTreeMap::new(),
            symbolizers: HashMap::new(),
            unwalkable: 0,
            evicted_stacks: 0,
            wants_stacks,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.window_opened_ns);
        let (entries, drain_losses) = match program.drain() {
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

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("an off-cpu total was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the off-cpu map held more entries than one drain can walk");
        }

        for (index, entry) in entries.into_iter().enumerate() {
            let mut attrs = attributes(&entry, window_ns);
            if let Some(id) = self.resolve_stack(&entry) {
                attrs.push(Attr::uint(STACK_ID_ATTR, u64::from(id)));
            }
            events.push(Event {
                ts_ns: closed_ns,
                ts_err_ns: 0,
                kind: "sched.offcpu".to_owned(),
                // The blocked task is the series: this is its time, not the
                // machine's, and not the time of whatever it was waiting for.
                keys: vec![KeyValue::task(entry.tid as i32)],
                attrs,
                collector_id: self.id.clone(),
                seq: seq + index as u64,
            });
        }
    }

    /// Reads the target's mappings while it still has some.
    ///
    /// Not at `start`, and not when a stack first needs resolving. A symbolizer
    /// snapshots `/proc/<pid>/maps` when it is built, and both of those moments
    /// are wrong for a launched program: at `start` the child has not execed
    /// yet, so the maps are this tool's own, and by the time a block is drained
    /// the program has often exited and there are no maps at all. That is what
    /// a stack of bare addresses looks like, and this is the only window in
    /// between.
    ///
    /// Processes this run did not launch are still resolved lazily, because
    /// there is no list of them to walk until one of them blocks.
    fn open_target_symbols(&mut self) {
        if !self.wants_stacks {
            return;
        }
        let Some(tgid) = self.filter else { return };
        self.symbolizers
            .entry(tgid as i32)
            .or_insert_with(|| Symbolizer::for_process(tgid as i32));
    }

    /// Symbolizes the stack behind an entry, once per distinct id.
    ///
    /// Returns the id to put on the event, or `None` when there is nothing to
    /// point at: the kernel could not walk the task's stack, or the stack it
    /// walked is no longer in the map. Both are counted and reported; neither
    /// is an empty stack, which would read as a task that blocked at no call
    /// site at all.
    fn resolve_stack(&mut self, entry: &OffCpu) -> Option<u32> {
        // Nothing downstream will read the frames, so there is no id worth
        // putting on the event either: an id whose table is empty points at
        // nothing. The kernel still walked the stack -- that happens in the
        // program, at the switch -- and what is skipped here is reading a
        // process's mappings and its binaries' symbols.
        if !self.wants_stacks {
            return None;
        }
        let id = match entry.stack {
            Some(id) => id,
            None => {
                self.unwalkable += entry.blocks;
                return None;
            }
        };
        if self.stacks.contains_key(&u64::from(id)) {
            return Some(id);
        }
        let program = self.program.as_ref()?;
        let Some(addresses) = program.stack(id) else {
            self.evicted_stacks += entry.blocks;
            return None;
        };
        let truncated = addresses.len() >= program.stack_depth();
        let symbolizer = self
            .symbolizers
            .entry(entry.tgid as i32)
            .or_insert_with(|| Symbolizer::for_process(entry.tgid as i32));
        // User frames only: `BPF_F_USER_STACK` asked for the task's own, and
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
            // one here. Said rather than left to be inferred from a stack that
            // begins in the middle of a program.
            frames.insert(0, "[truncated]".to_owned());
        }
        self.stacks.insert(u64::from(id), frames);
        Some(id)
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
        Attr::uint("total_ns", entry.total_ns),
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
        "total_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(OffCpuProgram::load(self.filter)?);
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
        if self.evicted_stacks > 0 {
            notes.push(format!(
                "{} blocks named a stack the kernel had already evicted from its stack map, so \
                 they are counted and cannot be placed",
                self.evicted_stacks
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            total_ns: 1_000,
            blocks: 2,
            comm: "sleep".to_owned(),
            stack: None,
        };
        let attributes = attributes(&entry, 100_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"sched.offcpu".parse().expect("a source id"))
            .expect("sched.offcpu is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty(), "the entry declares filter fields");
        for field in declared {
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
            total_ns: 1,
            blocks: 1,
            comm: "kworker".to_owned(),
            stack: None,
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
        let mut collector = OffCpuCollector::new(Some(std::process::id()), false);
        if skip_without_bpf(&mut collector) {
            return;
        }

        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
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
        let mut collector = OffCpuCollector::new(Some(std::process::id()), false);
        if skip_without_bpf(&mut collector) {
            return;
        }
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
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
        let mut collector = OffCpuCollector::new(Some(std::process::id()), false);
        if skip_without_bpf(&mut collector) {
            return;
        }
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
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
