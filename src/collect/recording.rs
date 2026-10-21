//! A run with no fixed end, keeping what its retention holds and handing a
//! document over whenever its trigger fires.

use super::*;

/// How recently a collector must have been read for that read to stand as the
/// window's closing read: well under any tick a collector keeps, and well over
/// the gap between two reads of one round.
pub(super) const CLOSING_READ_SLACK_NS: u64 = 1_000_000;

/// The span a recording's budget is weighed over, at the least: the retention
/// bound when it is longer. Two seconds is twenty budget checks, which is
/// enough for a percentage not to be one unlucky read, and short enough that a
/// burst is weighed against its own seconds rather than the quiet before it.
pub(super) const RECORDING_BUDGET_FLOOR: StdDuration = StdDuration::from_secs(2);

/// How a stop for a rolling budget names the span, so a caller reading the
/// reason can tell it from a stop over the whole window.
pub const ROLLING_BUDGET: &str = "the span a run with no fixed end is weighed over";

/// How every stop the budget made says the ceiling it broke, so a caller can
/// tell such a stop from the deadline's or the program's whatever the source.
pub const OVER_ITS_CEILING: &str = "against a ceiling of";

/// Whether a run ended because the request of `source` under `ceiling` (its
/// budget, in percent) was over it: the reason a stop of [`Spending::check`]
/// gives, for a fixed window or a rolling span. The ceiling is part of the
/// match because two requests of one source can carry two budgets, and the
/// one under the higher ceiling did not stop the run the lower one stopped.
pub fn stopped_by_budget(stopped_because: &str, source: &str, ceiling: f64) -> bool {
    stopped_because.starts_with(&format!("`{source}` spent"))
        && stopped_because.contains(&format!("{OVER_ITS_CEILING} {ceiling} %"))
}

/// How often a recording looks for events past its retention, as a fraction
/// of the bound: evicting on every round of a busy stream would walk the whole
/// buffer each time, and the buffer holds at most this fraction more than the
/// bound. A document is cut from the exact window whatever this is.
pub(super) const EVICTION_FRACTION: u64 = 16;

/// A document the trigger opened and `after` has not closed yet.
pub(super) struct Pending {
    pub(super) fired: Fired,
    pub(super) close_at_ns: u64,
}

/// A recording in progress: the runner's half of a [`Recorder`].
///
/// It keeps no events of its own. The ledger's are the buffer; this decides
/// what they may keep, reads the trigger's request after each round, and cuts
/// documents out of them.
pub(super) struct Recording<'r, 'a> {
    pub(super) asked: &'r mut Recorder<'a>,
    /// The trigger with `value` read as the collector's own name for it.
    pub(super) trigger: crate::filter::FilterExpr,
    pub(super) predicate: String,
    pub(super) value_attr: String,
    pub(super) labels: Vec<String>,
    pub(super) run_opened_ns: u64,
    /// True until the predicate matches, and again after a round in which it
    /// did not: a trigger fires on the round its predicate becomes true, so a
    /// level held for a second writes one document and not ten.
    pub(super) armed: bool,
    pub(super) pending: Option<Pending>,
    pub(super) written: Vec<Fired>,
    /// How many events at the front of each buffer are in a document already,
    /// so that evicting them is not a loss.
    pub(super) documented: Vec<usize>,
    /// When the last document was cut: a round read at or before it is in
    /// that document or was counted when it was evicted.
    pub(super) last_cut_ns: u64,
    /// When each round of each request read, for as long as it is retained.
    pub(super) rounds: Vec<std::collections::VecDeque<u64>>,
    pub(super) evicted_events: Vec<u64>,
    pub(super) evicted_rounds: Vec<u64>,
    pub(super) last_evicted_ns: u64,
    /// Each collection's losses at the last cut, runner's and collector's
    /// together, which a document's are the difference from.
    pub(super) losses_at_cut: Vec<Losses>,
    /// What was read after the last document and is in none, once the run
    /// is over.
    pub(super) unrecorded: Vec<Evicted>,
}

impl<'r, 'a> Recording<'r, 'a> {
    pub(super) fn new(
        asked: &'r mut Recorder<'a>,
        resolved: &[ResolvedRequest],
        slots: &[Slot],
        run_opened_ns: u64,
    ) -> Self {
        let value_attr = match &slots[asked.on] {
            Slot::Attached(collector) => collector.value_attr().to_owned(),
            _ => String::new(),
        };
        let predicate = asked.trigger.to_string();
        let trigger = read_value_as(&asked.trigger, &value_attr);
        let count = resolved.len();
        Recording {
            asked,
            trigger,
            predicate,
            value_attr,
            labels: resolved
                .iter()
                .map(|one| {
                    one.request
                        .label
                        .clone()
                        .unwrap_or_else(|| one.request.source.to_string())
                })
                .collect(),
            run_opened_ns,
            armed: true,
            pending: None,
            written: Vec::new(),
            documented: vec![0; count],
            last_cut_ns: run_opened_ns,
            rounds: vec![std::collections::VecDeque::new(); count],
            evicted_events: vec![0; count],
            evicted_rounds: vec![0; count],
            last_evicted_ns: run_opened_ns,
            losses_at_cut: vec![Losses::default(); count],
            unrecorded: Vec::new(),
        }
    }

