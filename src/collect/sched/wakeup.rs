//! `sched.wakeup` over a raw tracepoint.
//!
//! One event per wakeup: when it happened, which task did the waking, which
//! task was woken, and on which cpu. The measurement this source exists for is
//! not in any single event -- it is in the pairing of a wakeup with what the
//! woken task did next, which is how blocked time gets an explanation instead
//! of only a duration.
//!
//! The program is in `src/bpf/wakeup.rs`. This file is the part that belongs to
//! the family: what the events look like, what is refused, and what is counted
//! as lost.

use std::time::Duration as StdDuration;

use crate::bpf::{Wakeup, WakeupProgram, Whose};
use crate::collect::kernel::{KernelCounts, Lost, UserFilter};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained.
///
/// Wakeups are bursty in exactly the way that fills a ring: a thread pool being
/// released produces hundreds in a microsecond. Draining often is cheap, and
/// what it cannot absorb is counted rather than hidden.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// The ring the program writes into. Each record is 48 bytes plus an 8-byte
/// header, so this holds a few thousand wakeups between drains.
const RING_BYTES: usize = 256 * 1024;

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// The request's predicate, applied here per drained wakeup: the resolver
    /// credits a `bpf` backend with filtering in the kernel and then leaves its
    /// events alone, so a predicate this collector ignored would run nowhere
    /// (stream S's finding, `docs/fixes/silent.md`).
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot observe wakeups: a wakeup is an event, and sampling one in a \
                 hundred of them would attribute blocked time to whichever waker happened to be \
                 sampled",
                self.mode
            ));
        }
        // A machine-wide run keeps every wakeup on the machine; a process
        // target keeps the ones it takes part in, at either end.
        let filter = super::whose(target, "wakeups are kept by the task at either end")?;
        let mut collector = WakeupCollector::new(filter);
        collector.predicate = UserFilter::new(self.filter, "wakeups");
        Ok(Box::new(collector))
    }
}

/// What one wakeup says, as attributes.
fn attributes(wakeup: &Wakeup) -> Vec<Attr> {
    let context = wakeup.context();
    let mut attrs = vec![
        Attr::uint("count", 1),
        Attr::uint("wakee_tid", wakeup.wakee_tid as u64),
        Attr::uint("wakee_pid", wakeup.wakee_pid as u64),
        // 0 for a wakeup issued from an interrupt, with the interrupt named
        // beside it: the task that was running there is not the one that woke
        // anybody.
        Attr::uint("waker_tid", wakeup.waker_tid as u64),
        Attr::uint("waker_pid", wakeup.waker_pid as u64),
        Attr::text("waker_context", context.as_str()),
        Attr::uint("cpu", wakeup.cpu as u64),
        // The catalog declares the filter field as `pid`, and the event is
        // keyed on the wakee, so `pid` and `comm` are the wakee's. Each end has
        // its own name beside them, because a reader naming threads from
        // either end must not have to know which end the bare one is.
        Attr::uint("pid", wakeup.wakee_pid as u64),
        Attr::text("comm", wakeup.wakee_comm()),
        Attr::text("wakee_comm", wakeup.wakee_comm()),
    ];
    if context.is_interrupt() {
        // Not the waker: the task the interrupt was served on, which for a
        // softirq served in its own `local_bh_enable` is the one that caused
        // it, and otherwise only where the interrupt landed.
        attrs.push(Attr::uint("served_in_tid", wakeup.served_in_tid as u64));
        attrs.push(Attr::text("served_in_comm", wakeup.current_comm()));
        if let Some(irq) = wakeup.irq() {
            attrs.push(Attr::uint("irq", irq as u64));
        }
        if let Some(vector) = wakeup.softirq() {
            attrs.push(Attr::text("softirq", vector));
        }
    } else {
        attrs.push(Attr::text("waker_comm", wakeup.current_comm()));
    }
    attrs
}

pub struct WakeupCollector {
    /// Whose wakeups are kept, at either end.
    filter: Whose,
    program: Option<WakeupProgram>,
    losses: Losses,
    /// The program\'s loss counters, as last read (`collect::kernel`).
    counts: KernelCounts,
    /// Read from the program's counter at `stop`, when the map is still open.
    dropped: u64,
    /// The request's predicate, and what it kept and removed.
    predicate: UserFilter,
    /// Each woken process's parent, for `ppid`.
    parents: super::Parents,
    /// Wakeups issued from an interrupt, whose waker is the interrupt.
    from_interrupts: u64,
    /// Set once the program is loaded on a kernel whose preempt count it could
    /// not find, where an interrupt's wakeup is still the interrupted task's.
    interrupts_blind: bool,
    /// Why an interrupt's wakeup carries no `irq` or `softirq`, when the
    /// programs that read them could not be loaded.
    interrupts_unnamed: Option<String>,
}

