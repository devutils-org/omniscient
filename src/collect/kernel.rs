//! What every collector reading an eBPF program's output does the same way,
//! behind one interface each:
//!
//! - [`KernelCounts`]: the counters a program keeps of what it lost --
//!   runs the kernel skipped, records a full map or ring refused -- are
//!   cumulative, read at every drain; what grew since the last read is a loss,
//!   counted once and said in the program's words.
//! - [`UserFilter`]: a predicate the program could not carry is applied to
//!   what was drained, and what it kept and removed is said, so a filter that
//!   matched nothing does not read as a machine that did nothing.
//! - [`Drains`]: a program whose rows are taken whole at each drain, and
//!   [`drain_rows`], which turns what the walk lost into losses.
//!
//! Each was written into twenty collectors, with twenty spellings of the same
//! sentence; none of them can be wrong in one collector and right in another
//! now. All three are tested on fabricated counts and rows: nothing here needs
//! a program loaded, or the capabilities to load one.

use std::collections::BTreeMap;

use crate::collect::Losses;
use crate::event::Event;
use crate::filter::FilterExpr;

/// One cumulative counter a program keeps of what it lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lost {
    /// Names the counter across reads; not shown.
    pub key: &'static str,
    /// The counter's value now, since the program was loaded.
    pub total: u64,
    /// What one count of it means, said to the reader.
    pub said: String,
    /// Whether each count is an occurrence that is in no event -- a loss --
    /// rather than a fact about the run that loses nothing.
    pub dropped: bool,
}

impl Lost {
    /// An occurrence the program never saw, or saw and could not record.
    pub fn dropped(key: &'static str, total: u64, said: impl Into<String>) -> Self {
        Self {
            key,
            total,
            said: said.into(),
            dropped: true,
        }
    }

    /// The runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it. Counted in
    /// `bpf_prog_info.recursion_misses` and nowhere else; the count is the
    /// machine's, an upper bound on the target's share.
    pub fn skipped_runs(total: u64) -> Self {
        Self::dropped(
            "skipped",
            total,
            "the kernel skipped a run of this program because one was already running on that \
             cpu, so an occurrence was never seen; the count is the machine's, an upper bound on \
             the target's share, and every total here is a lower bound",
        )
    }
}

/// The last value read of each of a program's loss counters.
#[derive(Debug, Clone, Default)]
pub struct KernelCounts {
    seen: BTreeMap<&'static str, u64>,
}

impl KernelCounts {
    /// Adds to `losses` what each counter grew by since it was last read.
    pub fn absorb(&mut self, losses: &mut Losses, counters: impl IntoIterator<Item = Lost>) {
        for counter in counters {
            let seen = self.seen.entry(counter.key).or_insert(0);
            if counter.total <= *seen {
                continue;
            }
            let grew = counter.total - *seen;
            *seen = counter.total;
            if counter.dropped {
                losses.dropped += grew;
            }
            losses.note_times(counter.said, grew);
        }
    }

    /// The last value read of one counter.
    pub fn read(&self, key: &str) -> u64 {
        self.seen.get(key).copied().unwrap_or(0)
    }
}

/// A predicate applied to drained events, and its account.
#[derive(Debug, Clone)]
pub struct UserFilter {
    predicate: Option<FilterExpr>,
    /// What one kept or removed count is, plural: "waits", "requests".
    what: &'static str,
    kept: u64,
    removed: u64,
}

impl UserFilter {
    pub fn new(predicate: Option<FilterExpr>, what: &'static str) -> Self {
        Self {
            predicate,
            what,
            kept: 0,
            removed: 0,
        }
    }

    /// Whether `event` passes, counting it as `weight` occurrences -- a
    /// drained row stands for every occurrence in it.
    pub fn keep(&mut self, event: &Event, weight: u64) -> bool {
        let Some(predicate) = &self.predicate else {
            return true;
        };
        if crate::filter::matches(predicate, event) {
            self.kept += weight;
            true
        } else {
            self.removed += weight;
            false
        }
    }

    /// Whether `event` passes by the collector's own `matches` -- one half of
    /// a pair, a kind the predicate names -- counting it as `weight`
    /// occurrences as [`UserFilter::keep`] does.
    pub fn keep_with(
        &mut self,
        event: &Event,
        weight: u64,
        matches: impl FnOnce(&FilterExpr, &Event) -> bool,
    ) -> bool {
        let Some(predicate) = &self.predicate else {
            return true;
        };
        if matches(predicate, event) {
            self.kept += weight;
            true
        } else {
            self.removed += weight;
            false
        }
    }

    /// The predicate itself, for a collector that decides with more than
    /// [`UserFilter::keep`] does: one half of a pair, a field a program reads.
    pub fn expression(&self) -> Option<&FilterExpr> {
        self.predicate.as_ref()
    }

    /// Counts verdicts reached elsewhere -- inside a drain's closure, which
    /// cannot hold this filter mutably -- as `kept` and `removed` occurrences.
    /// A filter with no predicate counts nothing, as `keep` does not.
    pub fn tally(&mut self, kept: u64, removed: u64) {
        if self.predicate.is_some() {
            self.kept += kept;
            self.removed += removed;
        }
    }

