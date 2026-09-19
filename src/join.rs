//! The join engine.
//!
//! A join takes two event streams and says which events belong together, and on
//! what grounds. The grounds matter as much as the answer: a correlation backed
//! by a propagated identity and one backed by two things happening at the same
//! time are both "correlations", and only one of them is evidence. Every result
//! here carries the level it was obtained at.
//!
//! Rules are declarative, so a new boundary is a new rule rather than new
//! engine code. Four matches exist and they differ only in what "the same
//! thing" means:
//!
//! - `eq` is an equal key, and it is the cheap one.
//! - `scoped_eq` is an equal key within one life of a recycled identifier. A
//!   block request pointer is freed and handed straight back out, so a
//!   completion that arrives late would otherwise attach to whatever request
//!   now sits at that address. The catalog writes `blk:rq@open_ts` for exactly
//!   this.
//! - `interval` is an address falling inside a range: the match the first real
//!   join needed, attributing page faults to the allocations that own them.
//! - `wakeup` is the edge from whoever made a task runnable to what that task
//!   then did. It is the only join that crosses a block, which is why it is the
//!   only way to say that a request spent 41 ms waiting on somebody else.
//!
//! All four share one lifecycle: a `from` event opens a state, `to` events are
//! attributed to it, and the state ends by being closed, replaced, expired past
//! its window, evicted for space, or still open when the run ends. Every one of
//! those endings is counted, because an open state that is never released is a
//! process that grows until it is killed halfway through a measurement.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::catalog::MatchKind;
use crate::event::{AttrValue, Event, KeyValue};
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
    /// The attribute of a `from` event naming the task that did the waking.
    ///
    /// Required by `wakeup` and meaningless to every other match. It is named
    /// rather than assumed so the engine stays out of the business of knowing
    /// what a scheduler tracepoint calls its fields: `sched.wakeup` writes
    /// `waker_tid`, and a second producer of wakeup edges names its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waker: Option<String>,
    /// How many wakeup edges to follow back from a woken task. One by default:
    /// the task that released this one.
    ///
    /// A wakeup chain is a graph, not a pair -- A wakes B wakes C -- and every
    /// hop is one more chance that the waker was only the task a softirq
    /// interrupted. The chain is resolved once, when the wakeup arrives, which
    /// is the only moment it is true; a chain longer than this is reported
    /// truncated rather than followed, and one that returns to a task it has
    /// already passed stops there and says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
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

impl Rule {
    /// How many wakeup edges to follow. At least one: a chain of no edges is
    /// not a wakeup join, it is a request for nothing.
    fn depth(&self) -> usize {
        self.depth.unwrap_or(1).max(1)
    }
}

/// What a correlation read back from a file without one was: the only match
/// kind that existed when those files were written.
fn default_match_kind() -> MatchKind {
    MatchKind::Interval
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
    #[error("rule `{id}` cannot match by `{match_kind}` on `{via}`: {reason}")]
    UnmatchableKey {
        id: String,
        match_kind: MatchKind,
        via: KeyKind,
        reason: String,
    },
    #[error(
        "rule `{id}` matches by `wakeup` and names no waker attribute: say which attribute of a \
         `{from}` event carries the waking task, as in `waker: waker_tid`"
    )]
    NoWakerAttribute { id: String, from: String },
    #[error(
        "rule `{id}` reads `{attr}` off a `{kind}` event as the task that did the waking, and the \
         event carries no such attribute holding a task id"
    )]
    MissingWaker {
        id: String,
        attr: String,
        kind: String,
    },
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
    /// How the two sides were matched, carried here because it decides what the
    /// numbers below *mean*. A state is a range for `interval`, a key for `eq`,
    /// and an edge between two tasks for `wakeup`; a reader told "3 of 3 events
    /// fell outside every open range" about a wakeup join is being handed the
    /// wrong question's answer.
    #[serde(default = "default_match_kind")]
    pub match_kind: MatchKind,
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
///
/// Every state that opened ends exactly once, so `opened` equals `closed` plus
/// `expired` plus `overflowed` plus `still_open`. A run where it does not is an
/// engine dropping attributions on the floor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateStats {
    pub opened: u64,
    pub closed: u64,
    /// The subset of `closed` that ended because the same identity opened
    /// again: an address the allocator handed back out, a task woken a second
    /// time. Counted apart because a `closes` predicate firing and an identity
    /// being reused are different facts about the workload.
    #[serde(default)]
    pub replaced: u64,
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
    /// What the state was, written the way a reader needs it: the range for an
    /// `interval` match, the key for `eq` and `scoped_eq`, and the chain of who
    /// woke whom for a `wakeup`.
    pub key: String,
    /// The first address of the range, for an `interval` match. Zero for the
    /// other three, whose states are not ranges.
    pub base: u64,
    /// The length of the range in bytes, for an `interval` match. Zero for the
    /// other three, which have no extent.
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

