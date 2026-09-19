//! `cpu.profile` over `perf_event_open`, with call stacks.
//!
//! This is the source that makes the stack machinery mean something. Every
//! other collector answers "how much"; this one answers "where", and the answer
//! is only worth having if the frames under the leaf are really its callers.
//! So the unwinder's own account of what it could walk travels with the numbers
//! as a degradation rather than being left to be noticed in a flame graph that
//! looks plausible.
//!
//! The clock is `PERF_COUNT_SW_CPU_CLOCK`, whose period counts nanoseconds of
//! *on-cpu* time. A task that is asleep produces no samples, which is what an
//! on-cpu profile means: the count of samples on a stack is proportional to the
//! cpu time spent there, and a request for 199 Hz asks for one sample per
//! 5.025 ms of cpu, not one per 5.025 ms of wall clock.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use perf_event_open_sys::bindings as perf;

use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::emit::STACK_ID_ATTR;
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::perf::{Callchain, Record, SampledEvent, Scope};
use crate::request::{Mode, Target};
use crate::stack::Unwinder;

const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// A sample with a callchain is a word per frame rather than a fixed record, so
/// the same burst needs far more room than it would without stacks.
const RING_PAGES: usize = 256;

/// A frequency in hertz, as a period of cpu nanoseconds.
fn period_for(hz: u64) -> u64 {
    (1_000_000_000 / hz).max(1)
}

pub struct Prepare {
    pub mode: Mode,
    pub callchain: Callchain,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "profiling a whole machine needs one sampling event per cpu and the privileges \
                 for them; a {} target names no process to follow",
                target.kind()
            )
        })?;
        Ok(Box::new(ProfileCollector::new(
            pid,
            self.mode,
            self.callchain,
        )))
    }
}

pub struct ProfileCollector {
    id: CollectorId,
    pid: i32,
    mode: Mode,
    callchain: Callchain,
    event: Option<SampledEvent>,
    /// Built at construction rather than at `start`, because a symbolizer reads
    /// the target's `/proc/<pid>/maps` and those are gone the moment it exits.
    unwinder: Unwinder,
    losses: Losses,
}

