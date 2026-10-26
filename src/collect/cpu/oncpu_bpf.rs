//! `cpu.oncpu_time` through `bpf:raw_tp`: each thread's on-cpu time, to the
//! nanosecond the scheduler keeps, polled.
//!
//! The program (`src/bpf/oncpu.rs`) adds up, per thread, the increments the
//! scheduler makes to `se.sum_exec_runtime` -- the number
//! `/proc/<tid>/schedstat` publishes -- and every poll drains what was added
//! since the last one, one event per thread that ran. Where `fs:proc` keys a process's total `task:<pid>` in
//! ticks, this keys each thread `task:<tid>` in nanoseconds, with the process
//! as the `pid` attribute: summing the events of one poll gives the
//! process's total, and nothing has to be rounded to get there.

use crate::bpf::{OnCpu, OnCpuProgram, Whose};
use crate::clock::monotonic_ns;
use crate::collect::kernel::{KernelCounts, Lost};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// The request's predicate, applied here per drained row: the resolver
    /// credits a `bpf` backend with filtering, and this program filters by
    /// target only.
    pub filter: Option<FilterExpr>,
    /// Through the classic `sched:sched_stat_runtime` (`bpf:tp`), for a
    /// kernel without BTF.
    pub classic: bool,
}

impl Prepare {
    /// Refuses a mode the program does not serve before anything is launched.
    pub fn new(mode: Mode, filter: Option<FilterExpr>) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } => Ok(Self {
                mode,
                filter,
                classic: false,
            }),
            other => Err(refusal(other)),
        }
    }
}

/// The program adds increments up in the kernel and is drained; one event
/// per increment would be a stream nobody asked to pay for.
fn refusal(mode: Mode) -> String {
    format!(
        "`cpu.oncpu_time` through its eBPF program adds each thread's time up in the kernel and is \
         drained every poll, which is `--mode poll`; `{mode}` would be one event per slice, \
         which no backend of this build records"
    )
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Poll { .. }) {
            return Err(refusal(self.mode));
        }
        let whose = Whose::of(target).map_err(|reason| {
            format!(
                "{reason}, and on-cpu time belongs to a task; name a process with --pid, a \
                 thread with --tid, a cgroup with --cgroup, or use --system"
            )
        })?;
        let mut collector = OnCpuThreads::new(whose, self.filter);
        collector.classic = self.classic;
        Ok(Box::new(collector))
    }
}

pub struct OnCpuThreads {
    whose: Whose,
    /// Whether the program goes on the classic tracepoint.
    classic: bool,
    program: Option<OnCpuProgram>,
    predicate: Option<FilterExpr>,
    losses: Losses,
    /// The program's loss counters, as last read (`collect::kernel`).
    counts: KernelCounts,
    dropped: u64,
    /// When the last drain happened, which is where the next poll's window
    /// begins.
    last_ns: u64,
    filtered_out: u64,
}

impl OnCpuThreads {
    pub fn new(whose: Whose, predicate: Option<FilterExpr>) -> Self {
        Self {
            whose,
            classic: false,
            program: None,
            predicate,
            losses: Losses::default(),
            counts: KernelCounts::default(),
            dropped: 0,
            last_ns: 0,
            filtered_out: 0,
        }
    }

    fn event(&self, row: &OnCpu, ts_ns: u64, window_ns: u64, seq: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "cpu.oncpu_time".to_owned(),
            keys: vec![KeyValue::task(row.tid as i32)],
            attrs: attributes(row, window_ns),
            collector_id: CollectorId::unstamped(),
            seq,
        }
    }

    /// Drains the rows and turns them into this poll's events.
    fn drain(&mut self, seq: u64) -> Vec<Event> {
        let Some(program) = self.program.as_ref() else {
            return Vec::new();
        };
        let drained = program.drain();
        let now = monotonic_ns();
        let window_ns = now.saturating_sub(self.last_ns);
        self.last_ns = now;
        let (mut rows, drain_losses) = match drained {
            Ok(drained) => drained,
            Err(reason) => {
                self.losses.failed_reads += 1;
                self.losses.note_times(reason, 1);
                return Vec::new();
            }
        };
        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("an on-cpu row was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the on-cpu map held more rows than one drain can walk");
        }
        rows.sort_by_key(|row| row.tid);
        let mut events = Vec::with_capacity(rows.len());
        for row in &rows {
            let event = self.event(row, now, window_ns, seq);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += 1;
                    continue;
                }
            }
            events.push(event);
        }
        events
    }

    fn absorb_counters(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        self.counts
            .absorb(&mut self.losses, [Lost::skipped_runs(program.skipped())]);
        let unfollowed = program.unfollowed();
        if unfollowed > 0 {
            self.losses.note(format!(
                "{unfollowed} process(es) the target started were not followed: the map of \
                 the processes it started was full, so their time is in no row"
            ));
        }
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note_times(
                "more threads ran in one poll than the row map holds, so their time was \
                 never added up",
                dropped - self.dropped,
            );
            self.dropped = dropped;
        }
    }
}

