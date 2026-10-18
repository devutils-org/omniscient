//! `net.softirq` over two raw tracepoints, bucketed in the kernel.
//!
//! The time a packet costs before any process has asked for it. A received
//! frame is processed in softirq context -- the driver raises `NET_RX`, the
//! kernel runs the stack out of `__do_softirq` -- and that work belongs to no
//! process, so it is absent from every per-process number this tool produces.
//! On a machine moving real traffic it is where the cpu went, and the only
//! reason it is not a mystery is that the kernel says when each handler started
//! and finished.
//!
//! The programs are in `src/bpf/net_softirq.rs`. This file is the part that
//! belongs to the family: which vectors count as network vectors, what the
//! events look like, what is refused, and what is counted as lost.
//!
//! **What comes out is already a histogram**, for the reason
//! `sched.runqlat` gives: `NET_RX` fires tens of thousands of times a second on
//! a loaded link, and one event per handler would make the cost of the
//! measurement grow with the traffic. Each event is one log2 bucket of one
//! `(cpu, vector)` and carries the handler runs it stands for as
//! `emit::WEIGHT_ATTR`, so a histogram comes out the right height.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use super::softirqs::Vectors;
use crate::bpf::{SoftIrq, SoftIrqProgram};
use crate::clock::monotonic_ns;
use crate::collect::{not_built, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the rows are drained.
///
/// The cost of a drain is one syscall per `(cpu, vector, bucket)` row that saw
/// a handler in the interval, not per handler, which is the whole point of
/// bucketing in the kernel. Draining often also keeps the map far from the
/// capacity at which it would have to refuse rows.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// The request's predicate, which this collector applies itself.
    ///
    /// The resolver credits any backend whose id begins with `bpf` with pushing
    /// a filter into the kernel and then leaves its events alone. That credit
    /// comes from the backend's name, so a collector that does nothing with the
    /// predicate returns every event and nothing says the filter did not run.
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot measure softirq time: a handler's duration is the gap between \
                 its entry and its exit, and a sample that catches one of them without the \
                 other has no duration to report",
                self.mode
            ));
        }
        let cpus = match target {
            Target::System => None,
            Target::Cpu(set) => Some(set.cpus().to_vec()),
            other => {
                return Err(not_built(format!(
                    "a {} target cannot be served here: a softirq handler runs outside every \
                     process, so there is no process to attribute one to -- use --system, or \
                     --cpu to name the cpus whose handlers to measure",
                    other.kind()
                )))
            }
        };

        // Which number is `NET_RX` is the kernel's answer, not this build's:
        // the vector a tracepoint reports is an index into the kernel's own
        // table, and `/proc/softirqs` is that table. A number baked in here at
        // the wrong value would measure the timer softirq and report it as
        // network time.
        let vectors = Vectors::read()?;
        let (numbers, missing) = vectors.network();
        if !missing.is_empty() {
            return Err(format!(
                "this kernel names no {} softirq vector, so there is no number for this source \
                 to measure; the vectors it does have are the rows of /proc/softirqs",
                missing.join(" or ")
            ));
        }
        Ok(Box::new(SoftIrqCollector::new(
            numbers,
            vectors,
            cpus,
            self.mode,
            self.filter,
        )))
    }
}

pub struct SoftIrqCollector {
    id: CollectorId,
    /// The vector numbers the program compares against.
    vectors: Vec<u32>,
    /// The kernel's table, kept so an event can carry a vector's name.
    names: Vectors,
    /// The cpus a `--cpu` target named, or none for the whole machine.
    cpus: Option<Vec<u32>>,
    /// What the two declared modes differ by: `trace` is a series of drains,
    /// `count` is the single measurement of one window.
    mode: Mode,
    program: Option<SoftIrqProgram>,
    /// Runs of the programs the kernel skipped, as last read.
    skipped: u64,
    losses: Losses,
    notes: Vec<String>,
    /// The program's counters are cumulative, so what is new is the difference.
    dropped: u64,
    unpaired: u64,
    /// The end of the last interval, which is the start of this one.
    window_opened_ns: u64,
    /// When the whole window opened, which is the one `count` reports over.
    run_opened_ns: u64,
    /// The totals a `count` run accumulates, by the triple that keys a row.
    counted: BTreeMap<(u32, u32, u32), SoftIrq>,
    /// How many handler runs this collector has reported, which is not the
    /// event count: each event stands for as many runs as its `count` says.
    handlers: u64,
    rows: u64,
    /// Rows a `--cpu` target removed, in handler runs.
    off_target: u64,
    /// What the kernel's own counter said when the run opened and closed.
    ///
    /// `/proc/softirqs` counts every handler run since boot, per vector. It is
    /// the one number in reach that was produced by something other than this
    /// collector, so the run reports the comparison rather than leaving a
    /// reader to make it: a run that bucketed far fewer handlers than the
    /// kernel says ran is a run that missed some.
    kernel_runs_at_start: BTreeMap<u32, u64>,
    kernel_runs_at_stop: BTreeMap<u32, u64>,
    /// The request's predicate, applied to each row as it is drained.
    predicate: Option<FilterExpr>,
    filtered_in: u64,
    filtered_out: u64,
}

