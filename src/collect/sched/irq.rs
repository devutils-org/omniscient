//! `sched.irq` over a pair of raw tracepoints, bucketed in the kernel.
//!
//! The measurement is how long the machine spent inside hardware interrupt
//! handlers, per cpu and per interrupt line, and inside softirqs, per cpu and
//! per vector. It is the one kind of cpu time no
//! other source in this tool can attribute: a handler preempts whatever thread
//! is on the cpu, runs outside any schedule, and is charged to nobody. A
//! thread that takes longer than its own cpu time explains may have spent the
//! difference being interrupted, and `cpu.oncpu_time` cannot see it.
//!
//! `/proc/interrupts` counts the same interrupts and says nothing about how
//! long any of them took, which is the difference this source is for: a
//! million fast interrupts and ten thousand slow ones are the same file and
//! different machines.
//!
//! **The cpu is the series.** The catalog's key for this source is `cpu` and
//! not `task`, because an interrupt handler does not belong to a task. What it
//! belongs to is the cpu it stole, and the reader's question -- which core is
//! losing time to interrupts, and to which device -- is answered by that pair.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use crate::bpf::{IrqDrops, IrqProgram, IrqRun, KIND_SOFT};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::emit::WEIGHT_ATTR;
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the rows are drained.
///
/// The cardinality is bounded by the machine rather than by the workload --
/// cpus times interrupt lines times a few buckets -- so this is about keeping
/// the `rate` series fine rather than about keeping the map empty.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot measure interrupt handlers: a handler's length is the gap \
                 between two events, and a sample that catches one of them without the other \
                 has no duration to report",
                self.mode
            ));
        }
        let cpus = match target {
            Target::System => None,
            Target::Cpu(set) => Some(set.cpus().to_vec()),
            other => {
                return Err(format!(
                    "a {} target names a process, and an interrupt handler belongs to no \
                     process: it preempts whatever was on the cpu and is charged to nobody. \
                     Use --system, or --cpu to ask about one core",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(IrqCollector::new(cpus, self.mode, self.filter)))
    }
}

pub struct IrqCollector {
    id: CollectorId,
    /// The cpus a `--cpu` run asked about, or none for the machine.
    ///
    /// Applied here rather than in the program, and that is a real difference:
    /// both programs run on every cpu whatever this holds, so a `--cpu 3` run
    /// costs what a `--system` run costs and reports one core's worth. The
    /// alternative -- a cpu test in the program -- would save the map write
    /// and not the tracepoint, which is where the cost is.
    cpus: Option<Vec<u32>>,
    mode: Mode,
    program: Option<IrqProgram>,
    losses: Losses,
    /// The kernel's skipped runs of the program, cumulative, as last read.
    skipped: u64,
    notes: Vec<String>,
    dropped: IrqDrops,
    window_opened_ns: u64,
    /// When the whole window opened, which is the one `count` reports over.
    run_opened_ns: u64,
    /// The totals a `count` run accumulates, by the triple that keys a series.
    counted: BTreeMap<(u32, u32, u32, u32), IrqRun>,
    predicate: Option<FilterExpr>,
    filtered_out: u64,
    filtered_in: u64,
    /// Handler runs this run declined to report because their cpu was not the
    /// one asked about. Not a loss: they were measured and set aside.
    off_target: u64,
}

impl IrqCollector {
    pub fn new(cpus: Option<Vec<u32>>, mode: Mode, predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("sched.irq/bpf:raw_tp"),
            cpus,
            mode,
            program: None,
            losses: Losses::default(),
            skipped: 0,
            notes: Vec::new(),
            dropped: IrqDrops::default(),
            window_opened_ns: 0,
            run_opened_ns: 0,
            counted: BTreeMap::new(),
            predicate,
            filtered_out: 0,
            filtered_in: 0,
            off_target: 0,
        }
    }

    /// Whether this run reports one measurement at the end rather than a
    /// series.
    fn counting(&self) -> bool {
        self.mode == Mode::Count
    }

    fn wanted(&self, cpu: u32) -> bool {
        match &self.cpus {
            None => true,
            Some(cpus) => cpus.contains(&cpu),
        }
    }

    fn event(&self, row: &IrqRun, closed_ns: u64, window_ns: u64, seq: u64) -> Event {
        Event {
            // Stamped where the row's window began rather than at the
            // drain that read it, which is finding 44: a join that opens on
            // a bucketed source would otherwise open at the window's close.
            // A zero is the kernel having recorded no beginning, and falls
            // back to the drain rather than to the beginning of time.
            ts_ns: if row.first_ns == 0 {
                closed_ns
            } else {
                row.first_ns
            },
            ts_err_ns: 0,
            kind: "sched.irq".to_owned(),
            // The cpu is the series. An interrupt handler belongs to no task,
            // and the question a reader arrives with is which core is losing
            // time and to which device.
            keys: vec![KeyValue::cpu(row.cpu)],
            attrs: attributes(row, window_ns),
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
        let (rows, drain_losses) = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                self.losses.failed_reads += 1;
                self.losses.note_times(reason, 1);
                return;
            }
        };
        self.window_opened_ns = closed_ns;

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("an interrupt row was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the interrupt histogram held more rows than one drain can walk");
        }

        let counting = self.counting();
        let run_window_ns = closed_ns.saturating_sub(self.run_opened_ns);
        for (index, row) in rows.into_iter().enumerate() {
            if !self.wanted(row.cpu) {
                self.off_target += row.count;
                continue;
            }
            let event = self.event(
                &row,
                closed_ns,
                if counting { run_window_ns } else { window_ns },
                seq + index as u64,
            );
            if let Some(predicate) = &self.predicate {
                if !matches_half(predicate, &event, row.kind) {
                    self.filtered_out += row.count;
                    continue;
                }
                self.filtered_in += row.count;
            }
            if counting {
                // The map is still drained -- leaving it to fill would lose
                // rows -- but what comes out is added to the window's total
                // rather than published as a slice of it.
                self.counted
                    .entry((row.cpu, row.kind, row.irq, row.bucket))
                    .and_modify(|held| {
                        held.count += row.count;
                        held.total_ns += row.total_ns;
                    })
                    .or_insert(row);
                continue;
            }
            events.push(event);
        }
    }

    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        // The kernel's own count of runs it skipped, which no counter of the
        // program's can hold. It is the machine's -- the program runs on every
        // occurrence and narrows to the target after -- so it is an upper
        // bound on what the target lost, and the reason says so.
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note_times(
                "the kernel skipped a run of this program because one was already running on \
                 that cpu, so an occurrence was never seen; the count is the machine's, an \
                 upper bound on the target's share, and every total here is a lower bound",
                skipped - self.skipped,
            );
            self.skipped = skipped;
        }
        let now = program.dropped();
        for (fresh, before, reason) in [
            (
                now.rows_full,
                self.dropped.rows_full,
                "more distinct (cpu, irq, bucket) rows appeared in one interval than the \
                 histogram holds, so those handler runs were never counted",
            ),
            (
                now.rows_busy,
                self.dropped.rows_busy,
                "the kernel declined to add a row because the histogram's own bucket was \
                 already being updated on this cpu, so those handler runs were never counted",
            ),
            (
                now.backwards,
                self.dropped.backwards,
                "an interrupt handler was seen to finish before it started, which is not a \
                 duration; it was refused rather than bucketed",
            ),
            (
                now.unopened,
                self.dropped.unopened,
                "an interrupt handler finished that this run never saw start: it was already \
                 running when the programs attached, and its length is unknown rather than \
                 zero",
            ),
        ] {
            if fresh > before {
                self.losses.dropped += fresh - before;
                self.losses.note_times(reason, fresh - before);
            }
        }
        self.dropped = now;
    }
}

