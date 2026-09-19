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

use std::time::Duration as StdDuration;

use crate::bpf::{Migration, MigrationProgram};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

/// How often the counts are drained.
///
/// Slower than the sources that time something: a migration count is not a
/// duration, so a coarse interval costs resolution in the `rate` series and
/// nothing else. It is still frequent enough that the map stays far from the
/// capacity at which it would refuse rows.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(250);

/// Holds the request until the target exists.
pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.0, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot count migrations: a migration is an instant, and sampling \
                 instants at a frequency counts the samples rather than the migrations",
                self.0
            ));
        }
        let filter = match target {
            Target::System => None,
            other => Some(other.first_pid().ok_or_else(|| {
                format!(
                    "a {} target names no process, and a migration happens to a task; use \
                     --system to watch the whole machine",
                    other.kind()
                )
            })? as u32),
        };
        Ok(Box::new(MigrationCollector::new(filter)))
    }
}

pub struct MigrationCollector {
    id: CollectorId,
    filter: Option<u32>,
    program: Option<MigrationProgram>,
    losses: Losses,
    /// The program's counter is cumulative, so what is new is the difference.
    dropped: u64,
    /// The end of the last interval, which is the start of this one.
    window_opened_ns: u64,
}

impl MigrationCollector {
    pub fn new(filter: Option<u32>) -> Self {
        Self {
            id: CollectorId::new("sched.migrations/bpf:raw_tp"),
            filter,
            program: None,
            losses: Losses::default(),
            dropped: 0,
            window_opened_ns: 0,
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
                self.losses.note(reason);
                return;
            }
        };
        self.window_opened_ns = closed_ns;

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("a migration count was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the migration map held more rows than one drain can walk");
        }

        for (index, row) in rows.into_iter().enumerate() {
            events.push(Event {
                ts_ns: closed_ns,
                ts_err_ns: 0,
                kind: "sched.migrations".to_owned(),
                keys: vec![
                    // The migrated task is the series. Not the cpu: the cpu
                    // gained a task and the task lost its caches, and it is the
                    // second that anyone is measuring.
                    KeyValue::task(row.tid as i32),
                    KeyValue::cpu(row.dest_cpu),
                ],
                attrs: attributes(&row, window_ns),
                collector_id: self.id.clone(),
                seq: seq + index as u64,
            });
        }
    }

    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
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
        self.program = None;
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
        let prepared = Box::new(Prepare(Mode::Sample {
            freq: "997hz".parse().expect("a frequency"),
        }));
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
                Box::new(Prepare(mode)).attach(&Target::System).is_ok(),
                "`{mode}` is declared and refused"
            );
        }
    }

    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let Err(error) = Box::new(Prepare(Mode::Trace)).attach(&Target::Iface("eth0".to_owned()))
        else {
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
