//! `sched.slices` over a raw tracepoint: one event per time slice on a cpu.
//!
//! `perf sched timehist` prints a line a switch: when a task left the cpu,
//! how long it had run, how long it had waited on the run queue before that,
//! and how long it had been off the cpu before it was queued. The counting
//! sources of this family answer "how much" and none of them "which one":
//! a 40 ms slice among a thousand 50 us ones is a mean of 90 us everywhere
//! else.
//!
//! The program (`bpf::switch_count`, the per-switch half) writes one record
//! per switch either end of which is the target's, with the leaving task's
//! run and the coming task's queue wait read from the kernel's own
//! `sched_info` stamps. This file pairs a task's switch in with its switch
//! out: the event is stamped at the switch out, its value is the run, and the
//! queue wait and the time off the cpu come from the switch in that opened
//! the slice, when this run saw it.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use crate::bpf::{SwitchCountProgram, Whose};
use crate::collect::kernel::UserFilter;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained: a busy cpu switches every few
/// microseconds, and a machine-wide run is every cpu.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// 88 bytes and a header a switch: some forty thousand switches between two
/// drains.
const RING_BYTES: usize = 4 * 1024 * 1024;

/// `TASK_RUNNING`: the task left the cpu still runnable.
const TASK_RUNNING: u32 = 0;

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot report time slices: a slice is the gap between a task's \
                 switch in and its switch out, and only `trace` sees both",
                self.mode
            ));
        }
        let whose = super::whose(target, "a time slice is a task's")?;
        Ok(Box::new(SlicesCollector::new(whose, self.filter)))
    }
}

/// What the switch in that opened a slice said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Opened {
    in_ns: u64,
    /// The wait on the queue: the stamp's part, and what the kernel closed of
    /// it on other cpus. `None` when neither can be told.
    queue_ns: Option<u64>,
    /// The wait by the tracepoints, from the wakeup or preemption.
    wake_ns: Option<u64>,
}

/// One task's state between two of its switches.
#[derive(Debug, Default, Clone, Copy)]
struct Task {
    /// The switch in of the slice now running.
    opened: Option<Opened>,
    /// When the task last left the cpu.
    left_ns: Option<u64>,
    /// Its `sched_info.run_delay` then.
    left_delay: Option<u64>,
}

/// One slice, as the pairing produced it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Slice {
    pub tid: u32,
    pub pid: u32,
    pub comm: String,
    pub cpu: u32,
    pub out_ns: u64,
    pub run_ns: u64,
    pub queue_ns: Option<u64>,
    /// The same wait by the tracepoints: `perf sched timehist`'s `sch delay`.
    pub wake_ns: Option<u64>,
    pub off_ns: Option<u64>,
    pub state: u32,
    pub preempted: bool,
    pub next_tid: u32,
    pub next_comm: String,
}

/// The pairing of switches into slices, apart from the program so that it is
/// tested on switches written by hand.
#[derive(Debug, Default)]
pub(crate) struct Pairing {
    tasks: BTreeMap<u32, Task>,
    /// Slices whose switch in this run did not see: the first of each task.
    pub unopened: u64,
    /// Slices whose switch in held no queue stamp and whose switch out this
    /// run did not see, and so carry neither `queue_ns` nor `off_ns`: the gap
    /// since the task left could not be split between the two.
    pub unstamped: u64,
}

/// The ends of one switch, as the pairing reads them.
#[derive(Debug, Clone)]
pub(crate) struct Ends {
    pub ts_ns: u64,
    pub cpu: u32,
    pub prev: (u32, u32, String),
    pub next: (u32, u32, String),
    pub prev_state: u32,
    pub preempt: bool,
    pub prev_matched: bool,
    pub next_matched: bool,
    pub run_ns: u64,
    /// `rq->clock - last_queued` of the task coming in, `None` when it held
    /// no stamp, which is not a zero wait.
    pub queue_ns: Option<u64>,
    /// The wait since its wakeup or preemption by the tracepoints, `None`
    /// when the run saw neither.
    pub wake_ns: Option<u64>,
    /// `sched_info.run_delay` of each end.
    pub prev_delay: u64,
    pub next_delay: u64,
}