/// Whether a row passes the filter, with `kind` naming the row's half; see
/// [`super::matches_as`].
fn matches_half(predicate: &FilterExpr, event: &Event, kind: u32) -> bool {
    super::matches_as(predicate, event, half(kind))
}

/// The half a row's `kind` names.
fn half(kind: u32) -> &'static str {
    if kind == KIND_SOFT {
        "soft"
    } else {
        "hard"
    }
}

/// What an event carries beside its value.
///
/// `irq`, `cpu` and `name` are here because the catalog entry declares them as
/// filter fields: a predicate over a field no event carries is false
/// everywhere, which empties the series and looks exactly like a machine that
/// took no interrupts.
fn attributes(row: &IrqRun, window_ns: u64) -> Vec<Attr> {
    vec![
        // The value: the low bound of the bucket, which is the number the emit
        // layer's own log2 bucketing maps back to this same bucket.
        Attr::uint("handler_ns", row.bucket_low_ns()),
        // How many handler runs this event stands for.
        Attr::uint("count", row.count),
        Attr::uint(WEIGHT_ATTR, row.count),
        // Their exact total, which a log2 bucket cannot give back. This is the
        // number that answers "how much of this core went to interrupts".
        Attr::uint("total_ns", row.total_ns),
        Attr::uint("bucket", u64::from(row.bucket)),
        Attr::uint("window_ns", window_ns),
        Attr::uint("cpu", u64::from(row.cpu)),
        Attr::uint("irq", u64::from(row.irq)),
        // Which half: a hard handler's `irq` is its line, a softirq's is its
        // vector, and the numbers overlap.
        Attr::text("kind", half(row.kind)),
        // The handler's name as `/proc/interrupts` shows it -- an interrupt line
        // number is a machine's private business; `nvme0q3` is a device -- and
        // a softirq's as `/proc/softirqs` does.
        Attr::text(
            "name",
            if row.kind == KIND_SOFT {
                softirq_name(row.irq)
            } else {
                row.name.clone()
            },
        ),
    ]
}

