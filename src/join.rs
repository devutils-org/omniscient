//! The join engine.
//!
//! A join takes two event streams and says which events belong together, and on
//! what grounds. The grounds matter as much as the answer: a correlation backed
//! by a propagated identity and one backed by two things happening at the same
//! time are both "correlations", and only one of them is evidence. Every result
//! here carries the level it was obtained at.
//!
//! Rules are declarative, so a new boundary is a new rule rather than new
//! engine code. Seven matches exist and they differ only in what "the same
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
//! - `woke` is the same edge read the other way: a close -- a completion --
//!   paired with the next wakeup of the task it names. `wakeup` answers "what
//!   did this release"; `woke` answers "what released the task this completion
//!   was for, and how long after it".
//! - `between` is two things moving in the same interval: a polled total's
//!   change between two readings, credited to the states that opened or closed
//!   in between. Nothing ties the two sides but the clock, and it is one of the
//!   two matches that say `TEMPORAL` -- the level that exists so a reader can
//!   tell a coincidence in time from a key, and that nothing produced until the
//!   seventh wave's crossings asked for it three times over.
//! - `overlap` is the other: a downstream event's window lying across the
//!   intervals of an upstream poll, credited to each by the share of it that
//!   overlaps. A block of 250 ms across three 100 ms readings is split three
//!   ways rather than credited to the reading it ended in.
//!
//! All seven share one lifecycle: a `from` event opens a state, `to` events are
//! attributed to it, and the state ends by being closed, replaced, expired past
//! its window, evicted for space, or still open when the run ends. Every one of
//! those endings is counted, because an open state that is never released is a
//! process that grows until it is killed halfway through a measurement.
//!
//! The second half of this module is the decomposition: a whole, the parts it
//! is made of, and a signed residue that is never rounded away. It is not a
//! join -- nothing is matched -- but it is what the joins were for, and it
//! carries the same evidence levels beside each row.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::catalog::{Catalog, MatchKind, Unit};
use crate::collect::Watched;
use crate::emit::{Results, Shortfall, TOTAL_OF_ATTR, WEIGHT_ATTR};
use crate::event::{AttrValue, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{KeyKind, Mode, ModeKind, Request, Target};
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
    /// Every level, weakest first.
    pub const LADDER: [Evidence; 4] = [
        Evidence::Temporal,
        Evidence::Wakeup,
        Evidence::Key,
        Evidence::Identity,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Evidence::Temporal => "TEMPORAL",
            Evidence::Wakeup => "WAKEUP",
            Evidence::Key => "KEY",
            Evidence::Identity => "IDENTITY",
        }
    }

    /// What a level establishes about the two sides it ties.
    ///
    /// Said here, beside the level, so that every surface that shows one --
    /// the text renderer, `omn-mcp` -- says the same thing, and a level added
    /// to the ladder does not compile until someone has said what it means.
    pub fn establishes(self) -> &'static str {
        match self {
            Evidence::Temporal => {
                "the two sides overlapped in time, and nothing else ties them: a hypothesis, \
                 nothing more"
            }
            Evidence::Wakeup => {
                "a wakeup chain links them: the one kind of tie that crosses a block"
            }
            Evidence::Key => {
                "both sides carry the same typed key (a task, a device, a request pointer)"
            }
            Evidence::Identity => {
                "the downstream event carries an identity the upstream one propagated and it \
                 read back"
            }
        }
    }

    /// What a level does not establish, which is the half a summary drops.
    pub fn does_not_establish(self) -> &'static str {
        match self {
            Evidence::Temporal => {
                "that one side caused, served or contains the other: anything else that ran in \
                 the same interval fits the same way"
            }
            Evidence::Wakeup => {
                "what the waker was doing on behalf of the woken task beyond releasing it"
            }
            Evidence::Key => {
                "that the two events belong to the same unit of work: a key is shared by every \
                 event of that task or device, and a recycled key by events of different units"
            }
            Evidence::Identity => {
                "anything outside the propagated identity: the row is exact about whose it is, \
                 not about why it took what it took"
            }
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
    /// The attribute of a `to` event naming the task a `woke` match pairs it
    /// with: `wakee_tid` on `sched.wakeup`. The event's own `via` key when
    /// absent, which is the same task for `sched.wakeup` -- it is keyed on the
    /// woken task -- and named when a producer keys on the other end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub woken: Option<String>,
    /// The attribute of a `to` event holding how long it lasted, for an
    /// `overlap` match: the event covers `[ts - span, ts]`. `total_ns` on
    /// `sched.offcpu`. Absent, each `to` event is an instant and is credited
    /// whole to the interval it falls in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<String>,
    /// Which end of its `span` a `to` event is stamped at, for `overlap`: the
    /// end, when absent, which is when most durations become known; `start`
    /// for a source that stamps an event when the thing it times began, as
    /// the drained `sched.futex` and `sched.offcpu` rows say they are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamped: Option<Stamped>,
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
    /// The attribute of a `to` reading whose change a `between` match credits:
    /// `bytes` on `mem.rss`, `ns` on `cpu.oncpu_time`. For `overlap`, the
    /// amount of a `to` event that is shared out by overlap.
    ///
    /// Required by `between` and `overlap` and meaningless to the rest, named for
    /// the same reason `waker` is: a reading carries several numbers, and
    /// which of them is the total is the source's business, not the engine's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// What one reading of `value` is: a level, or what happened since the
    /// reading before. Required when the `to` side is polled, because a poll
    /// can be either and nothing in the reading says which.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading: Option<Reading>,
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

/// Which end of its duration an event's timestamp is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stamped {
    /// The event is stamped when the thing it times ended: `[ts - span, ts]`.
    End,
    /// The event is stamped when the thing it times began: `[ts, ts + span]`.
    Start,
}

/// What one reading of a polled number is.
///
/// The same poll can be either, and the difference decides every number
/// derived from it: `mem.rss` is a level, and what happened between two
/// readings is their difference; `cpu.oncpu_time` is the tick counter's delta
/// since the poll before, and what happened is the reading itself. Taking one
/// for the other is not a rounding error -- the difference of two deltas is a
/// quantity of nothing -- and the catalog does not say which a source is, so a
/// rule or a decomposition over a poll has to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reading {
    /// A total at an instant: a resident size, a count of descriptors.
    Level,
    /// What happened since the reading before: a tick counter's delta, a total
    /// drained from the kernel.
    Delta,
}

impl Reading {
    pub fn as_str(self) -> &'static str {
        match self {
            Reading::Level => "level",
            Reading::Delta => "delta",
        }
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
    #[error(
        "rule `{id}` matches by `between` and names no value: say which attribute of a `{to}` \
         reading is the total whose change is credited, as in `value: bytes`"
    )]
    NoValueAttribute { id: String, to: String },
    #[error(
        "rule `{id}` matches by `overlap` and names no value: say which attribute of a `{to}` \
         event is the amount shared out by overlap, as in `value: total_ns`"
    )]
    NoShareValue { id: String, to: String },
    #[error(
        "rule `{id}` matches by `overlap` from `{from}`, which this session collects in \
         `{mode}` mode: `overlap` reads each `from` event as a reading that covers the interval \
         since the one before, and a `{mode}` event covers no interval. Collect `{from}` with \
         `poll`, or match by a key"
    )]
    NotAnInterval {
        id: String,
        from: String,
        mode: ModeKind,
    },
    #[error(
        "rule `{id}` reads `{attr}` off a `{kind}` event as how long it lasted, and the event \
         carries no such number"
    )]
    MissingSpan {
        id: String,
        attr: String,
        kind: String,
    },
    #[error(
        "rule `{id}` reads `{attr}` off a `{kind}` event as the task it woke, and the event \
         carries no such attribute holding a task id"
    )]
    MissingWoken {
        id: String,
        attr: String,
        kind: String,
    },
    #[error(
        "rule `{id}` matches by `between` onto `{to}`, which this session collects in `{mode}` \
         mode: `between` credits the change between two readings of a total, and a `{mode}` \
         event has no reading before it to differ from. Collect `{to}` with `poll` or \
         `snapshot`, or join it by a key"
    )]
    NotAReading {
        id: String,
        to: String,
        mode: ModeKind,
    },
    #[error(
        "rule `{id}` matches by `between` onto a polled `{to}`, and a polled number is either a \
         level or the change since the poll before; nothing in the reading says which. Say \
         `reading: level` for a total at an instant, such as a resident size, or `reading: \
         delta` for a counter's change since the last poll"
    )]
    ReadingUnstated { id: String, to: String },
    #[error(
        "rule `{id}` reads `{attr}` off a `{kind}` reading as the total it credits, and the \
         reading carries no such number"
    )]
    MissingValue {
        id: String,
        attr: String,
        kind: String,
    },
    #[error(
        "rule `{id}` matches by `{match_kind}` onto `{to}`, whose events are totals drained \
         from the kernel: each stands for many occurrences, and a wakeup edge ties one \
         occurrence to one other. Every block in a drain would be credited to whichever wakeup \
         came last before it, or every wakeup in one to a single completion, and reported as \
         WAKEUP. Join onto a source with one event per occurrence, such as `sched.switches` or \
         `sched.wakeup`"
    )]
    DrainedTotal {
        id: String,
        match_kind: MatchKind,
        to: String,
    },
    #[error(
        "rule `{id}` joins from `{from}`, which this session collects twice with different \
         windows ({windows}): a state opened by one request and an event outside the other's \
         window cannot be told apart, and taking the union would claim the source watched \
         when one of them was detached. Give both requests the same window, or join from one"
    )]
    WindowsDisagree {
        id: String,
        from: String,
        windows: String,
    },
    #[error("decomposition `{id}` {reason}")]
    Decomposition { id: String, reason: String },
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
    /// How a downstream event was credited to one state among several that
    /// held the same key in turn, when that is weaker than `evidence`.
    ///
    /// An `eq` on `task` from an allocation to a resident size is right to say
    /// KEY: the reading and the allocation belong to one process. But every
    /// allocator call replaces the state before it, and which of them a
    /// reading lands on is whichever came last -- a choice the clock made, not
    /// the key. Absent when no state was replaced, because then the
    /// association and the crediting are the same claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_key: Option<Evidence>,
    /// Downstream events that landed inside an open state.
    pub matched: u64,
    /// Downstream events that landed in no open state. Not an error: an address
    /// outside every tracked allocation is a real thing, and hiding it would
    /// make the attributed share look like the whole.
    pub unmatched: u64,
    /// Downstream events that carried no value for the key at all, and so were
    /// outside every state by construction: a block request issued outside
    /// any request, a switch of a task no rule tracks. Counted apart from
    /// `unmatched`, which is an event that had the key and found no state.
    /// Refusing the whole rule at the first one, which is what this used to
    /// do, made every real session refuse, because every real session has
    /// them.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub keyless: u64,
    /// Downstream events stamped outside the `from` source's window: before it
    /// was attached or after it was detached. Neither matched against a state
    /// nor counted as unmatched -- the run was not watching for what would have
    /// opened or closed one, and a wakeup the run did not see is not a wakeup
    /// that did not happen.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub outside_window: u64,
    pub states: StateStats,
    /// The heaviest attributions, most matched first.
    pub top: Vec<Attribution>,
    /// Every attribution, totalled.
    pub total_attributed: u64,
    /// What a `between` match did with the readings, which its `matched` and
    /// `unmatched` alone cannot say: how much of the change was credited, and
    /// how much moved with nothing opening or closing around it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readings: Option<Readings>,
}

/// The readings of a `between` match, accounted for.
///
/// Every `to` reading is exactly one of three things: the first of its key,
/// with nothing to differ from; a change in an interval where some state
/// opened or closed, which is `matched`; or a change in an interval where none
/// did. The last is not an error -- a resident size moves for reasons no
/// allocation tracker sees -- and hiding it would make the credited share look
/// like the whole.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Readings {
    /// The attribute whose change was credited.
    pub value: String,
    pub reading: Reading,
    /// Readings that were the first of their key. Counted in `unmatched`.
    pub baselines: u64,
    /// The summed change of every matched reading, signed.
    pub credited: f64,
    /// The summed change of every reading whose interval held no state
    /// change, signed. Counted in `unmatched`.
    pub uncredited: f64,
    /// State changes that happened before the first reading of their key, and
    /// so fell inside no interval a change could be taken over.
    pub before_first: u64,
}

/// The lifecycle of open states, which is where a join engine leaks if it does.
///
/// Every state that opened ends exactly once, so `opened` equals `closed` plus
/// `expired` plus `overflowed` plus `window_closed` plus `still_open`. A run
/// where it does not is an engine dropping attributions on the floor.
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
    /// The subset of `replaced` where the state that ended and the one that
    /// replaced it had the same scope as well as the same identity: two lives
    /// of one recycled pointer the key cannot tell apart.
    ///
    /// A block request's scope is `request.start_time_ns`, which the kernel
    /// reads from a clock it caches for the length of the submitter's plug,
    /// so every request one plug section creates carries the same stamp, and
    /// a pointer freed and reissued inside one section keeps its whole key.
    /// Nothing else the two collectors both read changes per request (the
    /// tag is assigned at issue and cleared at completion, the internal tag is
    /// unset under the default scheduler, the sector repeats on a rewrite).
    /// What is left is order, and order is sound: a pointer is reissued only
    /// after its completion, so the earlier life's completion arrives while
    /// the earlier life is the one open. It stops being sound when a side lost
    /// the event that would have opened the later life, and so a match with a
    /// collapse behind it says `within_key: TEMPORAL`, with this count.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub scope_collapsed: u64,
    /// Evicted because they outlived the window.
    pub expired: u64,
    /// Evicted because the map was full. A non-zero count means attributions
    /// are missing, not that nothing matched.
    pub overflowed: u64,
    /// Ended because the `from` source was detached while they were open.
    ///
    /// Counted apart from `expired`, which is a state outliving the rule's own
    /// window: this is the run no longer watching for whatever would have
    /// ended or replaced the state, and keeping it open would credit every
    /// later event to a state the run could not see end -- the stale wakeup
    /// that took 63 blocks after its source came off.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub window_closed: u64,
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
    /// Downstream events that fell in this range. For `between`, the states
    /// that opened or closed in the interval; for a `wakeup` chain, the
    /// events of every wakeup folded into it.
    pub matched: u64,
    /// How the state left the engine. For a `wakeup` chain, how the latest of
    /// its wakeups did.
    pub outcome: Outcome,
    /// For `between`: the reading's change over this interval, and what
    /// opened and closed while it happened. For `overlap`, what of the `to`
    /// side was credited to this interval; for `woke`, how long the closes
    /// folded here waited for their wakeup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<Change>,
    /// For `wakeup`: how many wakeups released the task along this chain.
    ///
    /// A wakeup opens one state, so a chain that released a task six hundred
    /// times is six hundred states of one event each; ranked one by one, the
    /// top of the table is whichever wakeups happened to precede a
    /// preemption. The chain is what a reader ranks, so the states are folded
    /// into it and this says how many.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wakeups: Option<u64>,
}