/// What a state is, beyond when it opened.
///
/// The three matches that need more than a key keep it here rather than in
/// three fields that are unset in every case but one.
enum Extent {
    /// An `interval` state: the range an address has to fall inside.
    Range { base: u64, len: u64 },
    /// A `scoped_eq` state: which life of a recycled identity this is.
    Scope(Option<u64>),
    /// A `wakeup` state: the task that made this one runnable.
    Waker(i32),
    /// An `eq` state: the key is the whole of it.
    Key,
}

struct OpenState {
    /// How an attribution names this state.
    label: String,
    extent: Extent,
    opened_ns: u64,
    matched: u64,
}

/// The open states of one rule, and everything that can happen to them.
///
/// Generic over the identity a state is filed under -- an address for
/// `interval`, a rendered key for `eq`, a task for `wakeup` -- because the
/// lifecycle is the same whatever that is, and writing it once is what keeps
/// the four matches from each growing their own way of leaking.
struct States<K: Ord + Clone> {
    open: BTreeMap<K, OpenState>,
    stats: StateStats,
    finished: Vec<Attribution>,
    capacity: usize,
    window_ns: Option<u64>,
}

impl<K: Ord + Clone> States<K> {
    fn new(rule: &Rule) -> Self {
        Self {
            open: BTreeMap::new(),
            stats: StateStats::default(),
            finished: Vec::new(),
            // A capacity of zero would bound nothing: the eviction finds no
            // victim and the map grows anyway.
            capacity: rule.capacity.max(1),
            window_ns: rule.window.map(Duration::as_nanos),
        }
    }

    /// Opens a state, ending whatever held that identity before.
    ///
    /// The same identity opening twice is the previous state ending, not a
    /// second state beside it: an address handed back out by the allocator, a
    /// task woken again. Overwriting it silently would lose every attribution
    /// it had collected while still counting it as opened.
    fn insert(&mut self, key: K, state: OpenState) {
        if let Some(previous) = self.open.remove(&key) {
            self.stats.closed += 1;
            self.stats.replaced += 1;
            let at = state.opened_ns;
            finish(&mut self.finished, previous, Some(at), Outcome::Closed);
        } else if self.open.len() >= self.capacity {
            self.evict_oldest();
        }
        self.open.insert(key, state);
        self.stats.opened += 1;
        self.stats.peak_open = self.stats.peak_open.max(self.open.len() as u64);
    }

    fn close(&mut self, key: &K, closed_ns: u64) {
        if let Some(state) = self.open.remove(key) {
            self.stats.closed += 1;
            finish(&mut self.finished, state, Some(closed_ns), Outcome::Closed);
        }
    }

    /// Drops states that outlived the window, counting them.
    ///
    /// An open state that is never closed and never expires is a leak, and a
    /// leak in a join engine is a process that grows until it is killed halfway
    /// through a measurement.
    fn expire(&mut self, now_ns: u64) {
        let Some(window_ns) = self.window_ns else {
            return;
        };
        let cutoff = now_ns.saturating_sub(window_ns);
        let stale: Vec<K> = self
            .open
            .iter()
            .filter(|(_, state)| state.opened_ns < cutoff)
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            if let Some(state) = self.open.remove(&key) {
                self.stats.expired += 1;
                finish(&mut self.finished, state, None, Outcome::Expired);
            }
        }
    }

    fn evict_oldest(&mut self) {
        let oldest = self
            .open
            .iter()
            .min_by_key(|(_, state)| state.opened_ns)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            if let Some(state) = self.open.remove(&key) {
                self.stats.overflowed += 1;
                finish(&mut self.finished, state, None, Outcome::Overflowed);
            }
        }
    }

    fn finish(mut self) -> (StateStats, Vec<Attribution>) {
        self.stats.still_open = self.open.len() as u64;
        for (_, state) in std::mem::take(&mut self.open) {
            finish(&mut self.finished, state, None, Outcome::StillOpen);
        }
        (self.stats, self.finished)
    }
}

fn finish(
    finished: &mut Vec<Attribution>,
    state: OpenState,
    closed_ns: Option<u64>,
    outcome: Outcome,
) {
    let (base, len) = match state.extent {
        Extent::Range { base, len } => (base, len),
        _ => (0, 0),
    };
    finished.push(Attribution {
        key: state.label,
        base,
        len,
        opened_ns: state.opened_ns,
        closed_ns,
        matched: state.matched,
        outcome,
    });
}