    /// When the open document is due to be cut, so the run wakes for it.
    pub(super) fn due_ns(&self) -> Option<u64> {
        self.pending.as_ref().map(|pending| pending.close_at_ns)
    }

    /// After a round's reads: note which requests read, read the trigger's
    /// new events, cut what is due, and evict what the retention no longer
    /// holds. Returns why the run ends, once the recording has written all
    /// it may or could not write what it cut.
    pub(super) fn round(
        &mut self,
        read: &[bool],
        before: &[usize],
        ledger: &mut Ledger,
        slots: &[Slot],
        spending: &Spending<'_>,
    ) -> Option<String> {
        let now_ns = monotonic_ns();
        for (index, was_read) in read.iter().enumerate() {
            if *was_read {
                self.rounds[index].push_back(now_ns);
            }
        }

        let on = self.asked.on;
        if read[on] {
            let fresh = &ledger.events[on][before[on].min(ledger.events[on].len())..];
            let serving = &spending.serving[on];
            let filtered_here = matches!(serving.filter, Some(FilterPlacement::Userspace(_)));
            let matched = fresh.iter().find(|event| {
                let kept = !filtered_here
                    || serving
                        .request
                        .filter
                        .as_ref()
                        .is_none_or(|filter| crate::filter::matches(filter, event));
                kept && crate::filter::matches(&self.trigger, event)
            });
            match matched {
                None => self.armed = true,
                Some(_) if !self.armed => {}
                Some(event) => {
                    self.armed = false;
                    match self.pending.as_mut() {
                        Some(pending) => pending.fired.fired_again += 1,
                        None => {
                            let fired = self.fire(now_ns, event);
                            self.pending = Some(Pending {
                                close_at_ns: now_ns + self.asked.after_ns,
                                fired,
                            });
                        }
                    }
                }
            }
        }

        if self
            .pending
            .as_ref()
            .is_some_and(|pending| now_ns >= pending.close_at_ns)
        {
            if let Some(reason) = self.cut(now_ns, None, ledger, slots, spending) {
                return Some(reason);
            }
        }

        let every_ns = (self.asked.retain_ns / EVICTION_FRACTION).max(1);
        if now_ns.saturating_sub(self.last_evicted_ns) >= every_ns {
            self.last_evicted_ns = now_ns;
            let mut cutoff_ns = now_ns.saturating_sub(self.asked.retain_ns);
            if let Some(pending) = &self.pending {
                cutoff_ns = cutoff_ns.min(pending.fired.opened_ns);
            }
            for index in 0..ledger.events.len() {
                self.evict_before(index, cutoff_ns, ledger);
            }
        }
        None
    }

    /// The trigger half of a document, at the round that fired.
    fn fire(&self, now_ns: u64, event: &Event) -> Fired {
        let fields = self
            .asked
            .trigger
            .fields()
            .into_iter()
            .filter_map(|field| {
                let name = if field == Recording::VALUE_NAME {
                    self.value_attr.as_str()
                } else {
                    field
                };
                let value = if name == "kind" {
                    Some(crate::event::AttrValue::Text(event.kind.clone()))
                } else {
                    event.attr(name).cloned()
                };
                value.map(|value| (field.to_owned(), value))
            })
            .collect();
        Fired {
            number: self.written.len() as u32 + 1,
            of_at_most: self.asked.documents,
            predicate: self.predicate.clone(),
            on: self.labels[self.asked.on].clone(),
            fired_ns: now_ns,
            event_ts_ns: event.ts_ns,
            value_attr: self.value_attr.clone(),
            value: event
                .attr(&self.value_attr)
                .and_then(|value| value.as_f64()),
            fields,
            run_opened_ns: self.run_opened_ns,
            opened_ns: now_ns
                .saturating_sub(self.asked.retain_ns)
                .max(self.run_opened_ns),
            closed_ns: now_ns,
            retain_ns: self.asked.retain_ns,
            after_ns: self.asked.after_ns,
            cut_short: None,
            fired_again: 0,
            evicted: Vec::new(),
        }
    }