/// One interval of a `between` or `overlap` match, or one waker's pairs of a
/// `woke` match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    /// The `to` total's change over the interval, signed. For `overlap`, the
    /// share of the `to` events' `value` credited to this interval. For
    /// `woke`, the time from each close to the wakeup that followed it,
    /// summed, in nanoseconds.
    pub delta: f64,
    /// States that opened in the interval; the closes folded, for `woke`;
    /// zero for `overlap`, whose interval is a reading and opens nothing.
    pub opened: u64,
    /// States that closed in the interval; the closes a wakeup followed, for
    /// `woke`; zero for `overlap`.
    pub closed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Closed,
    Expired,
    Overflowed,
    /// Open when the `from` source was detached.
    WindowClosed,
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
    /// A `between` state: a state change waiting for the next reading of its
    /// key, and which way it went.
    Change { opening: bool },
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

    /// Ends every open state at the instant the `from` source was detached.
    fn window_closed(&mut self, closed_ns: u64) {
        self.stats.window_closed += self.open.len() as u64;
        for (_, state) in std::mem::take(&mut self.open) {
            finish(
                &mut self.finished,
                state,
                Some(closed_ns),
                Outcome::WindowClosed,
            );
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

impl States<(String, u64)> {
    /// Closes every state filed under one key, handing them back rather than
    /// finishing them: `between` credits them together, as one interval.
    fn take_key(&mut self, key: &str) -> Vec<OpenState> {
        let from = (key.to_owned(), 0);
        let through = (key.to_owned(), u64::MAX);
        let ids: Vec<(String, u64)> = self
            .open
            .range(from..=through)
            .map(|(id, _)| id.clone())
            .collect();
        let taken: Vec<OpenState> = ids.iter().filter_map(|id| self.open.remove(id)).collect();
        self.stats.closed += taken.len() as u64;
        taken
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
        change: None,
        wakeups: None,
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
        MatchKind::Between if rule.value.is_none() => Err(JoinError::NoValueAttribute {
            id: rule.id.clone(),
            to: rule.to.clone(),
        }),
        MatchKind::Woke if rule.via != KeyKind::Task => Err(unmatchable(
            "`woke` pairs a close with the next wakeup of the task it names, so `task:tid` is \
             the only key it can follow",
        )),
        MatchKind::Overlap if rule.value.is_none() => Err(JoinError::NoShareValue {
            id: rule.id.clone(),
            to: rule.to.clone(),
        }),
        _ => Ok(()),
    }
}

/// Whether a rule can be applied to the sources a session actually collects,
/// the way it collects them.
///
/// [`check`] asks what a rule means on its own. Two refusals need more than
/// the rule: whether the `to` side is a reading at all, which is the request's
/// mode, and whether one of its events stands for many occurrences, which is
/// the catalog's. Both are known before anything is attached, and finding
/// either out afterwards costs the measurement -- or, worse, does not: a
/// wakeup join onto a drained total runs, matches, and reports WAKEUP.
pub fn check_in(rule: &Rule, requests: &[Request], catalog: &Catalog) -> Result<(), JoinError> {
    check(rule)?;
    // [`apply_within`] reads one window per source. Two requests of the
    // `from` source watching different spans are the one case that cannot
    // say, and it is known from the session alone.
    let windows: BTreeSet<String> = requests
        .iter()
        .filter(|request| request.source.as_str() == rule.from)
        .map(|request| {
            request
                .window
                .map_or_else(|| "the whole run".to_owned(), |window| window.to_string())
        })
        .collect();
    if windows.len() > 1 {
        return Err(JoinError::WindowsDisagree {
            id: rule.id.clone(),
            from: rule.from.clone(),
            windows: windows.into_iter().collect::<Vec<_>>().join(", "),
        });
    }
    let onto = requests
        .iter()
        .filter(|request| request.source.as_str() == rule.to);

    match rule.match_kind {
        MatchKind::Between => {
            for request in onto {
                match request.mode.kind() {
                    ModeKind::Poll if rule.reading.is_none() => {
                        return Err(JoinError::ReadingUnstated {
                            id: rule.id.clone(),
                            to: rule.to.clone(),
                        })
                    }
                    ModeKind::Poll | ModeKind::Snapshot => {}
                    mode => {
                        return Err(JoinError::NotAReading {
                            id: rule.id.clone(),
                            to: rule.to.clone(),
                            mode,
                        })
                    }
                }
            }
            Ok(())
        }
        MatchKind::Overlap => {
            let from = requests
                .iter()
                .filter(|request| request.source.as_str() == rule.from);
            for request in from {
                match request.mode.kind() {
                    ModeKind::Poll | ModeKind::Snapshot => {}
                    mode => {
                        return Err(JoinError::NotAnInterval {
                            id: rule.id.clone(),
                            from: rule.from.clone(),
                            mode,
                        })
                    }
                }
            }
            Ok(())
        }
        MatchKind::Wakeup | MatchKind::Woke => {
            let drained = rule
                .to
                .parse()
                .ok()
                .and_then(|id| catalog.get(&id))
                .is_some_and(|entry| entry.always_drained());
            if drained {
                return Err(JoinError::DrainedTotal {
                    id: rule.id.clone(),
                    match_kind: rule.match_kind,
                    to: rule.to.clone(),
                });
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Applies one rule to a stream of events, in timestamp order, every source
/// having watched for the whole run.
pub fn apply(rule: &Rule, events: &[Event]) -> Result<Correlation, JoinError> {
    apply_within(rule, events, &BTreeMap::new())
}

/// `apply`, knowing when each source was watching. `windows` maps an event
/// kind (a source id) to the span its collection reports; a kind absent from
/// it was watched for the whole run.
///
/// Only the `from` source's window changes the answer. A `to` event outside it
/// is counted in `outside_window` and not looked up: whatever would have
/// opened or closed a state for it happened, or did not, while the run was not
/// looking. And a state still open when the `from` window closes ends there,
/// counted in `window_closed`, because every event after it would otherwise be
/// credited to the last state the run saw open -- a wakeup from before the
/// detach standing in for every wakeup after it.
pub fn apply_within(
    rule: &Rule,
    events: &[Event],
    windows: &BTreeMap<String, Watched>,
) -> Result<Correlation, JoinError> {
    check(rule)?;
    let mut watch = Watch::of(rule, windows);

    // Two streams merge here, and at an equal timestamp the order decides the
    // answer: an allocation has to be open before a fault inside it can be
    // attributed, and a wakeup before the work it released. Sorting the opening
    // side first at each instant is the only ordering that cannot invent an
    // unmatched event.
    let mut ordered: Vec<&Event> = events.iter().collect();
    ordered.sort_by_key(|event| (event.ts_ns, u8::from(event.kind != rule.from)));

    let mut correlation = match rule.match_kind {
        MatchKind::Interval => interval(rule, &ordered, &mut watch),
        MatchKind::Eq | MatchKind::ScopedEq => keyed(rule, &ordered, &mut watch),
        MatchKind::Wakeup => wakeup(rule, &ordered, &mut watch),
        MatchKind::Between => between(rule, &ordered, &mut watch),
        MatchKind::Woke => woke(rule, &ordered, &mut watch).map(|(correlation, _)| correlation),
        MatchKind::Overlap => overlap(rule, &ordered, &mut watch),
    }?;
    correlation.outside_window = watch.outside;
    Ok(correlation)
}

/// When the `from` source was watching, as one match walks the stream.
struct Watch {
    /// Absent when it watched the whole run, which is every join without a
    /// window and the only case `apply` knows.
    from: Option<Watched>,
    /// The latest any source of the run was still watching. A `from` window
    /// that closed before it closed before the run ended, and the states it
    /// left open end at its close even when no event came after it.
    run_closed_ns: u64,
    /// The states open at the `from` window's close have been ended.
    cut: bool,
    outside: u64,
}

impl Watch {
    fn of(rule: &Rule, windows: &BTreeMap<String, Watched>) -> Self {
        Self {
            from: windows.get(&rule.from).copied(),
            run_closed_ns: windows
                .values()
                .map(|watched| watched.closed_ns)
                .max()
                .unwrap_or(0),
            cut: false,
            outside: 0,
        }
    }

    /// Ends the states still open once the stream has passed the `from`
    /// window's close. The rule's own expiry is applied up to the close
    /// first, so a state that outlived the rule's window before the detach
    /// is counted as expired and not as cut.
    fn advance<K: Ord + Clone>(&mut self, ts_ns: u64, states: &mut States<K>) {
        match self.from {
            Some(from) if !self.cut && ts_ns > from.closed_ns => {
                states.expire(from.closed_ns);
                states.window_closed(from.closed_ns);
                self.cut = true;
            }
            _ => {}
        }
    }

    /// Whether a `to` event fell outside the `from` window, counting it.
    fn outside(&mut self, ts_ns: u64) -> bool {
        let outside = self.from.is_some_and(|from| !from.contains(ts_ns));
        self.outside += u64::from(outside);
        outside
    }

    /// The end of the stream: a `from` window that closed before the run did
    /// ends its states at its close, whether or not anything came after it.
    fn finish<K: Ord + Clone>(&mut self, states: &mut States<K>) {
        if let Some(from) = self.from {
            if from.closed_ns < self.run_closed_ns {
                self.advance(u64::MAX, states);
            }
        }
    }
}

/// An address inside a range: the match that attributes a page fault to the
/// allocation that owns the page.
fn interval(rule: &Rule, events: &[&Event], watch: &mut Watch) -> Result<Correlation, JoinError> {
    let mut keyless = 0u64;
    let mut states: States<u64> = States::new(rule);
    let (mut matched, mut unmatched) = (0u64, 0u64);

    for event in events {
        watch.advance(event.ts_ns, &mut states);
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
            if watch.outside(event.ts_ns) {
                continue;
            }
            let Some(KeyValue::Mem { base: address, .. }) = event.key(rule.via) else {
                // A downstream event without the key is outside every state by
                // construction -- a block request issued outside any request --
                // and is counted apart rather than refusing the whole rule.
                keyless += 1;
                continue;
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

    watch.finish(&mut states);
    Ok(with_keyless(
        assemble(rule, matched, unmatched, states),
        keyless,
    ))
}

/// Equal keys, optionally within one life of a recycled identity.
///
/// The two differ in one place and it is the place that matters: `eq` files a
/// state under the identity and asks nothing else of a downstream event, while
/// `scoped_eq` also requires the scope to agree. A block completion that
/// arrives after its request was freed and the pointer reissued carries the
/// scope of the request it belongs to, so under `scoped_eq` it does not match
/// and is counted; under `eq` it lands on a stranger.
fn keyed(rule: &Rule, events: &[&Event], watch: &mut Watch) -> Result<Correlation, JoinError> {
    let mut keyless = 0u64;
    let scoped = rule.match_kind == MatchKind::ScopedEq;
    let mut states: States<String> = States::new(rule);
    let (mut matched, mut unmatched) = (0u64, 0u64);

    for event in events {
        watch.advance(event.ts_ns, &mut states);
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
                let collapsed = scoped
                    && states.open.get(&id).is_some_and(
                        |open| matches!(open.extent, Extent::Scope(held) if held == scope(key)),
                    );
                states.stats.scope_collapsed += u64::from(collapsed);
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
            if watch.outside(event.ts_ns) {
                continue;
            }
            let Some(key) = event.key(rule.via) else {
                // A downstream event without the key is outside every state by
                // construction -- a block request issued outside any request --
                // and is counted apart rather than refusing the whole rule.
                keyless += 1;
                continue;
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

    watch.finish(&mut states);
    Ok(with_keyless(
        assemble(rule, matched, unmatched, states),
        keyless,
    ))
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
fn wakeup(rule: &Rule, events: &[&Event], watch: &mut Watch) -> Result<Correlation, JoinError> {
    let mut keyless = 0u64;
    let Some(attr) = rule.waker.as_deref() else {
        return Err(JoinError::NoWakerAttribute {
            id: rule.id.clone(),
            from: rule.from.clone(),
        });
    };
    let depth = rule.depth();
    let mut states: States<i32> = States::new(rule);
    let (mut matched, mut unmatched) = (0u64, 0u64);
    // Which interrupt last woke each task, for the tasks an interrupt woke: a
    // waker of zero is an interrupt when its event says so, and the idle task
    // only otherwise. Kept beside the states because a chain that reaches a
    // task through its own waker needs the same answer for that hop.
    let mut interrupted: BTreeMap<i32, String> = BTreeMap::new();

    for event in events {
        watch.advance(event.ts_ns, &mut states);
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
                match event.attr("waker_context") {
                    Some(AttrValue::Text(context)) if waker == IDLE && context != "task" => {
                        interrupted.insert(wakee, context.clone());
                    }
                    _ => {
                        interrupted.remove(&wakee);
                    }
                }
                // Resolved now rather than when the work arrives: the chain is
                // true at the instant of the wakeup, and a hop taken later
                // would read whoever woke the waker since.
                let chain = Chain::resolve(&states.open, &interrupted, wakee, waker, depth);
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
            if watch.outside(event.ts_ns) {
                continue;
            }
            // The catalog's `drained` refuses this before the run for the
            // sources it names; this is the same refusal for a source whose
            // entry does not say so, found on the first event that shows it.
            if stands_for_many(event) {
                return Err(JoinError::DrainedTotal {
                    id: rule.id.clone(),
                    match_kind: rule.match_kind,
                    to: rule.to.clone(),
                });
            }
            let Some(KeyValue::Task { tid }) = event.key(rule.via) else {
                // A downstream event without the key is outside every state by
                // construction -- a block request issued outside any request --
                // and is counted apart rather than refusing the whole rule.
                keyless += 1;
                continue;
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

    watch.finish(&mut states);
    Ok(with_keyless(
        assemble(rule, matched, unmatched, states),
        keyless,
    ))
}

/// Two things moving in the same interval: a polled total's change, credited
/// to the states that opened or closed while it happened.
///
/// Each `to` reading's change since the previous reading with the same `via`
/// key is credited to the `from` states that opened or closed in that
/// interval. Always [`Evidence::Temporal`]: a resident size that rose by 32 MiB
/// in the interval four 8 MiB ranges opened in is consistent with them, and is
/// not a measurement of what any one of them cost. [`check_in`] refuses it when
/// `to` is not polled or read once, since a traced event has no previous
/// reading to differ from.
///
/// A state here is one `from` event waiting for the next reading of its key:
/// it opens when the event arrives and closes when a reading takes it into an
/// interval, so the lifecycle balances exactly as the other four do. The
/// attributions are the intervals rather than the states, because the
/// interval is the finest thing the clock can tell apart -- splitting a change
/// between four states that opened in it would be inventing a precision the
/// evidence does not have.
fn between(rule: &Rule, events: &[&Event], watch: &mut Watch) -> Result<Correlation, JoinError> {
    let mut keyless = 0u64;
    let Some(attr) = rule.value.as_deref() else {
        return Err(JoinError::NoValueAttribute {
            id: rule.id.clone(),
            to: rule.to.clone(),
        });
    };
    let reading = rule.reading.unwrap_or(Reading::Level);
    // Filed under the key and the order of arrival, so every state of one key
    // is one contiguous range of the map.
    let mut states: States<(String, u64)> = States::new(rule);
    let mut arrived = 0u64;
    let mut last: BTreeMap<String, (u64, f64)> = BTreeMap::new();
    let mut intervals: Vec<Attribution> = Vec::new();
    let (mut matched, mut unmatched) = (0u64, 0u64);
    let mut readings = Readings {
        value: attr.to_owned(),
        reading,
        baselines: 0,
        credited: 0.0,
        uncredited: 0.0,
        before_first: 0,
    };

    for event in events {
        watch.advance(event.ts_ns, &mut states);
        states.expire(event.ts_ns);

        if event.kind == rule.from {
            let Some(key) = event.key(rule.via) else {
                return Err(missing_key(rule, event));
            };
            let closing = closes(rule, event);
            if closing || opens(rule, event) {
                arrived += 1;
                states.insert(
                    (key.to_string(), arrived),
                    OpenState {
                        label: key.to_string(),
                        extent: Extent::Change { opening: !closing },
                        opened_ns: event.ts_ns,
                        matched: 0,
                    },
                );
            }
            continue;
        }

        if event.kind == rule.to {
            if watch.outside(event.ts_ns) {
                continue;
            }
            let Some(key) = event.key(rule.via) else {
                // A downstream event without the key is outside every state by
                // construction -- a block request issued outside any request --
                // and is counted apart rather than refusing the whole rule.
                keyless += 1;
                continue;
            };
            let key = key.to_string();
            let value = event
                .attr(attr)
                .and_then(AttrValue::as_f64)
                .ok_or_else(|| JoinError::MissingValue {
                    id: rule.id.clone(),
                    attr: attr.to_owned(),
                    kind: event.kind.clone(),
                })?;

            let pending = states.take_key(&key);
            let (opened, closed) =
                pending
                    .iter()
                    .fold((0, 0), |(opened, closed), state| match state.extent {
                        Extent::Change { opening: true } => (opened + 1, closed),
                        _ => (opened, closed + 1),
                    });
            let previous = last.insert(key.clone(), (event.ts_ns, value));
            let (start_ns, delta) = match (reading, previous) {
                (Reading::Level, None) => {
                    readings.baselines += 1;
                    readings.before_first += pending.len() as u64;
                    unmatched += 1;
                    continue;
                }
                (Reading::Level, Some((at, before))) => (at, value - before),
                (Reading::Delta, Some((at, _))) => (at, value),
                // A delta needs nothing before it; its interval is its own,
                // and the earliest change it takes in is as far back as the
                // engine can say it reaches.
                (Reading::Delta, None) => (
                    pending
                        .iter()
                        .map(|state| state.opened_ns)
                        .min()
                        .unwrap_or(event.ts_ns),
                    value,
                ),
            };
            if pending.is_empty() {
                unmatched += 1;
                readings.uncredited += delta;
                continue;
            }
            matched += 1;
            readings.credited += delta;
            intervals.push(Attribution {
                key,
                base: 0,
                len: 0,
                opened_ns: start_ns,
                closed_ns: Some(event.ts_ns),
                matched: pending.len() as u64,
                outcome: Outcome::Closed,
                change: Some(Change {
                    delta,
                    opened,
                    closed,
                }),
                wakeups: None,
            });
        }
    }

    watch.finish(&mut states);
    let mut correlation = assemble_with(rule, matched, unmatched, states, intervals);
    correlation.keyless = keyless;
    correlation.readings = Some(readings);
    Ok(correlation)
}

/// One pair a `woke` match made: the event that opened the state, and how
/// long after it the wakeup that ended it came.
type Pair<'a> = (&'a Event, u64);

/// The pairs a [`PAIRED`] part reads, as events of the part's own kind, with
/// the join that made them -- or why they could not be made.
type Paired<'r> = Result<(&'r Rule, Vec<Event>), String>;

/// A close paired with the next wakeup of the task it names.
///
/// A `from` event -- a block completion, which names the task that submitted
/// the request -- opens a state for that task, and the first `to` wakeup of
/// the same task after it ends the state and is its match. The pair is the
/// boundary a reader blocked on I/O crosses last: the device finished, and
/// this is how long it was before the task waiting on it was made runnable,
/// and by whom.
///
/// The other way round from [`wakeup`], which opens on the wakeup and credits
/// what the woken task did next. Against `io.latency` that attributes each
/// request to the wakeup that released its submitter *before* it was issued,
/// which for reads one at a time is the previous read's delivery -- a true
/// statement about the wrong pair.
///
/// So `matched` and `unmatched` count the `from` side here, which is the
/// question: a completion that some wakeup of its task followed, and one that
/// none did -- replaced by the task's next completion, expired past the
/// rule's window, or still open at the end. The second is a finding rather
/// than a loss: a reader that never went to sleep for its I/O was served from
/// a cache, or spun. Wakeups of a task with no completion waiting are the
/// rest of the scheduler's traffic, and not part of the question.
///
/// When the rule names a `waker`, each pair is labelled with who issued the
/// wakeup, an interrupt named as the interrupt, and the pairs are folded by
/// that label: which worker delivers the completions is what a reader ranks.
fn woke<'a>(
    rule: &Rule,
    events: &[&'a Event],
    watch: &mut Watch,
) -> Result<(Correlation, Vec<Pair<'a>>), JoinError> {
    let mut keyless = 0u64;
    let mut states: States<i32> = States::new(rule);
    // The event that opened each open state, for the pairs a decomposition
    // reads: its keys are what the pair is credited to.
    let mut opener: BTreeMap<i32, &'a Event> = BTreeMap::new();
    let mut pairs: Vec<Pair<'a>> = Vec::new();
    let mut matched = 0u64;

    for event in events {
        watch.advance(event.ts_ns, &mut states);
        states.expire(event.ts_ns);

        if event.kind == rule.from {
            let Some(KeyValue::Task { tid }) = event.key(rule.via).cloned() else {
                return Err(missing_key(rule, event));
            };
            if closes(rule, event) {
                states.close(&tid, event.ts_ns);
                opener.remove(&tid);
                continue;
            }
            if opens(rule, event) {
                states.insert(
                    tid,
                    OpenState {
                        label: task_name(tid),
                        extent: Extent::Key,
                        opened_ns: event.ts_ns,
                        matched: 0,
                    },
                );
                opener.insert(tid, event);
            }
            continue;
        }

        if event.kind == rule.to {
            if watch.outside(event.ts_ns) {
                continue;
            }
            if stands_for_many(event) {
                return Err(JoinError::DrainedTotal {
                    id: rule.id.clone(),
                    match_kind: rule.match_kind,
                    to: rule.to.clone(),
                });
            }
            let woken = match rule.woken.as_deref() {
                Some(attr) => {
                    Some(
                        task_attr(event, attr).ok_or_else(|| JoinError::MissingWoken {
                            id: rule.id.clone(),
                            attr: attr.to_owned(),
                            kind: event.kind.clone(),
                        })?,
                    )
                }
                None => match event.key(rule.via) {
                    Some(KeyValue::Task { tid }) => Some(*tid),
                    _ => None,
                },
            };
            let Some(tid) = woken else {
                keyless += 1;
                continue;
            };
            let Some(mut state) = states.open.remove(&tid) else {
                continue;
            };
            if let Some(attr) = rule.waker.as_deref() {
                let waker = waker_of(rule, event, attr)?;
                let by = match event.attr("waker_context") {
                    Some(AttrValue::Text(context)) if waker == IDLE && context != "task" => {
                        format!("interrupt ({context})")
                    }
                    _ => task_name(waker),
                };
                state.label = format!("{by} -> {}", task_name(tid));
            }
            state.matched = 1;
            matched += 1;
            states.stats.closed += 1;
            if let Some(from) = opener.remove(&tid) {
                pairs.push((from, event.ts_ns.saturating_sub(state.opened_ns)));
            }
            finish(
                &mut states.finished,
                state,
                Some(event.ts_ns),
                Outcome::Closed,
            );
        }
    }

    watch.finish(&mut states);
    let unmatched = states.stats.opened - matched;
    let mut correlation = assemble(rule, matched, unmatched, states);
    correlation.keyless = keyless;
    Ok((correlation, pairs))
}

/// Whether a drained row says it stands for more than one occurrence: the
/// count, weight or blocks the draining collectors write beside a total.
fn several(event: &Event) -> bool {
    ["count", WEIGHT_ATTR, "blocks"].iter().any(|attr| {
        event
            .attr(attr)
            .and_then(AttrValue::as_f64)
            .is_some_and(|many| many > 1.0)
    })
}

/// A task id read off an attribute, as `waker_of` reads one.
fn task_attr(event: &Event, attr: &str) -> Option<i32> {
    event.attr(attr).and_then(|value| match value {
        AttrValue::Uint(tid) => i32::try_from(*tid).ok(),
        AttrValue::Int(tid) => i32::try_from(*tid).ok(),
        AttrValue::Float(_) | AttrValue::Text(_) => None,
    })
}

/// One interval of an `overlap` match: a reading of the `from` side and what
/// it covers.
struct Covered {
    start_ns: u64,
    end_ns: u64,
    credited: f64,
    events: u64,
}

/// A downstream event's window across the intervals of an upstream poll,
/// credited to each by the share of it that overlaps.
///
/// Each `from` reading covers the interval since the reading before it of the
/// same `via` key -- or `[ts - interval_ns, ts]` when the reading says so
/// itself -- and the first reading of a key with neither is a baseline. Each
/// `to` event covers `[ts - span, ts]`, or is an instant when the rule names no
/// `span`. A `to` event is attributed to every interval its window overlaps,
/// with `value` times the share of its window that overlap is: a block of
/// 250 ms across three readings 100 ms apart is 50, 100 and 100 ms to the
/// three, not 250 ms to the one it ended in. What of a window lies outside
/// every interval -- before the first reading, after the last -- is counted
/// as uncredited rather than folded into its neighbour.
///
/// Always [`Evidence::Temporal`]. The share is exact about the clock and
/// assumes nothing is spread evenly only where it is: an off-cpu interval is
/// blocked from end to end, so splitting it by time is a measurement, where
/// splitting a count of instructions by time is an assumption the evidence
/// level has to carry.
///
/// Read after the fact rather than as the stream arrives, because an interval
/// ends at the reading that closes it, and a window that began inside it has
/// usually ended before that reading comes.
fn overlap(rule: &Rule, events: &[&Event], watch: &mut Watch) -> Result<Correlation, JoinError> {
    let Some(attr) = rule.value.as_deref() else {
        return Err(JoinError::NoShareValue {
            id: rule.id.clone(),
            to: rule.to.clone(),
        });
    };
    let mut readings = Readings {
        value: attr.to_owned(),
        reading: Reading::Delta,
        baselines: 0,
        credited: 0.0,
        uncredited: 0.0,
        before_first: 0,
    };

    // The intervals each key's readings cover, in the order they ended.
    let mut last: BTreeMap<String, u64> = BTreeMap::new();
    let mut intervals: BTreeMap<String, Vec<Covered>> = BTreeMap::new();
    for event in events.iter().filter(|event| event.kind == rule.from) {
        let Some(key) = event.key(rule.via) else {
            return Err(missing_key(rule, event));
        };
        if !opens(rule, event) {
            continue;
        }
        let key = key.to_string();
        let stated = event
            .attr("interval_ns")
            .and_then(AttrValue::as_f64)
            .map(|interval| event.ts_ns.saturating_sub(interval as u64));
        let previous = last.insert(key.clone(), event.ts_ns);
        let Some(start_ns) = stated.or(previous) else {
            readings.baselines += 1;
            continue;
        };
        if start_ns >= event.ts_ns {
            continue;
        }
        intervals.entry(key).or_default().push(Covered {
            start_ns,
            end_ns: event.ts_ns,
            credited: 0.0,
            events: 0,
        });
    }
    let longest: BTreeMap<String, u64> = intervals
        .iter()
        .map(|(key, list)| {
            let longest = list
                .iter()
                .map(|covered| covered.end_ns - covered.start_ns)
                .max()
                .unwrap_or(0);
            (key.clone(), longest)
        })
        .collect();

    let (mut matched, mut unmatched, mut keyless) = (0u64, 0u64, 0u64);
    for event in events.iter().filter(|event| event.kind == rule.to) {
        if watch.outside(event.ts_ns) {
            continue;
        }
        let Some(key) = event.key(rule.via) else {
            keyless += 1;
            continue;
        };
        let key = key.to_string();
        let value = event
            .attr(attr)
            .and_then(AttrValue::as_f64)
            .ok_or_else(|| JoinError::MissingValue {
                id: rule.id.clone(),
                attr: attr.to_owned(),
                kind: event.kind.clone(),
            })?;
        let span = match rule.span.as_deref() {
            Some(span) => event
                .attr(span)
                .and_then(AttrValue::as_f64)
                .ok_or_else(|| JoinError::MissingSpan {
                    id: rule.id.clone(),
                    attr: span.to_owned(),
                    kind: event.kind.clone(),
                })?
                .max(0.0) as u64,
            None => 0,
        };
        // A row drained from the kernel that stands for several occurrences
        // has no one window to place: its duration is a sum of waits that
        // may lie anywhere in the drain's interval. It is counted as
        // unmatched and its amount uncredited, rather than spread over an
        // interval it may not have occupied.
        if several(event) {
            unmatched += 1;
            readings.uncredited += value;
            continue;
        }
        let (start_ns, end_ns) = match rule.stamped.unwrap_or(Stamped::End) {
            Stamped::End => (event.ts_ns.saturating_sub(span), event.ts_ns),
            Stamped::Start => (event.ts_ns, event.ts_ns.saturating_add(span)),
        };

        let mut share = 0.0;
        if let (Some(list), Some(&longest)) = (intervals.get_mut(&key), longest.get(&key)) {
            // Intervals are in the order they ended; the first that can reach
            // this window ends after its start, and none that ends later than
            // one interval past its end can begin inside it.
            let first = list.partition_point(|covered| covered.end_ns < start_ns);
            for covered in &mut list[first..] {
                if covered.end_ns.saturating_sub(longest) > end_ns {
                    break;
                }
                let part = if span == 0 {
                    // An instant belongs to the interval that ends at or after
                    // it and starts before it: `(start, end]`, the reading
                    // that closed over it.
                    f64::from(u8::from(
                        covered.start_ns < end_ns && end_ns <= covered.end_ns,
                    ))
                } else {
                    let lo = start_ns.max(covered.start_ns);
                    let hi = end_ns.min(covered.end_ns);
                    hi.saturating_sub(lo) as f64 / span as f64
                };
                if part > 0.0 {
                    covered.credited += value * part;
                    covered.events += 1;
                    share += part;
                }
            }
        }
        if share > 0.0 {
            matched += 1;
        } else {
            unmatched += 1;
        }
        readings.credited += value * share;
        readings.uncredited += value * (1.0 - share).max(0.0);
    }

    let mut states: States<u8> = States::new(rule);
    let mut attributed = Vec::new();
    for (key, list) in intervals {
        states.stats.peak_open += 1;
        for covered in list {
            states.stats.opened += 1;
            states.stats.closed += 1;
            attributed.push(Attribution {
                key: key.clone(),
                base: 0,
                len: 0,
                opened_ns: covered.start_ns,
                closed_ns: Some(covered.end_ns),
                matched: covered.events,
                outcome: Outcome::Closed,
                change: Some(Change {
                    delta: covered.credited,
                    opened: 0,
                    closed: 0,
                }),
                wakeups: None,
            });
        }
    }
    let mut correlation = assemble_with(rule, matched, unmatched, states, attributed);
    correlation.keyless = keyless;
    correlation.readings = Some(readings);
    Ok(correlation)
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
    /// The interrupt the chain starts at, when its oldest waker is zero and
    /// the wakeup said it came from an interrupt rather than the idle task.
    interrupt: Option<String>,
}

impl Chain {
    fn resolve(
        open: &BTreeMap<i32, OpenState>,
        interrupted: &BTreeMap<i32, String>,
        wakee: i32,
        waker: i32,
        depth: usize,
    ) -> Self {
        let mut chain = Chain {
            hops: vec![wakee],
            truncated: false,
            cycle: false,
            self_woken: waker == wakee,
            interrupt: None,
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
            if task == IDLE {
                // The task this hop reached from -- the one before it -- was
                // woken by zero, and its wakeup says whether that was an
                // interrupt.
                let woken = *chain
                    .hops
                    .last()
                    .expect("the wakee is always the first hop");
                chain.interrupt = interrupted.get(&woken).cloned();
                chain.hops.push(task);
                break;
            }
            chain.hops.push(task);
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
            .map(|tid| match (&self.interrupt, *tid) {
                (Some(context), IDLE) => format!("interrupt ({context})"),
                _ => task_name(*tid),
            })
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
    assemble_with(rule, matched, unmatched, states, Vec::new())
}

/// Assembles a correlation from the states and any attributions a match
/// produced outside them, which `between`'s intervals are.
fn assemble_with<K: Ord + Clone>(
    rule: &Rule,
    matched: u64,
    unmatched: u64,
    states: States<K>,
    extra: Vec<Attribution>,
) -> Correlation {
    let (stats, mut finished) = states.finish();
    finished.extend(extra);
    match rule.match_kind {
        MatchKind::Wakeup => finished = by_chain(finished),
        MatchKind::Woke => finished = by_waker(finished),
        _ => {}
    }
    finished.retain(|attribution| attribution.matched > 0);
    if matches!(rule.match_kind, MatchKind::Between | MatchKind::Overlap) {
        // The heaviest interval is the one the total moved most in, either
        // way: a free that gave 32 MiB back is as much the answer as the
        // allocation that took it.
        let size = |attribution: &Attribution| {
            attribution
                .change
                .as_ref()
                .map_or(0.0, |change| change.delta.abs())
        };
        finished.sort_by(|left, right| {
            size(right)
                .total_cmp(&size(left))
                .then(left.opened_ns.cmp(&right.opened_ns))
        });
    } else {
        finished.sort_by(|left, right| {
            right
                .matched
                .cmp(&left.matched)
                .then(right.wakeups.cmp(&left.wakeups))
                .then(left.base.cmp(&right.base))
                .then_with(|| left.key.cmp(&right.key))
        });
    }
    let total_attributed = finished.iter().map(|attribution| attribution.matched).sum();
    finished.truncate(rule.top);

    let evidence = evidence(rule);
    // Only `eq` chooses among a key's successive states by the clock.
    // `scoped_eq` chooses by the scope, which is what it is for; `interval`
    // by the address, which is a key; `wakeup` by the task, and says WAKEUP.
    // And `scoped_eq` by the clock too, once two lives of one identity have
    // shared a scope: the key can no longer tell them apart, and which of
    // them an event landed on is the one open when it arrived.
    let within_key = match rule.match_kind {
        MatchKind::Eq => stats.replaced > 0 && evidence > Evidence::Temporal,
        MatchKind::ScopedEq => stats.scope_collapsed > 0,
        _ => false,
    }
    .then_some(Evidence::Temporal);

    Correlation {
        rule: rule.id.clone(),
        from: rule.from.clone(),
        to: rule.to.clone(),
        match_kind: rule.match_kind,
        evidence,
        within_key,
        matched,
        unmatched,
        keyless: 0,
        outside_window: 0,
        states: stats,
        top: finished,
        total_attributed,
        readings: None,
    }
}

/// Folds a `woke` join's pairs into one row per label -- per waker, when the
/// rule names one.
///
/// Counted over the pairs, spanning the earliest close to the latest wakeup,
/// and carrying in `change` what a reader of this match wants summed: the time
/// from each close to the wakeup that followed it, in `delta`, over `opened`
/// closes of which `closed` were woken -- the same number here, since a state
/// that no wakeup ended is not folded.
fn by_waker(finished: Vec<Attribution>) -> Vec<Attribution> {
    let mut folded: BTreeMap<String, Attribution> = BTreeMap::new();
    for attribution in finished.into_iter().filter(|a| a.matched > 0) {
        let waited = attribution
            .closed_ns
            .map_or(0, |closed| closed.saturating_sub(attribution.opened_ns))
            as f64;
        match folded.get_mut(&attribution.key) {
            None => {
                let key = attribution.key.clone();
                folded.insert(
                    key,
                    Attribution {
                        wakeups: Some(1),
                        change: Some(Change {
                            delta: waited,
                            opened: 1,
                            closed: 1,
                        }),
                        ..attribution
                    },
                );
            }
            Some(row) => {
                row.matched += attribution.matched;
                row.wakeups = row.wakeups.map(|pairs| pairs + 1);
                if let Some(change) = row.change.as_mut() {
                    change.delta += waited;
                    change.opened += 1;
                    change.closed += 1;
                }
                row.opened_ns = row.opened_ns.min(attribution.opened_ns);
                row.closed_ns = row.closed_ns.max(attribution.closed_ns);
            }
        }
    }
    folded.into_values().collect()
}

/// Folds a wakeup join's attributions into one row per chain.
///
/// Summed over the events, counted over the wakeups, spanning from the
/// earliest wakeup to the latest end, and ending the way the latest wakeup
/// did: the chain is the thing a reader ranks, and the individual wakeups are
/// what it is made of.
fn by_chain(finished: Vec<Attribution>) -> Vec<Attribution> {
    let mut chains: BTreeMap<String, Attribution> = BTreeMap::new();
    for attribution in finished {
        match chains.get_mut(&attribution.key) {
            None => {
                let key = attribution.key.clone();
                chains.insert(
                    key,
                    Attribution {
                        wakeups: Some(1),
                        ..attribution
                    },
                );
            }
            Some(chain) => {
                chain.matched += attribution.matched;
                chain.wakeups = chain.wakeups.map(|wakeups| wakeups + 1);
                if attribution.opened_ns >= chain.opened_ns {
                    chain.outcome = attribution.outcome;
                }
                chain.opened_ns = chain.opened_ns.min(attribution.opened_ns);
                chain.closed_ns = chain.closed_ns.max(attribution.closed_ns);
            }
        }
    }
    chains.into_values().collect()
}

/// What a match of this kind, on a key of this kind, is worth.
///
/// A wakeup is a chain and says so. Everything else here is a shared typed key,
/// except a propagated request identity, which is the one case where the
/// downstream event carries something the upstream one put there rather than
/// something both happen to know.
fn evidence(rule: &Rule) -> Evidence {
    match rule.match_kind {
        MatchKind::Between | MatchKind::Overlap => Evidence::Temporal,
        MatchKind::Wakeup | MatchKind::Woke => Evidence::Wakeup,
        // A bare equality on a recycled identity is the key `scoped_eq`
        // exists to correct. The pointer is shared, and which life of it an
        // event lands on is whichever held the address last -- two events at
        // one address around the same time, which is all it establishes.
        MatchKind::Eq if is_scoped(rule.via) => Evidence::Temporal,
        _ if rule.via == KeyKind::Req => Evidence::Identity,
        _ => Evidence::Key,
    }
}

/// Whether one event stands for many occurrences: a row drained from a
/// kernel map, weighed, or the total of a window. Every collector that drains
/// marks its rows with at least one of the three.
fn stands_for_many(event: &Event) -> bool {
    [WEIGHT_ATTR, TOTAL_OF_ATTR, "window_ns"]
        .iter()
        .any(|attr| event.attr(attr).is_some())
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
fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn with_keyless(mut correlation: Correlation, keyless: u64) -> Correlation {
    correlation.keyless = keyless;
    correlation
}

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

// -- the decomposition ------------------------------------------------------

/// A whole, the parts it is made of, and what is left over.
///
/// Declared in a session beside the joins:
///
/// ```yaml
/// decompose:
///   - id: where-the-time-went
///     whole: window
///     parts:
///       - {state: on-cpu,   from: cpu.profile,   value: samples}
///       - {state: runnable, from: sched.runqlat, value: total_ns}
///       - {state: blocked,  from: sched.offcpu,  value: total_ns}
/// ```
///
/// The output is one table per key, each row carrying what kind of number it
/// is, the evidence that ties it to the key, its resolution and its shortfall,
/// and under them a residue that is signed and never rounded away: negative is
/// something counted twice, positive is a state no row covers. Both have
/// turned out to be findings every time one was measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decomposition {
    pub id: String,
    pub whole: Whole,
    pub parts: Vec<Part>,
    /// The key every row is read per, so that the rows of one table are about
    /// one thing. `task` unless said otherwise.
    #[serde(default = "default_by")]
    pub by: KeyKind,
    /// How many keys to report, heaviest first.
    #[serde(default = "default_tables")]
    pub top: usize,
}

fn default_by() -> KeyKind {
    KeyKind::Task
}

fn default_tables() -> usize {
    3
}

/// What the parts are parts of.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    untagged,
    expecting = "`window`, or a measurement: {from: <source>, value: <attribute>, reading: \
                 level|delta}"
)]
pub enum Whole {
    Span(Span),
    Measured(Measure),
}

/// A whole the run itself defines rather than a source measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Span {
    /// The run's window, from its start to the last drain. An upper bound on
    /// the life of anything measured in it, and a loose one: it is late by
    /// however late that drain was, which the compute crossing measured at 14
    /// to 146 ms.
    Window,
    /// The launched program's own life, from the gate that released it to the
    /// reap that found it gone.
    ///
    /// The exact whole of a time decomposition, where `window` is only a bound
    /// on one. It is not exact about the exit, and says by how much: the reap
    /// learns of the exit when it next asks, so the exit is known to lie in
    /// `[exited_ns - exited_err_ns, exited_ns]`, and that bracket is the whole's
    /// resolution. It goes into the residue's resolution with the parts',
    /// because a residue smaller than the bracket is not a measurement of
    /// anything.
    ///
    /// Refused at validation in a session that launches nothing: there is no
    /// life to be the whole of.
    Lifetime,
}

/// A whole read off a source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measure {
    pub from: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub less: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading: Option<Reading>,
    /// Which of the source's events it is read from, as for [`Part`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<FilterExpr>,
}

/// One row of a decomposition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Part {
    /// What the row is called: `on-cpu`, `retained`.
    pub state: String,
    /// The source it is read from, which the session has to collect.
    pub from: String,
    /// The attribute read off each event, or `samples` for a sampled source,
    /// which is the count of samples times the period.
    pub value: String,
    /// An attribute subtracted from `value`, for a row that contains another:
    /// `total_ns` less `queued_ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub less: Option<String>,
    /// What one reading of `value` is. Required for a polled source, for the
    /// reason [`Reading`] gives; a traced or sampled one is always a delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading: Option<Reading>,
    /// Which of the source's events the row is read from; every one, when
    /// absent. A level's change is its last reading less its first, and the
    /// first poll of a launched program can land before it exec'd -- a
    /// reading of this tool's own forked image, which `live > 0` leaves out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<FilterExpr>,
    /// An attribute whose value the row names beside its amount: who released
    /// a wait (`holder_tid`), which device a request went to (`dev`). The most
    /// frequent value among the row's events for the key, with how many of
    /// them carried it -- because "the lock was released by 8821" and "by 8821
    /// three times of four" are different claims, and a row that names one
    /// should not read as the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<String>,
    /// The key the row's value was carried through before it reached the
    /// decomposition's `by`: `sock` for a row the socket's cookie tied to the
    /// request. The row is rated the weaker of the two keys -- a request's id
    /// read back off a socket some other thread's program stamped is as good
    /// as the socket, not as the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<KeyKind>,
}

impl Part {
    /// What the row reads, without what it is called.
    fn measure(&self) -> Measure {
        Measure {
            from: self.from.clone(),
            value: self.value.clone(),
            less: self.less.clone(),
            reading: self.reading,
            filter: self.filter.clone(),
        }
    }
}

/// The `value` that means a sample count rather than an attribute.
pub const SAMPLES: &str = "samples";

/// The `value` that means a part is read from a join rather than a source:
/// its `from` names one of the session's `joins`, and the row is the time
/// between the two ends of every pair that join made, summed per key.
///
/// The row the I/O crossing could not declare: *completion -> wakeup* is not
/// an attribute of any event, it is the distance between two events of two
/// sources, and only a join knows which two belong together. Only `woke`
/// makes pairs whose distance means something -- a close and the wakeup that
/// ended it -- so it is the one match a part may read; an `eq` state never
/// ends on its match, and the distance from an allocation to a fault inside
/// it is not a duration of anything.
///
/// ```yaml
/// joins:
///   - {id: completion-released, from: io.latency, to: sched.wakeup, via: task,
///      match: woke, woken: wakee_tid, waker: waker_tid}
/// decompose:
///   - id: one-read
///     whole: {from: io.syscall, value: total_ns}
///     parts:
///       - {state: device, from: io.latency, value: latency_ns}
///       - {state: completion -> wakeup, from: completion-released, value: paired}
/// ```
pub const PAIRED: &str = "paired";

/// What kind of number a row is, which is what a reader needs before adding
/// two of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A count of samples, each standing for one period: exact about the
    /// samples, and a count's plus or minus one about the time.
    Sampled,
    /// Read on a tick: short by whatever happened after the last reading.
    Polled,
    /// Every occurrence, summed: exact.
    Traced,
    /// A counter read once over the run: exact.
    Counted,
    /// The run's own window: a bound, not a measurement.
    Window,
    /// A launched program's life, gate to reap: exact at the start and
    /// bracketed at the end, with the bracket as its resolution.
    Lifetime,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Sampled => "sampled",
            Kind::Polled => "polled",
            Kind::Traced => "traced",
            Kind::Counted => "counted",
            Kind::Window => "window",
            Kind::Lifetime => "lifetime",
        }
    }

    fn of(mode: Mode) -> Self {
        match mode {
            Mode::Sample { .. } => Kind::Sampled,
            Mode::Poll { .. } | Mode::Snapshot => Kind::Polled,
            Mode::Trace => Kind::Traced,
            Mode::Count => Kind::Counted,
        }
    }
}

/// What a decomposition produced over one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecompositionOutput {
    pub id: String,
    pub by: KeyKind,
    /// The unit every row is in. The validator refuses a decomposition whose
    /// parts are not all in one.
    pub unit: Unit,
    /// The heaviest keys, one table each.
    pub tables: Vec<Breakdown>,
    /// How many keys the parts' events named, of which `tables` holds the
    /// heaviest. A decomposition of one task that finds three has found the
    /// children it forked.
    pub keys_seen: usize,
}

/// One key's decomposition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Breakdown {
    pub key: String,
    pub whole: Row,
    pub rows: Vec<Row>,
    /// The rows added up. Absent when one of them could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<f64>,
    /// `whole - parts`, signed, and never rounded. Negative is something
    /// counted twice; positive is a state no row covers. Absent when the whole
    /// or a part could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residue: Option<f64>,
    /// How far the residue may be from zero on resolution alone: the sum of
    /// every row's. Absent when one of them has none that can be stated in
    /// the decomposition's unit, which is every polled level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residue_resolution: Option<f64>,
}

/// One row of a decomposition, or its whole.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub state: String,
    pub source: String,
    /// The attribute read, `samples`, or `observed_ns` for the window.
    pub value: String,
    pub kind: Kind,
    /// What ties the row to the table's key. KEY for a row whose events carry
    /// it; absent for the window, which is a bracket rather than a join.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
    /// Absent when it could not be read, and `missing` says why: a row that
    /// reads as zero because no event carried its attribute would be a
    /// measurement of nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    /// In the decomposition's unit: one period for a sample count, zero for a
    /// traced sum, one poll for a polled delta in nanoseconds. Absent where it
    /// cannot be stated in that unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<f64>,
    /// The events the row was read from.
    pub readings: u64,
    /// The source's run was lossy, so the amount is a lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
    /// How the amount was obtained, when that is more than a sum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// What the part asked the row to name, and how firmly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named: Option<Named>,
}

/// The value a row names, and how many of its events agreed on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Named {
    pub attr: String,
    pub value: String,
    /// Events that carried this value.
    pub events: u64,
    /// Events that carried the attribute at all. `events < of` is a row whose
    /// events disagreed, and the name is only the most frequent answer.
    pub of: u64,
}

/// The most frequent value of `attr` among `events`, ties broken by the value
/// so the answer does not depend on the order events arrived in.
fn most_frequent(events: &[&Event], attr: &str) -> Option<Named> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for event in events {
        if let Some(value) = event.attr(attr) {
            *counts.entry(value.to_string()).or_default() += 1;
        }
    }
    let of = counts.values().sum();
    counts
        .into_iter()
        .max_by(|(left_value, left), (right_value, right)| {
            left.cmp(right).then_with(|| right_value.cmp(left_value))
        })
        .map(|(value, events)| Named {
            attr: attr.to_owned(),
            value,
            events,
            of,
        })
}

