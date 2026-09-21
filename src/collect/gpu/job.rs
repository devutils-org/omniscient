//! `gpu.job` over the DRM scheduler's classic tracepoints.
//!
//! The measurement is how long one command buffer spent between the scheduler
//! handing it to the hardware and its fence signalling: a submission, IRQ
//! included. It is not a kernel's execution time and cannot say whether the
//! GPU was saturating its shader cores or waiting on memory -- the catalog
//! entry's own caveat says so -- but it is the only number in this tool that
//! comes from the GPU's own timeline rather than from a driver's sampled
//! summary, which is what every `vendor:nvml` source in this family is.
//!
//! **This collector has never attached.** The machine this repository is
//! developed on runs the NVIDIA proprietary driver, which has no DRM scheduler
//! and publishes neither tracepoint; its tracefs is also unreadable by the uid
//! the tool runs as, which is the second of the two reasons `omn gpu.job`
//! answers *this machine cannot serve it* here. So what follows is written
//! against the kernel's own definitions and checked everywhere it can be --
//! the record layout, the refusals, the event shape -- and it is labelled as
//! unexercised in `docs/families/gpu.md` rather than reported as working.
//!
//! **What `ring` is, and is not.** The catalog offers `ring` as a filter
//! field. The tracepoint's own ring name is a `__data_loc` string, which needs
//! a variable-offset read this eBPF layer does not have, so this build takes
//! the name of the kernel thread that ran the job instead: on every driver
//! with a DRM scheduler that thread is named after the ring it serves. The
//! events say both, under `ring` and `comm`, and they are the same string.

use std::time::Duration as StdDuration;

use crate::bpf::{GpuJob, GpuJobDrops, GpuJobProgram};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often finished jobs are drained.
///
/// A GPU retires thousands of command buffers a second, and each is its own
/// row here rather than a bucket -- the catalog gives this source a join key
/// per submission, and an aggregate has no sequence number to join on. So the
/// interval is short enough to keep the map far from full.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot measure GPU jobs: a job's latency is the gap between its \
                 submission and its fence, and a sample that catches one of them without the \
                 other has no duration to report",
                self.mode
            ));
        }
        if !matches!(target, Target::System) {
            // The catalog declares a `pid` target for this source and this
            // build cannot serve one, which is a gap rather than a nonsense
            // request -- so the refusal says which it is.
            return Err(crate::collect::not_built(format!(
                "a {} target cannot be served for `gpu.job` by this build: `drm_run_job` fires \
                 in the DRM scheduler's own kernel thread, so the process that submitted the \
                 work is not in the tracepoint's context and there is nothing to compare a pid \
                 against. Attributing a job to a process needs the submission path -- \
                 `drm_sched_job_arm` or the driver's ioctl -- which this build does not attach \
                 to. Use --system",
                target.kind()
            )));
        }
        Ok(Box::new(JobCollector::new(self.filter)))
    }
}

pub struct JobCollector {
    id: CollectorId,
    program: Option<GpuJobProgram>,
    losses: Losses,
    notes: Vec<String>,
    dropped: GpuJobDrops,
    window_opened_ns: u64,
    predicate: Option<FilterExpr>,
    filtered_out: u64,
    filtered_in: u64,
}

impl JobCollector {
    pub fn new(predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("gpu.job/bpf:tp"),
            program: None,
            losses: Losses::default(),
            notes: Vec::new(),
            dropped: GpuJobDrops::default(),
            window_opened_ns: 0,
            predicate,
            filtered_out: 0,
            filtered_in: 0,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.window_opened_ns);
        let (jobs, drain_losses) = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                self.losses.failed_reads += 1;
                self.losses.note(reason);
                return;
            }
        };
        self.window_opened_ns = closed_ns;

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("a finished GPU job was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("more GPU jobs finished in one interval than a drain can walk");
        }

        for (index, job) in jobs.into_iter().enumerate() {
            let event = Event {
                ts_ns: closed_ns,
                ts_err_ns: 0,
                kind: "gpu.job".to_owned(),
                keys: vec![
                    // The fence is the submission's identity and the catalog's
                    // join key. The pair rather than a pointer, because a
                    // fence pointer is recycled and a context and sequence
                    // number are not.
                    KeyValue::Gpu {
                        fence_ctx: job.fence_context,
                        seqno: job.fence_seqno,
                    },
                    // The scheduler thread that ran it, which is the ring.
                    KeyValue::task(job.tid as i32),
                ],
                attrs: attributes(&job, window_ns),
                collector_id: self.id.clone(),
                seq: seq + index as u64,
            };
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += 1;
                    continue;
                }
                self.filtered_in += 1;
            }
            events.push(event);
        }
    }

    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let now = program.dropped();
        for (fresh, before, reason) in [
            (
                now.in_flight_full,
                self.dropped.in_flight_full,
                "more command buffers were on the GPU at once than the in-flight map holds, so \
                 their latency was never timed",
            ),
            (
                now.in_flight_busy,
                self.dropped.in_flight_busy,
                "the kernel declined to record a submission because the in-flight map's own \
                 bucket was already being updated on this cpu, so those jobs were never timed",
            ),
            (
                now.done_full,
                self.dropped.done_full,
                "more jobs finished between two drains than the finished map holds, so their \
                 latency was measured and then thrown away",
            ),
            (
                now.done_busy,
                self.dropped.done_busy,
                "the kernel declined to record a completion because the finished map's own \
                 bucket was already being updated on this cpu",
            ),
            (
                now.backwards,
                self.dropped.backwards,
                "a GPU job's fence signalled before the job was submitted, which is not a \
                 duration; it was refused rather than reported",
            ),
        ] {
            if fresh > before {
                self.losses.dropped += fresh - before;
                self.losses.note(reason);
            }
        }
        self.dropped = now;
    }
}