impl Pairing {
    /// Takes one switch; returns the slice it closed, if the leaving task is
    /// the target's.
    pub(crate) fn take(&mut self, ends: &Ends) -> Option<Slice> {
        let mut closed = None;
        if ends.prev_matched && ends.prev.0 != 0 {
            let task = self.tasks.entry(ends.prev.0).or_default();
            let opened = task.opened.take();
            if opened.is_none() {
                self.unopened += 1;
            }
            if opened.is_some_and(|opened| opened.queue_ns.is_none()) {
                self.unstamped += 1;
            }
            // The gap from switch out to switch in is the time off the cpu and
            // then the wait on the queue: `off_ns` is the first part only, so
            // that the two add up to the gap rather than count the wait twice.
            let queue_ns = opened.and_then(|opened| opened.queue_ns);
            let off_ns = opened.zip(queue_ns).and_then(|(opened, queue_ns)| {
                task.left_ns
                    .map(|left| opened.in_ns.saturating_sub(left).saturating_sub(queue_ns))
            });
            task.left_ns = Some(ends.ts_ns);
            task.left_delay = Some(ends.prev_delay);
            closed = Some(Slice {
                tid: ends.prev.0,
                pid: ends.prev.1,
                comm: ends.prev.2.clone(),
                cpu: ends.cpu,
                out_ns: ends.ts_ns,
                run_ns: ends.run_ns,
                queue_ns,
                wake_ns: opened.and_then(|opened| opened.wake_ns),
                off_ns,
                state: ends.prev_state,
                preempted: ends.preempt,
                next_tid: ends.next.0,
                next_comm: ends.next.2.clone(),
            });
        }
        // The idle task is pid 0 on every cpu and is no one's slice.
        if ends.next_matched && ends.next.0 != 0 {
            let task = self.tasks.entry(ends.next.0).or_default();
            // A runnable task the load balancer moved was dequeued from the
            // first cpu, which closed that part of its wait into `run_delay`
            // and cleared the stamp; the second cpu stamped it again. The
            // stamp alone is then the part on the cpu that ran it: under
            // twice as many spinning threads as cpus, about one preempted
            // slice in six read a wait of 0 or a fraction of its gap that way.
            let moved = task
                .left_delay
                .map(|left| ends.next_delay.saturating_sub(left));
            let queue_ns = match (ends.queue_ns, moved) {
                (None, None) => None,
                (stamp, moved) => Some(stamp.unwrap_or(0) + moved.unwrap_or(0)),
            };
            task.opened = Some(Opened {
                in_ns: ends.ts_ns,
                queue_ns,
                wake_ns: ends.wake_ns,
            });
        }
        closed
    }
}

/// The kernel's name for a task state, as `sched.offcpu` gives it, or
/// `preempted` for a task that left still runnable.
fn state_name(state: u32, preempted: bool) -> &'static str {
    if preempted || state == TASK_RUNNING {
        "preempted"
    } else {
        crate::bpf::OffCpu::state_name(state)
    }
}

fn event(slice: &Slice, seq: u64) -> Event {
    let mut attrs = vec![
        Attr::uint("run_ns", slice.run_ns),
        Attr::uint("count", 1),
        Attr::uint("tid", u64::from(slice.tid)),
        Attr::uint("pid", u64::from(slice.pid)),
        Attr::text("comm", slice.comm.clone()),
        Attr::uint("cpu", u64::from(slice.cpu)),
        Attr::text("state", state_name(slice.state, slice.preempted)),
        Attr::uint("state_bits", u64::from(slice.state)),
        Attr::uint("next_tid", u64::from(slice.next_tid)),
        Attr::text("next_comm", slice.next_comm.clone()),
    ];
    if let Some(queue_ns) = slice.queue_ns {
        attrs.push(Attr::uint("queue_ns", queue_ns));
    }
    if let Some(off_ns) = slice.off_ns {
        attrs.push(Attr::uint("off_ns", off_ns));
    }
    if let Some(wake_ns) = slice.wake_ns {
        attrs.push(Attr::uint("wake_ns", wake_ns));
    }
    Event {
        ts_ns: slice.out_ns,
        ts_err_ns: 0,
        kind: "sched.slices".to_owned(),
        keys: vec![
            KeyValue::task(slice.tid as i32),
            KeyValue::cpu(slice.cpu),
            KeyValue::comm(slice.comm.clone()),
        ],
        attrs,
        collector_id: CollectorId::unstamped(),
        seq,
    }
}

pub struct SlicesCollector {
    whose: Whose,
    program: Option<SwitchCountProgram>,
    pairing: Pairing,
    parents: super::Parents,
    losses: Losses,
    skipped: u64,
    dropped: u64,
    predicate: UserFilter,
    slices: u64,
}