/// Whether a decomposition can be computed from what a session collects.
///
/// Every refusal here is a table that would otherwise be printed and be wrong:
/// a part whose source is not collected reads as zero, a part about another
/// target is a row about a different program, a sample count of a traced
/// source multiplies by a period that does not exist, and two units added
/// together are a number of nothing.
pub fn check_decomposition(
    decomposition: &Decomposition,
    requests: &[Request],
    catalog: &Catalog,
) -> Result<(), JoinError> {
    check_decomposition_in(decomposition, requests, &[], catalog)
}

/// [`check_decomposition`], knowing the session's joins, which a part read
/// from a join ([`PAIRED`]) names.
pub fn check_decomposition_in(
    decomposition: &Decomposition,
    requests: &[Request],
    joins: &[Rule],
    catalog: &Catalog,
) -> Result<(), JoinError> {
    let refuse = |reason: String| JoinError::Decomposition {
        id: decomposition.id.clone(),
        reason,
    };
    if decomposition.parts.is_empty() {
        return Err(refuse(
            "has no parts, so there is nothing to decompose into".to_owned(),
        ));
    }

    let request_of = |source: &str, role: &str| -> Result<&Request, JoinError> {
        let mut found = requests
            .iter()
            .filter(|request| request.source.as_str() == source);
        match (found.next(), found.next()) {
            (Some(request), None) => Ok(request),
            (None, _) => Err(refuse(format!(
                "reads {role} from `{source}`, which this session does not collect"
            ))),
            (Some(_), Some(_)) => Err(refuse(format!(
                "reads {role} from `{source}`, which this session collects twice: a row has to \
                 come from one request"
            ))),
        }
    };

    // The unit and the target every row has to share, set by the whole when a
    // source measures it and by the first part when the whole is the window.
    let (unit, mut target): ((Unit, String), Option<(&Target, String)>) = match &decomposition.whole
    {
        Whole::Measured(measure) => {
            let request = request_of(&measure.from, "its whole")?;
            let what = format!("the whole, `{}`", measure.from);
            check_read(
                decomposition,
                request,
                &measure.value,
                measure.less.is_some(),
                measure.reading,
                &what,
            )?;
            (
                (unit_of(catalog, request, &measure.value), what.clone()),
                Some((&request.target, what)),
            )
        }
        Whole::Span(Span::Window) => ((Unit::Ns, "the window".to_owned()), None),
        Whole::Span(Span::Lifetime) => {
            let launches = decomposition.parts.iter().any(|part| {
                requests.iter().any(|request| {
                    request.source.as_str() == part.from
                        && matches!(request.target, Target::Launch(_))
                })
            });
            if !launches {
                return Err(refuse(
                    "takes `lifetime` as its whole, and no part reads from a program this session \
                     launches: a lifetime is the life of a launched program, from the gate to the \
                     reap, and there is none here to be the whole of. `window` is the whole of a \
                     run that attaches to something already running"
                        .to_owned(),
                ));
            }
            ((Unit::Ns, "the lifetime".to_owned()), None)
        }
    };

    for part in &decomposition.parts {
        let what = format!("`{}`, from `{}`", part.state, part.from);
        // A part read from a join is checked against the join's `from`
        // source, which is whose keys its pairs carry and whose target they
        // are about.
        let (request, source) = if part.value == PAIRED {
            let rule = paired_rule(decomposition, part, joins)?;
            request_of(&rule.to, &format!("`{}`'s other end", part.state))?;
            (
                request_of(&rule.from, &format!("`{}`", part.state))?,
                rule.from.as_str(),
            )
        } else {
            let request = request_of(&part.from, &format!("`{}`", part.state))?;
            check_read(
                decomposition,
                request,
                &part.value,
                part.less.is_some(),
                part.reading,
                &what,
            )?;
            (request, part.from.as_str())
        };

        match &target {
            Some((reference, whose)) if *reference != &request.target => {
                return Err(refuse(format!(
                    "reads {what} for {}, and {whose} for {}: a row about another target is a \
                     row about a different program",
                    request.target, reference
                )))
            }
            Some(_) => {}
            None => target = Some((&request.target, what.clone())),
        }

        let part_unit = if part.value == PAIRED {
            Unit::Ns
        } else {
            unit_of(catalog, request, &part.value)
        };
        let (reference, whose) = &unit;
        if *reference != part_unit {
            return Err(refuse(format!(
                "reads {what} in {part_unit}, and {whose} is in {reference}: added together \
                 they are a quantity of nothing"
            )));
        }

        if let Some(entry) = source.parse().ok().and_then(|id| catalog.get(&id)) {
            if !entry.keys.contains(&decomposition.by) {
                return Err(refuse(format!(
                    "is read per `{}`, which `{source}` does not carry",
                    decomposition.by
                )));
            }
            if let Some(via) = part.via.filter(|via| !entry.keys.contains(via)) {
                return Err(refuse(format!(
                    "reads {what} through `{via}`, which `{source}` does not carry: a row cannot \
                     be rated by a key its events never had"
                )));
            }
        }
    }
    Ok(())
}