/// Whether a rule can be applied at all, without looking at a single event.
///
/// Worth asking early and separately: every one of these is a rule that does
/// not fit its own match kind, and finding that out after a measurement has run
/// costs the measurement. The engine asks it again for itself, so a caller that
/// forgets cannot produce a wrong answer, only a late one.
pub fn check(rule: &Rule) -> Result<(), JoinError> {
    let unmatchable = |reason: &str| JoinError::UnmatchableKey {
        id: rule.id.clone(),
        match_kind: rule.match_kind,
        via: rule.via,
        reason: reason.to_owned(),
    };

    match rule.match_kind {
        MatchKind::Interval if rule.via != KeyKind::Mem => Err(unmatchable(
            "an interval match needs a range to fall inside, and `mem:[base,len)` is the only key \
             that is one",
        )),
        MatchKind::ScopedEq if !is_scoped(rule.via) => Err(unmatchable(
            "a scoped match needs a key that says which life of an identity it names, written \
             `<id>@<scope>`; `blk:rq@open_ts` is the only one today, and on a key without a scope \
             `scoped_eq` is `eq` under a name that promises more",
        )),
        MatchKind::Wakeup if rule.via != KeyKind::Task => Err(unmatchable(
            "a wakeup is an edge between two tasks, so `task:tid` is the only key it can follow",
        )),
        MatchKind::Wakeup if rule.waker.is_none() => Err(JoinError::NoWakerAttribute {
            id: rule.id.clone(),
            from: rule.from.clone(),
        }),
        _ => Ok(()),
    }
}

/// Applies one rule to a stream of events, in timestamp order.
pub fn apply(rule: &Rule, events: &[Event]) -> Result<Correlation, JoinError> {
    check(rule)?;

    // Two streams merge here, and at an equal timestamp the order decides the
    // answer: an allocation has to be open before a fault inside it can be
    // attributed, and a wakeup before the work it released. Sorting the opening
    // side first at each instant is the only ordering that cannot invent an
    // unmatched event.
    let mut ordered: Vec<&Event> = events.iter().collect();
    ordered.sort_by_key(|event| (event.ts_ns, u8::from(event.kind != rule.from)));

    match rule.match_kind {
        MatchKind::Interval => interval(rule, &ordered),
        MatchKind::Eq | MatchKind::ScopedEq => keyed(rule, &ordered),
        MatchKind::Wakeup => wakeup(rule, &ordered),
    }
}

/// An address inside a range: the match that attributes a page fault to the
/// allocation that owns the page.
fn interval(rule: &Rule, events: &[&Event]) -> Result<Correlation, JoinError> {
    let mut states: States<u64> = States::new(rule);
    let (mut matched, mut unmatched) = (0u64, 0u64);

    for event in events {
        states.expire(event.ts_ns);

        if event.kind == rule.from {
            let Some(KeyValue::Mem { base, len }) = event.key(rule.via).cloned() else {
                return Err(missing_key(rule, event));
            };

            if closes(rule, event) {
                states.close(&base, event.ts_ns);
                continue;
            }
            if opens(rule, event) {
                let key = KeyValue::Mem { base, len };
                states.insert(
                    base,
                    OpenState {
                        label: key.to_string(),
                        extent: Extent::Range { base, len },
                        opened_ns: event.ts_ns,
                        matched: 0,
                    },
                );
            }
            continue;
        }

        if event.kind == rule.to {
            let Some(KeyValue::Mem { base: address, .. }) = event.key(rule.via) else {
                return Err(missing_key(rule, event));
            };
            // The state whose range contains this address: the last one that
            // starts at or before it, if it is long enough to reach it.
            match states
                .open
                .range_mut(..=*address)
                .next_back()
                .filter(|(_, state)| match state.extent {
                    Extent::Range { base, len } => *address < base.saturating_add(len),
                    _ => false,
                }) {
                Some((_, state)) => {
                    state.matched += 1;
                    matched += 1;
                }
                None => unmatched += 1,
            }
        }
    }

    Ok(assemble(rule, matched, unmatched, states))
}

/// Equal keys, optionally within one life of a recycled identity.
///
/// The two differ in one place and it is the place that matters: `eq` files a
/// state under the identity and asks nothing else of a downstream event, while
/// `scoped_eq` also requires the scope to agree. A block completion that
/// arrives after its request was freed and the pointer reissued carries the
/// scope of the request it belongs to, so under `scoped_eq` it does not match
/// and is counted; under `eq` it lands on a stranger.
fn keyed(rule: &Rule, events: &[&Event]) -> Result<Correlation, JoinError> {
    let scoped = rule.match_kind == MatchKind::ScopedEq;
    let mut states: States<String> = States::new(rule);
    let (mut matched, mut unmatched) = (0u64, 0u64);

    for event in events {
        states.expire(event.ts_ns);

        if event.kind == rule.from {
            let Some(key) = event.key(rule.via) else {
                return Err(missing_key(rule, event));
            };
            let id = identity(key);

            if closes(rule, event) {
                states.close(&id, event.ts_ns);
                continue;
            }
            if opens(rule, event) {
                let label = if scoped { key.to_string() } else { id.clone() };
                states.insert(
                    id,
                    OpenState {
                        label,
                        extent: if scoped {
                            Extent::Scope(scope(key))
                        } else {
                            Extent::Key
                        },
                        opened_ns: event.ts_ns,
                        matched: 0,
                    },
                );
            }
            continue;
        }

        if event.kind == rule.to {
            let Some(key) = event.key(rule.via) else {
                return Err(missing_key(rule, event));
            };
            let fits = |state: &OpenState| match (&state.extent, scoped) {
                (Extent::Scope(open_ts), true) => *open_ts == scope(key),
                _ => true,
            };
            match states.open.get_mut(&identity(key)).filter(|s| fits(s)) {
                Some(state) => {
                    state.matched += 1;
                    matched += 1;
                }
                None => unmatched += 1,
            }
        }
    }

    Ok(assemble(rule, matched, unmatched, states))
}