impl SoftIrqCollector {
    fn new(
        vectors: Vec<u32>,
        names: Vectors,
        cpus: Option<Vec<u32>>,
        mode: Mode,
        predicate: Option<FilterExpr>,
    ) -> Self {
        Self {
            id: CollectorId::new("net.softirq/bpf:raw_tp"),
            vectors,
            names,
            cpus,
            mode,
            program: None,
            losses: Losses::default(),
            notes: Vec::new(),
            dropped: 0,
            skipped: 0,
            unpaired: 0,
            window_opened_ns: 0,
            run_opened_ns: 0,
            counted: BTreeMap::new(),
            handlers: 0,
            rows: 0,
            off_target: 0,
            kernel_runs_at_start: BTreeMap::new(),
            kernel_runs_at_stop: BTreeMap::new(),
            predicate,
            filtered_in: 0,
            filtered_out: 0,
        }
    }

    /// Whether this run reports one measurement at the end rather than a
    /// series.
    fn counting(&self) -> bool {
        self.mode == Mode::Count
    }

    /// How many handler runs the kernel counted between the two reads, or
    /// `None` when either read failed and the comparison cannot be made.
    fn kernel_ran(&self) -> Option<u64> {
        if self.kernel_runs_at_start.is_empty() || self.kernel_runs_at_stop.is_empty() {
            return None;
        }
        let mut total = 0;
        for (vector, before) in &self.kernel_runs_at_start {
            let after = self.kernel_runs_at_stop.get(vector)?;
            total += after.saturating_sub(*before);
        }
        Some(total)
    }

