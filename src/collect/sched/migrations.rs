//! `sched.migrations` over a raw tracepoint, counted in the kernel.
//!
//! A migration is the scheduler moving a runnable task from one cpu to
//! another. It is cheap to count and expensive to suffer: the task arrives on
//! a cpu whose caches know nothing about it, and on a NUMA machine possibly a
//! long way from its own memory. Nothing here prices that; what this source
//! says is how often it happened and to whom, which is the question someone
//! arrives with when a program is slower than its cpu time explains.
//!
//! Each event is a count over the interval between two drains, keyed by the
//! task and the cpu it arrived on. The destination is part of the key because
//! a task bouncing between two cpus and a task walking across sixteen produce
//! the same total and are different problems -- and because the origin is not
//! available at all, which `src/bpf/migrate.rs` explains.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use crate::bpf::{Migration, MigrationProgram, Renames, Whose};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the counts are drained.
///
/// Slower than the sources that time something: a migration count is not a
/// duration, so a coarse interval costs resolution in the `rate` series and
/// nothing else. It is still frequent enough that the map stays far from the
/// capacity at which it would refuse rows.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(250);

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// The request's predicate, applied here per drained row: the resolver
    /// credits a `bpf` backend with filtering in the kernel and then leaves its
    /// events alone, so a predicate this collector ignored would run nowhere
    /// (stream S's finding, `docs/fixes/silent.md`).
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot count migrations: a migration is an instant, and sampling \
                 instants at a frequency counts the samples rather than the migrations",
                self.mode
            ));
        }
        let filter = super::whose(target, "a migration happens to a task")?;
        let mut collector = MigrationCollector::new(filter, self.mode);
        collector.predicate = self.filter;
        Ok(Box::new(collector))
    }
}

pub struct MigrationCollector {
    id: CollectorId,
    filter: Whose,
    /// What the two declared modes actually differ by.
    ///
    /// `trace` is a series: one event per (task, cpu) per drain, each covering
    /// the interval between two drains. `count` is the single measurement of
    /// one window, which is what [`Collector::stop`] calls "the closing read
    /// **is** the result": the drains still happen, because the kernel's map
    /// has to be emptied, but what they produce is added up and reported once,
    /// over the window the request asked for.
    ///
    /// Taking the mode and discarding it -- which is what this did -- made
    /// `count` an alias for `trace`, and a mode that changes nothing is worse
    /// than one that is refused, because the refusal is visible.
    mode: Mode,
    program: Option<MigrationProgram>,
    losses: Losses,
    /// The kernel's skipped runs of the program, cumulative, as last read.
    skipped: u64,
    /// The program's counter is cumulative, so what is new is the difference.
    dropped: u64,
    /// The end of the last interval, which is the start of this one.
    window_opened_ns: u64,
    /// When the whole window opened, which is the one `count` reports over.
    run_opened_ns: u64,
    /// The totals a `count` run accumulates, by the pair that keys a series.
    counted: BTreeMap<(u32, u32), Migration>,
    /// The request's predicate, and the migrations it kept and removed.
    predicate: Option<FilterExpr>,
    filtered_in: u64,
    filtered_out: u64,
    /// The names tasks took at exec, applied to the rows of the window.
    renames: Option<Renames>,
    /// Why they could not be, when the exec program was refused.
    exec_note: Option<String>,
}

impl MigrationCollector {
    pub fn new(filter: Whose, mode: Mode) -> Self {
        Self {
            id: CollectorId::new("sched.migrations/bpf:raw_tp"),
            filter,
            mode,
            program: None,
            losses: Losses::default(),
            skipped: 0,
            dropped: 0,
            window_opened_ns: 0,
            run_opened_ns: 0,
            counted: BTreeMap::new(),
            predicate: None,
            filtered_in: 0,
            filtered_out: 0,
            renames: None,
            exec_note: None,
        }
    }

    /// One row as the event it becomes.
    fn event(&self, row: &Migration, ts_ns: u64, window_ns: u64, seq: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "sched.migrations".to_owned(),
            keys: vec![
                // The migrated task is the series. Not the cpu: the cpu gained
                // a task and the task lost its caches, and it is the second
                // that anyone is measuring.
                KeyValue::task(row.tid as i32),
                KeyValue::cpu(row.dest_cpu),
            ],
            attrs: attributes(row, window_ns),
            collector_id: self.id.clone(),
            seq,
        }
    }

    /// Whether the predicate keeps a row, counting what it keeps and removes.
    fn keeps(&mut self, row: &Migration, window_ns: u64) -> bool {
        let Some(predicate) = &self.predicate else {
            return true;
        };
        let kept = crate::filter::matches(predicate, &self.event(row, 0, window_ns, 0));
        if kept {
            self.filtered_in += row.count;
        } else {
            self.filtered_out += row.count;
        }
        kept
    }

    /// Whether this run reports one measurement at the end rather than a
    /// series.
    fn counting(&self) -> bool {
        self.mode == Mode::Count
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.window_opened_ns);
        let (mut rows, drain_losses) = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                self.losses.failed_reads += 1;
                self.losses.note(reason);
                return;
            }
        };
        self.window_opened_ns = closed_ns;
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

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("a migration count was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the migration map held more rows than one drain can walk");
        }

        if self.counting() {
            // The map is still drained -- leaving it to fill would lose rows --
            // but what comes out is added to the window's total rather than
            // published as a slice of it.
            for row in rows {
                if !self.keeps(&row, window_ns) {
                    continue;
                }
                let key = (row.tid, row.dest_cpu);
                self.counted
                    .entry(key)
                    .and_modify(|held| {
                        held.count += row.count;
                        held.comm.clone_from(&row.comm);
                    })
                    .or_insert(row);
            }
            return;
        }

        let mut index = 0;
        for row in rows {
            if !self.keeps(&row, window_ns) {
                continue;
            }
            events.push(self.event(&row, closed_ns, window_ns, seq + index));
            index += 1;
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
            self.losses.note(
                "the kernel skipped a run of this program because one was already running on \
                 that cpu, so an occurrence was never seen; the count is the machine's, an \
                 upper bound on the target's share, and every total here is a lower bound",
            );
            self.skipped = skipped;
        }
        let unfollowed = program.unfollowed();
        if unfollowed > 0 {
            self.losses.note(format!(
                "{unfollowed} process{} the target started {} not followed: the map of the \
                 processes it started was full, so {} migrations are in no row",
                if unfollowed == 1 { "" } else { "es" },
                if unfollowed == 1 { "was" } else { "were" },
                if unfollowed == 1 { "its" } else { "their" },
            ));
        }
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note(
                "more distinct (task, cpu) pairs migrated in one interval than the map holds, \
                 so those migrations were never counted",
            );
            self.dropped = dropped;
        }
    }
}