/// Who released this task, and what it did once released.
///
/// A `from` event says a task was made runnable and by whom; every `to` event
/// of that task until it is woken again is work that would not have happened
/// yet without the waker. That is the only correlation in this engine that
/// crosses a block, and it is also the weakest: softirqs run in the context of
/// whatever task they interrupted, so a waker can be a task that had nothing to
/// do with it, and a waker of zero is the idle task rather than a task at all.
/// The evidence level says so, and the chain names what it found.
fn wakeup(rule: &Rule, events: &[&Event]) -> Result<Correlation, JoinError> {
    let Some(attr) = rule.waker.as_deref() else {
        return Err(JoinError::NoWakerAttribute {
            id: rule.id.clone(),
            from: rule.from.clone(),
        });
    };
    let depth = rule.depth();
    let mut states: States<i32> = States::new(rule);
    let (mut matched, mut unmatched) = (0u64, 0u64);

    for event in events {
        states.expire(event.ts_ns);

        if event.kind == rule.from {
            let Some(KeyValue::Task { tid: wakee }) = event.key(rule.via).cloned() else {
                return Err(missing_key(rule, event));
            };

            if closes(rule, event) {
                states.close(&wakee, event.ts_ns);
                continue;
            }
            if opens(rule, event) {
                let waker = waker_of(rule, event, attr)?;
                // Resolved now rather than when the work arrives: the chain is
                // true at the instant of the wakeup, and a hop taken later
                // would read whoever woke the waker since.
                let chain = Chain::resolve(&states.open, wakee, waker, depth);
                states.insert(
                    wakee,
                    OpenState {
                        label: chain.render(),
                        extent: Extent::Waker(waker),
                        opened_ns: event.ts_ns,
                        matched: 0,
                    },
                );
            }
            continue;
        }

        if event.kind == rule.to {
            let Some(KeyValue::Task { tid }) = event.key(rule.via) else {
                return Err(missing_key(rule, event));
            };
            match states.open.get_mut(tid) {
                Some(state) => {
                    state.matched += 1;
                    matched += 1;
                }
                None => unmatched += 1,
            }
        }
    }

    Ok(assemble(rule, matched, unmatched, states))
}

/// The idle task. A wakeup credited to it is an interrupt arriving on a cpu
/// that had nothing to run, not one task releasing another.
const IDLE: i32 = 0;

/// A path back through wakeups, from the task that was woken to whoever is
/// furthest up the chain the engine could still see.
struct Chain {
    /// Woken task first, each waker after it.
    hops: Vec<i32>,
    /// There was another waker and the requested depth ran out before it.
    truncated: bool,
    /// The chain came back to a task it had already passed.
    cycle: bool,
    /// The task woke itself, so nothing released it.
    self_woken: bool,
}

impl Chain {
    fn resolve(open: &BTreeMap<i32, OpenState>, wakee: i32, waker: i32, depth: usize) -> Self {
        let mut chain = Chain {
            hops: vec![wakee],
            truncated: false,
            cycle: false,
            self_woken: waker == wakee,
        };
        if chain.self_woken {
            return chain;
        }

        let mut seen: BTreeSet<i32> = BTreeSet::new();
        seen.insert(wakee);
        let mut next = Some(waker);
        while let Some(task) = next {
            if chain.hops.len() > depth {
                chain.truncated = true;
                break;
            }
            if !seen.insert(task) {
                chain.cycle = true;
                break;
            }
            chain.hops.push(task);
            if task == IDLE {
                break;
            }
            next = match open.get(&task).map(|state| &state.extent) {
                Some(Extent::Waker(waker)) => Some(*waker),
                _ => None,
            };
        }
        chain
    }

    /// Oldest waker first, ending at the task that was woken, which is the
    /// direction the causality runs.
    fn render(&self) -> String {
        let mut text = self
            .hops
            .iter()
            .rev()
            .map(|tid| task_name(*tid))
            .collect::<Vec<_>>()
            .join(" -> ");
        if self.truncated {
            text = format!("... -> {text}");
        }
        if self.self_woken {
            text.push_str(" (self)");
        } else if self.cycle {
            text.push_str(" (cycle)");
        }
        text
    }
}

/// A waker of zero is the idle task, and saying so is the difference between a
/// reader who believes a process released this one and one who knows a timer
/// did.
fn task_name(tid: i32) -> String {
    if tid == IDLE {
        "task:0 (idle)".to_owned()
    } else {
        format!("task:{tid}")
    }
}