/// What an event carries beside its key.
fn attributes(row: &OnCpu, window_ns: u64) -> Vec<Attr> {
    vec![
        Attr::uint("ns", row.ns),
        Attr::uint("updates", row.updates),
        // The scheduler's own nanoseconds: nothing here is rounded.
        Attr::uint("resolution_ns", 1),
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", u64::from(row.tid)),
        Attr::uint("pid", u64::from(row.tgid)),
        Attr::text("comm", row.comm.clone()),
    ]
}

impl Collector for OnCpuThreads {
    fn value_attr(&self) -> &'static str {
        "ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(if self.classic {
            OnCpuProgram::load_classic(self.whose)?
        } else {
            OnCpuProgram::load(self.whose)?
        });
        self.last_ns = monotonic_ns();
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let events = self.drain(seq);
        self.absorb_counters();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let events = self.drain(u64::MAX / 2);
        self.absorb_counters();
        self.program = None;
        events
    }

    fn tick(&self) -> Option<std::time::Duration> {
        None
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.filtered_out > 0 {
            notes.push(format!(
                "the filter removed {} row{}; it ran here, over the drained rows, not in the \
                 kernel",
                self.filtered_out,
                if self.filtered_out == 1 { "" } else { "s" },
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> OnCpu {
        OnCpu {
            tid: 11,
            tgid: 10,
            ns: 2_500_000,
            updates: 4,
            comm: "spinner".to_owned(),
        }
    }

    /// `poll` is this backend's one mode, refused in the factory -- before the
    /// launch -- and in the attach alike.
    #[test]
    fn a_mode_other_than_poll_is_refused_before_anything_starts() {
        let error = Prepare::new(Mode::Trace, None)
            .err()
            .expect("trace is refused");
        assert!(error.contains("--mode poll"), "{error}");
        let poll = Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        };
        assert!(Prepare::new(poll, None).is_ok());
        assert!(Box::new(Prepare::new(poll, None).unwrap())
            .attach(&Target::System)
            .is_ok());
    }

    /// Every filter field the catalog lets this backend take is on the event.
    #[test]
    fn every_filter_field_this_backend_declares_is_on_an_event() {
        let attributes = attributes(&row(), 1);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let entry = catalog
            .get(&"cpu.oncpu_time".parse().expect("a source id"))
            .expect("cpu.oncpu_time is in the catalog");
        let backend = entry
            .backends
            .iter()
            .find(|backend| backend.id == "bpf:raw_tp")
            .expect("the program's backend");
        let fields = backend
            .filter_fields
            .clone()
            .unwrap_or_else(|| entry.filter_fields.clone());
        assert!(!fields.is_empty());
        for field in fields {
            assert!(
                attributes.iter().any(|attr| attr.name == field),
                "the catalog offers a `{field}` filter on this backend and no event carries one"
            );
        }
    }

    /// The value is the event's `ns`, keyed by the thread, with the process
    /// beside it.
    #[test]
    fn an_event_is_keyed_by_its_thread_and_carries_its_process() {
        let collector = OnCpuThreads::new(Whose::Any, None);
        let event = collector.event(&row(), 5, 1, 0);
        assert_eq!(event.keys, vec![KeyValue::task(11)]);
        let uint = |name: &str| {
            event
                .attrs
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
        };
        assert_eq!(uint(collector.value_attr()), Some(2_500_000.0));
        assert_eq!(uint("pid"), Some(10.0));
        assert_eq!(uint("updates"), Some(4.0));
    }
}