/// What an event carries beside its value.
///
/// `comm`, `pid` and `cpu` are here because the catalog entry declares them as
/// filter fields: a predicate over a field no event carries is false
/// everywhere, which empties the series and looks exactly like a task that
/// never moved.
fn attributes(row: &Migration, window_ns: u64) -> Vec<Attr> {
    vec![
        Attr::uint("migrations", row.count),
        // The interval the count covers. A count without its window cannot be
        // compared with another count.
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", u64::from(row.tid)),
        Attr::uint("pid", u64::from(row.tgid)),
        Attr::text("comm", row.comm.clone()),
        // The cpu the task arrived on. Named `cpu` because that is the filter
        // field the catalog declares, and `dest_cpu` beside it because "the
        // cpu" of a migration is ambiguous and this one is not the origin.
        Attr::uint("cpu", u64::from(row.dest_cpu)),
        Attr::uint("dest_cpu", u64::from(row.dest_cpu)),
    ]
}

impl Collector for MigrationCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "migrations"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(MigrationProgram::load(self.filter)?);
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
                self.exec_note = Some(format!(
                    "a task that exec'd during a window is named what it was when its first \
                     row was made, because the exec program could not be attached: {reason}"
                ));
                None
            }
        };
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
            let counted = std::mem::take(&mut self.counted);
            trailing.extend(
                counted
                    .values()
                    .enumerate()
                    .map(|(index, row)| self.event(row, closed_ns, window_ns, seq + index as u64)),
            );
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
        let mut notes: Vec<String> = self.exec_note.iter().cloned().collect();
        if self.filtered_in > 0 || self.filtered_out > 0 {
            notes.push(format!(
                "the filter kept {} migrations and removed {}; it ran here, over the drained \
                 rows, not in the kernel",
                self.filtered_in, self.filtered_out,
            ));
        }
        if self.counting() {
            notes.push(
                "`count` reports one total per (task, cpu) over the whole window, so there is \
                 no series here and `rate` over it is the window's mean; `--mode trace` is the \
                 one that reports a count per drain"
                    .to_owned(),
            );
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Migration {
        Migration {
            tid: 11,
            tgid: 11,
            dest_cpu: 5,
            count: 3,
            comm: "spinner".to_owned(),
        }
    }

    /// A migration is an instant, and a frequency of instants counts how often
    /// the sampler looked rather than how often the task moved.
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
        assert!(error.contains("cannot count migrations"), "{error}");
    }

    /// Both modes the catalog declares must be served, or a request the
    /// validator accepted is refused here, which sends the reader nowhere.
    #[test]
    fn every_mode_the_catalog_declares_is_accepted() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let entry = catalog
            .get(&"sched.migrations".parse().expect("a source id"))
            .expect("sched.migrations is in the catalog");
        for mode in [Mode::Trace, Mode::Count] {
            assert!(
                entry.modes.iter().any(|declared| *declared == mode.kind()),
                "this test is written against the entry's own modes"
            );
            // A target with no process is the only thing that may be refused
            // here, and `--system` is not one.
            assert!(
                Box::new(Prepare { mode, filter: None })
                    .attach(&Target::System)
                    .is_ok(),
                "`{mode}` is declared and refused"
            );
        }
    }

    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let Err(error) = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        })
        .attach(&Target::Iface("eth0".to_owned())) else {
            panic!("an interface names no task to migrate");
        };
        assert!(
            error.contains("iface") && error.contains("--system"),
            "{error}"
        );
    }

    /// Every filter field the catalog offers must be on the events, or a
    /// predicate over it is false everywhere and silently empties the series.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&row(), 250_000_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"sched.migrations".parse().expect("a source id"))
            .expect("sched.migrations is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty(), "the entry declares filter fields");
        for field in declared {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// "The cpu" of a migration is ambiguous and this one is the destination.
    /// Both names carry it, and they must carry the same number: a reader who
    /// filters on `cpu` and a reader who reads `dest_cpu` must not be looking
    /// at two different things.
    #[test]
    fn the_cpu_on_the_event_is_the_one_the_task_arrived_on() {
        let attributes = attributes(&row(), 1);
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
        };
        assert_eq!(of("cpu"), Some(5.0));
        assert_eq!(of("dest_cpu"), of("cpu"));
    }
}
