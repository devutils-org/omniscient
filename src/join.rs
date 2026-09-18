//! The join engine.
//!
//! A join takes two event streams and says which events belong together, and on
//! what grounds. The grounds matter as much as the answer: a correlation backed
//! by a propagated identity and one backed by two things happening at the same
//! time are both "correlations", and only one of them is evidence. Every result
//! here carries the level it was obtained at.
//!
//! Rules are declarative, so a new boundary is a new rule rather than new
//! engine code. Only the interval match is implemented: it is the one the first
//! real join needed, and generalizing an engine against match kinds nothing has
//! exercised yet is how the abstraction ends up fitting the imagined cases
//! instead of the real ones.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::catalog::MatchKind;
use crate::event::{Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::KeyKind;
use crate::scalar::Duration;

/// How strongly two events are tied together.
///
/// The order is the whole point: `IDENTITY` is a key the upstream event carried
/// and the downstream one read back, `TEMPORAL` is a guess that happens to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Evidence {
    /// Two things overlapped in time. A hypothesis, nothing more.
    Temporal,
    /// A wakeup chain links them: the only join that crosses a block.
    Wakeup,
    /// They share a typed key.
    Key,
    /// The downstream event carries an identity the upstream one propagated.
    Identity,
}

impl Evidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Evidence::Temporal => "TEMPORAL",
            Evidence::Wakeup => "WAKEUP",
            Evidence::Key => "KEY",
            Evidence::Identity => "IDENTITY",
        }
    }
}

impl std::fmt::Display for Evidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One declared boundary between two sources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    /// The event kind that opens a state.
    pub from: String,
    /// The event kind that is matched against open states.
    pub to: String,
    /// The typed key both sides carry.
    pub via: KeyKind,
    #[serde(rename = "match")]
    pub match_kind: MatchKind,
    /// Which `from` events open a state. Every one of them, when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opens: Option<FilterExpr>,
    /// Which `from` events close a state they had opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closes: Option<FilterExpr>,
    /// How long a state may stay open before it is evicted and counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Duration>,
    /// Most states kept at once. Exceeding it evicts the oldest, and says so.
    #[serde(default = "default_capacity")]
    pub capacity: usize,
    /// How many attributions to report.
    #[serde(default = "default_top")]
    pub top: usize,
}

fn default_capacity() -> usize {
    1_000_000
}

fn default_top() -> usize {
    10
}

/// Why a rule could not be applied.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum JoinError {
    #[error(
        "`{match_kind:?}` matching is not implemented yet: this milestone joins by interval. \
         The engine is generalized once there are several real joins to generalize from."
    )]
    UnsupportedMatch { match_kind: MatchKind },
    #[error("rule `{id}` joins on `{via}`, which `{kind}` events do not carry")]
    MissingKey {
        id: String,
        via: KeyKind,
        kind: String,
    },
}

/// What a rule produced over one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Correlation {
    pub rule: String,
    pub from: String,
    pub to: String,
    /// How strongly this join ties its two sides together.
    pub evidence: Evidence,
    /// Downstream events that landed inside an open state.
    pub matched: u64,
    /// Downstream events that landed in no open state. Not an error: an address
    /// outside every tracked allocation is a real thing, and hiding it would
    /// make the attributed share look like the whole.
    pub unmatched: u64,
    pub states: StateStats,
    /// The heaviest attributions, most matched first.
    pub top: Vec<Attribution>,
    /// Every attribution, totalled.
    pub total_attributed: u64,
}

/// The lifecycle of open states, which is where a join engine leaks if it does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateStats {
    pub opened: u64,
    pub closed: u64,
    /// Evicted because they outlived the window.
    pub expired: u64,
    /// Evicted because the map was full. A non-zero count means attributions
    /// are missing, not that nothing matched.
    pub overflowed: u64,
    /// Still open when the run ended.
    pub still_open: u64,
    pub peak_open: u64,
}

impl StateStats {
    pub fn is_clean(&self) -> bool {
        self.expired == 0 && self.overflowed == 0
    }
}

/// One upstream state and what was attributed to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attribution {
    /// The key as the catalog writes it.
    pub key: String,
    pub base: u64,
    pub len: u64,
    pub opened_ns: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_ns: Option<u64>,
    /// Downstream events that fell in this range.
    pub matched: u64,
    /// How the state left the engine.
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Closed,
    Expired,
    Overflowed,
    StillOpen,
}

struct OpenState {
    base: u64,
    len: u64,
    opened_ns: u64,
    matched: u64,
}