impl WakeupCollector {
    pub fn new(filter: Whose) -> Self {
        Self {
            filter,
            program: None,
            losses: Losses::default(),
            counts: KernelCounts::default(),
            dropped: 0,
            predicate: UserFilter::new(None, "wakeups"),
            parents: super::Parents::default(),
            from_interrupts: 0,
            interrupts_blind: false,
            interrupts_unnamed: None,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let mut index = seq;
        let mut undecoded = 0;
        let predicate = self.predicate.expression();
        let parents = &mut self.parents;
        let (mut kept, mut removed, mut from_interrupts) = (0u64, 0u64, 0u64);
        program.drain(
            |wakeup: Wakeup| {
                if wakeup.context().is_interrupt() {
                    from_interrupts += 1;
                }
                let mut event = Event {
                    ts_ns: wakeup.ts_ns,
                    ts_err_ns: 0,
                    kind: "sched.wakeup".to_owned(),
                    // The woken task is the series: a wakeup is about the task
                    // that stopped being blocked. The waker is an attribute of
                    // that event, which is what the join reads.
                    keys: vec![KeyValue::task(wakeup.wakee_tid as i32)],
                    attrs: attributes(&wakeup),
                    collector_id: CollectorId::unstamped(),
                    seq: index,
                };
                // The woken task's parent, as the other sched sources give it.
                if let Some(ppid) = parents.of(wakeup.wakee_pid) {
                    event.attrs.push(Attr::uint("ppid", u64::from(ppid)));
                }
                if let Some(predicate) = predicate {
                    if !crate::filter::matches(predicate, &event) {
                        removed += 1;
                        return;
                    }
                    kept += 1;
                }
                events.push(event);
                index += 1;
            },
            &mut undecoded,
        );
        self.predicate.tally(kept, removed);
        self.from_interrupts += from_interrupts;
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the wakeup ring was shorter than one wakeup");
        }
    }

    /// Reads the program's own drop counter, which is the only place a wakeup
    /// the ring could not hold is recorded.
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
                 processes it started was full, so {} wakeups are not here",
                if unfollowed == 1 { "" } else { "es" },
                if unfollowed == 1 { "was" } else { "were" },
                if unfollowed == 1 { "its" } else { "their" },
            ));
        }
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses
                .note("the wakeup ring was full: wakeups happened faster than they were drained");
            self.dropped = dropped;
        }
    }
}

impl Collector for WakeupCollector {
    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        let program = WakeupProgram::load(self.filter, RING_BYTES)?;
        self.interrupts_blind = !program.interrupts_told_apart();
        self.interrupts_unnamed = program.interrupts_unnamed().map(str::to_owned);
        self.program = Some(program);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_drops();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        // Whatever the program published between the last drain and the end of
        // the run belongs to the run. The counter is read before the program is
        // dropped, because dropping it closes the map it lives in.
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

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.interrupts_blind {
            notes.push(
                "this kernel's preempt count is not where the program looks for it (x86's \
                 `__preempt_count` or `pcpu_hot`), so a wakeup issued from an interrupt is \
                 credited to whatever task the interrupt landed on, and for a target it is kept \
                 as that task's"
                    .to_owned(),
            );
        } else if self.from_interrupts > 0 {
            notes.push(format!(
                "{} wakeup{} issued from an interrupt, softirq or NMI; the waker is the \
                 interrupt (`waker_tid` 0, `waker_context` says which), not the task it landed \
                 on, and a target keeps such a wakeup only as its wakee",
                self.from_interrupts,
                if self.from_interrupts == 1 {
                    " was"
                } else {
                    "s were"
                },
            ));
        }
        if let (false, Some(reason)) = (self.interrupts_blind, &self.interrupts_unnamed) {
            notes.push(format!(
                "an interrupt's wakeup carries no `irq` or `softirq`: the programs that say which \
                 interrupt a cpu is in were not loaded ({reason})"
            ));
        }
        notes.extend(self.predicate.note());
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This process by the kernel's id, which the program compares, and which
    /// in a container is not the one `std::process::id()` returns.
    fn own_kernel_tgid() -> u32 {
        crate::bpf::pidns::kernel_tgid(std::process::id()).expect("this process")
    }

