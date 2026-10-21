//! `sched.runqlat` over two raw tracepoints, bucketed in the kernel.
//!
//! The measurement is the time a task spent *runnable and not running*: it had
//! work to do and the scheduler had not given it a cpu yet. It is the other
//! half of the two ways a program can be slow without burning cycles --
//! `sched.offcpu` is the time it was waiting for something, and this is the
//! time it was waiting for nothing but a turn.
//!
//! No single event says it. A wait opens at `sched_wakeup`, or at the
//! `sched_switch` that preempted a still-runnable task, and closes at the
//! `sched_switch` that gave it a cpu. `src/bpf/runqlat.rs` holds both programs
//! and the map between them; this file drains what they counted.
//!
//! **What comes out is already a histogram.** Each drained row is one log2
//! bucket of one `(task, cpu, priority)`, with how many waits fell into it and
//! their exact total. That is what makes the source affordable at all: a
//! wakeup happens hundreds of thousands of times a second on a loaded machine,
//! and the alternative shape -- one event per wait -- is a measurement whose
//! cost grows with the thing being measured.
//!
//! It is also what the source gives up today. An event here stands for `count`
//! waits, and the emit layer weighs every event as one, so a `hist` built from
//! these events has the right buckets and the wrong heights until it learns to
//! read that attribute. The run says so in its own output rather than leaving
//! it to be discovered; the request for the weight is in
//! `docs/open-findings.md`.

use std::time::Duration as StdDuration;

use crate::bpf::{Renames, RunqLat, RunqLatProgram, Whose};
use crate::clock::monotonic_ns;
use crate::collect::kernel::{drain_rows, Drains, KernelCounts, UserFilter};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the rows are drained.
///
/// The cost of a drain is one syscall per `(task, cpu, priority, bucket)` row
/// that saw a wait in the interval, not per wait, which is the whole point of
/// bucketing in the kernel. Draining often also keeps the map far from the
/// capacity at which it would have to start refusing rows.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// The request's predicate, which this collector applies itself.
    ///
    /// The resolver credits any backend whose id begins with `bpf` with pushing
    /// a filter into the kernel, and then leaves its events alone. That credit
    /// is given from the backend's *name*, so a collector that does nothing
    /// with the predicate returns every event and nothing anywhere says the
    /// filter did not run. Applying it here is what makes the resolver's claim
    /// true of the numbers; what it is not yet true of is the cost, which is
    /// written down in `docs/open-findings.md`.
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot measure run queue latency: a wait is the gap between two \
                 events, and a sample that catches one of them without the other has no \
                 duration to report",
                self.mode
            ));
        }
        // A cpu target is the queue of that cpu: the program watches every
        // task, and the rows are kept for the cpu each wait was served on,
        // which is in the kernel's key.
        if let Target::Cpu(set) = target {
            let mut collector = RunqLatCollector::new(Whose::Any, self.filter);
            collector.cpus = Some(set.cpus().to_vec());
            return Ok(Box::new(collector));
        }
        let filter = super::whose(target, "a run queue wait is attributed per task")?;
        Ok(Box::new(RunqLatCollector::new(filter, self.filter)))
    }
}

pub struct RunqLatCollector {
    /// Whose waits are kept.
    filter: Whose,
    /// The cpus whose queues are kept, for a cpu target; every cpu otherwise.
    cpus: Option<Vec<u32>>,
    /// Waits on the other cpus, which the program measured and the collector
    /// set aside for a cpu target.
    off_target: u64,
    program: Option<RunqLatProgram>,
    losses: Losses,
    /// The program's loss counters, as last read (`collect::kernel`).
    counts: KernelCounts,
    notes: Vec<String>,
    /// Waits the kernel held no queue stamp for, as last read.
    unstamped: u64,
    /// The names tasks took at exec, applied to the rows of the window.
    renames: Option<Renames>,
    /// The end of the last interval, which is the start of this one.
    window_opened_ns: u64,
    /// How many waits this run has bucketed, which is what the event count is
    /// not: each event stands for as many waits as its `count` says.
    waits: u64,
    /// How many rows those waits were reported as, which is the event count.
    rows: u64,
    /// The exact sums of the kept waits, by the kernel's `sched_info` and by
    /// the tracepoints: the run's summary over `wake_to_run_ns`, which is the
    /// quantity `perf sched latency` and `runqlat.bt` report.
    wait_total_ns: u64,
    wake_to_run_total_ns: u64,
    /// Each process's parent, for the rows' `ppid`.
    parents: super::Parents,
    /// The request's predicate, applied to each row as it is drained, its
    /// account kept in waits rather than in rows, because a row is not one
    /// measurement here.
    predicate: UserFilter,
}

