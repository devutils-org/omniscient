//! `sched.switches -m count` over a `sched_switch` program, counted in the
//! kernel: the backend that serves a cgroup, and the one that tells a
//! voluntary switch from an involuntary one.
//!
//! The chain's first backend for this source is `pmu:sw`, an inherited
//! software counter that is exact and free and follows threads and cpus
//! (`docs/DECISIONS.md`, the fourteenth wave, says why it stays first). This one
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
use crate::collect::kernel::{KernelCounts, Lost, UserFilter};
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
    /// Through the classic `sched:sched_switch` (`bpf:tp`), for a kernel
    /// without BTF.
    pub classic: bool,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Count {
            return Err(format!(
                "the `sched_switch` program counts switches in the kernel and serves `count` \
                 only; `{}` is \
                 `pmu:sw`'s, which records each switch as it happens",
                self.mode
            ));
        }
        let whose = super::whose(target, "a context switch belongs to a task")?;
        let mut collector = SwitchCountCollector::new(whose, self.filter);
        collector.classic = self.classic;
        Ok(Box::new(collector))
    }
}

pub struct SwitchCountCollector {
    whose: Whose,
    /// Whether the program goes on the classic tracepoint.
    classic: bool,
    program: Option<SwitchCountProgram>,
    losses: Losses,
    /// The program's loss counters, as last read (`collect::kernel`).
    counts: KernelCounts,
    /// The program's own drop counter, cumulative, as last read.
    dropped: u64,
    /// When the window opened.
    opened_ns: u64,
    /// The window's totals, by `(tid, voluntary)`.
    counted: BTreeMap<(u32, bool), SwitchCount>,
    predicate: UserFilter,
    /// Each process's parent, for the rows' `ppid`: asked from `event`,
    /// which only reads the collector.
    parents: std::cell::RefCell<super::Parents>,
    renames: Option<Renames>,
    exec_note: Option<String>,
}

impl SwitchCountCollector {
    pub fn new(whose: Whose, predicate: Option<FilterExpr>) -> Self {
        Self {
            whose,
            classic: false,
            program: None,
            losses: Losses::default(),
            counts: KernelCounts::default(),
            dropped: 0,
            opened_ns: 0,
            counted: BTreeMap::new(),
            predicate: UserFilter::new(predicate, "switches"),
            parents: Default::default(),
            renames: None,
            exec_note: None,
        }
    }

    fn event(&self, row: &SwitchCount, ts_ns: u64, window_ns: u64, seq: u64) -> Event {
        let mut attrs = attributes(row, window_ns);
        if let Some(ppid) = self.parents.borrow_mut().of(row.tgid) {
            attrs.push(Attr::uint("ppid", u64::from(ppid)));
        }
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "sched.switches".to_owned(),
            keys: vec![KeyValue::task(row.tid as i32)],
            attrs,
            collector_id: CollectorId::unstamped(),
            seq,
        }
    }

    fn keeps(&mut self, row: &SwitchCount, window_ns: u64) -> bool {
        if self.predicate.expression().is_none() {
            return true;
        }
        let event = self.event(row, 0, window_ns, 0);
        self.predicate.keep(&event, row.count)
    }

    fn drain(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        // What the kernel saw the target fork, before `/proc` is asked.
        self.parents.borrow_mut().absorb(program.parents());
        let (mut rows, drain_losses) = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                self.losses.failed_reads += 1;
                self.losses.note_times(reason, 1);
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
        self.counts
            .absorb(&mut self.losses, [Lost::skipped_runs(program.skipped())]);
        let unfollowed = program.unfollowed();
        if unfollowed > 0 {
            self.losses.note(format!(
                "{unfollowed} process{} the target started {} not followed: the map of the \
                 processes it started was full, so {} switches are in no row",
                if unfollowed == 1 { "" } else { "es" },
                if unfollowed == 1 { "was" } else { "were" },
                if unfollowed == 1 { "its" } else { "their" },
            ));
        }
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note_times(
                "more distinct (task, kind) pairs switched in one interval than the map holds, \
                 so those switches were never counted",
                dropped - self.dropped,
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
    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(if self.classic {
            SwitchCountProgram::load_classic(self.whose)?
        } else {
            SwitchCountProgram::load(self.whose)?
        });
        // A process target is followed into the processes it starts, and
        // those exec: the renames are the machine's then, applied by thread
        // id to the rows of the window, which are the target's alone.
        let renamed = match self.whose {
            Whose::Process(_) => Whose::Any,
            other => other,
        };
        self.renames = match Renames::load(renamed) {
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
        notes.extend(self.predicate.note());
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

    /// A row of a live process carries its parent as `ppid`, and a
    /// predicate over it keeps that process's rows: `--filter 'ppid == N'`
    /// is "the children of N".
    #[test]
    fn a_row_carries_its_process_s_parent_and_a_predicate_reads_it() {
        let me = std::process::id();
        let parent = std::os::unix::process::parent_id();
        let row = SwitchCount {
            tgid: me,
            ..row(true)
        };
        let collector = SwitchCountCollector::new(
            Whose::Any,
            Some(format!("ppid == {parent}").parse().expect("a predicate")),
        );
        let event = collector.event(&row, 1, 1, 0);
        assert_eq!(
            event.attr("ppid").and_then(|value| value.as_f64()),
            Some(f64::from(parent))
        );
        let mut collector = collector;
        assert!(collector.keeps(&row, 1));
    }

    /// `count` is this backend's one mode, and the refusal of the other names
    /// the backend that serves it.
    #[test]
    fn a_mode_other_than_count_is_refused_and_names_the_backend_that_serves_it() {
        let Err(error) = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
            classic: false,
        })
        .attach(&Target::System) else {
            panic!("trace is pmu:sw's");
        };
        assert!(error.contains("pmu:sw"), "{error}");
        assert!(Box::new(Prepare {
            mode: Mode::Count,
            filter: None,
            classic: false,
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
        // `ppid` is put on the event by the collector, from the process's
        // parent (`super::Parents`), not by the row's attributes.
        for field in fields.into_iter().filter(|field| field != "ppid") {
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
