//! `sched.switches` over `perf_event_open`.
//!
//! The first backend of the chain for every target it can follow: an exact
//! count that costs nothing. It needs less than the eBPF path where
//! `perf_event_paranoid` is 1 or lower, and the same `CAP_PERFMON` at the
//! kernel's default of 2, because a context switch happens in kernel mode and
//! a user-mode-only counter of one counts zero (`kernel_mode_counted` in
//! `mod.rs` refuses that). What it gives up is the second party.
//! `PERF_COUNT_SW_CONTEXT_SWITCHES` fires for the task being observed; it does
//! not say what ran next, and it does not distinguish a task that blocked from
//! one whose slice ran out. That distinction is the whole
//! diagnosis, which the `sched_switch` program behind it in the chain
//! (`switch_count.rs`) keeps, in `count` mode.

use std::collections::BTreeSet;
use std::time::Duration as StdDuration;

use perf_event_open_sys::bindings as perf;

use crate::collect::task::{Comms, Tgids};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::perf::{Callchain, Record, SampleSpec, ThreadSampler};
use crate::request::{Mode, Target};

const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// Switches are far rarer than page faults, so a smaller ring is enough. Per
/// cpu: every task followed on a cpu shares that cpu's ring.
const RING_PAGES: usize = 64;

/// What the events are opened on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Followed {
    /// Every thread the process has when the run starts, each inherited into
    /// what it starts afterwards, and the samples kept for the process's own
    /// threads: an inherited event also follows a child process, and the
    /// family's other sources observe the target's process, not its tree.
    Process(i32),
    /// The one task a `--tid` target named (0: the calling thread).
    Task(i32),
    /// Every task on these cpus: `pid = -1`, one event per cpu.
    Cpus(Vec<u32>),
}

pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let followed = match target {
            Target::System => Followed::Cpus(
                crate::perf::online_cpus()
                    .map_err(|error| format!("cannot list the online cpus: {error}"))?,
            ),
            Target::Cpu(set) => Followed::Cpus(set.cpus().to_vec()),
            // A `--tid` target is the one task that was asked for; anything
            // else names a process, which is a task and whatever it starts.
            Target::Tid(tids) => Followed::Task(tids[0]),
            other => Followed::Process(other.first_pid().ok_or_else(|| {
                format!(
                    "a switch event follows tasks or cpus, and a {} target names neither; \
                     name its processes with --pid, or watch the machine with --system",
                    other.kind()
                )
            })?),
        };
        let mut collector = SwitchCollector::new(0, self.0);
        collector.followed = followed;
        Ok(Box::new(collector))
    }
}

pub struct SwitchCollector {
    id: CollectorId,
    followed: Followed,
    mode: Mode,
    event: Option<ThreadSampler>,
    /// `comm` and `pid` are declared filter fields on this source, and the
    /// samples name a thread, so the attributes are looked up per thread.
    comms: Comms,
    tgids: Tgids,
    losses: Losses,
    /// The tasks samples actually arrived from.
    sampled_tasks: BTreeSet<i32>,
    /// Threads an event was opened on, and threads that appeared while they
    /// were being opened, read at `start` and kept: `stop` closes the events
    /// and the notes are gathered afterwards.
    opened: (usize, usize),
    /// Switches of tasks outside the target that the inherited events
    /// followed anyway -- a child process, or a thread of the named task --
    /// and the tasks they came from.
    outside: u64,
    outside_tasks: BTreeSet<i32>,
}

impl SwitchCollector {
    /// A collector over the process `pid` (0: the calling thread).
    pub fn new(pid: i32, mode: Mode) -> Self {
        Self {
            id: CollectorId::new("sched.switches/pmu:sw"),
            followed: if pid == 0 {
                Followed::Task(0)
            } else {
                Followed::Process(pid)
            },
            mode,
            event: None,
            comms: Comms::new(),
            tgids: Tgids::new(),
            losses: Losses::default(),
            sampled_tasks: BTreeSet::new(),
            opened: (0, 0),
            outside: 0,
            outside_tasks: BTreeSet::new(),
        }
    }

    /// Whether a sample is of a task the target names.
    fn keeps(followed: &Followed, pid: u32, tid: u32) -> bool {
        match followed {
            Followed::Process(target) => pid as i32 == *target,
            Followed::Task(0) | Followed::Cpus(_) => true,
            Followed::Task(target) => tid as i32 == *target,
        }
    }

