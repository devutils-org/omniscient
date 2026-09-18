//! `mem.faults` over `perf_event_open`.
//!
//! Every page fault is a sample, and every sample carries the address that
//! faulted. That address is the whole point: it is what lets a fault be
//! attributed to the allocation whose range contains it, which is the only way
//! to answer "which allocation actually cost memory" rather than "how much
//! memory exists".
//!
//! The kernel pushes these into a ring buffer, so this collector streams: it
//! asks to be drained often, and reports what the ring dropped when it was not
//! drained often enough.

use std::time::Duration as StdDuration;

use perf_event_open_sys::bindings as perf;

use super::{Collector, Losses, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::perf::{Record, SampledEvent, Scope};
use crate::request::Mode;

/// How often the ring is drained.
///
/// Page faults arrive in bursts -- a program that touches a fresh arena
/// produces thousands in a millisecond -- so the ring is sized generously and
/// drained often. When that is still not enough, the loss counter says so
/// instead of the numbers quietly shrinking.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// Data pages to ask for. `perf_event_mlock_kb` usually caps an unprivileged
/// process well below this, and the ring shrinks to what the machine allows.
const RING_PAGES: usize = 512;

pub struct PageFaultCollector {
    id: CollectorId,
    pid: i32,
    mode: Mode,
    event: Option<SampledEvent>,
    /// Counted here rather than in the ring so it survives `stop`.
    losses: Losses,
}

impl PageFaultCollector {
    pub fn new(pid: i32, mode: Mode) -> Self {
        Self {
            id: CollectorId::new("mem.faults/pmu:sw"),
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
        let mut undecoded = 0u64;
        event.drain(|record| match record {
            Record::Sample(sample) => {
                events.push(Event {
                    ts_ns: sample.time_ns,
                    ts_err_ns: 0,
                    kind: "mem.faults".to_owned(),
                    keys: vec![
                        // First key is the series key, so faults group by the
                        // thread that took them rather than by address, which
                        // would make one series per page.
                        KeyValue::task(sample.tid as i32),
                        // The faulting address, as a one-byte range: an
                        // interval join asks which allocation contains it.
                        KeyValue::Mem {
                            base: sample.addr,
                            len: 1,
                        },
                    ],
                    attrs: vec![
                        Attr::uint("count", 1),
                        Attr::uint("addr", sample.addr),
                        Attr::uint("ip", sample.ip),
                    ],
                    collector_id: id.clone(),
                    seq: index,
                });
                index += 1;
            }
            Record::Lost { count } => dropped += count,
            Record::Throttle { .. } => {}
            Record::Other { .. } => undecoded += 1,
        });
        if dropped > 0 {
            self.losses.dropped += dropped;
            self.losses
                .note("the page-fault ring overflowed: faults happened faster than they were read");
        }
        self.losses.undecoded += undecoded;
    }
}

impl Collector for PageFaultCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        // `trace` means every fault, which is what an interval join needs: a
        // sampled subset would attribute a fraction of the pages and look like
        // an allocation that cost less than it did.
        let period = match self.mode {
            Mode::Trace => 1,
            Mode::Sample { freq } => freq.as_hz(),
            other => return Err(format!("`{other}` mode does not sample page faults")),
        };

        let event = SampledEvent::open_software(
            perf::PERF_COUNT_SW_PAGE_FAULTS as u64,
            period,
            Scope::Process(self.pid),
            RING_PAGES,
        )
        .map_err(|error| {
            format!(
                "perf_event_open for page faults on pid {} was refused ({error}); \
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
        // Whatever the kernel wrote between the last drain and now is still
        // part of the run; dropping it would silently truncate the tail.
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

    #[test]
    fn faults_are_reported_at_the_address_that_faulted() {
        let mut collector = PageFaultCollector::new(0, Mode::Trace);
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses unprivileged perf sampling");
            return;
        }

        const PAGES: usize = 64;
        let page = 4096;
        let mut block = vec![0u8; PAGES * page];
        let base = block.as_ptr() as u64;
        for index in 0..PAGES {
            block[index * page] = 1;
        }
        std::hint::black_box(&block);

        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());

        let inside = events
            .iter()
            .filter(|event| match event.key(crate::request::KeyKind::Mem) {
                Some(KeyValue::Mem { base: addr, .. }) => {
                    (base..base + (PAGES * page) as u64).contains(addr)
                }
                _ => false,
            })
            .count();
        assert!(
            inside as f64 >= 0.9 * PAGES as f64,
            "only {inside} of {} faults landed in the block that was touched",
            events.len()
        );
        assert!(collector.losses().is_clean(), "{:?}", collector.losses());
    }
}