    /// Evicts what is older than `cutoff_ns` from one request's buffer, and
    /// counts what no document held.
    fn evict_before(&mut self, index: usize, cutoff_ns: u64, ledger: &mut Ledger) {
        let events = &mut ledger.events[index];
        if events.iter().any(|event| event.ts_ns < cutoff_ns) {
            let documented = self.documented[index];
            let mut position = 0usize;
            let mut kept_documented = 0usize;
            let mut evicted = 0u64;
            events.retain(|event| {
                let was_documented = position < documented;
                position += 1;
                let keep = event.ts_ns >= cutoff_ns;
                if keep && was_documented {
                    kept_documented += 1;
                }
                if !keep && !was_documented {
                    evicted += 1;
                }
                keep
            });
            self.documented[index] = kept_documented;
            self.evicted_events[index] += evicted;
        }
        let rounds = &mut self.rounds[index];
        while rounds.front().is_some_and(|at| *at < cutoff_ns) {
            let at = rounds.pop_front().unwrap_or_default();
            if at > self.last_cut_ns {
                self.evicted_rounds[index] += 1;
            }
        }
    }

    /// Cuts the open document at `now_ns` and hands it out. Returns why the
    /// run ends, when it does: the last document the bound allows, or one
    /// that could not be written.
    fn cut(
        &mut self,
        now_ns: u64,
        cut_short: Option<String>,
        ledger: &mut Ledger,
        slots: &[Slot],
        spending: &Spending<'_>,
    ) -> Option<String> {
        let mut fired = self.pending.take()?.fired;
        fired.closed_ns = now_ns;
        fired.cut_short = cut_short;
        let opened_ns = fired.opened_ns;
        for index in 0..ledger.events.len() {
            self.evict_before(index, opened_ns, ledger);
        }
        fired.evicted = (0..ledger.events.len())
            .map(|index| Evicted {
                request: self.labels[index].clone(),
                events: self.evicted_events[index],
                rounds: self.evicted_rounds[index],
            })
            .collect();

        let at_s = |ns: u64| ns.saturating_sub(self.run_opened_ns) as f64 / 1e9;
        let self_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
        let mut collections = Vec::with_capacity(slots.len());
        for (index, slot) in slots.iter().enumerate() {
            let (
                value_attr,
                stacks,
                collector_notes,
                collector_losses,
                collector_facts,
                row_stamp,
                failure,
            ) = match slot {
                Slot::Attached(collector) => (
                    collector.value_attr(),
                    collector.stacks(),
                    collector.notes(),
                    collector.losses(),
                    collector.facts(),
                    collector.row_stamp(),
                    collector.failure(),
                ),
                Slot::Detached(record) => (
                    record.value_attr,
                    record.stacks.clone(),
                    record.notes.clone(),
                    Losses::default(),
                    record.facts.clone(),
                    record.row_stamp,
                    record.failure.clone(),
                ),
                Slot::Waiting(_) | Slot::Missed => (
                    "",
                    BTreeMap::new(),
                    Vec::new(),
                    Losses::default(),
                    Vec::new(),
                    None,
                    None,
                ),
            };
            let mut events: Vec<Event> = ledger.events[index]
                .iter()
                .filter(|event| event.ts_ns >= opened_ns && event.ts_ns <= now_ns)
                .cloned()
                .collect();
            events.sort_by_key(|event| event.ts_ns);
            let filtered = apply_filter(&spending.serving[index], &mut events);

            let mut cumulative = ledger.losses[index].clone();
            cumulative.absorb(collector_losses);
            let mut losses = cumulative.since(&self.losses_at_cut[index]);
            losses.evicted = self.evicted_events[index];
            if losses.evicted > 0 {
                losses.note(format!(
                    "the retention held {}, so what this run read before {:.3} s -- and after \
                     the document before this one -- was evicted: observed, and in no document",
                    span_ns(fired.retain_ns),
                    at_s(opened_ns),
                ));
            }
            losses.summarize();
            self.losses_at_cut[index] = cumulative;

            let watched = ledger.watched[index];
            let window = Watched {
                run_opened_ns: self.run_opened_ns,
                opened_ns: opened_ns.max(watched.opened_ns),
                closed_ns: now_ns,
                attached: watched.attached,
                attach_ns: None,
            };
            let mut notes = spending.notes[index].clone();
            notes.extend(collector_notes);
            let observed_cpu_ns = spending.meters[index].read();
            collections.push(Collection {
                observed_ns: window.span_ns(),
                value_attr: value_attr.to_owned(),
                events,
                losses,
                stopped_because: format!(
                    "the trigger `{}` fired on `{}` at {:.3} s into the run{}",
                    fired.predicate,
                    fired.on,
                    at_s(fired.fired_ns),
                    fired.value.map_or_else(String::new, |value| format!(
                        ", with {} = {value}",
                        fired.value_attr
                    )),
                ),
                stacks,
                notes,
                filtered,
                served_by: spending.served_by[index].clone(),
                overhead: spending.spent(
                    index,
                    spending.whole_run(index),
                    self_cpu_ns,
                    observed_cpu_ns,
                    &spending.inside[index].peek(),
                    now_ns,
                ),
                lifetime: None,
                window,
                detached: false,
                // A document's losses are those since the one before it, and a
                // ring's are the run's: they cannot be split per document.
                by_ring: Vec::new(),
                facts: collector_facts,
                row_stamp,
                // The drains read inside this document's window. The first
                // may hold drops made before it opened: a drain counts what
                // was dropped since the one before it.
                drains: ledger.drains[index]
                    .placed
                    .iter()
                    .filter(|drain| drain.ts_ns >= opened_ns && drain.ts_ns <= now_ns)
                    .copied()
                    .collect(),
                failure,
                // The collector's counts so far: a count is the run's up to
                // this document, as its facts are.
                fields: slot_fields(slot),
                provenance: slot_provenance(slot),
            });
        }

        // What this document holds is documented: evicting it later is not
        // a loss, and what is evicted from here on is counted for the next.
        for index in 0..ledger.events.len() {
            self.documented[index] = ledger.events[index].len();
            self.evicted_events[index] = 0;
            self.evicted_rounds[index] = 0;
        }
        self.last_cut_ns = now_ns;
        self.written.push(fired.clone());
        let number = fired.number;
        if let Err(why) = (self.asked.sink)(Document {
            trigger: fired,
            collections,
        }) {
            return Some(format!("document {number} could not be written: {why}"));
        }
        (self.written.len() as u32 >= self.asked.documents).then(|| {
            format!(
                "the recording wrote {} document(s), the most it was allowed",
                self.written.len()
            )
        })
    }

