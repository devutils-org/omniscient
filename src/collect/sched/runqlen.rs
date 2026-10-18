//! `sched.runqlen`: each cpu's run queue length, as `runqlen` samples it.
//!
//! The program (`src/bpf/sched_runqlen.rs`) counts, per cpu, how many of its
//! 99 Hz ticks found each queue length. A drain reads every cpu's slots and
//! publishes what grew since the last one: one event per cpu and length that
//! was seen, `len` the value and `ticks` its weight, so `-e hist` is runqlen's
//! histogram and `-k cpu -e summary` each cpu's mean queue. `sched.runqueue`
//! remains the machine's sum, read from `/proc/stat`.

use std::time::Duration as StdDuration;

use crate::bpf::{RunqLenProgram, RUNQ_LENGTHS};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::emit::WEIGHT_ATTR;
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

/// How often the histogram is read.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_secs(1);

pub struct Prepare {
    pub mode: Mode,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Sample { .. }) {
            return Err(format!(
                "`{}` mode cannot read a run queue's length: it is sampled on each cpu's clock, \
                 at {} Hz; ask for `-m sample`",
                self.mode,
                crate::bpf::sched_runqlen_hz()
            ));
        }
        let cpus = match target {
            Target::System => None,
            Target::Cpu(set) => Some(set.cpus().to_vec()),
            other => {
                return Err(format!(
                    "a run queue belongs to a cpu, not to a {}; use --system or --cpu",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(RunqLen {
            id: CollectorId::new("sched.runqlen/bpf:perf_event"),
            cpus,
            program: None,
            previous: Vec::new(),
            losses: Losses::default(),
            skipped: 0,
        }))
    }
}

pub struct RunqLen {
    id: CollectorId,
    /// The cpus kept, or every one.
    cpus: Option<Vec<u32>>,
    program: Option<RunqLenProgram>,
    /// The ticks at the last drain, `[length][cpu]`.
    previous: Vec<Vec<u64>>,
    losses: Losses,
    skipped: u64,
}

/// One event per cpu and length whose ticks grew, from two readings of the
/// histogram.
fn events(
    before: &[Vec<u64>],
    after: &[Vec<u64>],
    cpus: Option<&[u32]>,
    id: &CollectorId,
    ts_ns: u64,
    seq: u64,
) -> Vec<Event> {
    let mut events = Vec::new();
    for (length, per_cpu) in after.iter().enumerate() {
        for (cpu, ticks) in per_cpu.iter().enumerate() {
            let earlier = before
                .get(length)
                .and_then(|row| row.get(cpu))
                .copied()
                .unwrap_or(0);
            let grew = ticks.saturating_sub(earlier);
            if grew == 0 || cpus.is_some_and(|kept| !kept.contains(&(cpu as u32))) {
                continue;
            }
            let last = length as u32 == RUNQ_LENGTHS - 1;
            let mut attrs = vec![
                Attr::uint("len", length as u64),
                Attr::uint("ticks", grew),
                Attr::uint(WEIGHT_ATTR, grew),
                Attr::uint("cpu", cpu as u64),
            ];
            if last {
                attrs.push(Attr::uint("at_least", 1));
            }
            events.push(Event {
                ts_ns,
                ts_err_ns: 0,
                kind: "sched.runqlen".to_owned(),
                keys: vec![KeyValue::cpu(cpu as u32)],
                attrs,
                collector_id: id.clone(),
                seq: seq + events.len() as u64,
            });
        }
    }
    events
}

impl RunqLen {
    fn drain(&mut self, seq: u64) -> Vec<Event> {
        let Some(program) = self.program.as_ref() else {
            return Vec::new();
        };
        let after = program.ticks();
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note_times(
                "the kernel skipped a tick of the run queue program because one was already \
                 running on that cpu",
                skipped - self.skipped,
            );
            self.skipped = skipped;
        }
        let events = events(
            &self.previous,
            &after,
            self.cpus.as_deref(),
            &self.id,
            monotonic_ns(),
            seq,
        );
        self.previous = after;
        events
    }
}

impl Collector for RunqLen {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "len"
    }

    fn start(&mut self) -> Result<(), String> {
        let program = RunqLenProgram::load()?;
        self.previous = program.ticks();
        self.program = Some(program);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        Ok(self.drain(seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        let events = self.drain(u64::MAX / 2);
        self.program = None;
        events
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        vec![format!(
            "each cpu's cfs run queue looked at {} times a second, on its own clock: a queue \
             that filled and drained between two ticks is not seen, and `len` {} counts every \
             longer queue too",
            crate::bpf::sched_runqlen_hz(),
            RUNQ_LENGTHS - 1
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drain_publishes_what_grew_per_cpu_and_length() {
        let id = CollectorId::new("t");
        // Two lengths, three cpus.
        let before = vec![vec![10, 0, 5], vec![0, 0, 1]];
        let after = vec![vec![110, 0, 5], vec![3, 0, 1]];
        let all = events(&before, &after, None, &id, 1, 0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].attr("ticks").unwrap().as_f64(), Some(100.0));
        assert_eq!(all[1].attr("len").unwrap().as_f64(), Some(1.0));
        let cpu2 = events(&before, &after, Some(&[2]), &id, 1, 0);
        assert!(cpu2.is_empty());
    }

    #[test]
    fn every_declared_filter_field_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the catalog");
        let entry = catalog
            .get(&"sched.runqlen".parse().expect("an id"))
            .expect("sched.runqlen");
        let event = &events(&[], &[vec![1]], None, &CollectorId::new("t"), 1, 0)[0];
        for field in &entry.filter_fields {
            assert!(event.attr(field).is_some(), "{field}");
        }
    }
}