/// The join a [`PAIRED`] part reads, refused when it is not one a part can.
fn paired_rule<'a>(
    decomposition: &Decomposition,
    part: &Part,
    joins: &'a [Rule],
) -> Result<&'a Rule, JoinError> {
    let refuse = |reason: String| JoinError::Decomposition {
        id: decomposition.id.clone(),
        reason,
    };
    let Some(rule) = joins.iter().find(|rule| rule.id == part.from) else {
        return Err(refuse(format!(
            "reads `{}` as `paired`, the pairs of a join, and `{}` names no join {}",
            part.state,
            part.from,
            if joins.is_empty() {
                "this caller passed: it read the decomposition without the session's joins"
                    .to_owned()
            } else {
                format!(
                    "of this session's: it declares {}",
                    joins
                        .iter()
                        .map(|rule| format!("`{}`", rule.id))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        )));
    };
    if rule.match_kind != MatchKind::Woke {
        return Err(refuse(format!(
            "reads `{}` as the pairs of `{}`, which matches by `{}`: only `woke` ends a state on \
             the event it is paired with, so only its pairs have a distance that is a duration. \
             An `{}` state is still open after its match, and the time from it to one event it \
             matched is not the time of anything",
            part.state, rule.id, rule.match_kind, rule.match_kind
        )));
    }
    if part.less.is_some() || part.reading.is_some() {
        return Err(refuse(format!(
            "reads `{}` as the pairs of `{}` and gives it `less` or `reading`: a pair's distance \
             is one duration, with nothing in it to subtract and no reading to be a level of",
            part.state, rule.id
        )));
    }
    Ok(rule)
}

/// Whether one row's `value` can be read the way its request collects it.
fn check_read(
    decomposition: &Decomposition,
    request: &Request,
    value: &str,
    less: bool,
    reading: Option<Reading>,
    what: &str,
) -> Result<(), JoinError> {
    let refuse = |reason: String| JoinError::Decomposition {
        id: decomposition.id.clone(),
        reason,
    };
    let sampled = matches!(request.mode, Mode::Sample { .. });
    if value == SAMPLES && !sampled {
        return Err(refuse(format!(
            "reads {what} as `samples`, and `{}` is collected in `{}` mode: a sample count is a \
             count times a period, and there is no period here. Name the attribute to add up",
            request.source,
            request.mode.kind()
        )));
    }
    if value == SAMPLES && less {
        return Err(refuse(format!(
            "reads {what} as `samples` less an attribute, and a sample has no attribute worth \
             subtracting from its period"
        )));
    }
    if matches!(request.mode, Mode::Poll { .. }) && reading.is_none() {
        return Err(refuse(format!(
            "reads {what} from a poll, and a polled number is either a level or the change since \
             the poll before; nothing in the reading says which. Say `reading: level` or \
             `reading: delta`"
        )));
    }
    Ok(())
}

/// The unit a row comes out in: time, for a sample count, and the source's own
/// unit for an attribute of it.
fn unit_of(catalog: &Catalog, request: &Request, value: &str) -> Unit {
    if value == SAMPLES {
        return Unit::Ns;
    }
    catalog
        .get(&request.source)
        .map(|entry| entry.unit)
        .unwrap_or(Unit::Count)
}

/// Computes a decomposition over one run.
///
/// `requests` are the requests that served each of `results`, in the same
/// order -- the served ones, since a budget can lower a sampling rate and the
/// period a sample stands for has to be the one it was taken at. `events` is
/// the run's merged stream, the one the joins read.
///
/// Infallible on purpose: [`check_decomposition`] has refused everything that
/// cannot mean anything, and what is left that the run did not supply -- a
/// source that produced no event carrying the attribute -- is a row saying so,
/// not an error that takes every other result of the run with it.
pub fn decompose(
    decomposition: &Decomposition,
    requests: &[Request],
    results: &[Results],
    events: &[Event],
) -> DecompositionOutput {
    decompose_with(decomposition, requests, results, events, &[])
}

/// [`decompose`], knowing the session's joins, which a part read from a join
/// ([`PAIRED`]) names. The join is applied again here, over the same events
/// and windows the run's own correlation saw, so the pairs are the ones that
/// correlation counted.
pub fn decompose_with(
    decomposition: &Decomposition,
    requests: &[Request],
    results: &[Results],
    events: &[Event],
    joins: &[Rule],
) -> DecompositionOutput {
    let served = |source: &str| {
        requests
            .iter()
            .zip(results)
            .find(|(request, _)| request.source.as_str() == source)
    };
    // The pairs every joined part reads, made before any readout borrows
    // them: one event per pair, the `from` event's keys and the pair's
    // distance as `paired`.
    let windows = windows_of(results);
    let pairs: Vec<Option<Paired>> = decomposition
        .parts
        .iter()
        .map(|part| {
            (part.value == PAIRED)
                .then(|| paired_events(decomposition, part, joins, events, &windows))
        })
        .collect();

    // Each row's events, per key, in time order.
    let mut per_key: BTreeMap<String, ()> = BTreeMap::new();
    let mut read_parts: Vec<Readout> = Vec::new();
    // The span the run watched, to tell a source that watched all of it from
    // one a window narrowed: only the first can say a key it never saw had
    // nothing to see.
    let run_closed_ns = results
        .iter()
        .map(|result| result.window.closed_ns)
        .max()
        .unwrap_or(0);
    for (part, pairs) in decomposition.parts.iter().zip(&pairs) {
        let mut readout = match pairs {
            None => Readout::new(
                &part.measure(),
                decomposition.by,
                served(&part.from),
                events,
                run_closed_ns,
            ),
            Some(Ok((rule, paired))) => {
                let mut readout = Readout::new(
                    &part.measure(),
                    decomposition.by,
                    served(&rule.from),
                    paired,
                    run_closed_ns,
                );
                readout.unit = Unit::Ns;
                readout.carried = true;
                readout.paired = Some((
                    evidence(rule),
                    format!(
                        "each `{}` to the next `{}` of its task, as `{}` paired them",
                        rule.from, rule.to, rule.id
                    ),
                ));
                // Either side's losses are the row's: a lost wakeup is a
                // completion that pairs with a later one, or with none.
                if let Some((_, other)) = served(&rule.to) {
                    let records = events.iter().filter(|event| event.kind == rule.to).count();
                    let short = Shortfall::of(&other.losses, records as u64, false);
                    readout.absent_is_not_zero = readout.absent_is_not_zero.or_else(|| {
                        absent_is_not_zero(&rule.to, other, short.as_ref(), run_closed_ns)
                    });
                    readout.short = readout.short.or(short);
                } else {
                    readout.collected = false;
                }
                readout
            }
            Some(Err(why)) => {
                let mut readout =
                    Readout::new(&part.measure(), decomposition.by, None, &[], run_closed_ns);
                readout.unreadable = Some(why.clone());
                readout.unit = Unit::Ns;
                readout
            }
        };
        readout.via = part.via;
        per_key.extend(readout.keys().map(|key| (key, ())));
        read_parts.push(readout);
    }
    let whole_readout = match &decomposition.whole {
        Whole::Measured(measure) => Some(Readout::new(
            measure,
            decomposition.by,
            served(&measure.from),
            events,
            run_closed_ns,
        )),
        Whole::Span(_) => None,
    };
    // A key the whole carries and no part does is still a table: a request
    // that waited on the disk and whose id no block request carries -- a page
    // lock, a metadata read in a kworker -- has a whole and no parts, and the
    // answer for it is a residue that is all of it, not silence.
    if let Some(readout) = &whole_readout {
        per_key.extend(readout.keys().map(|key| (key, ())));
    }
    let window_ns = decomposition
        .parts
        .iter()
        .filter_map(|part| served(&part.from))
        .map(|(_, result)| result.observed_ns)
        .max()
        .unwrap_or(0);

    let unit = read_parts
        .first()
        .map(|readout| readout.unit)
        .unwrap_or(Unit::Ns);

    // One life per run, published on every result of it: the run model stamps
    // it at the gate and the reap, and the emit layer carries it.
    let lifetime = results.iter().find_map(|result| result.lifetime);

    let mut tables: Vec<Breakdown> = per_key
        .keys()
        .map(|key| {
            let rows: Vec<Row> = decomposition
                .parts
                .iter()
                .zip(&read_parts)
                .map(|(part, readout)| {
                    let mut row = readout.row(&part.state, key);
                    // A row that measured nothing had nothing released and went
                    // nowhere, and naming a value for it -- a holder of `0`, a
                    // device nobody was sent to -- would read as a finding.
                    let measured = row.amount.is_some_and(|amount| amount != 0.0);
                    if let (Some(attr), true) = (&part.names, measured) {
                        let events = readout.events.get(key).map(Vec::as_slice).unwrap_or(&[]);
                        row.named = most_frequent(events, attr);
                    }
                    row
                })
                .collect();
            let whole = match (&whole_readout, &decomposition.whole) {
                (Some(readout), _) => readout.row("whole", key),
                (None, Whole::Span(Span::Lifetime)) => lifetime_row(lifetime),
                (None, _) => Row {
                    state: "whole".to_owned(),
                    source: "the run's window".to_owned(),
                    value: "observed_ns".to_owned(),
                    kind: Kind::Window,
                    evidence: None,
                    amount: Some(window_ns as f64),
                    resolution: None,
                    readings: 0,
                    short: None,
                    missing: None,
                    note: Some(
                        "an upper bound: the window runs to the last drain, and the life of \
                         what is decomposed may end well before it"
                            .to_owned(),
                    ),
                    named: None,
                },
            };
            let parts: Option<f64> = rows.iter().map(|row| row.amount).sum();
            let residue = whole.amount.zip(parts).map(|(whole, parts)| whole - parts);
            let residue_resolution: Option<f64> = rows
                .iter()
                .chain(std::iter::once(&whole).filter(|whole| whole.kind != Kind::Window))
                .map(|row| row.resolution)
                .sum();
            Breakdown {
                key: key.clone(),
                whole,
                rows,
                parts,
                residue,
                residue_resolution,
            }
        })
        .collect();

    let keys_seen = tables.len();
    let weight = |table: &Breakdown| -> f64 {
        table
            .rows
            .iter()
            .filter_map(|row| row.amount)
            .map(f64::abs)
            .sum()
    };
    tables.sort_by(|left, right| {
        weight(right)
            .total_cmp(&weight(left))
            .then_with(|| left.key.cmp(&right.key))
    });
    tables.truncate(decomposition.top.max(1));

    DecompositionOutput {
        id: decomposition.id.clone(),
        by: decomposition.by,
        unit,
        tables,
        keys_seen,
    }
}

/// When each source of a run was watching, from its results, as `main.rs`
/// reads them for the joins: two results of one source by their union.
fn windows_of(results: &[Results]) -> BTreeMap<String, Watched> {
    let mut windows: BTreeMap<String, Watched> = BTreeMap::new();
    for result in results {
        windows
            .entry(result.source.clone())
            .and_modify(|watched| match (watched.attached, result.window.attached) {
                (_, false) => {}
                (false, true) => *watched = result.window,
                (true, true) => {
                    watched.opened_ns = watched.opened_ns.min(result.window.opened_ns);
                    watched.closed_ns = watched.closed_ns.max(result.window.closed_ns);
                }
            })
            .or_insert(result.window);
    }
    // A run that never said when a source watched said nothing to narrow it.
    windows.retain(|_, watched| *watched != Watched::default());
    windows
}

/// The pairs a [`PAIRED`] part reads, as events of the part's own kind: the
/// `from` event of each pair, carrying its keys, with the pair's distance as
/// `paired`.
fn paired_events<'r>(
    decomposition: &Decomposition,
    part: &Part,
    joins: &'r [Rule],
    events: &[Event],
    windows: &BTreeMap<String, Watched>,
) -> Paired<'r> {
    let rule = paired_rule(decomposition, part, joins).map_err(|error| error.to_string())?;
    let mut ordered: Vec<&Event> = events
        .iter()
        .filter(|event| event.kind == rule.from || event.kind == rule.to)
        .collect();
    ordered.sort_by_key(|event| (event.ts_ns, u8::from(event.kind != rule.from)));
    let mut watch = Watch::of(rule, windows);
    let (_, pairs) = woke(rule, &ordered, &mut watch)
        .map_err(|error| format!("the join `{}` failed on this run: {error}", rule.id))?;
    Ok((
        rule,
        pairs
            .into_iter()
            .map(|(from, distance)| {
                let mut event = from.clone();
                event.kind = part.from.clone();
                event.attrs.push(crate::event::Attr::uint(PAIRED, distance));
                event
            })
            .collect(),
    ))
}