impl SlicesCollector {
    pub fn new(whose: Whose, predicate: Option<FilterExpr>) -> Self {
        Self {
            whose,
            program: None,
            pairing: Pairing::default(),
            parents: super::Parents::default(),
            losses: Losses::default(),
            skipped: 0,
            dropped: 0,
            predicate: UserFilter::new(predicate, "slices"),
            slices: 0,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        self.parents.absorb(program.parents());
        let mut switches = Vec::new();
        let mut undecoded = 0;
        program.drain_switches(
            |switch| {
                switches.push(Ends {
                    ts_ns: switch.ts_ns,
                    cpu: switch.cpu,
                    prev: (switch.prev_tid, switch.prev_pid, switch.prev_comm()),
                    next: (switch.next_tid, switch.next_pid, switch.next_comm()),
                    prev_state: switch.prev_state,
                    preempt: switch.preempt != 0,
                    prev_matched: switch.matched & 2 != 0,
                    next_matched: switch.matched & 1 != 0,
                    run_ns: switch.run_ns,
                    queue_ns: switch.queue(),
                    wake_ns: switch.wake(),
                    prev_delay: switch.prev_delay,
                    next_delay: switch.next_delay,
                })
            },
            &mut undecoded,
        );
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the switch ring was shorter than one switch");
        }
        let mut index = seq;
        for ends in &switches {
            let Some(slice) = self.pairing.take(ends) else {
                continue;
            };
            let mut event = event(&slice, index);
            if let Some(ppid) = self.parents.of(slice.pid) {
                event.attrs.push(Attr::uint("ppid", u64::from(ppid)));
            }
            if !self.predicate.keep(&event, 1) {
                continue;
            }
            self.slices += 1;
            index += 1;
            events.push(event);
        }
    }

    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note_times(
                "the kernel skipped a run of this program because one was already running on \
                 that cpu, so a switch was never seen",
                skipped - self.skipped,
            );
            self.skipped = skipped;
        }
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note_times(
                "the switch ring was full: switches happened faster than they were drained, \
                 and the slices they closed are not here",
                dropped - self.dropped,
            );
            self.dropped = dropped;
        }
        let unfollowed = program.unfollowed();
        if unfollowed > 0 {
            self.losses.note(format!(
                "{unfollowed} process{} the target started {} not followed: the map of the \
                 processes it started was full",
                if unfollowed == 1 { "" } else { "es" },
                if unfollowed == 1 { "was" } else { "were" },
            ));
        }
    }
}