    fn open(&self, spec: &SampleSpec) -> std::io::Result<ThreadSampler> {
        match &self.followed {
            Followed::Process(pid) => ThreadSampler::open_process(spec, *pid, RING_PAGES),
            Followed::Task(tid) => crate::perf::online_cpus()
                .and_then(|cpus| ThreadSampler::open(spec, &[*tid], &cpus, RING_PAGES)),
            Followed::Cpus(cpus) => ThreadSampler::open(spec, &[-1], cpus, RING_PAGES),
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(event) = self.event.as_mut() else {
            return;
        };
        let id = self.id.clone();
        let comms = &mut self.comms;
        let tgids = &mut self.tgids;
        let seen = &mut self.sampled_tasks;
        let followed = &self.followed;
        let (outside, outside_tasks) = (&mut self.outside, &mut self.outside_tasks);
        let mut index = seq;
        let mut dropped = 0u64;
        let mut throttles = 0u64;
        let mut undecoded = 0u64;
        event.drain(|record| match record {
            Record::Sample(sample) if !Self::keeps(followed, sample.pid, sample.tid) => {
                *outside += 1;
                outside_tasks.insert(sample.tid as i32);
            }
            Record::Sample(sample) => {
                seen.insert(sample.tid as i32);
                events.push(Event {
                    ts_ns: sample.time_ns,
                    ts_err_ns: 0,
                    kind: "sched.switches".to_owned(),
                    keys: vec![KeyValue::task(sample.tid as i32)],
                    attrs: {
                        let tid = sample.tid as i32;
                        let mut attrs = vec![
                            Attr::uint("count", 1),
                            // Where the task was when it left the cpu. Without
                            // a stack this is one frame, but it is the frame
                            // that blocked.
                            Attr::uint("ip", sample.ip),
                            Attr::uint("tid", u64::from(sample.tid)),
                        ];
                        // The sample carries its process; a lookup in /proc
                        // is the fallback for a kernel that wrote none.
                        match sample.pid {
                            0 if tid != 0 => {
                                if let Some(pid) = tgids.get(tid) {
                                    attrs.push(Attr::uint("pid", pid as u64));
                                }
                            }
                            pid => attrs.push(Attr::uint("pid", u64::from(pid))),
                        }
                        if let Some(comm) = comms.get(tid) {
                            attrs.push(Attr::text("comm", comm));
                        }
                        if let Some(cpu) = sample.cpu {
                            attrs.push(Attr::uint("cpu", u64::from(cpu)));
                        }
                        attrs
                    },
                    collector_id: id.clone(),
                    seq: index,
                });
                index += 1;
            }
            Record::Lost { count } => dropped += count,
            Record::Throttle { throttled } => throttles += u64::from(throttled),
            Record::Other { .. } => undecoded += 1,
        });
        if dropped > 0 {
            self.losses.dropped += dropped;
            self.losses.note(
                "the context-switch ring overflowed: switches happened faster than they were read",
            );
        }
        // Throttling is the kernel deciding this event costs too much and
        // sampling less of it, up to `kernel.perf_event_max_sample_rate`. It is
        // a loss like any other -- switches happened and are not here -- but
        // one whose size the kernel does not report, so it is counted in
        // throttled periods and labelled as such. Adding it to `dropped`
        // instead, which is what this did while saying otherwise, reports a
        // number of periods as a number of switches and leaves the field a
        // consumer filters on at zero.
        if throttles > 0 {
            self.losses.throttled += throttles;
            self.losses.note(
                "the kernel throttled context-switch sampling: an unknown number of switches in \
                 each throttled period were not sampled (see kernel.perf_event_max_sample_rate)",
            );
        }
        self.losses.undecoded += undecoded;
    }
}

impl Collector for SwitchCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        let period = match self.mode {
            Mode::Trace => 1,
            // A counting request opens no ring and reads the counter at the
            // end. The perf layer does not do that yet, and sampling every
            // switch to then add the samples up would be a different, more
            // expensive measurement wearing the cheap one's name.
            Mode::Count => {
                return Err(
                    "`count` mode for `pmu:sw` needs the perf layer's counting support \
                     (see STATE.md); ask for `--mode trace` to get one event per switch"
                        .to_owned(),
                )
            }
            other => return Err(format!("`{other}` mode does not observe context switches")),
        };