/// What an event carries beside its value.
///
/// `ring` and `comm` are here because the catalog entry declares them as
/// filter fields. They carry the same string, and what that string is -- the
/// scheduler thread's name rather than the tracepoint's own `name` field -- is
/// this module's opening note.
fn attributes(job: &GpuJob, window_ns: u64) -> Vec<Attr> {
    vec![
        // The value: submission to fence, IRQ included. Not a kernel's time.
        Attr::uint("job_ns", job.duration_ns),
        Attr::uint("window_ns", window_ns),
        Attr::uint("fence_context", job.fence_context),
        Attr::uint("fence_seqno", job.fence_seqno),
        Attr::uint("tid", u64::from(job.tid)),
        Attr::uint("pid", u64::from(job.tgid)),
        Attr::text("comm", job.comm.clone()),
        Attr::text("ring", job.comm.clone()),
    ]
}

impl Collector for JobCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "job_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(GpuJobProgram::load()?);
        self.window_opened_ns = monotonic_ns();
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
        if let Some(program) = self.program.as_ref() {
            let open = program.still_running();
            if open > 0 {
                self.notes.push(format!(
                    "{open} command buffer{} still on the GPU when the run ended; that time is \
                     real and is in no total, because the fence has not signalled",
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

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        // Said with every run rather than left to documentation, because the
        // field is one the catalog offers and a reader filtering on it should
        // know where the string came from.
        notes.push(
            "`ring` is the name of the kernel thread that ran the job, which on a DRM scheduler \
             is named after its ring. The tracepoint's own ring name is a `__data_loc` string \
             and this build's eBPF layer cannot read one"
                .to_owned(),
        );
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} jobs and removed {}; it is applied to drained rows rather \
                 than in the program, so the cost of the ones it removed was still paid",
                self.filtered_in, self.filtered_out
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> GpuJob {
        GpuJob {
            fence_context: 77,
            fence_seqno: 123_456,
            duration_ns: 2_500_000,
            tid: 931,
            tgid: 931,
            comm: "gfx_0.0.0".to_owned(),
        }
    }

    fn prepare(mode: Mode) -> Box<Prepare> {
        Box::new(Prepare { mode, filter: None })
    }

    #[test]
    fn every_mode_but_the_one_the_catalog_declares_is_refused_with_the_reason() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let entry = catalog
            .get(&"gpu.job".parse().expect("a source id"))
            .expect("gpu.job is in the catalog");
        assert_eq!(
            entry.modes.len(),
            1,
            "this test is written against the entry"
        );
        let Err(error) = prepare(Mode::Count).attach(&Target::System) else {
            panic!("a counting mode must be refused");
        };
        assert!(error.contains("cannot measure GPU jobs"), "{error}");
    }

    /// The catalog declares a `pid` target for this source and this build
    /// cannot serve one. That is this build's gap rather than a request that
    /// cannot mean anything, and the two have different exit codes -- so the
    /// refusal has to be marked as the first.
    #[test]
    fn a_pid_target_is_refused_as_this_builds_gap_and_says_what_is_missing() {
        let Err(error) = prepare(Mode::Trace).attach(&Target::Pid(vec![1])) else {
            panic!("this build cannot attribute a job to a process");
        };
        assert!(error.contains("scheduler's own kernel thread"), "{error}");
        assert!(error.contains("drm_sched_job_arm"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// The catalog gives this source a `gpu:fence_ctx+seqno` join key, so the
    /// events have to carry a gpu key with both halves -- an aggregate keyed
    /// by context alone would have nothing to join on.
    #[test]
    fn an_event_carries_the_fences_whole_identity() {
        let collector = JobCollector::new(None);
        let mut events = Vec::new();
        // Built the way `drain_into` builds one, without a program to drain.
        let job = job();
        events.push(Event {
            ts_ns: 1,
            ts_err_ns: 0,
            kind: "gpu.job".to_owned(),
            keys: vec![
                KeyValue::Gpu {
                    fence_ctx: job.fence_context,
                    seqno: job.fence_seqno,
                },
                KeyValue::task(job.tid as i32),
            ],
            attrs: attributes(&job, 1),
            collector_id: collector.id().clone(),
            seq: 0,
        });
        let Some(KeyValue::Gpu { fence_ctx, seqno }) = events[0]
            .keys
            .iter()
            .find(|key| matches!(key, KeyValue::Gpu { .. }))
        else {
            panic!("an event with no gpu key cannot be joined on one");
        };
        assert_eq!((*fence_ctx, *seqno), (77, 123_456));
    }

    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&job(), 100_000_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"gpu.job".parse().expect("a source id"))
            .expect("gpu.job is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty());
        for field in declared {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// `ring` is the scheduler thread's name here, not the tracepoint's own,
    /// and a reader filtering on it is entitled to be told that by the run
    /// rather than by a document.
    #[test]
    fn the_run_says_where_the_ring_name_came_from() {
        let collector = JobCollector::new(None);
        assert!(
            collector
                .notes()
                .iter()
                .any(|note| note.contains("__data_loc")),
            "{:?}",
            collector.notes()
        );
        let attributes = attributes(&job(), 1);
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .map(|attr| attr.value.to_string())
        };
        assert_eq!(of("ring"), of("comm"));
    }

    #[test]
    fn the_value_attribute_is_one_the_events_carry() {
        let collector = JobCollector::new(None);
        let name = collector.value_attr();
        assert!(attributes(&job(), 1).iter().any(|attr| attr.name == name));
    }
}