impl RunqLatCollector {
    pub fn new(filter: Whose, predicate: Option<FilterExpr>) -> Self {
        Self {
            filter,
            cpus: None,
            off_target: 0,
            program: None,
            losses: Losses::default(),
            counts: KernelCounts::default(),
            notes: Vec::new(),
            unstamped: 0,
            renames: None,
            window_opened_ns: 0,
            waits: 0,
            rows: 0,
            wait_total_ns: 0,
            wake_to_run_total_ns: 0,
            parents: super::Parents::default(),
            predicate: UserFilter::new(predicate, "waits"),
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.window_opened_ns);
        let Some(mut rows) = drain_rows(program, &mut self.losses, "a run queue row") else {
            return;
        };
        self.window_opened_ns = closed_ns;
        self.parents.absorb(program.parents());
        // A task that exec'd in this window is named what it became, not what
        // it was when its row was made; see `bpf::Renames`.
        if let Some(renames) = self.renames.as_mut() {
            let renamed = renames.take();
            for row in rows.iter_mut() {
                if let Some(name) = renamed.get(&row.tid) {
                    row.comm.clone_from(name);
                }
            }
        }

        for (index, row) in rows.into_iter().enumerate() {
            if self
                .cpus
                .as_ref()
                .is_some_and(|cpus| !cpus.contains(&row.cpu))
            {
                self.off_target += row.count;
                continue;
            }
            let mut event = Event {
                // Stamped where the row's first wait began rather than at the
                // drain that read it, which is finding 44: a join that opens on
                // a bucketed source would otherwise open at the window's close.
                // A zero means the kernel recorded no beginning, and falls back
                // to the drain rather than to the beginning of time.
                ts_ns: if row.first_ns == 0 {
                    closed_ns
                } else {
                    row.first_ns
                },
                ts_err_ns: 0,
                kind: "sched.runqlat".to_owned(),
                keys: vec![
                    // The waiting task is the series: this is its wait, not the
                    // machine's.
                    KeyValue::task(row.tid as i32),
                    // The cpu that eventually served it, so a request keyed by
                    // cpu can ask which queue was the long one.
                    KeyValue::cpu(row.cpu),
                ],
                attrs: attributes(&row, window_ns),
                collector_id: CollectorId::unstamped(),
                seq: seq + index as u64,
            };
            if let Some(ppid) = self.parents.of(row.tgid) {
                event.attrs.push(Attr::uint("ppid", u64::from(ppid)));
            }
            if !self.predicate.keep(&event, row.count) {
                continue;
            }
            self.waits += row.count;
            self.rows += 1;
            self.wait_total_ns += row.total_ns;
            self.wake_to_run_total_ns += row.wake_to_run_ns;
            events.push(event);
        }
    }

    /// Reads the programs' own counters of what they could not record.
    ///
    /// A full map is the only way this source loses a wait, and nothing else
    /// anywhere would record that it happened: the helper returns an error in
    /// the kernel and the wait is simply never counted.
    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        self.counts.absorb(&mut self.losses, program.lost());
        let unfollowed = program.unfollowed();
        if unfollowed > 0 {
            self.losses.note(format!(
                "{unfollowed} process{} the target started {} not followed: the map of the \
                 processes it started was full, so {} waits are in no row",
                if unfollowed == 1 { "" } else { "es" },
                if unfollowed == 1 { "was" } else { "were" },
                if unfollowed == 1 { "its" } else { "their" },
            ));
        }
        self.unstamped = program.unstamped();
    }
}