/// Applies one rule to a stream of events, in timestamp order.
pub fn apply(rule: &Rule, events: &[Event]) -> Result<Correlation, JoinError> {
    if rule.match_kind != MatchKind::Interval {
        return Err(JoinError::UnsupportedMatch {
            match_kind: rule.match_kind,
        });
    }

    // Two streams merge here, and at an equal timestamp the order decides the
    // answer: an allocation has to be open before a fault inside it can be
    // attributed. Sorting the opening side first at each instant is the only
    // ordering that cannot invent an unmatched fault.
    let mut ordered: Vec<&Event> = events.iter().collect();
    ordered.sort_by_key(|event| (event.ts_ns, u8::from(event.kind != rule.from)));
    let events = ordered;

    let mut open: BTreeMap<u64, OpenState> = BTreeMap::new();
    let mut stats = StateStats::default();
    let mut finished: Vec<Attribution> = Vec::new();
    let (mut matched, mut unmatched) = (0u64, 0u64);
    let window_ns = rule.window.map(Duration::as_nanos);

    for event in events {
        if let Some(window_ns) = window_ns {
            expire(&mut open, &mut stats, &mut finished, event.ts_ns, window_ns);
        }

        if event.kind == rule.from {
            let Some(KeyValue::Mem { base, len }) = event.key(rule.via).cloned() else {
                // A `from` event without the key cannot open anything. This is
                // a rule that does not fit its sources, not a runtime hiccup.
                return Err(JoinError::MissingKey {
                    id: rule.id.clone(),
                    via: rule.via,
                    kind: event.kind.clone(),
                });
            };

            let closes = rule
                .closes
                .as_ref()
                .is_some_and(|predicate| crate::filter::matches(predicate, event));
            if closes {
                if let Some(state) = open.remove(&base) {
                    stats.closed += 1;
                    push(&mut finished, state, Some(event.ts_ns), Outcome::Closed);
                }
                continue;
            }

            let opens = rule
                .opens
                .as_ref()
                .is_none_or(|predicate| crate::filter::matches(predicate, event));
            if opens {
                if open.len() >= rule.capacity {
                    evict_oldest(&mut open, &mut stats, &mut finished);
                }
                open.insert(
                    base,
                    OpenState {
                        base,
                        len,
                        opened_ns: event.ts_ns,
                        matched: 0,
                    },
                );
                stats.opened += 1;
                stats.peak_open = stats.peak_open.max(open.len() as u64);
            }
            continue;
        }

        if event.kind == rule.to {
            let Some(KeyValue::Mem { base: address, .. }) = event.key(rule.via) else {
                return Err(JoinError::MissingKey {
                    id: rule.id.clone(),
                    via: rule.via,
                    kind: event.kind.clone(),
                });
            };
            // The state whose range contains this address: the last one that
            // starts at or before it, if it is long enough to reach it.
            match open
                .range_mut(..=*address)
                .next_back()
                .filter(|(_, state)| *address < state.base.saturating_add(state.len))
            {
                Some((_, state)) => {
                    state.matched += 1;
                    matched += 1;
                }
                None => unmatched += 1,
            }
        }
    }

    stats.still_open = open.len() as u64;
    for (_, state) in std::mem::take(&mut open) {
        push(&mut finished, state, None, Outcome::StillOpen);
    }

    finished.retain(|attribution| attribution.matched > 0);
    finished.sort_by(|left, right| {
        right
            .matched
            .cmp(&left.matched)
            .then(left.base.cmp(&right.base))
    });
    let total_attributed = finished.iter().map(|attribution| attribution.matched).sum();
    finished.truncate(rule.top);

    Ok(Correlation {
        rule: rule.id.clone(),
        from: rule.from.clone(),
        to: rule.to.clone(),
        // A shared typed key, which is what an interval match is.
        evidence: Evidence::Key,
        matched,
        unmatched,
        states: stats,
        top: finished,
        total_attributed,
    })
}

fn push(
    finished: &mut Vec<Attribution>,
    state: OpenState,
    closed_ns: Option<u64>,
    outcome: Outcome,
) {
    finished.push(Attribution {
        key: KeyValue::Mem {
            base: state.base,
            len: state.len,
        }
        .to_string(),
        base: state.base,
        len: state.len,
        opened_ns: state.opened_ns,
        closed_ns,
        matched: state.matched,
        outcome,
    });
}

/// Drops states that outlived the window, counting them.
///
/// An open state that is never closed and never expires is a leak, and a leak
/// in a join engine is a process that grows until it is killed halfway through
/// a measurement.
fn expire(
    open: &mut BTreeMap<u64, OpenState>,
    stats: &mut StateStats,
    finished: &mut Vec<Attribution>,
    now_ns: u64,
    window_ns: u64,
) {
    let cutoff = now_ns.saturating_sub(window_ns);
    let stale: Vec<u64> = open
        .iter()
        .filter(|(_, state)| state.opened_ns < cutoff)
        .map(|(base, _)| *base)
        .collect();
    for base in stale {
        if let Some(state) = open.remove(&base) {
            stats.expired += 1;
            push(finished, state, None, Outcome::Expired);
        }
    }
}