    /// The name this kernel gives a vector, or the number when it gives none.
    fn vector_name(&self, number: u32) -> String {
        self.names
            .name_of(number)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("vector {number}"))
    }

    /// The kernel's own count of handler runs for the vectors being measured.
    ///
    /// A read that fails leaves the map empty, which is reported as "the
    /// comparison could not be made" rather than as a count of zero.
    fn kernel_runs(&self) -> BTreeMap<u32, u64> {
        let Ok(now) = Vectors::read() else {
            return BTreeMap::new();
        };
        self.vectors
            .iter()
            .filter_map(|vector| now.runs(*vector).map(|runs| (*vector, runs)))
            .collect()
    }

    /// When a row happened, as far as an aggregated source can say: the moment
    /// its window's first one began, or the drain that read it when the kernel
    /// recorded no beginning -- which falls back to the drain rather than to
    /// the beginning of time. Finding 44: a join that opens on a bucketed
    /// source would otherwise open at every window's close.
    fn stamp(first_ns: u64, closed_ns: u64) -> u64 {
        if first_ns == 0 {
            closed_ns
        } else {
            first_ns
        }
    }

    fn event(&self, row: &SoftIrq, window_ns: u64, ts_ns: u64, seq: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "net.softirq".to_owned(),
            // The cpu is the series: a softirq handler belongs to the cpu it
            // ran on and to nothing else, which is why this source declares no
            // process target.
            keys: vec![KeyValue::cpu(row.cpu)],
            attrs: attributes(row, &self.vector_name(row.vec), window_ns),
            collector_id: self.id.clone(),
            seq,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.window_opened_ns);
        let drained = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                // A drain that failed is a window whose rows are still in the
                // map: they are not lost, they arrive with the next one. What
                // is lost is knowing which window they belonged to.
                self.losses.failed_reads += 1;
                self.losses.note_times(reason, 1);
                return;
            }
        };
        self.window_opened_ns = closed_ns;

        if drained.vanished > 0 {
            self.losses.undecoded += drained.vanished;
            self.losses
                .note("a softirq row was gone between being listed and being read");
        }
        if drained.truncated {
            self.losses
                .note("the softirq histogram held more rows than one drain can walk");
        }

        let mut index = 0;
        for row in drained.rows {
            if let Some(cpus) = &self.cpus {
                if !cpus.contains(&row.cpu) {
                    self.off_target += row.count;
                    continue;
                }
            }
            if let Some(predicate) = &self.predicate {
                let candidate =
                    self.event(&row, window_ns, Self::stamp(row.first_ns, closed_ns), seq);
                if !crate::filter::matches(predicate, &candidate) {
                    self.filtered_out += row.count;
                    continue;
                }
                self.filtered_in += row.count;
            }
            self.handlers += row.count;
            self.rows += 1;
            if self.counting() {
                // The map is still drained -- leaving it to fill would lose
                // rows -- but what comes out is added to the window's total
                // rather than published as a slice of it.
                self.counted
                    .entry((row.cpu, row.vec, row.bucket))
                    .and_modify(|held| {
                        held.count += row.count;
                        held.total_ns += row.total_ns;
                    })
                    .or_insert(row);
                continue;
            }
            events.push(self.event(
                &row,
                window_ns,
                Self::stamp(row.first_ns, closed_ns),
                seq + index,
            ));
            index += 1;
        }
    }

    /// Reads the programs' own counters.
    ///
    /// The two are different facts and are kept apart. A row the histogram
    /// could not hold is a handler nobody will ever count, which is a loss. An
    /// exit whose entry this run never saw is a handler that was already
    /// running when the programs attached, which happens once per cpu per
    /// vector and is a fact about the window.
    fn absorb_counters(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note_times(
                "the kernel skipped a run of these programs because one was already running on \
                 its cpu, so a handler went unseen; the count is the machine's, and an upper bound \
                 on this target's",
                skipped - self.skipped,
            );
            self.skipped = skipped;
        }
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note_times(
                "more distinct (cpu, vector, bucket) rows appeared in one interval than the \
                 softirq histogram holds, so those handlers were never counted",
                dropped - self.dropped,
            );
            self.dropped = dropped;
        }
        self.unpaired = program.unpaired();
    }
}

/// What an event carries beside its value.
///
/// `cpu` and `vec` are here because the catalog entry declares them as filter
/// fields: a predicate over a field no event carries is false everywhere, which
/// empties the series and looks exactly like a machine with no network traffic.
fn attributes(row: &SoftIrq, vector_name: &str, window_ns: u64) -> Vec<Attr> {
    vec![
        // The value. The low bound of the bucket rather than its mean, because
        // it is the one number the emit layer's own bucketing maps back to this
        // same bucket.
        Attr::uint("duration_ns", row.bucket_low_ns()),
        // How many handler runs this event stands for. Every number derived
        // from `duration_ns` is this many times over, which is what the emit
        // layer reads out of `weight`.
        Attr::uint("count", row.count),
        Attr::uint(crate::emit::WEIGHT_ATTR, row.count),
        // The exact sum of those durations, which the bucket cannot give back:
        // a log2 bucket is a factor of two wide, so a total built from its
        // bounds inherits that error and this does not.
        Attr::uint("total_ns", row.total_ns),
        Attr::uint("bucket", u64::from(row.bucket)),
        // The interval the row was collected over. A count without its window
        // cannot be compared with another count.
        Attr::uint("window_ns", window_ns),
        Attr::uint("cpu", u64::from(row.cpu)),
        // The kernel's own vector number, and the name it prints for it. The
        // number is what the tracepoint reports and what a machine-written
        // filter should use; the name is what a person reads.
        Attr::uint("vec", u64::from(row.vec)),
        Attr::text("vec_name", vector_name.to_owned()),
    ]
}