/// What an event carries beside its value.
///
/// `comm`, `pid`, `cpu` and `prio` are here because the catalog entry declares
/// them as filter fields: a predicate over a field no event carries is false
/// everywhere, which empties the series and looks exactly like a machine that
/// never queued.
fn attributes(row: &RunqLat, window_ns: u64) -> Vec<Attr> {
    vec![
        // The value. The low bound of the bucket rather than its mean, because
        // it is the one number that puts the row back in the same bucket when
        // the emit layer buckets it again: a histogram built in the kernel and
        // one built in userspace from these events agree exactly.
        Attr::uint("wait_ns", row.bucket_low_ns()),
        // How many waits this event stands for. Every number derived from
        // `wait_ns` is this many times over, which is what the emit layer reads
        // out of `weight`; `count` carries the same number under the name the
        // filter grammar and a reader expect.
        Attr::uint("count", row.count),
        Attr::uint(crate::emit::WEIGHT_ATTR, row.count),
        // The exact sum of those waits, which the bucket cannot give back: a
        // log2 bucket is a factor of two wide, so a total built from bounds
        // inherits that error and this does not.
        Attr::uint("total_ns", row.total_ns),
        // The same waits by the tracepoints, where `total_ns` is the kernel's
        // own `sched_info` account (`bpf::runqlat`, `VALUE_WAKE_TO_RUN_NS`):
        // the difference is a cpu coming out of idle for the task it was woken
        // for, which the kernel counts as blocked time.
        Attr::uint("wake_to_run_ns", row.wake_to_run_ns),
        Attr::uint("bucket", u64::from(row.bucket)),
        // The interval the row was collected over. Not a denominator: a wait is
        // counted where it ended, so a queue longer than the tick reports
        // nanoseconds the window does not contain.
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", u64::from(row.tid)),
        Attr::uint("pid", u64::from(row.tgid)),
        Attr::text("comm", row.comm.clone()),
        Attr::uint("cpu", u64::from(row.cpu)),
        // Signed: a real-time task's priority is negative, and reported
        // unsigned the most urgent thing on the machine reads as the least.
        Attr::int("prio", i64::from(row.prio)),
    ]
}

/// The run's waits and their two sums, and the means, by name.
fn summary_fields(
    waits: u64,
    wait_total_ns: u64,
    wake_to_run_total_ns: u64,
) -> crate::emit::Fields {
    let mut fields = crate::emit::Fields::from([
        ("waits".to_owned(), waits),
        ("wait_total_ns".to_owned(), wait_total_ns),
        ("wake_to_run_total_ns".to_owned(), wake_to_run_total_ns),
    ]);
    if let (Some(wait), Some(wake)) = (
        wait_total_ns.checked_div(waits),
        wake_to_run_total_ns.checked_div(waits),
    ) {
        fields.insert("wait_mean_ns".to_owned(), wait);
        fields.insert("wake_to_run_mean_ns".to_owned(), wake);
    }
    fields
}