    /// The run is over: cuts a document still open, short, and counts what
    /// was read after the last document and is in none.
    pub(super) fn end(
        &mut self,
        why: &str,
        ledger: &mut Ledger,
        slots: &[Slot],
        spending: &Spending<'_>,
    ) {
        if self.pending.is_some() {
            let now_ns = monotonic_ns();
            let cut_short = format!("the run ended before `after` had passed ({why})");
            let _ = self.cut(now_ns, Some(cut_short), ledger, slots, spending);
        }
        self.unrecorded = (0..ledger.events.len())
            .map(|index| {
                let retained = ledger.events[index]
                    .len()
                    .saturating_sub(self.documented[index]) as u64;
                let rounds = self.rounds[index]
                    .iter()
                    .filter(|at| **at > self.last_cut_ns)
                    .count() as u64;
                Evicted {
                    request: self.labels[index].clone(),
                    events: self.evicted_events[index] + retained,
                    rounds: self.evicted_rounds[index] + rounds,
                }
            })
            .collect();
    }

    pub(super) fn recorded(
        &self,
        stopped_because: &str,
        closed_ns: u64,
        budget_interval_ns: u64,
    ) -> Recorded {
        Recorded {
            predicate: self.predicate.clone(),
            on: self.labels[self.asked.on].clone(),
            retain_ns: self.asked.retain_ns,
            after_ns: self.asked.after_ns,
            documents_at_most: self.asked.documents,
            documents: self.written.clone(),
            stopped_because: stopped_because.to_owned(),
            opened_ns: self.run_opened_ns,
            closed_ns,
            budget_interval_ns,
            unrecorded: self.unrecorded.clone(),
        }
    }
}

impl Recording<'_, '_> {
    const VALUE_NAME: &'static str = crate::request::Recording::VALUE;
}

/// The trigger with `value` renamed to the attribute the collector measures
/// in, so the filter grammar evaluates it as it would any field.
pub(super) fn read_value_as(
    expression: &crate::filter::FilterExpr,
    value_attr: &str,
) -> crate::filter::FilterExpr {
    use crate::filter::FilterExpr;
    match expression {
        FilterExpr::And(left, right) => FilterExpr::And(
            Box::new(read_value_as(left, value_attr)),
            Box::new(read_value_as(right, value_attr)),
        ),
        FilterExpr::Or(left, right) => FilterExpr::Or(
            Box::new(read_value_as(left, value_attr)),
            Box::new(read_value_as(right, value_attr)),
        ),
        FilterExpr::Not(inner) => FilterExpr::Not(Box::new(read_value_as(inner, value_attr))),
        FilterExpr::Compare {
            field,
            operator,
            value,
        } => FilterExpr::Compare {
            field: if field == crate::request::Recording::VALUE && !value_attr.is_empty() {
                value_attr.to_owned()
            } else {
                field.clone()
            },
            operator: *operator,
            value: value.clone(),
        },
    }
}