/// The whole of a decomposition whose whole is the launched program's life.
///
/// Exact at the start, where this process wrote the gate, and bracketed at the
/// end, where it learned of the exit by asking: the bracket is the resolution,
/// and it is what the residue's resolution has to include, because a residue
/// inside it says nothing. A program this tool killed at the deadline has a
/// life that ended at the tool's choosing, which the note says, since a
/// decomposition of a program that did not finish is a decomposition of the
/// deadline.
fn lifetime_row(lifetime: Option<crate::collect::Lifetime>) -> Row {
    let base = Row {
        state: "whole".to_owned(),
        source: "the program's life".to_owned(),
        value: "gate to reap".to_owned(),
        kind: Kind::Lifetime,
        evidence: None,
        amount: None,
        resolution: None,
        readings: 0,
        short: None,
        missing: None,
        note: None,
        named: None,
    };
    match lifetime {
        None => Row {
            missing: Some(
                "the run carried no lifetime: the program was never released, or the run \
                 failed before it was reaped"
                    .to_owned(),
            ),
            ..base
        },
        Some(life) => Row {
            amount: Some(life.exited_ns.saturating_sub(life.released_ns) as f64),
            resolution: Some(life.exited_err_ns as f64),
            note: Some(if life.terminated {
                "the program did not finish: this tool ended it at the run's deadline, so its \
                 life is the deadline's and not its own"
                    .to_owned()
            } else {
                format!(
                    "exact at the gate; the exit is bracketed to the last {}, which is how long \
                     before the reap the program was last seen alive",
                    period_label(life.exited_err_ns as f64)
                )
            }),
            ..base
        },
    }
}

/// One row's source, read once for every key.
struct Readout<'a> {
    source: String,
    value: String,
    less: Option<String>,
    reading: Reading,
    kind: Kind,
    unit: Unit,
    /// One period, for a sample count.
    period_ns: Option<f64>,
    /// One poll, for a polled delta.
    poll_ns: Option<f64>,
    /// Per key, the events in time order.
    events: BTreeMap<String, Vec<&'a Event>>,
    /// Whether any event of the source carried each attribute, before the
    /// part's filter: a row that finds none reads as missing rather than as
    /// zero. Counted before the filter because a filter that kept nothing is
    /// the source answering, not the source unable to.
    carried: bool,
    /// The keys the source produced any event for, before the part's filter.
    seen: BTreeSet<String>,
    /// The part asked for a subset of the source's events.
    filtered: bool,
    /// Why a key the source produced nothing for cannot be read as zero, when
    /// it cannot: the source lost events, or watched less than the run.
    absent_is_not_zero: Option<String>,
    short: Option<Shortfall>,
    collected: bool,
    /// The key the rows are read by. A request identity is propagated -- the
    /// event carries what the request's birth put there -- so a row read by it
    /// is IDENTITY, the same rating `evidence` gives a join on `req`. Every
    /// other key is shared by two sides without being carried from one to the
    /// other, which is KEY.
    by: KeyKind,
    /// The key the value was carried through before it reached `by`; the row
    /// is rated the weaker of the two.
    via: Option<KeyKind>,
    /// For a part read from a join: the join's evidence, which the row is
    /// rated no better than, and the note that says what was summed.
    paired: Option<(Evidence, String)>,
    /// Why the row could not be read at all, when that is decided before any
    /// event is: a join a part reads that failed on this run's events.
    unreadable: Option<String>,
}

impl<'a> Readout<'a> {
    fn new(
        measure: &Measure,
        by: KeyKind,
        served: Option<(&Request, &Results)>,
        events: &'a [Event],
        run_closed_ns: u64,
    ) -> Self {
        let (source, value, less) = (
            measure.from.as_str(),
            measure.value.as_str(),
            measure.less.as_deref(),
        );
        let (reading, filter) = (measure.reading, measure.filter.as_ref());
        let mut per_key: BTreeMap<String, Vec<&Event>> = BTreeMap::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut ordered: Vec<&Event> = events.iter().filter(|event| event.kind == source).collect();
        ordered.sort_by_key(|event| event.ts_ns);
        let mut carried = value == SAMPLES;
        let mut records = 0u64;
        let mut many = false;
        for event in ordered {
            records += 1;
            many |= stands_for_many(event);
            if value != SAMPLES {
                let has_value = event.attr(value).is_some();
                let has_less = less.is_none_or(|less| event.attr(less).is_some());
                carried |= has_value && has_less;
            }
            let Some(key) = event.key(by) else {
                continue;
            };
            let key = key.to_string();
            if filter.is_none_or(|predicate| crate::filter::matches(predicate, event)) {
                per_key.entry(key.clone()).or_default().push(event);
            }
            seen.insert(key);
        }

        let (kind, period_ns, poll_ns) = match served.map(|(request, _)| request.mode) {
            Some(Mode::Sample { freq }) => {
                (Kind::Sampled, Some(1e9 / freq.as_hz().max(1) as f64), None)
            }
            Some(Mode::Poll { interval }) => (Kind::Polled, None, Some(interval.as_nanos() as f64)),
            Some(mode) => (Kind::of(mode), None, None),
            None => (Kind::Traced, None, None),
        };
        let unit = if value == SAMPLES {
            Unit::Ns
        } else {
            served.map(|(_, result)| result.unit).unwrap_or(Unit::Count)
        };
        let reading = match kind {
            Kind::Polled => reading.unwrap_or(Reading::Level),
            _ => reading.unwrap_or(Reading::Delta),
        };
        let short = served.and_then(|(_, result)| Shortfall::of(&result.losses, records, many));
        let absent_is_not_zero = served.and_then(|(_, result)| {
            absent_is_not_zero(source, result, short.as_ref(), run_closed_ns)
        });

        Self {
            source: source.to_owned(),
            value: value.to_owned(),
            less: less.map(str::to_owned),
            reading,
            kind,
            unit,
            period_ns,
            poll_ns,
            events: per_key,
            carried,
            seen,
            filtered: filter.is_some(),
            absent_is_not_zero,
            short,
            collected: served.is_some(),
            by,
            via: None,
            paired: None,
            unreadable: None,
        }
    }

    fn keys(&self) -> impl Iterator<Item = String> + '_ {
        self.events.keys().cloned()
    }

    fn row(&self, state: &str, key: &str) -> Row {
        let events = self.events.get(key).map(Vec::as_slice).unwrap_or(&[]);
        let missing = if let Some(why) = &self.unreadable {
            Some(why.clone())
        } else if !self.collected {
            Some(format!("the run collected no `{}`", self.source))
        } else if !self.carried {
            Some(match self.less.as_deref() {
                Some(less) => format!(
                    "no `{}` event carried both `{}` and `{less}`",
                    self.source, self.value
                ),
                None => format!("no `{}` event carried `{}`", self.source, self.value),
            })
        } else if events.is_empty() && !(self.filtered && self.seen.contains(key)) {
            // The source had nothing at all for this key. That is a zero only
            // from a source that watched the whole run and lost nothing.
            self.absent_is_not_zero.clone()
        } else {
            None
        };

        let read = |attr: &str| -> f64 {
            let values = events
                .iter()
                .filter_map(|event| event.attr(attr).and_then(AttrValue::as_f64));
            match self.reading {
                Reading::Delta => values.sum(),
                Reading::Level => {
                    let values: Vec<f64> = values.collect();
                    match (values.first(), values.last()) {
                        (Some(first), Some(last)) => last - first,
                        _ => 0.0,
                    }
                }
            }
        };
        let (amount, note) = if missing.is_some() {
            (None, None)
        } else if let (Some((_, note)), false) = (&self.paired, events.is_empty()) {
            (
                Some(read(&self.value)),
                Some(format!("{note}, {} pairs", events.len())),
            )
        } else if events.is_empty() {
            (
                Some(0.0),
                Some(if self.seen.contains(key) {
                    "no event of this key passed the filter".to_owned()
                } else {
                    format!(
                        "no `{}` event of this key, from a source that watched the whole run \
                         and lost nothing",
                        self.source
                    )
                }),
            )
        } else if self.value == SAMPLES {
            let period = self.period_ns.unwrap_or(0.0);
            (
                Some(events.len() as f64 * period),
                Some(format!(
                    "{} samples x {}",
                    events.len(),
                    period_label(period)
                )),
            )
        } else {
            let value = read(&self.value) - self.less.as_deref().map_or(0.0, read);
            let note = match (self.reading, self.less.as_deref()) {
                (Reading::Level, Some(less)) => Some(format!(
                    "`{}` less `{less}`, last reading minus first",
                    self.value
                )),
                (Reading::Level, None) => Some("last reading minus first".to_owned()),
                (Reading::Delta, Some(less)) => {
                    Some(format!("`{}` less `{less}`, summed", self.value))
                }
                (Reading::Delta, None) => None,
            };
            (Some(value), note)
        };

        let resolution = match (self.kind, self.reading) {
            (Kind::Sampled, _) => self.period_ns,
            (Kind::Traced | Kind::Counted, _) => Some(0.0),
            (Kind::Polled, Reading::Delta) if self.unit == Unit::Ns => self.poll_ns,
            _ => None,
        };

        Row {
            state: state.to_owned(),
            source: self.source.to_owned(),
            value: self.value.to_owned(),
            kind: self.kind,
            evidence: Some({
                let by = match self.via {
                    Some(via) => rate(self.by).min(rate(via)),
                    None => rate(self.by),
                };
                self.paired
                    .as_ref()
                    .map_or(by, |(joined, _)| by.min(*joined))
            }),
            amount,
            resolution,
            readings: events.len() as u64,
            short: self.short.clone(),
            missing,
            note,
            named: None,
        }
    }
}