fn assemble<K: Ord + Clone>(
    rule: &Rule,
    matched: u64,
    unmatched: u64,
    states: States<K>,
) -> Correlation {
    let (stats, mut finished) = states.finish();
    finished.retain(|attribution| attribution.matched > 0);
    finished.sort_by(|left, right| {
        right
            .matched
            .cmp(&left.matched)
            .then(left.base.cmp(&right.base))
            .then_with(|| left.key.cmp(&right.key))
    });
    let total_attributed = finished.iter().map(|attribution| attribution.matched).sum();
    finished.truncate(rule.top);

    Correlation {
        rule: rule.id.clone(),
        from: rule.from.clone(),
        to: rule.to.clone(),
        match_kind: rule.match_kind,
        evidence: evidence(rule),
        matched,
        unmatched,
        states: stats,
        top: finished,
        total_attributed,
    }
}

/// What a match of this kind, on a key of this kind, is worth.
///
/// A wakeup is a chain and says so. Everything else here is a shared typed key,
/// except a propagated request identity, which is the one case where the
/// downstream event carries something the upstream one put there rather than
/// something both happen to know.
fn evidence(rule: &Rule) -> Evidence {
    match rule.match_kind {
        MatchKind::Wakeup => Evidence::Wakeup,
        _ if rule.via == KeyKind::Req => Evidence::Identity,
        _ => Evidence::Key,
    }
}

/// The part of a key that names the thing, without the scope saying which life
/// of it this is.
fn identity(key: &KeyValue) -> String {
    match key {
        KeyValue::Blk { ptr, .. } => format!("blk:{ptr:#x}"),
        other => other.to_string(),
    }
}

/// Which life of an identity a key names, for the identities that are recycled.
fn scope(key: &KeyValue) -> Option<u64> {
    match key {
        KeyValue::Blk { open_ts, .. } => Some(*open_ts),
        _ => None,
    }
}

/// Whether a key kind carries a scope at all, which is what `scoped_eq` needs
/// and what a rule can be checked for before any event arrives.
fn is_scoped(kind: KeyKind) -> bool {
    matches!(kind, KeyKind::Blk)
}

fn waker_of(rule: &Rule, event: &Event, attr: &str) -> Result<i32, JoinError> {
    let waker = event.attr(attr).and_then(|value| match value {
        AttrValue::Uint(tid) => i32::try_from(*tid).ok(),
        AttrValue::Int(tid) => i32::try_from(*tid).ok(),
        AttrValue::Float(_) | AttrValue::Text(_) => None,
    });
    waker.ok_or_else(|| JoinError::MissingWaker {
        id: rule.id.clone(),
        attr: attr.to_owned(),
        kind: event.kind.clone(),
    })
}

/// A rule whose side carries no usable key is a rule that does not fit its
/// sources: a mistake in the session, not a runtime hiccup.
fn missing_key(rule: &Rule, event: &Event) -> JoinError {
    JoinError::MissingKey {
        id: rule.id.clone(),
        via: rule.via,
        kind: event.kind.clone(),
    }
}

fn opens(rule: &Rule, event: &Event) -> bool {
    rule.opens
        .as_ref()
        .is_none_or(|predicate| crate::filter::matches(predicate, event))
}