        let spec = SampleSpec::software(
            perf::PERF_COUNT_SW_CONTEXT_SWITCHES as u64,
            period,
            Callchain::Off,
        );
        let event = self.open(&spec).map_err(|error| {
            let what = match &self.followed {
                Followed::Process(pid) => format!("process {pid}"),
                Followed::Task(tid) => format!("task {tid}"),
                Followed::Cpus(cpus) => format!("{} cpus", cpus.len()),
            };
            format!(
                "perf_event_open for context switches on {what} was refused ({error}); \
                 check /proc/sys/kernel/perf_event_paranoid, and for a cpu or the machine, \
                 CAP_PERFMON"
            )
        })?;
        event.enable().map_err(|error| error.to_string())?;
        self.opened = (event.threads().len(), event.unlisted());
        self.event = Some(event);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        if let Some(event) = self.event.as_ref() {
            let _ = event.disable();
        }
        self.drain_into(&mut trailing, u64::MAX / 2);
        if let Some(event) = self.event.as_ref() {
            self.losses.undecoded += event.undecoded();
        }
        self.event = None;
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    /// What the events followed, when it is more than a launched program's
    /// one thread, and what they may have missed -- `mem.faults`'s wording,
    /// over the same `ThreadSampler`.
    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.outside > 0 {
            notes.push(format!(
                "{} switches of {} task{} outside the target -- a process it started, or \
                 another thread of a named task -- were followed by the inherited events and \
                 left out: this family's sources observe the target's process, its threads \
                 included, and not the processes it starts",
                self.outside,
                self.outside_tasks.len(),
                if self.outside_tasks.len() == 1 {
                    ""
                } else {
                    "s"
                },
            ));
        }
        let Followed::Process(pid) = self.followed else {
            return notes;
        };
        let (threads, unlisted) = self.opened;
        let tasks = self.sampled_tasks.len();
        if threads > 1 {
            notes.push(format!(
                "process {pid} was already running {threads} threads when counting began, so \
                 each has an event of its own and each follows what it starts; switches came \
                 from {tasks} task{}",
                if tasks == 1 { "" } else { "s" }
            ));
        }
        if unlisted > 0 {
            notes.push(format!(
                "{unlisted} thread{} of process {pid} appeared while the events were being \
                 opened; each is counted if the thread that started it already had its event, \
                 and not otherwise, and which it was cannot be told from here",
                if unlisted == 1 { "" } else { "s" },
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's own count of switches for this thread, which is what the
    /// measurement is checked against.
    ///
    /// `thread-self`, not `self`: the perf event follows the task that opened
    /// it, and `/proc/self/status` reports the thread group leader, which in a
    /// test binary is asleep while another thread does the work. Reading the
    /// wrong one is how a ground-truth test ends up comparing a number against
    /// a constant zero.
    fn kernel_switches() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/thread-self/status").ok()?;
        let mut total = 0;
        for line in status.lines() {
            if line.starts_with("voluntary_ctxt_switches")
                || line.starts_with("nonvoluntary_ctxt_switches")
            {
                total += line.split_whitespace().last()?.parse::<u64>().ok()?;
            }
        }
        Some(total)
    }

    /// Ground truth against a counter nobody here computed: `/proc/self/status`
    /// counts the same switches from the other side of the kernel. A collector
    /// that returned a plausible constant would pass a self-consistency test
    /// and fail this one.
    #[test]
    fn the_switch_count_agrees_with_the_kernels_own() {
        let mut collector = SwitchCollector::new(0, Mode::Trace);
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses unprivileged perf sampling");
            return;
        }
        let Some(before) = kernel_switches() else {
            eprintln!("skipped: this kernel does not report context switches in /proc");
            return;
        };

        // Every sleep is at least one switch off the cpu and one back on.
        for _ in 0..20 {
            std::thread::sleep(StdDuration::from_millis(2));
        }

        // The perf window is closed before the reference is read, so the two
        // cover the same interval. `stop()` disables the event and *then*
        // drains it, and the drain allocates an `Event` per switch, so under
        // load it is itself preempted repeatedly -- switches `/proc` counts
        // and the event, already disabled, does not. That made this test fail
        // as a threefold shortfall "with nothing reported as lost", which is
        // the one diagnosis it exists to give and here it was the test's own
        // bracketing rather than the collector.
        collector
            .event
            .as_ref()
            .expect("the event is open until stop")
            .disable()
            .expect("disabling an open event");
        let after = kernel_switches().expect("the file was readable a moment ago");

        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());

        let kernel = after - before;
        let measured = events.len() as u64;
        assert!(
            kernel >= 20,
            "twenty sleeps should show in the kernel's own counter, which moved by {kernel}"
        );

        // Either the sampler saw what the kernel counted, or it says what it
        // lost. Both cases are asserted rather than the comparison being
        // loosened until it always passes: a shortfall with nothing reported as
        // lost is the failure this test exists to catch.
        // The two counters are still not read at the same instant, and cannot
        // be: the reference is read by opening a file, which is itself liable
        // to switch off the cpu. Each edge of the window is worth a switch or
        // two that one side counted and the other had not yet. That is the
        // only slack given; the ratio bound below is what a collector
        // returning a constant fails.
        const EDGES: u64 = 4;

        let losses = collector.losses();
        if losses.is_clean() {
            assert!(
                measured <= kernel + EDGES && measured * 10 >= kernel * 8,
                "counted {measured} switches against the kernel's own {kernel}, with nothing \
                 reported as lost"
            );
        } else {
            assert!(
                measured > 0,
                "a run reporting losses must still carry what it did see: {losses:?}"
            );
        }
    }

    /// The events follow tasks or cpus. A target that names neither is
    /// refused with the reason and the target's own name; the machine and a
    /// cpu set are served, one event per cpu.
    #[test]
    fn a_target_without_a_task_or_a_cpu_is_refused_by_name() {
        let Err(error) = Box::new(Prepare(Mode::Trace)).attach(&Target::Iface("lo".into())) else {
            panic!("an interface names no task and no cpu");
        };
        assert!(error.contains("iface"), "{error}");
        assert!(error.contains("--system"), "{error}");
        for target in [
            Target::System,
            Target::Cpu("0".parse().expect("a cpu list")),
        ] {
            assert!(Box::new(Prepare(Mode::Trace)).attach(&target).is_ok());
        }
    }

    #[test]
    fn counting_mode_is_refused_with_what_would_make_it_work() {
        let error = SwitchCollector::new(0, Mode::Count).start().unwrap_err();
        assert!(error.contains("--mode trace"), "{error}");
    }
}