impl Collector for SlicesCollector {
    fn value_attr(&self) -> &'static str {
        "run_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(SwitchCountProgram::load_stream(self.whose, RING_BYTES)?);
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
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn facts(&self) -> Vec<String> {
        let mut facts = vec![format!(
            "{} slices, each stamped where its task left the cpu: `run_ns` is how long it ran, \
             by the kernel's `sched_info` (`rq->clock - last_arrival`); `queue_ns` how long it \
             waited on the run queue before it ran, by the kernel's `sched_info` (the stamp \
             and what `run_delay` grew by while the task was away), and `off_ns` how long it \
             was off the cpu before it was queued; the two add up to the gap between the \
             switch out before and the switch in that opened the slice",
            self.slices
        )];
        if self.pairing.unopened > 0 {
            facts.push(format!(
                "{} slices began before the run did, or with a switch in this run did not see, \
                 and carry no `queue_ns` or `off_ns`",
                self.pairing.unopened
            ));
        }
        if self.pairing.unstamped > 0 {
            facts.push(format!(
                "{} slices began with a switch in whose task held no queue stamp in the kernel's \
                 `sched_info`, or one ahead of its run queue's clock, after a switch out this run \
                 did not see, and carry no `queue_ns` or `off_ns`: their gap since the task left \
                 could not be split between the two",
                self.pairing.unstamped
            ));
        }
        facts
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        notes.extend(self.predicate.note());
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ends(ts_ns: u64, prev: u32, next: u32, run_ns: u64, queue_ns: Option<u64>) -> Ends {
        Ends {
            ts_ns,
            cpu: 2,
            prev: (prev, prev, format!("t{prev}")),
            next: (next, next, format!("t{next}")),
            prev_state: 1,
            preempt: false,
            prev_matched: prev == 7,
            next_matched: next == 7,
            run_ns,
            queue_ns,
            wake_ns: queue_ns.map(|ns| ns + 3_000),
            prev_delay: 0,
            next_delay: 0,
        }
    }

    /// A task's switch in opens its slice and its switch out closes it: the
    /// queue wait comes from the switch in, the time off from the switch out
    /// before it less that wait, the run from the kernel's own stamp.
    #[test]
    fn a_switch_in_and_out_are_one_slice() {
        let mut pairing = Pairing::default();
        // Task 7 leaves for the first time: a slice this run did not open.
        let first = pairing
            .take(&ends(1_000, 7, 0, 400, Some(0)))
            .expect("the target's slice");
        assert_eq!(first.queue_ns, None);
        assert_eq!(pairing.unopened, 1);
        // Back in at 5 000 after 300 ns on the queue, out at 9 000.
        assert!(pairing.take(&ends(5_000, 0, 7, 0, Some(300))).is_none());
        let second = pairing
            .take(&ends(9_000, 7, 3, 3_990, Some(0)))
            .expect("the target's slice");
        assert_eq!(second.run_ns, 3_990);
        assert_eq!(second.queue_ns, Some(300));
        // The tracepoints' wait travels from the switch in that opened it.
        assert_eq!(second.wake_ns, Some(3_300));
        assert_eq!(first.wake_ns, None);
        // Off at 1 000, in at 5 000: 3 700 off the cpu, then 300 queued.
        assert_eq!(second.off_ns, Some(3_700));
        assert_eq!(second.next_comm, "t3");
        // Another task's slice is not the target's.
        assert!(pairing.take(&ends(9_500, 3, 0, 500, Some(0))).is_none());
    }

    /// A switch in with no queue stamp is not a zero wait: with no switch out
    /// to measure `run_delay` from, the slice it opens carries neither
    /// `queue_ns` nor `off_ns`, and is counted.
    #[test]
    fn a_missing_queue_stamp_is_counted_not_zero() {
        let mut pairing = Pairing::default();
        pairing.take(&ends(5_000, 0, 7, 0, None));
        let slice = pairing
            .take(&ends(9_000, 7, 3, 3_990, Some(0)))
            .expect("the target's slice");
        assert_eq!(slice.queue_ns, None);
        assert_eq!(slice.off_ns, None);
        assert_eq!(pairing.unstamped, 1);
        // A wait longer than the gap, by two cpus' clocks, reads no time off.
        pairing.take(&ends(9_100, 0, 7, 0, Some(500)));
        let slice = pairing
            .take(&ends(9_900, 7, 3, 800, Some(0)))
            .expect("the target's slice");
        assert_eq!(slice.off_ns, Some(0));
        assert_eq!(pairing.unstamped, 1);
    }

    /// A preempted task the load balancer moved: the first cpu closed 2 000
    /// of its wait into `run_delay` and the second stamped the last 500. The
    /// wait is both, and the task was never off the cpu otherwise.
    #[test]
    fn a_wait_split_across_two_cpus_is_whole() {
        let mut pairing = Pairing::default();
        let mut out = ends(1_000, 7, 3, 400, Some(0));
        out.prev_delay = 10_000;
        pairing.take(&out);
        let mut back = ends(3_500, 0, 7, 0, Some(500));
        back.next_delay = 12_000;
        pairing.take(&back);
        let mut out = ends(4_000, 7, 3, 500, Some(0));
        out.prev_delay = 12_000;
        let slice = pairing.take(&out).expect("the target's slice");
        assert_eq!(slice.queue_ns, Some(2_500));
        assert_eq!(slice.off_ns, Some(0));
        // No stamp at all, and the whole wait closed on the first cpu.
        let mut back = ends(6_000, 0, 7, 0, None);
        back.next_delay = 14_000;
        pairing.take(&back);
        let slice = pairing
            .take(&ends(6_500, 7, 3, 500, Some(0)))
            .expect("the target's slice");
        assert_eq!(slice.queue_ns, Some(2_000));
        assert_eq!(slice.off_ns, Some(0));
        assert_eq!(pairing.unstamped, 0);
    }

    #[test]
    fn a_preemption_reads_as_one() {
        assert_eq!(state_name(0, false), "preempted");
        assert_eq!(state_name(1, true), "preempted");
        assert_eq!(state_name(1, false), "sleeping");
        assert_eq!(state_name(2, false), "uninterruptible");
    }
}