impl Collector for RunqLatCollector {
    fn value_attr(&self) -> &'static str {
        "wait_ns"
    }

    fn row_stamp(&self) -> Option<crate::collect::RowStamp> {
        use crate::collect::RowStamp;
        Some(RowStamp::FirstEvent)
    }

    fn start(&mut self) -> Result<(), String> {
        let program = RunqLatProgram::load(self.filter)?;
        // Said at the start, because `stop` drops the program before the
        // notes are read.
        self.notes
            .extend(program.clock_missing().map(str::to_owned));
        self.program = Some(program);
        // Optional: without it a row keeps the name its task had when the row
        // was made, which for a launched program's first rows is this tool's.
        // A process target is followed into the processes it starts, and
        // those exec: the renames are the machine's then, applied by thread
        // id to the rows of the window, which are the target's alone.
        let renamed = match self.filter {
            Whose::Process(_) => Whose::Any,
            other => other,
        };
        self.renames = match Renames::load(renamed) {
            Ok(renames) => Some(renames),
            Err(reason) => {
                self.notes.push(format!(
                    "a task that exec'd during a window is named what it was when its first \
                     row was made, because the exec program could not be attached: {reason}"
                ));
                None
            }
        };
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
        // A task still on a run queue when the run ended has a real wait that
        // is in no bucket, because it has no end yet. On a machine with more
        // runnable tasks than cpus that is the ordinary state rather than
        // something lost, so it is reported as a fact about the window.
        if let Some(program) = self.program.as_ref() {
            let open = program.still_queued();
            if open > 0 {
                self.notes.push(format!(
                    "{open} task{} still waiting for a cpu when the run ended; the wait it was \
                     in has no end yet and is in no bucket",
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

    /// The run's totals by both accounts, by name: `wake_to_run_mean_ns` is
    /// `perf sched latency`'s average delay and `runqlat.bt`'s mean, which no
    /// emit gives per key yet (`docs/fixes/w22-perfsched.md`, gap 8).
    fn fields(&self) -> crate::emit::Fields {
        summary_fields(self.waits, self.wait_total_ns, self.wake_to_run_total_ns)
    }

    /// The same summary in a sentence, which a reader without `results.json`
    /// sees.
    fn facts(&self) -> Vec<String> {
        if self.waits == 0 {
            return Vec::new();
        }
        let mean = |total: u64| total as f64 / self.waits as f64 / 1e3;
        vec![format!(
            "over the {} waits kept, the mean wait from wakeup (or preemption) to running \
             was {:.1} us by the tracepoints (`wake_to_run_ns`, what `perf sched latency` \
             averages) and {:.1} us by the kernel's `sched_info` (`total_ns`, what the \
             buckets hold)",
            self.waits,
            mean(self.wake_to_run_total_ns),
            mean(self.wait_total_ns),
        )]
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        if self.unstamped > 0 {
            notes.push(format!(
                "{} waits ended with no queue stamp in the kernel's `sched_info`, and are counted \
                 as no wait, as the task's `schedstat` counts them; their tracepoint wait is in \
                 `wake_to_run_ns`",
                self.unstamped
            ));
        }
        if self.off_target > 0 {
            notes.push(format!(
                "{} waits on other cpus were measured and set aside: the programs run on every \
                 cpu whatever `--cpu` says, so narrowing costs nothing less",
                self.off_target
            ));
        }
        notes.extend(self.predicate.note());
        if self.waits > 0 {
            // Said in the run's own output rather than left in documentation,
            // because every other source in this tool makes one event one
            // measurement and this one does not. It is not a loss: every wait
            // is counted, the emit layer weighs each row by `weight`, and
            // `total_ns` is the exact sum the buckets cannot give back.
            notes.push(format!(
                "{} waits are reported as {} rows of a kernel-side log2 histogram, each \
                 carrying the waits it stands for; \
                 `wait_ns` is its bucket's low bound, a factor of two below the next: the buckets \
                 and `min` are bucketed and `max` is a lower bound, while the sum, mean, rate, \
                 topk and stack totals come from `total_ns`, the exact sum",
                self.waits, self.rows,
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The run's summary over `wake_to_run_ns` is its exact sum over the
    /// waits, and a run with none has no mean rather than a zero one.
    #[test]
    fn the_summary_fields_are_the_sums_over_the_waits() {
        let fields = summary_fields(4, 4_000, 10_000);
        assert_eq!(fields["wake_to_run_mean_ns"], 2_500);
        assert_eq!(fields["wait_mean_ns"], 1_000);
        assert_eq!(fields["waits"], 4);
        let empty = summary_fields(0, 0, 0);
        assert!(!empty.contains_key("wake_to_run_mean_ns"));
        assert_eq!(empty["waits"], 0);
    }

    fn row() -> RunqLat {
        RunqLat {
            tid: 7,
            tgid: 7,
            cpu: 3,
            prio: 120,
            bucket: 11,
            count: 42,
            total_ns: 55_000,
            wake_to_run_ns: 56_000,
            first_ns: 0,
            comm: "spinner".to_owned(),
        }
    }

    /// A mode that cannot mean anything here is refused with the reason. A
    /// sampled wait is a wait whose beginning or end was not seen, and a
    /// duration built from one of the two is not a short duration, it is not a
    /// duration.
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
        assert!(error.contains("run queue latency"), "{error}");
    }

    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Iface("eth0".to_owned())) else {
            panic!("an interface names no task to attribute a wait to");
        };
        assert!(error.contains("iface"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// Every filter field the catalog offers must be on the events, or a
    /// predicate over it is false everywhere and silently empties the series.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&row(), 100_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"sched.runqlat".parse().expect("a source id"))
            .expect("sched.runqlat is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty(), "the entry declares filter fields");
        // `ppid` is put on the event by the collector, from the process's
        // parent (`super::Parents`), not by the row's attributes.
        for field in declared.iter().filter(|field| *field != "ppid") {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// The value on the event has to be the bucket's low bound, because that is
    /// the only value the emit layer re-buckets into the bucket it came from.
    /// The exact total travels beside it, and the two are different numbers:
    /// forty-two waits in `[1024, 2048)` are at least 43008 ns in total, and
    /// the low bound alone would say 1024.
    #[test]
    fn the_value_is_the_buckets_low_bound_and_the_total_is_exact() {
        let attributes = attributes(&row(), 1);
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
                .unwrap_or_else(|| panic!("no `{name}` attribute"))
        };
        assert_eq!(of("wait_ns"), 1024.0, "bucket 11 starts at 2^10");
        assert_eq!(of("count"), 42.0);
        assert_eq!(of("total_ns"), 55_000.0);
    }

    /// A real-time task's priority is negative, and an unsigned attribute would
    /// report the most urgent thing on the machine as the least.
    #[test]
    fn a_real_time_priority_reaches_the_event_as_a_negative_number() {
        let mut row = row();
        row.prio = -1;
        let attributes = attributes(&row, 1);
        let prio = attributes
            .iter()
            .find(|attr| attr.name == "prio")
            .expect("a prio attribute");
        assert_eq!(prio.value.as_f64(), Some(-1.0));
    }

    /// One event here is not one measurement, and every other source in this
    /// tool makes those the same thing -- so the run says which it is.
    ///
    /// It used to say something stronger, and worse: that the emit layer
    /// weighed each row as one, so a histogram's heights were the number of
    /// buckets rather than the number of waits. That is fixed --
    /// `emit::WEIGHT_ATTR` is on every row and honoured by `hist`, `summary`,
    /// `rate` and `topk` -- and what is left to say is the bucketing itself.
    #[test]
    fn the_run_says_that_an_event_stands_for_more_than_one_wait() {
        let mut collector = RunqLatCollector::new(Whose::Any, None);
        assert!(
            collector.notes().is_empty(),
            "a run that bucketed nothing has nothing to qualify"
        );
        collector.waits = 9;
        collector.rows = 2;
        let notes = collector.notes().join(" ");
        assert!(notes.contains("9 waits"), "{notes}");
        assert!(notes.contains("2 rows"), "{notes}");
        assert!(notes.contains("total_ns"), "{notes}");
        assert!(
            !notes.contains("weighs every"),
            "the emit layer weighs a row now, and the run must not still claim it does not: \
             {notes}"
        );
    }

    /// Every row carries the weight the emit layer reads, which is what makes
    /// a kernel-built histogram come out the right height.
    #[test]
    fn every_row_carries_the_waits_it_stands_for_as_a_weight() {
        let row = RunqLat {
            tid: 7,
            tgid: 7,
            cpu: 3,
            prio: 120,
            bucket: 4,
            count: 400,
            total_ns: 1_234_567,
            wake_to_run_ns: 1_234_567,
            first_ns: 0,
            comm: "worker".into(),
        };
        let attributes = attributes(&row, 100_000_000);
        let weight = attributes
            .iter()
            .find(|attr| attr.name == crate::emit::WEIGHT_ATTR)
            .expect("a weight attribute");
        assert_eq!(weight.value.as_f64(), Some(400.0));
    }

    /// The resolver hands a `bpf` backend the credit for filtering in the
    /// kernel and then leaves its events alone, so a collector that ignores the
    /// predicate returns everything and nothing says the filter never ran. A
    /// run that filtered has to say what the filter did.
    #[test]
    fn a_filter_that_removed_waits_says_how_many_it_removed() {
        let mut collector =
            RunqLatCollector::new(Whose::Any, Some("comm == 'kept'".parse().unwrap()));
        let row = |comm: &str| Event {
            ts_ns: 1,
            ts_err_ns: 0,
            kind: "sched.runqlat".to_owned(),
            keys: Vec::new(),
            attrs: vec![Attr::text("comm", comm)],
            collector_id: CollectorId::unstamped(),
            seq: 0,
        };
        assert!(collector.predicate.keep(&row("kept"), 4));
        assert!(!collector.predicate.keep(&row("other"), 11));
        let notes = collector.notes().join(" ");
        assert!(notes.contains("kept 4 waits and removed 11"), "{notes}");
        assert!(
            notes.contains("not in the kernel"),
            "the note must not claim the saving it did not make: {notes}"
        );
    }
}