impl Collector for SoftIrqCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "duration_ns"
    }

    fn row_stamp(&self) -> Option<crate::collect::RowStamp> {
        use crate::collect::RowStamp;
        Some(RowStamp::FirstEvent)
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(SoftIrqProgram::load(&self.vectors)?);
        self.kernel_runs_at_start = self.kernel_runs();
        self.window_opened_ns = monotonic_ns();
        self.run_opened_ns = self.window_opened_ns;
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_counters();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        self.absorb_counters();
        self.kernel_runs_at_stop = self.kernel_runs();
        self.program = None;
        if self.counting() {
            let closed_ns = monotonic_ns();
            let window_ns = closed_ns.saturating_sub(self.run_opened_ns);
            let seq = u64::MAX / 2;
            let counted = std::mem::take(&mut self.counted);
            for (index, row) in counted.into_values().enumerate() {
                trailing.push(self.event(
                    &row,
                    window_ns,
                    Self::stamp(row.first_ns, closed_ns),
                    seq + index as u64,
                ));
            }
        }
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
        // A qualification rather than a remark, which is what a note is for:
        // this is not the machine's softirq time, it is the network stack's
        // share of it, and the numbers the kernel gave those vectors are worth
        // printing because nothing in this build chose them.
        notes.push(format!(
            "{} time only, not every softirq the machine ran; the {} vectors this kernel \
             does have are the rows of /proc/softirqs",
            self.vectors
                .iter()
                .map(|vector| format!("{} ({vector})", self.vector_name(*vector)))
                .collect::<Vec<_>>()
                .join(" and "),
            self.names.count(),
        ));
        if self.unpaired > 0 {
            // Not a loss. These are the handlers that were already running on
            // some cpu when the programs attached: their entry happened before
            // this run existed, so their duration has no beginning and is in no
            // bucket. It is at most one per cpu per vector.
            notes.push(format!(
                "{} softirq handler{} already running when this run attached, so no duration \
                 was recorded for {}",
                self.unpaired,
                if self.unpaired == 1 { " was" } else { "s were" },
                if self.unpaired == 1 { "it" } else { "them" },
            ));
        }
        if self.off_target > 0 {
            notes.push(format!(
                "{} handler runs happened on cpus this run did not ask for; the rows are keyed \
                 by cpu so they were dropped after the drain rather than in the kernel",
                self.off_target,
            ));
        }
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} handler runs and removed {}; it ran here, over the drained \
                 rows, not in the kernel, so the removed ones were counted before they were \
                 dropped",
                self.filtered_in, self.filtered_out,
            ));
        }
        notes
    }

    /// What the run states about itself that it did not give up: the
    /// kernel's own count of the handler runs over a window containing this
    /// one, and how many runs the kernel bucketed into how many rows. Both
    /// used to be `notes`, which the text prints as `degraded:` -- a count the
    /// kernel kept is another party's account, not a concession.
    fn facts(&self) -> Vec<String> {
        let mut facts = Vec::new();
        if let Some(ran) = self.kernel_ran() {
            // Both numbers, and the comparison, rather than either alone: the
            // window this collector observed is strictly inside the one the
            // two file reads bound, so its own count can only be the smaller.
            // A run where it is not is a defect and says so here.
            facts.push(format!(
                "the kernel's own /proc/softirqs counted {ran} handler runs on these vectors \
                 over a window containing this run, and this run bucketed {}{}",
                self.handlers,
                if self.handlers > ran {
                    " -- which is more than happened, so something here is counting twice"
                } else {
                    ""
                },
            ));
        }
        if self.handlers > 0 {
            facts.push(format!(
                "{} softirq handler runs were drained as {} rows of a kernel-side log2 \
                 histogram, each carrying the runs it stands for; \
                 `duration_ns` is its bucket's low bound, a factor of two below the next: the buckets \
                 and `min` are bucketed and `max` is a lower bound, while the sum, mean, rate, \
                 topk and stack totals come from `total_ns`, the exact sum",
                self.handlers, self.rows,
            ));
        }
        facts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> SoftIrq {
        SoftIrq {
            cpu: 3,
            vec: 3,
            bucket: 11,
            count: 42,
            total_ns: 55_000,
            first_ns: 0,
        }
    }

    fn collector() -> SoftIrqCollector {
        let names = Vectors::read().expect("this machine has /proc/softirqs");
        let (numbers, _) = names.network();
        SoftIrqCollector::new(numbers, names, None, Mode::Trace, None)
    }

    /// A mode that cannot mean anything here is refused with the reason. A
    /// sampled duration is a duration whose beginning or end was not seen, and
    /// that is not a short duration, it is not a duration.
    #[test]
    fn a_sampled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Sample {
                freq: "997hz".parse().expect("a frequency"),
            },
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a sampled mode must be refused");
        };
        assert!(error.contains("gap between"), "{error}");
    }

    /// A process target is refused, and the refusal is this build's gap rather
    /// than this run's failure: a softirq handler runs outside every process,
    /// so there is nothing for a pid to select. The catalog offers no process
    /// target for this source, and the message says what to ask for instead.
    #[test]
    fn a_process_target_is_refused_and_says_what_to_ask_for() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("a process names nothing a softirq belongs to");
        };
        assert!(error.contains("outside every process"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// Every filter field the catalog offers must be on the events, or a
    /// predicate over it is false everywhere and silently empties the series.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&row(), "NET_RX", 100_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"net.softirq".parse().expect("a source id"))
            .expect("net.softirq is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty(), "the entry declares filter fields");
        for field in declared {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// The value is the bucket's low bound, because that is the only value the
    /// emit layer re-buckets into the bucket it came from, and the exact total
    /// travels beside it: forty-two handlers in `[1024, 2048)` are at least
    /// 43008 ns in total and the low bound alone would say 1024.
    #[test]
    fn the_value_is_the_buckets_low_bound_and_the_total_is_exact() {
        let attributes = attributes(&row(), "NET_RX", 1);
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
                .unwrap_or_else(|| panic!("no `{name}` attribute"))
        };
        assert_eq!(of("duration_ns"), 1024.0, "bucket 11 starts at 2^10");
        assert_eq!(of("count"), 42.0);
        assert_eq!(of("weight"), 42.0);
        assert_eq!(of("total_ns"), 55_000.0);
    }

    /// The vector's name comes from the kernel's own table, and a number the
    /// table does not cover is reported as a number rather than as a name that
    /// was guessed.
    #[test]
    fn a_vector_the_kernel_does_not_name_is_reported_as_a_number() {
        let Ok(names) = Vectors::read() else {
            eprintln!("skipped: this machine has no /proc/softirqs");
            return;
        };
        let collector = SoftIrqCollector::new(vec![3], names, None, Mode::Trace, None);
        assert_eq!(collector.vector_name(3), "NET_RX");
        assert_eq!(collector.vector_name(99), "vector 99");
    }

    /// One event here is not one measurement, and every other source in this
    /// family makes those the same thing -- so the run says which it is, and
    /// says which vectors it measured, because "network softirq time" is a
    /// choice this collector made rather than something the kernel reports.
    #[test]
    fn the_run_says_which_vectors_it_measured_and_what_a_row_stands_for() {
        let Ok(names) = Vectors::read() else {
            eprintln!("skipped: this machine has no /proc/softirqs");
            return;
        };
        let _ = names;
        let mut collector = collector();
        let notes = collector.notes().join(" ");
        assert!(notes.contains("NET_RX"), "{notes}");
        let facts = collector.facts().join(" ");
        assert!(
            !facts.contains("handler runs were drained as"),
            "a run that bucketed nothing has nothing to state: {facts}"
        );
        collector.handlers = 9;
        collector.rows = 2;
        // What a row stands for is a fact of the run, not a concession: it
        // reaches `notes`, and the text prints it as `note:`, not `degraded:`.
        let facts = collector.facts().join(" ");
        assert!(facts.contains("9 softirq handler runs"), "{facts}");
        assert!(facts.contains("2 rows"), "{facts}");
        let notes = collector.notes().join(" ");
        assert!(!notes.contains("handler runs were drained as"), "{notes}");
    }

    /// A handler already running at attach is not a loss, and must not be
    /// counted as one: it is one per cpu per vector at the start of every run,
    /// and a run that reported it as dropped would never be clean.
    #[test]
    fn a_handler_already_running_at_attach_is_a_note_and_not_a_loss() {
        let mut collector = collector();
        collector.unpaired = 4;
        let notes = collector.notes().join(" ");
        assert!(
            notes.contains("already running when this run attached"),
            "{notes}"
        );
        assert!(
            collector.losses().is_clean(),
            "an unpaired exit is a fact about the window, not a loss"
        );
    }
}