fn evict_oldest(
    open: &mut BTreeMap<u64, OpenState>,
    stats: &mut StateStats,
    finished: &mut Vec<Attribution>,
) {
    let oldest = open
        .iter()
        .min_by_key(|(_, state)| state.opened_ns)
        .map(|(base, _)| *base);
    if let Some(base) = oldest {
        if let Some(state) = open.remove(&base) {
            stats.overflowed += 1;
            push(finished, state, None, Outcome::Overflowed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Attr, CollectorId};

    fn allocation(ts_ns: u64, base: u64, len: u64, op: &str) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.heap.alloc".to_owned(),
            keys: vec![KeyValue::task(1), KeyValue::Mem { base, len }],
            attrs: vec![Attr::text("op", op), Attr::uint("bytes", len)],
            collector_id: CollectorId::new("alloc"),
            seq: 0,
        }
    }

    fn fault(ts_ns: u64, address: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.faults".to_owned(),
            keys: vec![
                KeyValue::task(1),
                KeyValue::Mem {
                    base: address,
                    len: 1,
                },
            ],
            attrs: vec![Attr::uint("count", 1)],
            collector_id: CollectorId::new("faults"),
            seq: 0,
        }
    }

    fn rule() -> Rule {
        Rule {
            id: "pages-of-allocation".to_owned(),
            from: "mem.heap.alloc".to_owned(),
            to: "mem.faults".to_owned(),
            via: KeyKind::Mem,
            match_kind: MatchKind::Interval,
            opens: Some("op == 'alloc'".parse().unwrap()),
            closes: Some("op == 'free'".parse().unwrap()),
            window: None,
            capacity: 1000,
            top: 10,
        }
    }

    #[test]
    fn a_fault_is_attributed_to_the_allocation_that_contains_it() {
        let events = vec![
            allocation(10, 0x1000, 0x1000, "alloc"),
            allocation(11, 0x8000, 0x1000, "alloc"),
            fault(20, 0x1500),
            fault(21, 0x1abc),
            fault(22, 0x8010),
        ];
        let correlation = apply(&rule(), &events).unwrap();
        assert_eq!((correlation.matched, correlation.unmatched), (3, 0));
        assert_eq!(correlation.evidence, Evidence::Key);
        assert_eq!(correlation.top[0].base, 0x1000);
        assert_eq!(correlation.top[0].matched, 2);
    }

    /// The ordering rule the engine sorts for, asserted rather than assumed.
    ///
    /// Two streams merge, and at an equal timestamp the order decides the
    /// answer: an allocation has to be open before a fault inside it can be
    /// attributed. The shim timestamps a `malloc` when it returns and the
    /// kernel timestamps the fault that follows, so equal timestamps are rare
    /// but not impossible -- and when they happen, sorting the wrong way
    /// invents an unmatched fault out of nothing.
    #[test]
    fn an_allocation_wins_a_tie_against_a_fault_inside_it() {
        // The fault is listed first and shares the allocation's timestamp.
        let events = vec![fault(10, 0x1500), allocation(10, 0x1000, 0x1000, "alloc")];
        let correlation = apply(&rule(), &events).unwrap();
        assert_eq!(
            (correlation.matched, correlation.unmatched),
            (1, 0),
            "a fault sharing an allocation's timestamp belongs to it"
        );
    }

    /// The same rule in reverse: a free at the same instant closes the range,
    /// and a fault at that instant is still inside it.
    #[test]
    fn input_order_does_not_change_the_answer() {
        let forwards = vec![
            allocation(10, 0x1000, 0x1000, "alloc"),
            fault(20, 0x1100),
            allocation(30, 0x1000, 1, "free"),
        ];
        let backwards: Vec<_> = forwards.iter().rev().cloned().collect();
        assert_eq!(
            apply(&rule(), &forwards).unwrap(),
            apply(&rule(), &backwards).unwrap(),
            "the engine sorts its input, so the order it arrives in cannot matter"
        );
    }

    #[test]
    fn a_fault_outside_every_allocation_is_counted_not_hidden() {
        let events = vec![
            allocation(10, 0x1000, 0x10, "alloc"),
            // Just past the end of the range, which is exclusive.
            fault(20, 0x1010),
            fault(21, 0x9999),
        ];
        let correlation = apply(&rule(), &events).unwrap();
        assert_eq!((correlation.matched, correlation.unmatched), (0, 2));
        assert!(correlation.top.is_empty());
    }

    #[test]
    fn a_freed_allocation_stops_collecting_faults() {
        let events = vec![
            allocation(10, 0x1000, 0x1000, "alloc"),
            fault(11, 0x1100),
            allocation(12, 0x1000, 1, "free"),
            fault(13, 0x1100),
        ];
        let correlation = apply(&rule(), &events).unwrap();
        assert_eq!((correlation.matched, correlation.unmatched), (1, 1));
        assert_eq!(correlation.states.closed, 1);
        assert_eq!(correlation.top[0].outcome, Outcome::Closed);
    }

    #[test]
    fn a_reused_address_does_not_inherit_the_previous_allocations_faults() {
        let events = vec![
            allocation(10, 0x1000, 0x1000, "alloc"),
            fault(11, 0x1100),
            allocation(12, 0x1000, 1, "free"),
            // The allocator hands the same address back out.
            allocation(13, 0x1000, 0x1000, "alloc"),
            fault(14, 0x1100),
        ];
        let correlation = apply(&rule(), &events).unwrap();
        assert_eq!(correlation.states.opened, 2);
        assert_eq!(
            correlation.top.len(),
            2,
            "each life of the address is its own state"
        );
        assert!(correlation
            .top
            .iter()
            .all(|attribution| attribution.matched == 1));
    }

    #[test]
    fn states_that_outlive_the_window_are_evicted_and_counted() {
        let mut rule = rule();
        rule.window = Some("50ns".parse().unwrap());
        let events = vec![
            allocation(10, 0x1000, 0x1000, "alloc"),
            // Far enough past the window that the state is gone.
            fault(200, 0x1100),
        ];
        let correlation = apply(&rule, &events).unwrap();
        assert_eq!(correlation.states.expired, 1);
        assert_eq!(correlation.unmatched, 1);
        assert!(
            !correlation.states.is_clean(),
            "an eviction must be visible"
        );
    }

    #[test]
    fn a_full_map_evicts_the_oldest_and_says_so() {
        let mut rule = rule();
        rule.capacity = 2;
        let events = vec![
            allocation(10, 0x1000, 0x10, "alloc"),
            allocation(11, 0x2000, 0x10, "alloc"),
            allocation(12, 0x3000, 0x10, "alloc"),
            fault(20, 0x1000),
        ];
        let correlation = apply(&rule, &events).unwrap();
        assert_eq!(correlation.states.overflowed, 1);
        assert_eq!(
            correlation.unmatched, 1,
            "the evicted range can no longer match"
        );
    }

    /// An address on the last byte of a range is inside it; one byte further
    /// is not. Ranges are half-open, and an off-by-one here silently moves
    /// every fault at a boundary to the neighbouring allocation.
    #[test]
    fn a_range_includes_its_first_byte_and_excludes_the_byte_past_its_end() {
        let events = vec![
            allocation(10, 0x1000, 0x100, "alloc"),
            fault(20, 0x1000),
            fault(21, 0x10ff),
            fault(22, 0x1100),
        ];
        let correlation = apply(&rule(), &events).unwrap();
        assert_eq!(
            (correlation.matched, correlation.unmatched),
            (2, 1),
            "the first byte and the last are in; the byte past the end is out"
        );
    }

    /// Two ranges side by side, and a fault in each. The lookup walks back to
    /// the nearest range starting at or before the address, so a neighbouring
    /// range must not swallow the other's faults.
    #[test]
    fn adjacent_ranges_do_not_take_each_others_faults() {
        let events = vec![
            allocation(10, 0x1000, 0x100, "alloc"),
            allocation(11, 0x1100, 0x100, "alloc"),
            fault(20, 0x1080),
            fault(21, 0x1180),
        ];
        let correlation = apply(&rule(), &events).unwrap();
        assert_eq!(correlation.matched, 2);
        assert_eq!(correlation.top.len(), 2);
        assert!(
            correlation
                .top
                .iter()
                .all(|attribution| attribution.matched == 1),
            "each range took exactly its own fault: {:?}",
            correlation.top
        );
    }

    /// A rule whose `to` side carries no usable key is a rule that does not fit
    /// its sources. That is a mistake in the session, not a runtime hiccup, and
    /// it fails by name rather than reporting that nothing correlated.
    #[test]
    fn a_rule_that_does_not_fit_its_sources_fails_by_name() {
        let mut unkeyed = fault(20, 0x1500);
        unkeyed.keys.clear();
        let events = vec![allocation(10, 0x1000, 0x1000, "alloc"), unkeyed];
        let error = apply(&rule(), &events).unwrap_err();
        assert!(
            matches!(error, JoinError::MissingKey { ref via, .. } if *via == KeyKind::Mem),
            "{error}"
        );
        assert!(error.to_string().contains("mem.faults"), "{error}");
    }

    #[test]
    fn an_unimplemented_match_kind_is_refused_with_the_reason() {
        let mut rule = rule();
        rule.match_kind = MatchKind::Wakeup;
        let error = apply(&rule, &[]).unwrap_err();
        assert!(error.to_string().contains("not implemented yet"), "{error}");
    }
}