/// A softirq vector's name, as `/proc/softirqs` spells it. The vectors are
/// `enum` constants the kernel has kept in this order since 2.6.36 and names in
/// `softirq_to_name`; one this table does not know is given its number.
fn softirq_name(vector: u32) -> String {
    const NAMES: [&str; 10] = [
        "HI", "TIMER", "NET_TX", "NET_RX", "BLOCK", "IRQ_POLL", "TASKLET", "SCHED", "HRTIMER",
        "RCU",
    ];
    NAMES
        .get(vector as usize)
        .map_or_else(|| format!("softirq {vector}"), |name| (*name).to_owned())
}

impl Collector for IrqCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "handler_ns"
    }

    fn row_stamp(&self) -> Option<crate::collect::RowStamp> {
        use crate::collect::RowStamp;
        Some(RowStamp::FirstEvent)
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(IrqProgram::load()?);
        self.window_opened_ns = monotonic_ns();
        self.run_opened_ns = self.window_opened_ns;
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
        self.program = None;
        if self.counting() {
            let closed_ns = monotonic_ns();
            let window_ns = closed_ns.saturating_sub(self.run_opened_ns);
            let seq = u64::MAX / 2;
            let rows: Vec<IrqRun> = std::mem::take(&mut self.counted).into_values().collect();
            for (index, row) in rows.iter().enumerate() {
                trailing.push(self.event(row, closed_ns, window_ns, seq + index as u64));
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
        // Said on every run: the two halves share `irq` numbers, and a reader
        // adding them up by line would add a vector to a device.
        notes.push(
            "hard handlers and softirqs are separate rows, told apart by `kind`: a hard row's \
             `irq` is its line and a soft row's its vector, and a softirq's time has the hard \
             handlers that landed inside it taken out"
                .to_owned(),
        );
        if self.off_target > 0 {
            notes.push(format!(
                "{} handler runs on other cpus were measured and set aside: the programs run on \
                 every cpu whatever `--cpu` says, so narrowing costs nothing less",
                self.off_target
            ));
        }
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} handler runs and removed {}; it is applied to drained rows \
                 rather than in the program, so the cost of the ones it removed was still paid",
                self.filtered_in, self.filtered_out
            ));
        }
        if self.counting() {
            notes.push(
                "`count` reports one total per (cpu, irq, bucket) over the whole window, so \
                 there is no series here; `--mode trace` is the one that reports per drain"
                    .to_owned(),
            );
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> IrqRun {
        IrqRun {
            cpu: 3,
            irq: 142,
            kind: crate::bpf::KIND_HARD,
            bucket: 13,
            count: 24,
            total_ns: 96_000,
            first_ns: 0,
            name: "nvme0q3".to_owned(),
        }
    }

    /// `kind` in a filter names the row's half, as the catalog declares it:
    /// the filter layer reads `kind` as the event's own kind, and
    /// `kind == "hard"` used to select nothing (`docs/fixes/w22-bpftrace.md`,
    /// gap 2).
    #[test]
    fn a_filter_on_kind_selects_the_half() {
        let collector = IrqCollector::new(None, Mode::Trace, None);
        let hard = collector.event(&row(), 1, 1, 0);
        let mut soft_row = row();
        soft_row.kind = KIND_SOFT;
        let soft = collector.event(&soft_row, 1, 1, 1);
        let hard_only: FilterExpr = r#"kind == "hard""#.parse().expect("a filter");
        let soft_only: FilterExpr = r#"kind == "soft""#.parse().expect("a filter");
        assert!(matches_half(&hard_only, &hard, crate::bpf::KIND_HARD));
        assert!(!matches_half(&hard_only, &soft, KIND_SOFT));
        assert!(matches_half(&soft_only, &soft, KIND_SOFT));
        assert!(!matches_half(&soft_only, &hard, crate::bpf::KIND_HARD));
        // What is reported keeps the source's own kind.
        assert_eq!(hard.kind, "sched.irq");
    }

    /// A softirq row is named by its vector the way `/proc/softirqs` names
    /// it, and says which half it is; a hard row keeps its handler's name.
    #[test]
    fn a_softirq_row_is_named_by_its_vector_and_says_it_is_soft() {
        let text = |attributes: &[Attr], name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .map(|attr| attr.value.clone())
        };
        let hard = attributes(&row(), 1);
        assert_eq!(
            text(&hard, "kind"),
            Some(crate::event::AttrValue::Text("hard".to_owned()))
        );
        let mut soft = row();
        soft.kind = KIND_SOFT;
        soft.irq = 3;
        let soft = attributes(&soft, 1);
        assert_eq!(
            text(&soft, "name"),
            Some(crate::event::AttrValue::Text("NET_RX".to_owned()))
        );
        assert_eq!(
            text(&soft, "kind"),
            Some(crate::event::AttrValue::Text("soft".to_owned()))
        );
        assert_eq!(softirq_name(42), "softirq 42");
    }

    fn prepare(mode: Mode) -> Box<Prepare> {
        Box::new(Prepare { mode, filter: None })
    }

    /// Both modes the catalog declares must be served, or a request the
    /// validator accepted is refused here, which sends the reader nowhere.
    #[test]
    fn every_mode_the_catalog_declares_is_accepted_and_the_rest_are_refused() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let entry = catalog
            .get(&"sched.irq".parse().expect("a source id"))
            .expect("sched.irq is in the catalog");
        for mode in [Mode::Trace, Mode::Count] {
            assert!(
                entry.modes.iter().any(|declared| *declared == mode.kind()),
                "this test is written against the entry's own modes"
            );
            assert!(prepare(mode).attach(&Target::System).is_ok(), "`{mode}`");
        }
        let sampled = Mode::Sample {
            freq: "997hz".parse().expect("a frequency"),
        };
        let Err(error) = prepare(sampled).attach(&Target::System) else {
            panic!("a sampled mode must be refused");
        };
        assert!(
            error.contains("cannot measure interrupt handlers"),
            "{error}"
        );
    }

    /// An interrupt handler belongs to no process, and the refusal has to say
    /// that rather than leaving a reader to think `--pid` is unimplemented.
    #[test]
    fn a_process_target_is_refused_with_the_reason_it_cannot_mean_anything() {
        let Err(error) = prepare(Mode::Trace).attach(&Target::Pid(vec![1])) else {
            panic!("an interrupt handler is not a process's");
        };
        assert!(error.contains("no process"), "{error}");
        assert!(error.contains("--cpu"), "{error}");
    }

    #[test]
    fn a_cpu_target_keeps_only_the_cpus_it_named() {
        let collector = IrqCollector::new(Some(vec![3, 5]), Mode::Trace, None);
        assert!(collector.wanted(3));
        assert!(collector.wanted(5));
        assert!(!collector.wanted(4));
        assert!(IrqCollector::new(None, Mode::Trace, None).wanted(17));
    }

    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&row(), 100_000_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"sched.irq".parse().expect("a source id"))
            .expect("sched.irq is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty());
        for field in declared {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    #[test]
    fn a_row_carries_its_own_weight_and_the_buckets_low_bound() {
        let attributes = attributes(&row(), 1);
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
        };
        assert_eq!(of("handler_ns"), Some(4096.0));
        assert_eq!(of(WEIGHT_ATTR), Some(24.0));
        assert_eq!(of("count"), of(WEIGHT_ATTR));
        assert_eq!(of("total_ns"), Some(96_000.0));
    }

    /// Every run says the two halves are separate rows, because their `irq`
    /// numbers overlap.
    #[test]
    fn every_run_says_the_two_halves_are_told_apart_by_kind() {
        let collector = IrqCollector::new(None, Mode::Trace, None);
        assert!(
            collector.notes().iter().any(|note| note.contains("`kind`")),
            "{:?}",
            collector.notes()
        );
    }

    #[test]
    fn the_value_attribute_is_one_the_events_carry() {
        let collector = IrqCollector::new(None, Mode::Trace, None);
        let name = collector.value_attr();
        assert!(attributes(&row(), 1).iter().any(|attr| attr.name == name));
    }
}