/// What a row read by one key is worth: a propagated request identity is
/// IDENTITY, every other key KEY -- the same ladder `evidence` gives a join.
fn rate(key: KeyKind) -> Evidence {
    if key == KeyKind::Req {
        Evidence::Identity
    } else {
        Evidence::Key
    }
}

/// Why a key a source produced no event for cannot be read as a zero, or
/// `None` when it can.
///
/// A source that watched the whole run and lost nothing, and saw nothing of a
/// key, measured that key doing nothing -- a task that never blocked has no
/// off-cpu row, and its blocked time is zero, not unknown. A lossy source may
/// have lost exactly that key's events, and one a window narrowed was not
/// watching for part of the run; neither can say the same, and a zero from
/// either would be a measurement of what it did not see.
fn absent_is_not_zero(
    source: &str,
    result: &Results,
    short: Option<&Shortfall>,
    run_closed_ns: u64,
) -> Option<String> {
    let window = result.window;
    if window == Watched::default() {
        return Some(format!(
            "no `{source}` event of this key, and the run did not say when `{source}` was \
             watching, so its absence is not a zero"
        ));
    }
    if !window.attached {
        return Some(format!(
            "no `{source}` event of this key: `{source}` was never attached"
        ));
    }
    if window.opened_ns > window.run_opened_ns || window.closed_ns < run_closed_ns {
        return Some(format!(
            "no `{source}` event of this key, and `{source}` watched only part of the run, so \
             its absence is not a zero"
        ));
    }
    if short.is_some() || !result.losses.is_clean() {
        return Some(format!(
            "no `{source}` event of this key, and `{source}` lost events, so its absence is not \
             a zero"
        ));
    }
    None
}