impl ProfileCollector {
    pub fn new(pid: i32, mode: Mode, callchain: Callchain) -> Self {
        Self {
            id: CollectorId::new("cpu.profile/pmu:sw"),
            pid,
            mode,
            callchain,
            event: None,
            unwinder: Unwinder::for_process(pid),
            losses: Losses::default(),
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(event) = self.event.as_mut() else {
            return;
        };
        let id = self.id.clone();
        let unwinder = &mut self.unwinder;
        let mut index = seq;
        let mut dropped = 0u64;
        let mut throttles = 0u64;
        let mut undecoded = 0u64;
        event.drain(|record| match record {
            Record::Sample(sample) => {
                let mut attrs = vec![Attr::uint("count", 1), Attr::uint("ip", sample.ip)];
                // Symbolized here, while the target is alive, and stored once:
                // the event carries the identity and the table travels beside
                // the results.
                if let Some(stack) = unwinder.record(&sample) {
                    attrs.push(Attr::uint(STACK_ID_ATTR, stack.0));
                }
                events.push(Event {
                    ts_ns: sample.time_ns,
                    ts_err_ns: 0,
                    kind: "cpu.profile".to_owned(),
                    keys: vec![KeyValue::task(sample.tid as i32)],
                    attrs,
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
                "the profile ring overflowed: samples were taken and never read, so the stacks \
                 here are a biased subset -- the busy phases are the ones that overflow",
            );
        }
        if throttles > 0 {
            self.losses.dropped += throttles;
            self.losses.note(
                "the kernel throttled profile sampling: an unknown number of samples in each \
                 throttled period were not taken (see kernel.perf_event_max_sample_rate)",
            );
        }
        self.losses.undecoded += undecoded;
    }
}

impl Collector for ProfileCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        let period = match self.mode {
            Mode::Sample { freq } => period_for(freq.as_hz()),
            other => {
                return Err(format!(
                    "`{other}` mode does not produce a profile: a profile is a sample rate, so \
                     ask for `--mode sample@99hz`"
                ))
            }
        };

        let event = SampledEvent::open_software_with(
            perf::PERF_COUNT_SW_CPU_CLOCK as u64,
            period,
            Scope::Process(self.pid),
            RING_PAGES,
            self.callchain,
        )
        .map_err(|error| {
            format!(
                "perf_event_open for a cpu-clock profile of pid {} was refused ({error}); \
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

    /// Root first, which is the order [`crate::emit::Results::stacks`] stores
    /// and the `folded` format writes.
    fn stacks(&self) -> BTreeMap<u64, Vec<String>> {
        self.unwinder
            .table()
            .iter()
            .map(|(id, stack)| {
                let frames = stack
                    .frames
                    .iter()
                    .rev()
                    .map(crate::stack::Frame::label)
                    .collect();
                (id.0, frames)
            })
            .collect()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.unwinder.notes();
        if let Some(event) = self.event.as_ref() {
            if event.callchain() == Callchain::Off {
                notes.push(
                    "no unwind mode was asked for, so each sample is one instruction pointer and \
                     there are no stacks to aggregate"
                        .to_owned(),
                );
            }
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frequency the caller asked for is the frequency the kernel is told,
    /// expressed in the unit this event counts in.
    #[test]
    fn a_frequency_becomes_a_period_of_cpu_nanoseconds() {
        assert_eq!(period_for(1000), 1_000_000);
        assert_eq!(period_for(99), 10_101_010);
        // A frequency higher than a sample per nanosecond is not expressible;
        // it clamps rather than dividing to zero, which the kernel refuses.
        assert_eq!(period_for(4_000_000_000), 1);
    }

    /// The whole chain in one test: burn cpu, sample it, and check that what
    /// comes out is stacks with names on them rather than bare addresses.
    ///
    /// The frames asserted are the ones nothing here can fake: the test binary
    /// is the target, so its own symbols must appear. A collector that returned
    /// plausible ids and an empty table passes every self-consistency check and
    /// fails this.
    #[test]
    fn a_profile_of_this_process_names_its_own_frames() {
        let mut collector = ProfileCollector::new(
            0,
            Mode::Sample {
                freq: crate::scalar::Frequency::from_hz(997),
            },
            Callchain::FramePointers,
        );
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses unprivileged perf sampling");
            return;
        }

        // Cpu time, not wall time: this event counts the former, so a sleep
        // here would produce no samples at all.
        let deadline = std::time::Instant::now() + StdDuration::from_millis(300);
        let mut spin = 0u64;
        while std::time::Instant::now() < deadline {
            spin = spin.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        assert!(spin != 1, "the loop must not be optimised away");

        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());

        assert!(
            !events.is_empty(),
            "300ms of cpu at 997hz produced no samples at all"
        );

        let table = collector.stacks();
        if table.is_empty() {
            // A kernel that refuses callchains to an unprivileged task gives
            // samples without them. That is a real configuration, not a bug
            // here, and it is distinguishable from a broken unwinder by the
            // events carrying no stack identity either.
            assert!(
                events
                    .iter()
                    .all(|event| event.attr(STACK_ID_ATTR).is_none()),
                "events name stacks that the table does not have"
            );
            eprintln!("skipped: this kernel gave no callchains");
            return;
        }

        let with_stacks = events
            .iter()
            .filter(|event| event.attr(STACK_ID_ATTR).is_some())
            .count();
        assert!(
            with_stacks * 2 > events.len(),
            "only {with_stacks} of {} samples carried a stack",
            events.len()
        );

        // Every identity an event names is in the table, which is the contract
        // the side table exists to keep.
        for event in &events {
            if let Some(crate::event::AttrValue::Uint(id)) = event.attr(STACK_ID_ATTR) {
                assert!(
                    table.contains_key(id),
                    "event names stack {id}, absent from the table"
                );
            }
        }

        let all: Vec<&String> = table.values().flatten().collect();
        assert!(
            all.iter().any(|frame| frame.contains("omn")
                || frame.contains("profile")
                || frame.contains("main")),
            "no frame of the test binary was symbolized: {:?}",
            &all[..all.len().min(12)]
        );
    }

    /// A per-task event needs a task. A target that names none is refused with
    /// the reason, rather than served this process's own profile.
    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let prepare = Box::new(Prepare {
            mode: Mode::Sample {
                freq: Mode::DEFAULT_SAMPLE_FREQ,
            },
            callchain: Callchain::FramePointers,
        });
        let Err(error) = prepare.attach(&Target::System) else {
            panic!("a system target names no process for a per-task event");
        };
        assert!(error.contains("system"), "{error}");
        assert!(error.contains("names no process"), "{error}");
    }

    /// Every other mode is refused by name with the one that works, rather than
    /// silently profiling at a default rate nobody asked for.
    #[test]
    fn a_mode_that_is_not_sampling_is_refused_with_the_one_that_is() {
        let error = ProfileCollector::new(0, Mode::Trace, Callchain::FramePointers)
            .start()
            .unwrap_err();
        assert!(error.contains("--mode sample@99hz"), "{error}");
    }
}