fn closes(rule: &Rule, event: &Event) -> bool {
    rule.closes
        .as_ref()
        .is_some_and(|predicate| crate::filter::matches(predicate, event))
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
            waker: None,
            depth: None,
            window: None,
            capacity: 1000,
            top: 10,
        }
    }

    /// Every state that opened has to end exactly once, whichever way it ends.
    /// A state that is dropped instead of ended takes its attributions with it,
    /// and the run still reports it as opened -- so the totals disagree with
    /// the heaviest ranges and nothing says why.
    fn states_balance(correlation: &Correlation) {
        let states = &correlation.states;
        assert_eq!(
            states.opened,
            states.closed + states.expired + states.overflowed + states.still_open,
            "a state opened and never ended: {states:?}"
        );
        assert!(
            states.replaced <= states.closed,
            "replacements are a subset of the states that closed: {states:?}"
        );
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
        states_balance(&correlation);
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
        states_balance(&correlation);
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

    /// The same address opening twice with nothing between: a shim that missed
    /// the `free`, a kernel pointer reissued, a task woken again. The state
    /// that was there ends; it does not evaporate along with everything that
    /// was attributed to it.
    #[test]
    fn an_identity_that_opens_again_ends_the_state_it_replaces() {
        let mut rule = rule();
        // No `closes`, so nothing but the second allocation ends the first.
        rule.closes = None;
        let events = vec![
            allocation(10, 0x1000, 0x1000, "alloc"),
            fault(11, 0x1100),
            fault(12, 0x1200),
            allocation(13, 0x1000, 0x1000, "alloc"),
            fault(14, 0x1100),
        ];
        let correlation = apply(&rule, &events).unwrap();
        assert_eq!(correlation.states.replaced, 1);
        assert_eq!(
            correlation.total_attributed, correlation.matched,
            "every matched event has to appear under some state: {correlation:?}"
        );
        assert_eq!(
            correlation.top.len(),
            2,
            "the replaced state keeps its two faults: {:?}",
            correlation.top
        );
        states_balance(&correlation);
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
        states_balance(&correlation);
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
        states_balance(&correlation);
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

    /// An interval match needs something to be inside, and only one key is a
    /// range. Refusing by name beats reporting that a rule matched nothing.
    #[test]
    fn an_interval_match_on_a_key_that_is_not_a_range_is_refused_with_the_reason() {
        let mut rule = rule();
        rule.via = KeyKind::Task;
        let error = check(&rule).unwrap_err();
        assert!(error.to_string().contains("mem:[base,len)"), "{error}");
        assert_eq!(
            apply(&rule, &[]).unwrap_err(),
            error,
            "the engine re-checks"
        );
    }

    // -- eq ----------------------------------------------------------------

    fn threads(ts_ns: u64, tid: i32) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "proc.threads".to_owned(),
            keys: vec![KeyValue::task(tid)],
            attrs: vec![Attr::uint("threads", 1)],
            collector_id: CollectorId::new("threads"),
            seq: 0,
        }
    }

    fn descriptors(ts_ns: u64, tid: i32) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "proc.fd".to_owned(),
            keys: vec![KeyValue::task(tid)],
            attrs: vec![Attr::uint("open", 3)],
            collector_id: CollectorId::new("fd"),
            seq: 0,
        }
    }

    fn eq_rule() -> Rule {
        Rule {
            id: "handles-of-a-thread".to_owned(),
            from: "proc.threads".to_owned(),
            to: "proc.fd".to_owned(),
            via: KeyKind::Task,
            match_kind: MatchKind::Eq,
            opens: None,
            closes: None,
            waker: None,
            depth: None,
            window: None,
            capacity: 1000,
            top: 10,
        }
    }

    /// Two tasks measured at once, and the whole point of matching on a key:
    /// each event goes to the state of its own task. An engine that attributed
    /// to whichever state is open would pass every single-task test.
    #[test]
    fn an_equal_key_attributes_each_event_to_the_state_of_its_own_task() {
        let events = vec![
            threads(10, 7),
            threads(11, 9),
            descriptors(20, 7),
            descriptors(21, 9),
            descriptors(22, 7),
            // A task no state was opened for.
            descriptors(23, 11),
        ];
        let correlation = apply(&eq_rule(), &events).unwrap();
        assert_eq!((correlation.matched, correlation.unmatched), (3, 1));
        assert_eq!(correlation.evidence, Evidence::Key);
        assert_eq!(correlation.top.len(), 2);
        assert_eq!(correlation.top[0].key, "task:7");
        assert_eq!(correlation.top[0].matched, 2);
        assert_eq!(correlation.top[1].key, "task:9");
        assert_eq!(correlation.top[1].matched, 1);
        states_balance(&correlation);
    }

    /// A key the upstream event propagated and the downstream one read back is
    /// stronger evidence than a key both sides happen to know, and the result
    /// says which it had.
    #[test]
    fn a_propagated_request_identity_is_reported_as_identity_not_as_a_shared_key() {
        let request = |ts_ns: u64, kind: &str, id: &str| Event {
            ts_ns,
            ts_err_ns: 0,
            kind: kind.to_owned(),
            keys: vec![KeyValue::Req { id: id.to_owned() }],
            attrs: vec![Attr::uint("count", 1)],
            collector_id: CollectorId::new("http"),
            seq: 0,
        };
        let mut rule = eq_rule();
        rule.via = KeyKind::Req;
        rule.from = "net.tcp.lifetime".to_owned();
        rule.to = "io.latency".to_owned();

        let events = vec![
            request(10, "net.tcp.lifetime", "abc"),
            request(20, "io.latency", "abc"),
            request(21, "io.latency", "def"),
        ];
        let correlation = apply(&rule, &events).unwrap();
        assert_eq!(correlation.evidence, Evidence::Identity);
        assert_eq!((correlation.matched, correlation.unmatched), (1, 1));
    }

    // -- scoped_eq ---------------------------------------------------------

    fn block(ts_ns: u64, kind: &str, ptr: u64, open_ts: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: kind.to_owned(),
            keys: vec![KeyValue::Blk { ptr, open_ts }],
            attrs: vec![Attr::uint("bytes", 4096)],
            collector_id: CollectorId::new("blk"),
            seq: 0,
        }
    }

    fn scoped_rule() -> Rule {
        Rule {
            id: "latency-of-a-request".to_owned(),
            from: "io.rq.issue".to_owned(),
            to: "io.rq.complete".to_owned(),
            via: KeyKind::Blk,
            match_kind: MatchKind::ScopedEq,
            opens: None,
            closes: None,
            waker: None,
            depth: None,
            window: None,
            capacity: 1000,
            top: 10,
        }
    }

    /// The reason the catalog writes `blk:rq@open_ts` and not `blk:rq`.
    ///
    /// A block request is freed on completion and the kernel hands the same
    /// address straight back to the next one, so a completion that arrives
    /// after its request was reissued carries the scope of the request it
    /// belongs to and matches nothing. Under a bare equality it would land on a
    /// stranger and be reported as a latency, which is the failure this match
    /// exists to prevent -- so the same stream is run through both, and the two
    /// answers have to differ.
    #[test]
    fn a_recycled_pointer_does_not_take_the_completion_of_the_request_before_it() {
        let events = vec![
            block(10, "io.rq.issue", 0xffff_0000, 10),
            // The address is handed back out for a different request.
            block(20, "io.rq.issue", 0xffff_0000, 20),
            // The first request's completion, arriving late.
            block(30, "io.rq.complete", 0xffff_0000, 10),
        ];

        let scoped = apply(&scoped_rule(), &events).unwrap();
        assert_eq!(
            (scoped.matched, scoped.unmatched),
            (0, 1),
            "a completion from another life of the pointer is not this request's"
        );
        assert_eq!(scoped.states.replaced, 1);
        states_balance(&scoped);

        let mut bare = scoped_rule();
        bare.match_kind = MatchKind::Eq;
        let bare = apply(&bare, &events).unwrap();
        assert_eq!(
            (bare.matched, bare.unmatched),
            (1, 0),
            "a bare equality is what mis-attributes it, which is why scoped_eq exists"
        );
    }

    #[test]
    fn a_completion_within_the_life_of_its_request_matches() {
        let events = vec![
            block(10, "io.rq.issue", 0xffff_0000, 10),
            block(15, "io.rq.complete", 0xffff_0000, 10),
        ];
        let correlation = apply(&scoped_rule(), &events).unwrap();
        assert_eq!((correlation.matched, correlation.unmatched), (1, 0));
        assert_eq!(
            correlation.top[0].key, "blk:0xffff0000@10",
            "the attribution names the life, not only the address"
        );
    }

    /// A scope that is not in the key cannot be matched on, and a `scoped_eq`
    /// that silently behaved as `eq` would promise protection it does not give.
    #[test]
    fn a_scoped_match_on_a_key_with_no_scope_is_refused_with_the_reason() {
        let mut rule = scoped_rule();
        rule.via = KeyKind::Task;
        let error = check(&rule).unwrap_err();
        assert!(error.to_string().contains("blk:rq@open_ts"), "{error}");
        assert!(error.to_string().contains("under a name"), "{error}");
    }

    // -- wakeup ------------------------------------------------------------

    fn woken(ts_ns: u64, wakee: i32, waker: i32) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "sched.wakeup".to_owned(),
            keys: vec![KeyValue::task(wakee)],
            attrs: vec![
                Attr::uint("count", 1),
                Attr::uint("wakee_tid", wakee as u64),
                Attr::uint("waker_tid", waker as u64),
            ],
            collector_id: CollectorId::new("wakeup"),
            seq: 0,
        }
    }

    fn switched(ts_ns: u64, tid: i32) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "sched.switches".to_owned(),
            keys: vec![KeyValue::task(tid)],
            attrs: vec![Attr::uint("count", 1)],
            collector_id: CollectorId::new("switches"),
            seq: 0,
        }
    }

    fn wakeup_rule() -> Rule {
        Rule {
            id: "who-released-this".to_owned(),
            from: "sched.wakeup".to_owned(),
            to: "sched.switches".to_owned(),
            via: KeyKind::Task,
            match_kind: MatchKind::Wakeup,
            opens: None,
            closes: None,
            waker: Some("waker_tid".to_owned()),
            depth: None,
            window: None,
            capacity: 1000,
            top: 10,
        }
    }

    /// The join that crosses a block: work a task did after being woken belongs
    /// to whoever woke it, and work by a task nobody woke belongs to nobody.
    #[test]
    fn work_after_a_wakeup_is_attributed_to_the_task_that_did_the_waking() {
        let events = vec![
            woken(10, 100, 99),
            switched(20, 100),
            switched(21, 100),
            // A task that took no part in the wakeup.
            switched(22, 555),
        ];
        let correlation = apply(&wakeup_rule(), &events).unwrap();
        assert_eq!((correlation.matched, correlation.unmatched), (2, 1));
        assert_eq!(correlation.evidence, Evidence::Wakeup);
        assert_eq!(correlation.top.len(), 1);
        assert_eq!(correlation.top[0].key, "task:99 -> task:100");
        states_balance(&correlation);
    }

    /// A chain is a graph, not a pair. One edge is the default because every
    /// further hop is another chance that the waker was only the task a softirq
    /// interrupted -- so a deeper chain is asked for, and a chain that goes
    /// further than was asked says so rather than presenting its first hop as
    /// the origin.
    #[test]
    fn a_chain_is_followed_to_the_depth_asked_for_and_says_when_it_stops_short() {
        let events = vec![woken(10, 50, 40), woken(11, 60, 50), switched(20, 60)];

        let shallow = apply(&wakeup_rule(), &events).unwrap();
        assert_eq!(
            shallow.top[0].key, "... -> task:50 -> task:60",
            "one hop, and the chain continues past it"
        );

        let mut deeper = wakeup_rule();
        deeper.depth = Some(2);
        let deeper = apply(&deeper, &events).unwrap();
        assert_eq!(
            deeper.top[0].key, "task:40 -> task:50 -> task:60",
            "two hops reach the task nobody woke, so nothing is left to truncate"
        );
    }

    /// Two tasks that wake each other are a loop the walk must not follow
    /// twice. Left to the data, it would walk until the depth ran out and
    /// report a chain that repeats.
    #[test]
    fn a_chain_that_returns_to_a_task_it_has_passed_stops_there_and_says_so() {
        let mut rule = wakeup_rule();
        rule.depth = Some(8);
        let events = vec![woken(10, 5, 6), woken(11, 6, 5), switched(20, 6)];
        let correlation = apply(&rule, &events).unwrap();
        assert_eq!(correlation.top[0].key, "task:5 -> task:6 (cycle)");
    }

    /// A task that woke itself was released by nobody, and reporting it as
    /// woken by itself reads as an attribution.
    #[test]
    fn a_task_that_woke_itself_is_named_as_having_no_releaser() {
        let events = vec![woken(10, 7, 7), switched(20, 7)];
        let correlation = apply(&wakeup_rule(), &events).unwrap();
        assert_eq!(correlation.top[0].key, "task:7 (self)");
    }

    /// A waker of zero is the idle task: a timer or an interrupt arriving on a
    /// cpu with nothing to run. It is the most common waker there is and the
    /// least informative, and a reader has to be able to tell it from a pid.
    #[test]
    fn a_waker_of_zero_is_named_as_the_idle_task() {
        let events = vec![woken(10, 8, 0), switched(20, 8)];
        let correlation = apply(&wakeup_rule(), &events).unwrap();
        assert_eq!(correlation.top[0].key, "task:0 (idle) -> task:8");
    }

    /// A task woken again is a new episode: what it does afterwards belongs to
    /// the waker that released it this time, and what it did before still
    /// belongs to the one before.
    #[test]
    fn a_second_wakeup_ends_the_first_and_what_follows_belongs_to_the_new_waker() {
        let events = vec![
            woken(10, 100, 99),
            switched(11, 100),
            woken(12, 100, 77),
            switched(13, 100),
        ];
        let correlation = apply(&wakeup_rule(), &events).unwrap();
        assert_eq!(correlation.states.replaced, 1);
        assert_eq!(correlation.matched, 2);
        let keys: Vec<&str> = correlation
            .top
            .iter()
            .map(|attribution| attribution.key.as_str())
            .collect();
        assert!(keys.contains(&"task:99 -> task:100"), "{keys:?}");
        assert!(keys.contains(&"task:77 -> task:100"), "{keys:?}");
        states_balance(&correlation);
    }

    /// The engine cannot invent the attribute that carries the waker, and a
    /// rule that does not name one would otherwise correlate nothing and look
    /// like a workload where nobody woke anybody.
    #[test]
    fn a_wakeup_rule_that_names_no_waker_attribute_is_refused_with_the_reason() {
        let mut rule = wakeup_rule();
        rule.waker = None;
        let error = check(&rule).unwrap_err();
        assert!(error.to_string().contains("waker: waker_tid"), "{error}");
        assert_eq!(
            apply(&rule, &[]).unwrap_err(),
            error,
            "the engine re-checks"
        );
    }

    #[test]
    fn a_wakeup_rule_on_a_key_that_is_not_a_task_is_refused_with_the_reason() {
        let mut rule = wakeup_rule();
        rule.via = KeyKind::Sock;
        let error = check(&rule).unwrap_err();
        assert!(
            error.to_string().contains("edge between two tasks"),
            "{error}"
        );
    }

    /// The rule names an attribute and the events do not carry it: a rule that
    /// does not fit its sources, like a missing key, and reported the same way.
    #[test]
    fn a_wakeup_whose_events_do_not_carry_the_named_attribute_fails_by_name() {
        let mut rule = wakeup_rule();
        rule.waker = Some("woken_by".to_owned());
        let error = apply(&rule, &[woken(10, 100, 99)]).unwrap_err();
        assert!(
            matches!(error, JoinError::MissingWaker { ref attr, .. } if attr == "woken_by"),
            "{error}"
        );
        assert!(error.to_string().contains("sched.wakeup"), "{error}");
    }

    /// A wakeup episode that nothing ends holds a state for the whole run, and
    /// a machine-wide wakeup trace opens one per task on the machine. The
    /// window is what bounds it, and what it drops is counted.
    #[test]
    fn a_wakeup_episode_outlives_its_window_and_the_work_after_it_is_unattributed() {
        let mut rule = wakeup_rule();
        rule.window = Some("50ns".parse().unwrap());
        let events = vec![woken(10, 100, 99), switched(200, 100)];
        let correlation = apply(&rule, &events).unwrap();
        assert_eq!(correlation.states.expired, 1);
        assert_eq!((correlation.matched, correlation.unmatched), (0, 1));
        states_balance(&correlation);
    }
}
