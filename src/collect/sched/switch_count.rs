//! `sched.switches -m count` over a `sched_switch` program, counted in the
//! kernel: the backend that serves a cgroup, and the one that tells a
//! voluntary switch from an involuntary one.
//!
//! The chain's first backend for this source is `pmu:sw`, an inherited
//! software counter that is exact and free and follows threads and cpus
//! (`DECISIONS.md`, the fourteenth wave, says why it stays first). This one
//! is reached where that one cannot serve -- a cgroup, which a counter that
//! follows threads cannot see into -- or when it is forced
//! (`--backend force:bpf:raw_tp`) for the split.
//!
//! One event per `(task, kind)` over the whole window, the kind being the
//! kernel's own `nvcsw`/`nivcsw` rule (`src/bpf/switch_count.rs`), so the
//! voluntary and involuntary counts of a task add up to what
//! `/proc/<pid>/status` says it did over the same window.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use crate::bpf::{Renames, SwitchCount, SwitchCountProgram, Whose};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the kernel's map is emptied. The result is one total at the end,
/// so this is only how far the map is kept from the capacity at which it
/// would refuse rows.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(250);

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// The request's predicate, applied here per drained row; see
    /// `migrations::Prepare`.
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Count {
            return Err(format!(
                "`bpf:raw_tp` counts switches in the kernel and serves `count` only; `{}` is \
                 `pmu:sw`'s, which records each switch as it happens",
                self.mode
            ));
        }
        let whose = super::whose(target, "a context switch belongs to a task")?;
        Ok(Box::new(SwitchCountCollector::new(whose, self.filter)))
    }
}

pub struct SwitchCountCollector {
    id: CollectorId,
    whose: Whose,
    program: Option<SwitchCountProgram>,
    losses: Losses,
    /// The kernel's skipped runs of the program, cumulative, as last read.
    skipped: u64,
    /// The program's own drop counter, cumulative, as last read.
    dropped: u64,
    /// When the window opened.
    opened_ns: u64,
    /// The window's totals, by `(tid, voluntary)`.
    counted: BTreeMap<(u32, bool), SwitchCount>,
    predicate: Option<FilterExpr>,
    filtered_in: u64,
    filtered_out: u64,
    renames: Option<Renames>,
    exec_note: Option<String>,
}

impl SwitchCountCollector {
    pub fn new(whose: Whose, predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("sched.switches/bpf:raw_tp"),
            whose,
            program: None,
            losses: Losses::default(),
            skipped: 0,
            dropped: 0,
            opened_ns: 0,
            counted: BTreeMap::new(),
            predicate,
            filtered_in: 0,
            filtered_out: 0,
            renames: None,
            exec_note: None,
        }
    }

    fn event(&self, row: &SwitchCount, ts_ns: u64, window_ns: u64, seq: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "sched.switches".to_owned(),
            keys: vec![KeyValue::task(row.tid as i32)],
            attrs: attributes(row, window_ns),
            collector_id: self.id.clone(),
            seq,
        }
    }

    fn keeps(&mut self, row: &SwitchCount, window_ns: u64) -> bool {
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

    fn drain(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let (mut rows, drain_losses) = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                self.losses.failed_reads += 1;
                self.losses.note(reason);
                return;
            }
        };
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
                .note("a switch count was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the switch count map held more rows than one drain can walk");
        }
        let window_ns = monotonic_ns().saturating_sub(self.opened_ns);
        for row in rows {
            if !self.keeps(&row, window_ns) {
                continue;
            }
            self.counted
                .entry((row.tid, row.voluntary))
                .and_modify(|held| {
                    held.count += row.count;
                    held.comm.clone_from(&row.comm);
                })
                .or_insert(row);
        }
    }

    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note(
                "the kernel skipped a run of this program because one was already running on \
                 that cpu, so a switch was never seen; the count is the machine's, an upper \
                 bound on the target's share, and every total here is a lower bound",
            );
            self.skipped = skipped;
        }
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note(
                "more distinct (task, kind) pairs switched in one interval than the map holds, \
                 so those switches were never counted",
            );
            self.dropped = dropped;
        }
    }
}

/// What an event carries beside its value. `comm` and `pid` are the entry's
/// filter fields this backend can carry; `cpu` is not one of them, because a
/// task's switches over a window happen on many.
fn attributes(row: &SwitchCount, window_ns: u64) -> Vec<Attr> {
    vec![
        Attr::uint("count", row.count),
        Attr::text(
            "kind",
            if row.voluntary {
                "voluntary"
            } else {
                "involuntary"
            },
        ),
        Attr::uint("voluntary", u64::from(row.voluntary)),
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", u64::from(row.tid)),
        Attr::uint("pid", u64::from(row.tgid)),
        Attr::text("comm", row.comm.clone()),
    ]
}

impl Collector for SwitchCountCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(SwitchCountProgram::load(self.whose)?);
        self.renames = match Renames::load(self.whose) {
            Ok(renames) => Some(renames),
            Err(reason) => {
                self.exec_note = Some(format!(
                    "a task that exec'd during the window is named what it was when its first \
                     row was made, because the exec program could not be attached: {reason}"
                ));
                None
            }
        };
        self.opened_ns = monotonic_ns();
        Ok(())
    }

    fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
        self.drain();
        self.absorb_drops();
        Ok(Vec::new())
    }

    fn stop(&mut self) -> Vec<Event> {
        self.drain();
        self.absorb_drops();
        self.program = None;
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.opened_ns);
        let seq = u64::MAX / 2;
        let counted = std::mem::take(&mut self.counted);
        counted
            .values()
            .enumerate()
            .map(|(index, row)| self.event(row, closed_ns, window_ns, seq + index as u64))
            .collect()
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
                "the filter kept {} switches and removed {}; it ran here, over the drained \
                 rows, not in the kernel",
                self.filtered_in, self.filtered_out,
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(voluntary: bool) -> SwitchCount {
        SwitchCount {
            tid: 11,
            tgid: 10,
            voluntary,
            count: 3,
            comm: "sleeper".to_owned(),
        }
    }

    /// `count` is this backend's one mode, and the refusal of the other names
    /// the backend that serves it.
    #[test]
    fn a_mode_other_than_count_is_refused_and_names_the_backend_that_serves_it() {
        let Err(error) = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        })
        .attach(&Target::System) else {
            panic!("trace is pmu:sw's");
        };
        assert!(error.contains("pmu:sw"), "{error}");
        assert!(Box::new(Prepare {
            mode: Mode::Count,
            filter: None,
        })
        .attach(&Target::System)
        .is_ok());
    }

    /// Every filter field the catalog lets this backend take is on the event.
    #[test]
    fn every_filter_field_this_backend_declares_is_on_an_event() {
        let attributes = attributes(&row(true), 1);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let entry = catalog
            .get(&"sched.switches".parse().expect("a source id"))
            .expect("sched.switches is in the catalog");
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

    #[test]
    fn the_kind_is_carried_twice_in_words_that_agree() {
        for voluntary in [true, false] {
            let attributes = attributes(&row(voluntary), 1);
            let text = attributes
                .iter()
                .find(|attr| attr.name == "kind")
                .map(|attr| attr.value.to_string())
                .expect("a kind");
            assert_eq!(
                text.contains("involuntary"),
                !voluntary,
                "{text} against voluntary={voluntary}"
            );
        }
    }
}
