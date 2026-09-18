//! `sched.switches` over `perf_event_open`.
//!
//! The degraded backend of the pair, and worth having anyway: it needs no
//! privileges beyond what a normal machine already grants, so a context-switch
//! rate is available where the eBPF path is refused. What it gives up is the
//! second party. `PERF_COUNT_SW_CONTEXT_SWITCHES` fires for the task being
//! observed; it does not say what ran next, and it does not distinguish a task
//! that blocked from one whose slice ran out. That distinction is the whole
//! diagnosis, which is why `bpf:tp` is ahead of it in the chain.

use std::time::Duration as StdDuration;

use perf_event_open_sys::bindings as perf;

use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::perf::{Record, SampledEvent, Scope};
use crate::request::{Mode, Target};

const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// Switches are far rarer than page faults, so a smaller ring is enough.
const RING_PAGES: usize = 64;

pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "counting the switches of a whole machine needs per-cpu events and the \
                 privileges for them; a {} target names no process to follow",
                target.kind()
            )
        })?;
        Ok(Box::new(SwitchCollector::new(pid, self.0)))
    }
}

pub struct SwitchCollector {
    id: CollectorId,
    pid: i32,
    mode: Mode,
    event: Option<SampledEvent>,
    losses: Losses,
}

impl SwitchCollector {
    pub fn new(pid: i32, mode: Mode) -> Self {
        Self {
            id: CollectorId::new("sched.switches/pmu:sw"),
            pid,
            mode,
            event: None,
            losses: Losses::default(),
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(event) = self.event.as_mut() else {
            return;
        };
        let id = self.id.clone();
        let mut index = seq;
        let mut dropped = 0u64;
        let mut throttles = 0u64;
        let mut undecoded = 0u64;
        event.drain(|record| match record {
            Record::Sample(sample) => {
                events.push(Event {
                    ts_ns: sample.time_ns,
                    ts_err_ns: 0,
                    kind: "sched.switches".to_owned(),
                    keys: vec![KeyValue::task(sample.tid as i32)],
                    attrs: vec![
                        Attr::uint("count", 1),
                        // Where the task was when it left the cpu. Without a
                        // stack this is one frame, but it is the frame that
                        // blocked.
                        Attr::uint("ip", sample.ip),
                    ],
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
        // throttled periods and labelled as such. A run that hit it is not
        // comparable to one that did not.
        if throttles > 0 {
            self.losses.dropped += throttles;
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

        let event = SampledEvent::open_software(
            perf::PERF_COUNT_SW_CONTEXT_SWITCHES as u64,
            period,
            Scope::Process(self.pid),
            RING_PAGES,
        )
        .map_err(|error| {
            format!(
                "perf_event_open for context switches on pid {} was refused ({error}); \
                 check /proc/sys/kernel/perf_event_paranoid",
                self.pid
            )
        })?;
        event.enable().map_err(|error| error.to_string())?;
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

        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());
        let after = kernel_switches().expect("the file was readable a moment ago");

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
        // The two counters are not read at the same instant, and cannot be: the
        // reference is read by opening a file, which is itself liable to switch
        // off the cpu. Each edge of the window is worth a switch or two that one
        // side counted and the other had not yet. That is the only slack given;
        // the ratio bound below is what a collector returning a constant fails.
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

    /// The perf event follows a task. A target that names none is refused with
    /// the reason and the target's own name, rather than being served a
    /// machine-wide number that would read as this process's.
    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let Err(error) = Box::new(Prepare(Mode::Trace)).attach(&Target::System) else {
            panic!("a system target names no process for a per-task event");
        };
        assert!(error.contains("system"), "{error}");
        assert!(error.contains("names no process"), "{error}");
    }

    #[test]
    fn counting_mode_is_refused_with_what_would_make_it_work() {
        let error = SwitchCollector::new(0, Mode::Count).start().unwrap_err();
        assert!(error.contains("--mode trace"), "{error}");
    }
}