    /// What the filter kept and removed, once it saw anything.
    pub fn note(&self) -> Option<String> {
        (self.kept > 0 || self.removed > 0).then(|| {
            format!(
                "the filter kept {} {what} and removed {}; it ran here, over what was drained, \
                 not in the kernel, so the removed ones were counted before they were dropped",
                self.kept,
                self.removed,
                what = self.what,
            )
        })
    }
}

/// A program whose rows are taken whole at each drain.
pub trait Drains {
    type Row;

    /// Takes every row the kernel accumulated since the last drain.
    fn drain(&self) -> Result<Drained<Self::Row>, String>;

    /// The program's loss counters, as they stand.
    fn lost(&self) -> Vec<Lost>;
}

/// One drain's rows, and what the walk lost.
#[derive(Debug, Clone, PartialEq)]
pub struct Drained<R> {
    pub rows: Vec<R>,
    /// Rows the walk listed and the read no longer found, or found the
    /// wrong size.
    pub vanished: u64,
    /// The walk stopped at the map's capacity rather than at its end.
    pub truncated: bool,
}

/// Drains `program`, saying in `losses` what the walk lost: `what` names one
/// row ("a run queue row"). `None` when the drain failed, which is a failed
/// read, counted.
pub fn drain_rows<P: Drains>(program: &P, losses: &mut Losses, what: &str) -> Option<Vec<P::Row>> {
    let drained = match program.drain() {
        Ok(drained) => drained,
        Err(reason) => {
            losses.failed_reads += 1;
            losses.note_times(reason, 1);
            return None;
        }
    };
    if drained.vanished > 0 {
        losses.undecoded += drained.vanished;
        losses.note(format!(
            "{what} was gone between being listed and being read"
        ));
    }
    if drained.truncated {
        losses.note("the map held more rows than one drain can walk");
    }
    Some(drained.rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Attr, CollectorId};
    use std::cell::RefCell;

    /// A program fed by hand: what each drain returns, and the counters.
    struct Fed {
        drains: RefCell<Vec<Result<Drained<u64>, String>>>,
        lost: Vec<Lost>,
    }

    impl Drains for Fed {
        type Row = u64;
        fn drain(&self) -> Result<Drained<u64>, String> {
            self.drains.borrow_mut().remove(0)
        }
        fn lost(&self) -> Vec<Lost> {
            self.lost.clone()
        }
    }

    #[test]
    fn a_counter_is_a_loss_by_what_it_grew_and_said_once_per_growth() {
        let mut counts = KernelCounts::default();
        let mut losses = Losses::default();
        counts.absorb(&mut losses, [Lost::skipped_runs(3)]);
        counts.absorb(&mut losses, [Lost::skipped_runs(3)]);
        counts.absorb(&mut losses, [Lost::skipped_runs(5)]);
        assert_eq!(losses.dropped, 5);
        assert_eq!(counts.read("skipped"), 5);
        let said = format!("{losses:?}");
        assert!(said.contains("skipped a run"), "{said}");
    }

    #[test]
    fn a_fact_counter_is_said_and_never_counted_as_dropped() {
        let mut counts = KernelCounts::default();
        let mut losses = Losses::default();
        let fact = Lost {
            key: "open",
            total: 2,
            said: "still open at the end".into(),
            dropped: false,
        };
        counts.absorb(&mut losses, [fact]);
        assert_eq!(losses.dropped, 0);
        assert!(!losses.is_clean() || format!("{losses:?}").contains("still open"));
    }

    #[test]
    fn a_filter_weighs_each_row_by_what_it_stands_for_and_says_so() {
        let mut filter = UserFilter::new(Some("comm == 'kept'".parse().unwrap()), "waits");
        let event = |comm: &str| Event {
            ts_ns: 1,
            ts_err_ns: 0,
            kind: "sched.runqlat".into(),
            keys: Vec::new(),
            attrs: vec![Attr::text("comm", comm)],
            collector_id: CollectorId::unstamped(),
            seq: 0,
        };
        assert!(filter.keep(&event("kept"), 4));
        assert!(!filter.keep(&event("other"), 11));
        let note = filter.note().expect("a filter that ran says so");
        assert!(note.contains("kept 4 waits and removed 11"), "{note}");
        assert!(note.contains("not in the kernel"), "{note}");
        assert_eq!(UserFilter::new(None, "waits").note(), None);
    }

    #[test]
    fn a_drain_says_what_the_walk_lost_and_a_failed_one_is_a_failed_read() {
        let program = Fed {
            drains: RefCell::new(vec![
                Ok(Drained {
                    rows: vec![1, 2],
                    vanished: 1,
                    truncated: true,
                }),
                Err("cannot read the map".into()),
            ]),
            lost: Vec::new(),
        };
        let mut losses = Losses::default();
        assert_eq!(drain_rows(&program, &mut losses, "a row"), Some(vec![1, 2]));
        assert_eq!(losses.undecoded, 1);
        assert_eq!(drain_rows(&program, &mut losses, "a row"), None);
        assert_eq!(losses.failed_reads, 1);
        assert!(program.lost().is_empty());
    }
}