    /// A refusal is the answer for a mode that cannot mean anything here, and
    /// it has to say why: sampling one wakeup in a hundred would attribute a
    /// block to whichever waker happened to be caught, which reads as a result.
    #[test]
    fn a_sampled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Sample {
                freq: "997hz".parse().expect("a frequency"),
            },
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("a sampled mode must be refused");
        };
        assert!(error.contains("cannot observe wakeups"), "{error}");
    }

    /// A target that names no process and is not the machine cannot be
    /// filtered on, and saying which target it was is what tells the reader
    /// what to pass instead.
    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Iface("eth0".to_owned())) else {
            panic!("an interface names no process to filter on");
        };
        assert!(error.contains("iface"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// The machine-wide target is not a missing filter, it is the absence of
    /// one, and the collector must carry that distinction into the program it
    /// builds.
    #[test]
    fn the_machine_target_filters_on_nothing_and_a_pid_target_filters_on_it() {
        let _collector = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        })
        .attach(&Target::System)
        .expect("a machine-wide collector");

        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        let _collector = prepared
            // A process that exists: inside a container a pid is translated to the
            // kernel's, and one nobody has is refused there.
            .attach(&Target::Pid(vec![std::process::id() as i32]))
            .expect("a per-process collector");
    }

    /// Whether this machine lets the tests load a program at all. Without the
    /// capabilities there is nothing to test here, and saying so beats a test
    /// that passes because it did nothing.
    fn skip_without_bpf(collector: &mut WakeupCollector) -> bool {
        match collector.start() {
            Ok(()) => false,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                true
            }
        }
    }

    /// Ground truth: a process that sleeps N times must be woken at least N
    /// times, because a timer expiring is a wakeup and there is no other way
    /// back onto the cpu.
    #[test]
    fn a_task_that_sleeps_is_woken_once_per_sleep() {
        const SLEEPS: usize = 20;
        let mut collector = WakeupCollector::new(Whose::Process(own_kernel_tgid()));
        if skip_without_bpf(&mut collector) {
            return;
        }

        for _ in 0..SLEEPS {
            std::thread::sleep(StdDuration::from_millis(2));
        }

        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());

        let woken = events
            .iter()
            .filter(|event| {
                event.attr("wakee_pid").and_then(|value| value.as_f64())
                    == Some(f64::from(own_kernel_tgid()))
            })
            .count();
        assert!(
            woken >= SLEEPS,
            "{SLEEPS} sleeps must produce at least {SLEEPS} wakeups, saw {woken}"
        );
        assert!(collector.losses().is_clean(), "{:?}", collector.losses());
    }

    /// A wakeup carries who did the waking, and for a self-inflicted timer the
    /// waker is the kernel's softirq context rather than the sleeping thread.
    /// What must hold for the join is that both ends are present and plausible.
    #[test]
    fn both_ends_of_a_wakeup_are_recorded() {
        let mut collector = WakeupCollector::new(Whose::Process(own_kernel_tgid()));
        if skip_without_bpf(&mut collector) {
            return;
        }
        std::thread::sleep(StdDuration::from_millis(5));
        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());

        let event = events.first().expect("at least one wakeup");
        for attribute in ["waker_tid", "waker_pid", "wakee_tid", "wakee_pid"] {
            assert!(
                event.attr(attribute).is_some(),
                "a wakeup without `{attribute}` cannot be joined"
            );
        }
        assert_eq!(
            event.key(crate::request::KeyKind::Task),
            Some(&KeyValue::task(
                event.attr("wakee_tid").unwrap().as_f64().unwrap() as i32
            )),
            "the series key is the task that was woken"
        );
    }

    /// The timestamps must be in the tool's clock domain, or every join across
    /// collectors is silently wrong.
    #[test]
    fn wakeup_timestamps_are_in_the_tools_clock_domain() {
        let mut collector = WakeupCollector::new(Whose::Process(own_kernel_tgid()));
        if skip_without_bpf(&mut collector) {
            return;
        }
        std::thread::sleep(StdDuration::from_millis(5));
        let events = collector.sample(0).unwrap();
        let after = crate::clock::monotonic_ns();
        let Some(event) = events.first() else {
            panic!("a sleeping thread is woken at least once");
        };
        assert!(
            event.ts_ns <= after && after - event.ts_ns < 5_000_000_000,
            "a wakeup at {} against a monotonic clock at {after} is a different clock",
            event.ts_ns
        );
    }
}