fn period_label(period_ns: f64) -> String {
    format!("{:.3} ms", period_ns / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Attr, CollectorId};

    #[test]
    fn the_ladder_is_in_order_and_every_level_says_what_it_does_not_establish() {
        let mut sorted = Evidence::LADDER;
        sorted.sort();
        assert_eq!(sorted, Evidence::LADDER, "the ladder is weakest first");
        for level in Evidence::LADDER {
            assert!(!level.establishes().is_empty(), "{level}");
            assert!(!level.does_not_establish().is_empty(), "{level}");
        }
        assert!(Evidence::Temporal.establishes().contains("hypothesis"));
        assert_ne!(
            Evidence::Temporal.establishes(),
            Evidence::Identity.establishes()
        );
    }

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
            woken: None,
            span: None,
            stamped: None,
            depth: None,
            value: None,
            reading: None,
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
            states.closed
                + states.expired
                + states.overflowed
                + states.window_closed
                + states.still_open,
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
        // The upstream side carrying no key is the rule not fitting its source:
        // nothing it opens could ever be found again. That stays a refusal.
        let mut unkeyed = allocation(10, 0x1000, 0x1000, "alloc");
        unkeyed.keys.clear();
        let events = vec![unkeyed, fault(20, 0x1500)];
        let error = apply(&rule(), &events).unwrap_err();
        assert!(
            matches!(error, JoinError::MissingKey { ref via, .. } if *via == KeyKind::Mem),
            "{error}"
        );
        assert!(error.to_string().contains("mem.heap.alloc"), "{error}");
    }

    /// A downstream event carrying no key is outside every state by
    /// construction, and is counted apart rather than refusing the rule.
    ///
    /// Streams T and V both met the refusal from the other side: a block
    /// request issued outside any request carries no `req`, and every real
    /// session has some. Refusing the whole rule at the first one meant a
    /// correct join could never be run on a real machine.
    #[test]
    fn a_downstream_event_without_the_key_is_counted_apart_not_refused() {
        let mut unkeyed = fault(20, 0x1500);
        unkeyed.keys.clear();
        let events = vec![
            allocation(10, 0x1000, 0x1000, "alloc"),
            unkeyed,
            fault(30, 0x1500),
        ];
        let correlation = apply(&rule(), &events).expect("the rule still runs");
        assert_eq!(correlation.keyless, 1, "the unkeyed fault is counted");
        assert_eq!(correlation.matched, 1, "and the keyed one still matches");
        assert_eq!(
            correlation.unmatched, 0,
            "an event that had no key is not one that found no state"
        );
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
            woken: None,
            span: None,
            stamped: None,
            depth: None,
            value: None,
            reading: None,
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
            woken: None,
            span: None,
            stamped: None,
            depth: None,
            value: None,
            reading: None,
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
            woken: None,
            span: None,
            stamped: None,
            depth: None,
            value: None,
            reading: None,
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

    /// A waker of zero whose wakeup came from an interrupt is that interrupt,
    /// not the idle task -- directly, and at the far end of a longer chain;
    /// and a waker of zero the program says was a task stays the idle task.
    #[test]
    fn a_waker_of_zero_from_an_interrupt_is_named_as_the_interrupt() {
        let from_irq = |ts: u64, wakee: i32, context: &str| {
            let mut event = woken(ts, wakee, 0);
            event.attrs.push(Attr::text("waker_context", context));
            event
        };
        let events = vec![from_irq(10, 8, "hardirq"), switched(20, 8)];
        let correlation = apply(&wakeup_rule(), &events).unwrap();
        assert_eq!(correlation.top[0].key, "interrupt (hardirq) -> task:8");

        let mut rule = wakeup_rule();
        rule.depth = Some(4);
        let events = vec![from_irq(10, 5, "softirq"), woken(11, 6, 5), switched(20, 6)];
        let correlation = apply(&rule, &events).unwrap();
        assert_eq!(
            correlation.top[0].key,
            "interrupt (softirq) -> task:5 -> task:6"
        );

        let events = vec![from_irq(10, 8, "task"), switched(20, 8)];
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

    // -- between -----------------------------------------------------------

    // -- woke ---------------------------------------------------------------

    fn completed(ts_ns: u64, tid: i32) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "io.latency".to_owned(),
            keys: vec![KeyValue::task(tid)],
            attrs: vec![Attr::uint("latency_ns", 1_000)],
            collector_id: CollectorId::new("io"),
            seq: 0,
        }
    }

    fn woke_rule() -> Rule {
        Rule {
            id: "completion-released".to_owned(),
            from: "io.latency".to_owned(),
            to: "sched.wakeup".to_owned(),
            match_kind: MatchKind::Woke,
            woken: Some("wakee_tid".to_owned()),
            waker: Some("waker_tid".to_owned()),
            ..wakeup_rule()
        }
    }

    /// The pairing the I/O crossing did in its test: a completion, and the
    /// first wakeup of the task it was for -- not the wakeup before the
    /// request, which is the previous read's delivery.
    #[test]
    fn a_completion_is_paired_with_the_next_wakeup_of_its_task_and_not_the_one_before() {
        let mut interrupt = woken(33, 100, 0);
        interrupt.attrs.push(Attr::text("waker_context", "softirq"));
        let events = vec![
            // The previous read's delivery, before this request completed.
            woken(5, 100, 7),
            completed(10, 100),
            // Another task's wakeup, in between: not part of the question.
            woken(12, 555, 7),
            woken(14, 100, 7),
            // A second wakeup of the same task: the pair is already made.
            woken(15, 100, 8),
            // A completion no wakeup follows: the reader never slept.
            completed(20, 100),
            completed(30, 100),
            interrupt,
        ];
        let correlation = apply(&woke_rule(), &events).unwrap();

        assert_eq!(correlation.evidence, Evidence::Wakeup);
        assert_eq!(
            correlation.matched, 2,
            "two completions were followed by a wakeup"
        );
        assert_eq!(
            correlation.unmatched, 1,
            "the completion at 20 was replaced by the one at 30 with no wakeup between"
        );
        assert_eq!(correlation.states.opened, 3);
        assert_eq!(correlation.states.replaced, 1);
        states_balance(&correlation);

        let by_worker = correlation
            .top
            .iter()
            .find(|row| row.key == "task:7 -> task:100")
            .expect("the worker that delivered the first completion");
        assert_eq!(by_worker.matched, 1);
        assert_eq!(
            by_worker.change.as_ref().map(|change| change.delta),
            Some(4.0),
            "completion at 10, wakeup at 14"
        );
        let by_interrupt = correlation
            .top
            .iter()
            .find(|row| row.key == "interrupt (softirq) -> task:100")
            .expect("a wakeup from a softirq is named as the softirq");
        assert_eq!(
            by_interrupt.change.as_ref().map(|change| change.delta),
            Some(3.0)
        );
    }

    #[test]
    fn a_woke_rule_on_a_key_that_is_not_a_task_is_refused_with_the_reason() {
        let rule = Rule {
            via: KeyKind::Blk,
            ..woke_rule()
        };
        let error = apply(&rule, &[]).unwrap_err().to_string();
        assert!(error.contains("`task:tid` is the only key"), "{error}");
    }

    #[test]
    fn a_wakeup_that_does_not_name_its_wakee_fails_by_name() {
        let rule = Rule {
            woken: Some("woken_task".to_owned()),
            ..woke_rule()
        };
        let events = vec![completed(10, 100), woken(14, 100, 7)];
        let error = apply(&rule, &events).unwrap_err();
        assert!(matches!(error, JoinError::MissingWoken { .. }), "{error}");
        assert!(error.to_string().contains("woken_task"), "{error}");
    }

    // -- overlap ------------------------------------------------------------

    fn polled(ts_ns: u64, tid: i32) -> Event {
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

    fn blocked(ts_ns: u64, tid: i32, total_ns: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "sched.offcpu".to_owned(),
            keys: vec![KeyValue::task(tid)],
            attrs: vec![Attr::uint("total_ns", total_ns)],
            collector_id: CollectorId::new("offcpu"),
            seq: 0,
        }
    }

    fn overlap_rule() -> Rule {
        Rule {
            id: "blocked-per-reading".to_owned(),
            from: "proc.threads".to_owned(),
            to: "sched.offcpu".to_owned(),
            match_kind: MatchKind::Overlap,
            waker: None,
            value: Some("total_ns".to_owned()),
            span: Some("total_ns".to_owned()),
            ..wakeup_rule()
        }
    }

    /// A block of 250 across readings 100 apart is split by the share of it
    /// each reading's interval holds, and what lies before the first reading
    /// is uncredited rather than given to it.
    #[test]
    fn a_window_across_several_readings_is_split_by_its_overlap_with_each() {
        let events = vec![
            polled(100, 7),
            polled(200, 7),
            polled(300, 7),
            polled(400, 7),
            // [120, 370]: 80 of (100, 200], 100 of (200, 300], 70 of (300, 400].
            blocked(370, 7, 250),
            // [50, 150]: 50 before the first reading, 50 in (100, 200].
            blocked(150, 7, 100),
            // Another task's block, which no reading of task 7 covers.
            blocked(250, 8, 10),
        ];
        let correlation = apply(&overlap_rule(), &events).unwrap();
        assert_eq!(correlation.evidence, Evidence::Temporal);
        assert_eq!((correlation.matched, correlation.unmatched), (2, 1));
        let credit = |start: u64| {
            correlation
                .top
                .iter()
                .find(|row| row.opened_ns == start)
                .and_then(|row| row.change.as_ref())
                .map(|change| change.delta)
        };
        assert_eq!(credit(100), Some(80.0 + 50.0));
        assert_eq!(credit(200), Some(100.0));
        assert_eq!(credit(300), Some(70.0));
        let readings = correlation.readings.as_ref().unwrap();
        assert_eq!(
            readings.baselines, 1,
            "the first reading of task 7 covers nothing"
        );
        assert_eq!(readings.credited, 300.0);
        assert_eq!(readings.uncredited, 50.0 + 10.0);
        states_balance(&correlation);
    }

    /// A reading that states its own interval covers it, first reading
    /// included; an instant belongs to the interval that closed over it.
    #[test]
    fn a_reading_that_states_its_interval_covers_it_and_an_instant_lands_in_one() {
        let mut first = polled(100, 7);
        first.attrs.push(Attr::uint("interval_ns", 100));
        let rule = Rule {
            span: None,
            stamped: None,
            ..overlap_rule()
        };
        let events = vec![
            first,
            polled(200, 7),
            blocked(100, 7, 5),
            blocked(101, 7, 7),
        ];
        let correlation = apply(&rule, &events).unwrap();
        let readings = correlation.readings.as_ref().unwrap();
        assert_eq!(readings.baselines, 0);
        let credit = |start: u64| {
            correlation
                .top
                .iter()
                .find(|row| row.opened_ns == start)
                .and_then(|row| row.change.as_ref())
                .map(|change| change.delta)
        };
        assert_eq!(credit(0), Some(5.0), "an instant at 100 is in (0, 100]");
        assert_eq!(credit(100), Some(7.0), "and one at 101 in (100, 200]");
    }

    #[test]
    fn an_overlap_rule_that_names_no_value_or_a_missing_span_is_refused_by_name() {
        let rule = Rule {
            value: None,
            ..overlap_rule()
        };
        let error = apply(&rule, &[]).unwrap_err();
        assert!(matches!(error, JoinError::NoShareValue { .. }), "{error}");

        let rule = Rule {
            span: Some("offcpu_ns".to_owned()),
            ..overlap_rule()
        };
        let events = vec![polled(100, 7), polled(200, 7), blocked(150, 7, 10)];
        let error = apply(&rule, &events).unwrap_err();
        assert!(matches!(error, JoinError::MissingSpan { .. }), "{error}");
    }

    #[test]
    fn overlap_from_a_traced_source_is_refused_before_the_run() {
        let requests = [
            request("sched.offcpu", "trace"),
            request("sched.offcpu", "trace"),
        ];
        let rule = Rule {
            from: "sched.offcpu".to_owned(),
            ..overlap_rule()
        };
        let error = check_in(&rule, &requests[..1], &catalog()).unwrap_err();
        assert!(matches!(error, JoinError::NotAnInterval { .. }), "{error}");
        let polled = [request("proc.threads", "poll@50ms")];
        check_in(&overlap_rule(), &polled, &catalog()).expect("a poll covers intervals");
    }

    // -- a scope two lives share --------------------------------------------

    /// Two lives of one pointer inside one plug section carry one key. The
    /// engine pairs them by order, which is right while nothing is lost, and
    /// says the crediting within the key was the clock's.
    #[test]
    fn two_lives_under_one_scope_are_paired_by_order_and_say_so() {
        let events = vec![
            block(10, "io.rq.issue", 0xa, 5),
            block(20, "io.rq.complete", 0xa, 5),
            // Reissued inside the same plug: the same start stamp.
            block(30, "io.rq.issue", 0xa, 5),
            block(40, "io.rq.complete", 0xa, 5),
            // And once more under a new stamp: a different life, told apart.
            block(50, "io.rq.issue", 0xa, 9),
            block(60, "io.rq.complete", 0xa, 9),
        ];
        let correlation = apply(&scoped_rule(), &events).unwrap();
        assert_eq!((correlation.matched, correlation.unmatched), (3, 0));
        assert_eq!(correlation.states.replaced, 2);
        assert_eq!(correlation.states.scope_collapsed, 1);
        assert_eq!(correlation.evidence, Evidence::Key);
        assert_eq!(correlation.within_key, Some(Evidence::Temporal));
        let clean = apply(&scoped_rule(), &events[4..]).unwrap();
        assert_eq!(clean.states.scope_collapsed, 0);
        assert_eq!(
            clean.within_key, None,
            "no collapse, nothing left to the clock"
        );
    }

    fn resident(ts_ns: u64, tid: i32, bytes: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.rss".to_owned(),
            keys: vec![KeyValue::task(tid)],
            attrs: vec![Attr::uint("bytes", bytes)],
            collector_id: CollectorId::new("rss"),
            seq: 0,
        }
    }

    fn held(ts_ns: u64, tid: i32, op: &str, bytes: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.heap.alloc".to_owned(),
            keys: vec![KeyValue::task(tid)],
            attrs: vec![Attr::text("op", op), Attr::uint("bytes", bytes)],
            collector_id: CollectorId::new("alloc"),
            seq: 0,
        }
    }

    fn between_rule() -> Rule {
        Rule {
            id: "residency".to_owned(),
            from: "mem.heap.alloc".to_owned(),
            to: "mem.rss".to_owned(),
            via: KeyKind::Task,
            match_kind: MatchKind::Between,
            opens: Some("op == 'alloc'".parse().unwrap()),
            closes: Some("op == 'free'".parse().unwrap()),
            waker: None,
            woken: None,
            span: None,
            stamped: None,
            depth: None,
            value: Some("bytes".to_owned()),
            reading: Some(Reading::Level),
            window: None,
            capacity: 1000,
            top: 10,
        }
    }

    const MIB: u64 = 1 << 20;

    /// The shape of the memory crossing's table, in miniature: four ranges
    /// open in one interval, the total rises in it, and a quiet interval that
    /// moved anyway is counted rather than credited.
    #[test]
    fn a_change_is_credited_to_what_opened_and_closed_in_its_interval() {
        let events = vec![
            resident(0, 7, 10 * MIB),
            held(10, 7, "alloc", 8 * MIB),
            held(11, 7, "alloc", 8 * MIB),
            held(12, 7, "alloc", 8 * MIB),
            held(13, 7, "alloc", 8 * MIB),
            resident(100, 7, 42 * MIB),
            // Nothing happened here, and the total moved anyway.
            resident(200, 7, 43 * MIB),
            held(250, 7, "free", 0),
            held(251, 7, "free", 0),
            resident(300, 7, 27 * MIB),
        ];
        let correlation = apply(&between_rule(), &events).unwrap();
        assert_eq!(correlation.evidence, Evidence::Temporal);
        assert_eq!(correlation.within_key, None);
        assert_eq!((correlation.matched, correlation.unmatched), (2, 2));
        let readings = correlation.readings.as_ref().unwrap();
        assert_eq!(readings.baselines, 1);
        assert_eq!(readings.uncredited, MIB as f64);
        assert_eq!(readings.credited, (32.0 - 16.0) * MIB as f64);

        // The larger move first, whichever way it went.
        let first = correlation.top[0].change.as_ref().unwrap();
        assert_eq!(
            (first.delta, first.opened, first.closed),
            (32.0 * MIB as f64, 4, 0)
        );
        assert_eq!(
            (correlation.top[0].opened_ns, correlation.top[0].closed_ns),
            (0, Some(100))
        );
        let second = correlation.top[1].change.as_ref().unwrap();
        assert_eq!(
            (second.delta, second.opened, second.closed),
            (-16.0 * MIB as f64, 0, 2)
        );
        states_balance(&correlation);
    }

    /// A key is still a key under `between`: a reading of one task is never
    /// credited to what another task opened, however close in time.
    #[test]
    fn a_reading_is_credited_only_with_the_changes_of_its_own_key() {
        let events = vec![
            resident(0, 7, 10 * MIB),
            resident(0, 9, 10 * MIB),
            held(10, 9, "alloc", 8 * MIB),
            resident(100, 7, 18 * MIB),
            resident(100, 9, 10 * MIB),
        ];
        let correlation = apply(&between_rule(), &events).unwrap();
        assert_eq!(correlation.matched, 1);
        let only = &correlation.top[0];
        assert_eq!(only.key, "task:9");
        assert_eq!(only.change.as_ref().unwrap().delta, 0.0);
        assert_eq!(
            correlation.readings.as_ref().unwrap().uncredited,
            8.0 * MIB as f64,
            "task 7 moved with nothing of its own opening"
        );
    }

    /// A change before the first reading of its key has no interval to fall
    /// in; one after the last reading has no reading to take it. Both are
    /// counted, and the lifecycle still balances.
    #[test]
    fn changes_outside_every_interval_are_counted_at_both_ends() {
        let events = vec![
            held(1, 7, "alloc", MIB),
            resident(10, 7, MIB),
            held(20, 7, "alloc", MIB),
        ];
        let correlation = apply(&between_rule(), &events).unwrap();
        assert_eq!(correlation.readings.as_ref().unwrap().before_first, 1);
        assert_eq!(correlation.states.still_open, 1);
        assert!(correlation.top.is_empty());
        states_balance(&correlation);
    }

    /// A delta reading is its own change: the difference of two deltas is a
    /// quantity of nothing, and the first one needs no reading before it.
    #[test]
    fn a_delta_reading_is_credited_as_itself() {
        let mut rule = between_rule();
        rule.reading = Some(Reading::Delta);
        let events = vec![
            held(5, 7, "alloc", MIB),
            resident(10, 7, 3 * MIB),
            held(15, 7, "alloc", MIB),
            resident(20, 7, 5 * MIB),
        ];
        let correlation = apply(&rule, &events).unwrap();
        assert_eq!(correlation.matched, 2);
        assert_eq!(
            correlation.readings.as_ref().unwrap().credited,
            8.0 * MIB as f64
        );
    }

    #[test]
    fn a_between_rule_that_names_no_value_is_refused_with_the_reason() {
        let mut rule = between_rule();
        rule.value = None;
        let error = check(&rule).unwrap_err();
        assert!(error.to_string().contains("value: bytes"), "{error}");
        assert_eq!(
            apply(&rule, &[]).unwrap_err(),
            error,
            "the engine re-checks"
        );
    }

    #[test]
    fn a_reading_without_the_named_value_fails_by_name() {
        let mut rule = between_rule();
        rule.value = Some("rss".to_owned());
        let error = apply(&rule, &[resident(0, 7, MIB)]).unwrap_err();
        assert!(
            matches!(error, JoinError::MissingValue { ref attr, .. } if attr == "rss"),
            "{error}"
        );
    }

    // -- evidence within a key, and a bare recycled pointer ----------------

    /// An `eq` join that replaced states chose among them by the clock, and
    /// says so beside the KEY it has for the association.
    #[test]
    fn an_equal_key_that_replaced_states_says_temporal_within_the_key() {
        let events = vec![
            threads(10, 7),
            descriptors(11, 7),
            threads(12, 7),
            descriptors(13, 7),
        ];
        let correlation = apply(&eq_rule(), &events).unwrap();
        assert_eq!(correlation.evidence, Evidence::Key);
        assert_eq!(correlation.within_key, Some(Evidence::Temporal));

        let once = apply(&eq_rule(), &[threads(10, 7), descriptors(11, 7)]).unwrap();
        assert_eq!(
            once.within_key, None,
            "nothing was replaced, so the key and the crediting are one claim"
        );
    }

    /// `scoped_eq` replaces states too, and chooses among them by the scope,
    /// which is what it is for: nothing about that is the clock's.
    #[test]
    fn a_scoped_match_is_not_temporal_within_its_key() {
        let events = vec![
            block(10, "io.rq.issue", 0xffff_0000, 10),
            block(20, "io.rq.issue", 0xffff_0000, 20),
            block(30, "io.rq.complete", 0xffff_0000, 20),
        ];
        let correlation = apply(&scoped_rule(), &events).unwrap();
        assert_eq!(correlation.states.replaced, 1);
        assert_eq!(correlation.within_key, None);
        assert_eq!(correlation.evidence, Evidence::Key);
    }

    /// A bare equality on the scoped key compares the pointer alone, and is
    /// rated for what that establishes.
    #[test]
    fn a_bare_equality_on_a_recycled_pointer_is_rated_temporal() {
        let mut rule = scoped_rule();
        rule.match_kind = MatchKind::Eq;
        let correlation = apply(&rule, &[block(10, "io.rq.issue", 0x10, 10)]).unwrap();
        assert_eq!(correlation.evidence, Evidence::Temporal);
        assert_eq!(
            correlation.within_key, None,
            "within-key never says more than the evidence already does"
        );
    }

    // -- wakeup chains -----------------------------------------------------

    /// A responder that releases a task many times, once for one event each,
    /// against a stranger that released it once before three events. Ranked
    /// by state, the stranger's single wakeup is the top row; ranked by chain,
    /// the responder is.
    #[test]
    fn wakeups_are_ranked_by_chain_and_not_one_by_one() {
        let mut events = vec![
            woken(1, 100, 55),
            switched(2, 100),
            switched(3, 100),
            switched(4, 100),
        ];
        for round in 0..10u64 {
            let at = 10 + round * 10;
            events.push(woken(at, 100, 99));
            events.push(switched(at + 1, 100));
        }
        let correlation = apply(&wakeup_rule(), &events).unwrap();
        let top = &correlation.top[0];
        assert_eq!(top.key, "task:99 -> task:100");
        assert_eq!((top.wakeups, top.matched), (Some(10), 10));
        assert_eq!(top.opened_ns, 10, "the chain spans from its first wakeup");
        assert_eq!(correlation.top[1].key, "task:55 -> task:100");
        assert_eq!(correlation.top[1].wakeups, Some(1));
        assert_eq!(correlation.total_attributed, correlation.matched);
        states_balance(&correlation);
    }

    /// A drained total stands for many blocks. A wakeup join onto one is
    /// refused on the first event that shows it, for a source whose catalog
    /// entry did not say so in advance.
    #[test]
    fn a_wakeup_join_onto_a_drained_total_is_refused_at_the_first_such_event() {
        let mut drained = switched(20, 100);
        drained.kind = "sched.offcpu".to_owned();
        drained.attrs = vec![
            Attr::uint("total_ns", 40_000_000),
            Attr::uint("blocks", 100),
            Attr::uint("window_ns", 100_000_000),
        ];
        let mut rule = wakeup_rule();
        rule.to = "sched.offcpu".to_owned();
        let error = apply(&rule, &[woken(10, 100, 99), drained]).unwrap_err();
        assert!(matches!(error, JoinError::DrainedTotal { .. }), "{error}");
        assert!(error.to_string().contains("sched.switches"), "{error}");
    }

    // -- refusals that need the session ------------------------------------

    fn request(source: &str, mode: &str) -> Request {
        let text = format!(
            "{{source: {source}, target: {{launch: [/bin/true]}}, mode: '{mode}', emit: [raw]}}"
        );
        serde_norway::from_str(&text).expect("a request")
    }

    fn catalog() -> Catalog {
        Catalog::embedded().expect("the embedded catalog")
    }

    #[test]
    fn between_onto_a_traced_source_is_refused_before_the_run() {
        let mut rule = between_rule();
        rule.to = "sched.offcpu".to_owned();
        let requests = [
            request("mem.heap.alloc", "trace"),
            request("sched.offcpu", "trace"),
        ];
        let error = check_in(&rule, &requests, &catalog()).unwrap_err();
        assert!(matches!(error, JoinError::NotAReading { .. }), "{error}");
        assert!(error.to_string().contains("poll"), "{error}");
    }

    #[test]
    fn between_onto_a_poll_must_say_what_a_reading_is() {
        let mut rule = between_rule();
        rule.reading = None;
        let requests = [
            request("mem.heap.alloc", "trace"),
            request("mem.rss", "poll@100ms"),
        ];
        let error = check_in(&rule, &requests, &catalog()).unwrap_err();
        assert!(error.to_string().contains("reading: level"), "{error}");

        rule.reading = Some(Reading::Level);
        check_in(&rule, &requests, &catalog()).expect("a stated level is accepted");
        let snapshot = [
            request("mem.heap.alloc", "trace"),
            request("mem.rss", "snapshot"),
        ];
        rule.reading = None;
        check_in(&rule, &snapshot, &catalog()).expect("one reading is a level");
    }

    #[test]
    fn a_wakeup_join_onto_a_source_the_catalog_says_is_drained_is_refused_before_the_run() {
        let mut rule = wakeup_rule();
        let requests = [
            request("sched.wakeup", "trace"),
            request("sched.offcpu", "trace"),
        ];
        rule.to = "sched.offcpu".to_owned();
        let error = check_in(&rule, &requests, &catalog()).unwrap_err();
        assert!(matches!(error, JoinError::DrainedTotal { .. }), "{error}");

        rule.to = "sched.switches".to_owned();
        let requests = [
            request("sched.wakeup", "trace"),
            request("sched.switches", "trace"),
        ];
        check_in(&rule, &requests, &catalog()).expect("one event per switch is joinable");
    }

    // -- the decomposition -------------------------------------------------

    fn decomposition(yaml: &str) -> Decomposition {
        serde_norway::from_str(yaml).expect("a decomposition")
    }

    fn result(source: &str, unit: Unit, observed_ns: u64) -> Results {
        serde_json::from_value(serde_json::json!({
            "label": source, "source": source, "unit": unit, "backend": "b", "outputs": [],
            "events_seen": 0, "observed_ns": observed_ns, "stopped_because": "done",
            "losses": crate::collect::Losses::default(),
        }))
        .expect("a result")
    }

    fn total(ts_ns: u64, kind: &str, tid: i32, attrs: Vec<Attr>) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: kind.to_owned(),
            keys: vec![KeyValue::task(tid)],
            attrs,
            collector_id: CollectorId::new(kind),
            seq: 0,
        }
    }

    /// Three kinds of number in one table, each read the way its request
    /// collects it, and a residue that keeps its sign.
    #[test]
    fn a_decomposition_reads_each_row_its_own_way_and_keeps_the_residue_signed() {
        let spec = decomposition(
            "{id: t, whole: window, parts: [\
               {state: on-cpu, from: cpu.profile, value: samples},\
               {state: blocked, from: sched.offcpu, value: total_ns}]}",
        );
        let requests = [
            request("cpu.profile", "sample@1000hz"),
            request("sched.offcpu", "trace"),
        ];
        let results = [
            result("cpu.profile", Unit::Count, 10_000_000),
            result("sched.offcpu", Unit::Ns, 9_000_000),
        ];
        let mut events: Vec<Event> = (0..4)
            .map(|i| total(i, "cpu.profile", 7, vec![Attr::uint("count", 1)]))
            .collect();
        events.push(total(
            5,
            "sched.offcpu",
            7,
            vec![Attr::uint("total_ns", 5_000_000)],
        ));
        events.push(total(
            6,
            "sched.offcpu",
            7,
            vec![Attr::uint("total_ns", 2_000_000)],
        ));

        let output = decompose(&spec, &requests, &results, &events);
        assert_eq!(output.unit, Unit::Ns);
        let table = &output.tables[0];
        assert_eq!(table.key, "task:7");
        assert_eq!(table.rows[0].kind, Kind::Sampled);
        assert_eq!(table.rows[0].amount, Some(4_000_000.0));
        assert_eq!(table.rows[0].resolution, Some(1_000_000.0));
        assert_eq!(table.rows[1].kind, Kind::Traced);
        assert_eq!(table.rows[1].amount, Some(7_000_000.0));
        assert_eq!(table.rows[1].resolution, Some(0.0));
        assert_eq!(table.whole.kind, Kind::Window);
        assert_eq!(table.whole.amount, Some(10_000_000.0));
        assert_eq!(table.residue, Some(-1_000_000.0), "negative: counted twice");
        assert_eq!(table.residue_resolution, Some(1_000_000.0));
    }

    /// The I/O crossing's table, declared: a part that is the distance
    /// between the two ends of a join's pairs, rated no better than the join.
    #[test]
    fn a_part_read_from_a_join_is_the_distance_its_pairs_span() {
        let spec = decomposition(
            "{id: one-read, whole: {from: io.syscall, value: total_ns}, parts: [\
               {state: device, from: io.latency, value: latency_ns},\
               {state: completion -> wakeup, from: completion-released, value: paired}]}",
        );
        let requests = [
            request("io.syscall", "trace"),
            request("io.latency", "trace"),
            request("sched.wakeup", "trace"),
        ];
        let joins = [woke_rule()];
        check_decomposition_in(&spec, &requests, &joins, &catalog()).expect("a valid table");
        let error = check_decomposition(&spec, &requests, &catalog()).unwrap_err();
        assert!(
            error.to_string().contains("without the session's joins"),
            "{error}"
        );

        let results = [
            result("io.syscall", Unit::Ns, 100),
            result("io.latency", Unit::Ns, 100),
            result("sched.wakeup", Unit::Count, 100),
        ];
        let events = vec![
            total(9, "io.latency", 7, vec![Attr::uint("latency_ns", 5)]),
            total(
                12,
                "sched.wakeup",
                7,
                vec![Attr::uint("wakee_tid", 7), Attr::uint("waker_tid", 3)],
            ),
            total(30, "io.latency", 7, vec![Attr::uint("latency_ns", 5)]),
            total(
                39,
                "sched.wakeup",
                7,
                vec![Attr::uint("wakee_tid", 7), Attr::uint("waker_tid", 3)],
            ),
            total(50, "io.syscall", 7, vec![Attr::uint("total_ns", 50)]),
        ];
        let output = decompose_with(&spec, &requests, &results, &events, &joins);
        let table = &output.tables[0];
        assert_eq!(table.key, "task:7");
        assert_eq!(table.rows[0].amount, Some(10.0));
        let paired = &table.rows[1];
        assert_eq!(paired.amount, Some(3.0 + 9.0), "12 - 9 and 39 - 30");
        assert_eq!(paired.readings, 2);
        assert_eq!(
            paired.evidence,
            Some(Evidence::Wakeup),
            "a pair is no firmer than its join"
        );
        assert_eq!(paired.source, "completion-released");
        assert!(
            paired.note.as_deref().unwrap_or("").contains("2 pairs"),
            "{paired:?}"
        );
        assert_eq!(table.residue, Some(50.0 - 10.0 - 12.0));

        // Without the joins, the row says why it could not be read, and the
        // rest of the table is still a table.
        let output = decompose(&spec, &requests, &results, &events);
        let row = &output.tables[0].rows[1];
        assert_eq!(row.amount, None);
        assert!(
            row.missing
                .as_deref()
                .unwrap_or("")
                .contains("names no join"),
            "{row:?}"
        );
        assert_eq!(output.tables[0].rows[0].amount, Some(10.0));
    }

    #[test]
    fn a_part_read_from_a_join_whose_pairs_span_nothing_is_refused_with_the_reason() {
        let requests = [
            request("io.syscall", "trace"),
            request("io.latency", "trace"),
            request("sched.wakeup", "trace"),
        ];
        let spec = decomposition(
            "{id: t, whole: {from: io.syscall, value: total_ns}, parts: [\
               {state: x, from: released, value: paired}]}",
        );
        let joins = [Rule {
            id: "released".to_owned(),
            from: "sched.wakeup".to_owned(),
            to: "io.latency".to_owned(),
            ..wakeup_rule()
        }];
        let error = check_decomposition_in(&spec, &requests, &joins, &catalog()).unwrap_err();
        assert!(error.to_string().contains("only `woke`"), "{error}");
        let error = check_decomposition_in(&spec, &requests, &[woke_rule()], &catalog())
            .unwrap_err()
            .to_string();
        assert!(error.contains("declares `completion-released`"), "{error}");
    }

    /// The memory crossing's decomposition: a level's change from first
    /// reading to last, whole and parts off one event at each end.
    #[test]
    fn a_level_is_read_as_its_last_reading_less_its_first() {
        let spec = decomposition(
            "{id: m, whole: {from: mem.heap.retained, value: rss, reading: level}, parts: [\
               {state: live, from: mem.heap.retained, value: live, reading: level},\
               {state: retained, from: mem.heap.retained, value: bytes, reading: level}]}",
        );
        let requests = [request("mem.heap.retained", "poll@100ms")];
        let results = [result("mem.heap.retained", Unit::Bytes, 1)];
        let reading = |ts, rss, live, bytes| {
            total(
                ts,
                "mem.heap.retained",
                7,
                vec![
                    Attr::uint("rss", rss),
                    Attr::uint("live", live),
                    Attr::uint("bytes", bytes),
                ],
            )
        };
        let events = vec![
            reading(0, 8 * MIB, 0, 0),
            reading(1, 40 * MIB, 32 * MIB, 0),
            reading(2, 72 * MIB, 48 * MIB, 15 * MIB),
        ];
        let table = &decompose(&spec, &requests, &results, &events).tables[0];
        assert_eq!(table.whole.amount, Some(64.0 * MIB as f64));
        assert_eq!(table.rows[0].amount, Some(48.0 * MIB as f64));
        assert_eq!(table.rows[1].amount, Some(15.0 * MIB as f64));
        assert_eq!(table.residue, Some(MIB as f64));
        assert_eq!(
            table.residue_resolution, None,
            "a level of bytes has no resolution in bytes to state"
        );
    }

    /// A row whose attribute no event carried is missing, not zero, and a
    /// missing row takes the residue with it.
    #[test]
    fn a_row_no_event_could_supply_is_missing_and_so_is_the_residue() {
        let spec = decomposition(
            "{id: t, whole: window, parts: [\
               {state: blocked, from: sched.offcpu, value: total_ns, less: queued_ns}]}",
        );
        let requests = [request("sched.offcpu", "trace")];
        let results = [result("sched.offcpu", Unit::Ns, 9)];
        let events = vec![total(5, "sched.offcpu", 7, vec![Attr::uint("total_ns", 5)])];
        let table = &decompose(&spec, &requests, &results, &events).tables[0];
        assert_eq!(table.rows[0].amount, None);
        assert!(
            table.rows[0]
                .missing
                .as_deref()
                .unwrap_or_default()
                .contains("queued_ns"),
            "{:?}",
            table.rows[0]
        );
        assert_eq!(table.residue, None);
    }

    fn refused(spec: &str, requests: &[Request]) -> String {
        check_decomposition(&decomposition(spec), requests, &catalog())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn a_decomposition_that_cannot_mean_anything_is_refused_with_the_reason() {
        let traced = [
            request("sched.offcpu", "trace"),
            request("sched.runqlat", "trace"),
        ];

        let missing = refused(
            "{id: t, whole: window, parts: [{state: s, from: cpu.profile, value: samples}]}",
            &traced,
        );
        assert!(missing.contains("does not collect"), "{missing}");

        let unsampled = refused(
            "{id: t, whole: window, parts: [{state: s, from: sched.offcpu, value: samples}]}",
            &traced,
        );
        assert!(unsampled.contains("no period"), "{unsampled}");

        let mut elsewhere = request("sched.runqlat", "trace");
        elsewhere.target = Target::Pid(vec![1]);
        let other_target = refused(
            "{id: t, whole: window, parts: [\
               {state: a, from: sched.offcpu, value: total_ns},\
               {state: b, from: sched.runqlat, value: total_ns}]}",
            &[request("sched.offcpu", "trace"), elsewhere],
        );
        assert!(other_target.contains("different program"), "{other_target}");

        let units = refused(
            "{id: t, whole: window, parts: [{state: a, from: mem.rss, value: bytes, reading: level}]}",
            &[request("mem.rss", "poll@100ms")],
        );
        assert!(units.contains("quantity of nothing"), "{units}");

        let unstated = refused(
            "{id: t, whole: {from: mem.rss, value: bytes}, parts: [\
               {state: a, from: mem.rss, value: bytes, reading: level}]}",
            &[request("mem.rss", "poll@100ms")],
        );
        assert!(unstated.contains("reading: level"), "{unstated}");

        check_decomposition(
            &decomposition(
                "{id: t, whole: window, parts: [\
                   {state: a, from: sched.offcpu, value: total_ns},\
                   {state: b, from: sched.runqlat, value: total_ns}]}",
            ),
            &traced,
            &catalog(),
        )
        .expect("two traced durations of one target decompose a window");
    }

    #[test]
    fn a_decomposition_survives_a_round_trip_through_every_session_format() {
        let spec = decomposition(
            "{id: m, whole: {from: mem.heap.retained, value: rss, reading: level}, parts: [\
               {state: live, from: mem.heap.retained, value: live, reading: level}]}",
        );
        let window = decomposition(
            "{id: w, whole: window, parts: [{state: a, from: sched.offcpu, value: total_ns}]}",
        );
        for spec in [spec, window] {
            let json = serde_json::to_string(&spec).unwrap();
            assert_eq!(serde_json::from_str::<Decomposition>(&json).unwrap(), spec);
            let yaml = serde_norway::to_string(&spec).unwrap();
            assert_eq!(
                serde_norway::from_str::<Decomposition>(&yaml).unwrap(),
                spec
            );
        }
    }

    // -- joins across a window ---------------------------------------------

    /// A span of the run, from `opened_ns` to `closed_ns`, in a run that
    /// opened at zero.
    fn watched(opened_ns: u64, closed_ns: u64) -> Watched {
        Watched {
            run_opened_ns: 0,
            opened_ns,
            closed_ns,
            attached: true,
            attach_ns: None,
        }
    }

    /// The run model of `tests/runner.rs`'s scenario, with its numbers: forty
    /// sleeps while `sched.wakeup` is attached, the detach, twenty-five after
    /// it. Before this, the last wakeup stayed open and took every block after
    /// the detach -- 65 matched, 0 unmatched, one chain `still open`.
    #[test]
    fn a_join_across_a_detached_source_counts_what_fell_after_it_apart() {
        let mut events = Vec::new();
        for i in 0..40u64 {
            events.push(woken(1_000 + i * 10, 100, 99));
            events.push(switched(1_005 + i * 10, 100));
        }
        for i in 0..25u64 {
            events.push(switched(5_000 + i * 10, 100));
        }
        let windows = BTreeMap::from([
            ("sched.wakeup".to_owned(), watched(0, 2_000)),
            ("sched.switches".to_owned(), watched(0, 9_000)),
        ]);

        let whole = apply(&wakeup_rule(), &events).unwrap();
        assert_eq!(
            (whole.matched, whole.outside_window),
            (65, 0),
            "the defect, kept as the contrast"
        );

        let within = apply_within(&wakeup_rule(), &events, &windows).unwrap();
        assert_eq!(within.matched, 40);
        assert_eq!(within.unmatched, 0);
        assert_eq!(within.outside_window, 25);
        assert_eq!(within.states.window_closed, 1, "{:?}", within.states);
        assert_eq!(within.states.still_open, 0);
        assert_eq!(within.states.expired, 0, "a detach is not an expiry");
        assert_eq!(within.top[0].outcome, Outcome::WindowClosed);
        assert_eq!(within.top[0].closed_ns, Some(2_000));
        states_balance(&within);
    }

    /// A downstream event before the `from` source was attached is outside its
    /// window too, and a state open at a detach with nothing after it still
    /// ends there when the run went on past it.
    #[test]
    fn a_window_that_opens_late_and_closes_early_bounds_both_ends() {
        let events = vec![
            switched(100, 7),
            woken(1_000, 7, 3),
            switched(1_100, 7),
            woken(1_500, 8, 3),
        ];
        let windows = BTreeMap::from([
            ("sched.wakeup".to_owned(), watched(900, 2_000)),
            ("sched.switches".to_owned(), watched(0, 3_000)),
        ]);
        let correlation = apply_within(&wakeup_rule(), &events, &windows).unwrap();
        assert_eq!(
            (
                correlation.matched,
                correlation.unmatched,
                correlation.outside_window
            ),
            (1, 0, 1)
        );
        assert_eq!(correlation.states.window_closed, 2, "both tasks' wakeups");
        states_balance(&correlation);

        // The same stream, the source watching until the run's end: nothing
        // was detached, so what is open at the end is open at the end.
        let windows = BTreeMap::from([
            ("sched.wakeup".to_owned(), watched(900, 3_000)),
            ("sched.switches".to_owned(), watched(0, 3_000)),
        ]);
        let correlation = apply_within(&wakeup_rule(), &events, &windows).unwrap();
        assert_eq!(correlation.states.window_closed, 0);
        assert_eq!(correlation.states.still_open, 2);
    }

    /// The other matches keep the lifecycle across a detach: an `eq` state
    /// open at the detach does not take what follows, and a range opened
    /// before it cannot be freed after it, so matches nothing more.
    #[test]
    fn every_match_kind_ends_its_states_at_the_detach() {
        let rule = eq_rule();
        let events = vec![threads(10, 7), descriptors(20, 7), descriptors(40, 7)];
        let windows = BTreeMap::from([(rule.from.clone(), watched(0, 30))]);
        let keyed = apply_within(&rule, &events, &windows).unwrap();
        assert_eq!((keyed.matched, keyed.outside_window), (1, 1));
        assert_eq!(keyed.states.window_closed, 1);
        states_balance(&keyed);

        let rule = self::rule();
        let events = vec![
            allocation(10, 0x1000, 0x100, "alloc"),
            fault(20, 0x1010),
            fault(40, 0x1020),
        ];
        let windows = BTreeMap::from([(rule.from.clone(), watched(0, 30))]);
        let ranged = apply_within(&rule, &events, &windows).unwrap();
        assert_eq!((ranged.matched, ranged.outside_window), (1, 1));
        assert_eq!(ranged.states.window_closed, 1);
        states_balance(&ranged);
    }

    /// Two requests of the `from` source that watch different spans cannot
    /// be one window, and the rule is refused by name before the run.
    #[test]
    fn a_join_from_a_source_collected_twice_with_different_windows_is_refused() {
        let mut early = request("sched.wakeup", "trace");
        early.window = Some("..1s".parse().unwrap());
        let late = request("sched.wakeup", "trace");
        let requests = [early.clone(), late, request("sched.switches", "trace")];
        let error = check_in(&wakeup_rule(), &requests, &catalog()).unwrap_err();
        assert!(
            matches!(error, JoinError::WindowsDisagree { .. }),
            "{error}"
        );
        let message = error.to_string();
        assert!(
            message.contains("..1s") && message.contains("the whole run"),
            "{message}"
        );

        let requests = [early.clone(), early, request("sched.switches", "trace")];
        check_in(&wakeup_rule(), &requests, &catalog()).expect("one window, said twice");
    }

    // -- the decomposition: the key a row was read through, and the zeroes --

    fn watched_result(source: &str, unit: Unit, window: Watched) -> Results {
        let mut result = result(source, unit, window.span_ns());
        result.window = window;
        result
    }

    fn by_state<'a>(table: &'a Breakdown, state: &str) -> &'a Row {
        table
            .rows
            .iter()
            .find(|row| row.state == state)
            .unwrap_or_else(|| panic!("no row `{state}`: {table:?}"))
    }

    /// A row carried through the socket to the request's id is worth the
    /// socket, not the id: KEY under `by: req`, where a row read off the
    /// serving thread is IDENTITY.
    #[test]
    fn a_row_read_through_another_key_is_rated_by_the_weaker() {
        let spec = decomposition(
            "{id: r, by: req, whole: {from: net.request, value: span_ns}, parts: [\
               {state: cpu_on, from: net.request, value: oncpu_ns},\
               {state: net_rx, from: net.request, value: net_rx_ns, via: sock}]}",
        );
        let requests = [request("net.request", "trace")];
        check_decomposition(&spec, &requests, &catalog()).expect("net.request carries sock");
        let results = [watched_result("net.request", Unit::Ns, watched(0, 100))];
        let events = vec![Event {
            keys: vec![KeyValue::Req {
                id: "a001".to_owned(),
            }],
            ..total(
                5,
                "net.request",
                7,
                vec![
                    Attr::uint("span_ns", 30),
                    Attr::uint("oncpu_ns", 10),
                    Attr::uint("net_rx_ns", 20),
                ],
            )
        }];
        let table = &decompose(&spec, &requests, &results, &events).tables[0];
        assert_eq!(by_state(table, "cpu_on").evidence, Some(Evidence::Identity));
        assert_eq!(by_state(table, "net_rx").evidence, Some(Evidence::Key));
        assert_eq!(table.residue, Some(0.0), "rating a row changes no amount");

        let refused = refused(
            "{id: r, by: req, whole: window, parts: [\
               {state: x, from: net.request, value: net_rx_ns, via: blk}]}",
            &requests,
        );
        assert!(refused.contains("through `blk`"), "{refused}");
    }

    /// The source ran, lost nothing and had events for the key, and none of
    /// them passed the part's filter: a measured zero, and the row says how it
    /// knows. Before, the filter emptied the source and every row, parts and
    /// residue read `not read`.
    #[test]
    fn a_filtered_part_that_matched_nothing_is_a_zero() {
        let spec = decomposition(
            "{id: t, whole: window, parts: [\
               {state: disk, from: sched.offcpu, value: total_ns, \
                filter: \"state == 'uninterruptible'\"},\
               {state: sleep, from: sched.offcpu, value: total_ns, \
                filter: \"state == 'sleeping'\"}]}",
        );
        let requests = [request("sched.offcpu", "trace")];
        let results = [watched_result("sched.offcpu", Unit::Ns, watched(0, 100))];
        let events = vec![total(
            5,
            "sched.offcpu",
            7,
            vec![Attr::uint("total_ns", 40), Attr::text("state", "sleeping")],
        )];
        let table = &decompose(&spec, &requests, &results, &events).tables[0];
        let disk = by_state(table, "disk");
        assert_eq!(disk.amount, Some(0.0), "{disk:?}");
        assert_eq!(disk.missing, None);
        assert!(
            disk.note.as_deref().unwrap_or_default().contains("filter"),
            "{disk:?}"
        );
        assert_eq!(by_state(table, "sleep").amount, Some(40.0));
        assert_eq!(table.parts, Some(40.0));
        assert_eq!(table.residue, Some(60.0));
    }

    /// The decision for a key the source saw nothing of at all: a zero from a
    /// source that watched the whole run and lost nothing, and `not read` from
    /// one that lost events, watched part of the run, or cannot say when it
    /// watched -- each with its reason.
    #[test]
    fn a_key_a_source_never_saw_is_a_zero_only_from_a_clean_source_that_watched_it_all() {
        let spec = decomposition(
            "{id: t, whole: window, parts: [\
               {state: on-cpu, from: cpu.profile, value: samples},\
               {state: blocked, from: sched.offcpu, value: total_ns}]}",
        );
        let requests = [
            request("cpu.profile", "sample@1000hz"),
            request("sched.offcpu", "trace"),
        ];
        // Task 8 ran and never blocked; task 7 blocked, so the source is
        // known to carry `total_ns`.
        let events = vec![
            total(1, "cpu.profile", 8, vec![Attr::uint("count", 1)]),
            total(2, "sched.offcpu", 7, vec![Attr::uint("total_ns", 5)]),
        ];
        let blocked_of_8 = |offcpu: Results| {
            let results = [
                watched_result("cpu.profile", Unit::Count, watched(0, 100)),
                offcpu,
            ];
            let output = decompose(&spec, &requests, &results, &events);
            let table = output
                .tables
                .iter()
                .find(|table| table.key == "task:8")
                .expect("task 8 has a table")
                .clone();
            by_state(&table, "blocked").clone()
        };

        let clean = blocked_of_8(watched_result("sched.offcpu", Unit::Ns, watched(0, 100)));
        assert_eq!(clean.amount, Some(0.0), "{clean:?}");
        assert!(
            clean
                .note
                .as_deref()
                .unwrap_or_default()
                .contains("whole run"),
            "{clean:?}"
        );

        let mut lossy = watched_result("sched.offcpu", Unit::Ns, watched(0, 100));
        lossy.losses.dropped = 3;
        let lossy = blocked_of_8(lossy);
        assert_eq!(lossy.amount, None);
        assert!(
            lossy
                .missing
                .as_deref()
                .unwrap_or_default()
                .contains("lost"),
            "{lossy:?}"
        );

        let narrowed = blocked_of_8(watched_result("sched.offcpu", Unit::Ns, watched(0, 60)));
        assert_eq!(narrowed.amount, None);
        assert!(
            narrowed
                .missing
                .as_deref()
                .unwrap_or_default()
                .contains("part of the run"),
            "{narrowed:?}"
        );

        let unknown = blocked_of_8(result("sched.offcpu", Unit::Ns, 100));
        assert_eq!(unknown.amount, None, "unknown never counts as available");
    }
}
