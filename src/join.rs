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
    ///
    /// Absent only for `overlap`, over state the machine holds once and no key
    /// names: `mem.pressure` read for the whole machine is one series, and a
    /// window of any task can be laid across its intervals. Every `from`
    /// reading is then one series and every `to` event is overlapped against
    /// it; a `from` source that turns out to carry several series is refused by
    /// name rather than read as one. Every other match needs a key to match
    /// on, and [`check`] refuses one without.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<KeyKind>,
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
    /// The attribute of a `to` row that stands for several occurrences --
    /// a drained bucket -- holding the length of a window, from the row's
    /// stamp and in the direction `stamped` says, that encloses every one of
    /// them, for an `overlap` match.
    ///
    /// `span` on such a row is a sum of durations that may lie anywhere in
    /// the drain's interval, so by default the row has no window to place and
    /// its amount is uncredited. Naming `enclosing` says which attribute is a
    /// window rather than a sum: a row whose enclosing window lies inside one
    /// interval is credited whole to it -- exactly, since every occurrence is
    /// inside -- and one whose window crosses an interval's end stays
    /// uncredited, in `unmatched` and `Readings::uncredited`, because how its
    /// amount divides between the intervals is not known. A row of one
    /// occurrence still uses `span`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosing: Option<String>,
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
    /// Where the rule is applied: `agent` (the default) as the run ends,
    /// `query` afterwards over the run's stored events ([`apply_stored`]), or
    /// `kernel`, which no match kind serves yet and [`check`] refuses with the
    /// reason. The rule is the same at every tier and so are the collectors:
    /// moving a join between tiers is this one line.
    #[serde(default, skip_serializing_if = "Tier::is_agent")]
    pub tier: Tier,
}

/// Where a join is applied (`docs/ROADMAP.md` phase 5, "join placement": decided
/// per rule, not globally).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// In this process, over the events the run collected, when it ends.
    #[default]
    Agent,
    /// Over a stored run's `raw` events, after it: [`apply_stored`]. The run
    /// has to keep every event of both sides, which [`check_in`] holds it to.
    Query,
    /// In the kernel, as an eBPF program per match kind. None exists: a
    /// collector that pairs its events in the kernel (`io.latency`'s issue
    /// and completion, `sched.offcpu`'s switch out and in) does so as its own
    /// code, not as a rule's.
    Kernel,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Agent => "agent",
            Tier::Query => "query",
            Tier::Kernel => "kernel",
        }
    }

    fn is_agent(&self) -> bool {
        *self == Tier::Agent
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    #[error(
        "rule `{id}` matches by `{match_kind}` and names no key: every match but `overlap` \
         pairs events that share one, so say which with `via`, as in `via: task`; a rule over \
         state the machine holds once, with no key, matches by `overlap`"
    )]
    NoKey { id: String, match_kind: MatchKind },
    #[error(
        "rule `{id}` names no key, so it reads every `{from}` reading as one series, and \
         `{from}` carried {series} of them ({sample}): the interval since the reading before \
         would be the gap between two series, not a poll. Name the key they differ by with \
         `via`, or read `{from}` for one of them"
    )]
    SeveralSeries {
        id: String,
        from: String,
        series: usize,
        sample: String,
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
        "rule `{id}` names `enclosing`, which only an `overlap` match reads: a `{match_kind}` \
         match places no window, and a field it would ignore is refused rather than dropped"
    )]
    EnclosingOutsideOverlap { id: String, match_kind: MatchKind },
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
    #[error(
        "rule `{id}` asks for the kernel tier, and no match kind has a program of `src/bpf/` \
         that applies a rule in the kernel: a collector that pairs its events there does it as \
         its own code. Apply it with `tier: agent` (the default), as the run ends, or `tier: \
         query`, over the run's stored events"
    )]
    KernelTier { id: String },
    #[error(
        "rule `{id}` is applied at the query tier, over the stored run, and `{side}` {reason}"
    )]
    NotStored {
        id: String,
        side: String,
        reason: String,
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
    /// The open states by age, oldest first: what expiry and eviction walk,
    /// so neither costs a pass over every open state on every event. Holds
    /// exactly the keys of `open`, which only this impl adds to or removes
    /// from.
    ages: std::collections::BTreeSet<(u64, K)>,
    stats: StateStats,
    /// What the states that ended attributed, ranked as they end and held to
    /// the rule's `top` rather than kept whole: the record of every state of
    /// a run was most of the engine's memory, and linear in the run.
    finished: Ranked,
    /// Whether a state that matched nothing is kept once it ends. Only a
    /// `wakeup` match reads one -- its chain counts every wakeup, matched or
    /// not -- and every other match drops it when it ranks, so keeping it
    /// would hold a record of every state of the run until the end for
    /// nothing: under load, most of the engine's memory.
    keep_unmatched: bool,
    capacity: usize,
    window_ns: Option<u64>,
}

impl<K: Ord + Clone> States<K> {
    fn new(rule: &Rule) -> Self {
        Self {
            open: BTreeMap::new(),
            ages: std::collections::BTreeSet::new(),
            stats: StateStats::default(),
            finished: Ranked::new(rule),
            keep_unmatched: rule.match_kind == MatchKind::Wakeup,
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
        if let Some(previous) = self.remove(&key) {
            self.stats.closed += 1;
            self.stats.replaced += 1;
            let at = state.opened_ns;
            self.end(previous, Some(at), Outcome::Closed);
        } else if self.open.len() >= self.capacity {
            self.evict_oldest();
        }
        self.ages.insert((state.opened_ns, key.clone()));
        self.open.insert(key, state);
        self.stats.opened += 1;
        self.stats.peak_open = self.stats.peak_open.max(self.open.len() as u64);
    }

    /// Records how a state ended, when anything will read it.
    fn end(&mut self, state: OpenState, closed_ns: Option<u64>, outcome: Outcome) {
        if state.matched > 0 || self.keep_unmatched {
            finish(&mut self.finished, state, closed_ns, outcome);
        }
    }

    /// Takes a state out of the map and out of the age index together.
    fn remove(&mut self, key: &K) -> Option<OpenState> {
        let state = self.open.remove(key)?;
        self.ages.remove(&(state.opened_ns, key.clone()));
        Some(state)
    }

    fn close(&mut self, key: &K, closed_ns: u64) {
        if let Some(state) = self.remove(key) {
            self.stats.closed += 1;
            self.end(state, Some(closed_ns), Outcome::Closed);
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
        // The oldest first, and only as far as the cutoff: on most events
        // nothing has outlived the window and this looks at one entry. Ended
        // in key order, as they always were, so the attributions come out in
        // the same order whatever the index.
        let mut stale: Vec<K> = self
            .ages
            .iter()
            .take_while(|(opened_ns, _)| *opened_ns < cutoff)
            .map(|(_, key)| key.clone())
            .collect();
        stale.sort();
        for key in stale {
            if let Some(state) = self.remove(&key) {
                self.stats.expired += 1;
                self.end(state, None, Outcome::Expired);
            }
        }
    }

    /// Ends every open state at the instant the `from` source was detached.
    fn window_closed(&mut self, closed_ns: u64) {
        self.stats.window_closed += self.open.len() as u64;
        self.ages.clear();
        for (_, state) in std::mem::take(&mut self.open) {
            self.end(state, Some(closed_ns), Outcome::WindowClosed);
        }
    }

    fn evict_oldest(&mut self) {
        // The first by age is the oldest, and among states opened at one
        // instant the least key, which is the one a scan of the map found.
        let oldest = self.ages.first().map(|(_, key)| key.clone());
        if let Some(key) = oldest {
            if let Some(state) = self.remove(&key) {
                self.stats.overflowed += 1;
                self.end(state, None, Outcome::Overflowed);
            }
        }
    }

    fn finish(mut self) -> (StateStats, Ranked) {
        self.stats.still_open = self.open.len() as u64;
        self.ages.clear();
        for (_, state) in std::mem::take(&mut self.open) {
            self.end(state, None, Outcome::StillOpen);
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
        let taken: Vec<OpenState> = ids.iter().filter_map(|id| self.remove(id)).collect();
        self.stats.closed += taken.len() as u64;
        taken
    }
}

fn finish(finished: &mut Ranked, state: OpenState, closed_ns: Option<u64>, outcome: Outcome) {
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
    if rule.tier == Tier::Kernel {
        return Err(JoinError::KernelTier {
            id: rule.id.clone(),
        });
    }
    if rule.enclosing.is_some() && rule.match_kind != MatchKind::Overlap {
        return Err(JoinError::EnclosingOutsideOverlap {
            id: rule.id.clone(),
            match_kind: rule.match_kind,
        });
    }
    let Some(via) = rule.via else {
        return match rule.match_kind {
            MatchKind::Overlap if rule.value.is_none() => Err(JoinError::NoShareValue {
                id: rule.id.clone(),
                to: rule.to.clone(),
            }),
            MatchKind::Overlap => Ok(()),
            match_kind => Err(JoinError::NoKey {
                id: rule.id.clone(),
                match_kind,
            }),
        };
    };
    let unmatchable = |reason: &str| JoinError::UnmatchableKey {
        id: rule.id.clone(),
        match_kind: rule.match_kind,
        via,
        reason: reason.to_owned(),
    };

    match rule.match_kind {
        MatchKind::Interval if via != KeyKind::Mem => Err(unmatchable(
            "an interval match needs a range to fall inside, and `mem:[base,len)` is the only key \
             that is one",
        )),
        MatchKind::ScopedEq if !is_scoped(via) => Err(unmatchable(
            "a scoped match needs a key that says which life of an identity it names, written \
             `<id>@<scope>`; `blk:rq@open_ts` is the only one today, and on a key without a scope \
             `scoped_eq` is `eq` under a name that promises more",
        )),
        MatchKind::Wakeup if via != KeyKind::Task => Err(unmatchable(
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
        MatchKind::Woke if via != KeyKind::Task => Err(unmatchable(
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
    if rule.tier == Tier::Query {
        stored_in_full(rule, requests)?;
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
    apply_merging(rule, &[events], windows)
}

/// Whether a run keeps everything a query-tier rule will read: every event of
/// both sides, for the whole run.
///
/// A side the session does not collect, or collects without `raw`, leaves
/// nothing in the stored run to join. A request with a `window` is not
/// refused: its stored result says when it watched and, in its
/// `stopped_because`, whether it stopped at a detach ([`apply_stored`]).
fn stored_in_full(rule: &Rule, requests: &[Request]) -> Result<(), JoinError> {
    let refuse = |source: &str, reason: &str| JoinError::NotStored {
        id: rule.id.clone(),
        side: source.to_owned(),
        reason: reason.to_owned(),
    };
    for source in [&rule.from, &rule.to] {
        let collected: Vec<&Request> = requests
            .iter()
            .filter(|request| request.source.as_str() == source.as_str())
            .collect();
        if collected.is_empty() {
            return Err(refuse(
                source,
                "is not collected by this session, so the stored run will hold none of its events",
            ));
        }
        for request in collected {
            if !request
                .emit
                .iter()
                .any(|emit| emit.kind() == crate::request::EmitKind::Raw)
            {
                return Err(refuse(
                    source,
                    "is collected without `raw`, so the stored run will hold what its events \
                     summed to and none of the events: add `raw` to its `emit`",
                ));
            }
        }
    }
    Ok(())
}

/// The sources a session's joins and decompositions read events of: the only
/// collections whose events a run needs to keep beside its results once each
/// has been aggregated. A run with no join and no decomposition reads none.
pub fn sources_read(joins: &[Rule], decompositions: &[Decomposition]) -> BTreeSet<String> {
    let mut read = BTreeSet::new();
    for rule in joins.iter().filter(|rule| rule.tier == Tier::Agent) {
        read.insert(rule.from.clone());
        read.insert(rule.to.clone());
    }
    for decomposition in decompositions {
        if let Whole::Measured(measure) = &decomposition.whole {
            read.insert(measure.from.clone());
        }
        for part in &decomposition.parts {
            read.insert(part.from.clone());
            // A part joined by a rule (`value: paired`) reads that rule's
            // sides, whatever tier the rule itself was applied at.
            if part.value == PAIRED {
                if let Ok(rule) = paired_rule(decomposition, part, joins) {
                    read.insert(rule.from.clone());
                    read.insert(rule.to.clone());
                }
            }
        }
    }
    read
}

/// A rule applied at the query tier: over a stored run's results, after the
/// run, with the engine the agent tier uses.
///
/// Each side's events are its results' `raw` output, which holds every event
/// the collection had -- what a run's joins walk -- and they are walked in
/// the order the results are stored, which is the order of the run's
/// collections. Each source's window is the union of its results' (as the
/// run merges two requests of one source), closed where its collection was
/// detached when its `stopped_because` says it was (a window that closed, a
/// budget, the escalation), and watching to the end of the run otherwise:
/// the agent tier's own reading, from what the stored run says.
pub fn apply_stored(rule: &Rule, results: &[Results]) -> Result<Correlation, JoinError> {
    check(rule)?;
    let mut streams: Vec<&[Event]> = Vec::new();
    let mut windows: BTreeMap<String, Watched> = BTreeMap::new();
    for source in [&rule.from, &rule.to] {
        let stored: Vec<&Results> = results
            .iter()
            .filter(|result| result.source == *source)
            .collect();
        if stored.is_empty() {
            return Err(JoinError::NotStored {
                id: rule.id.clone(),
                side: source.clone(),
                reason: "has no result in the stored run".to_owned(),
            });
        }
        for result in &stored {
            let raw = result.outputs.iter().find_map(|output| match output {
                crate::emit::Output::Raw { events } => Some(events),
                _ => None,
            });
            if raw.is_none() {
                return Err(JoinError::NotStored {
                    id: rule.id.clone(),
                    side: source.clone(),
                    reason: format!(
                        "was stored without `raw` (result `{}`), so none of its events is there \
                         to join",
                        result.label
                    ),
                });
            }
        }
        // The run's own union (`assemble` in `src/main.rs`): an attached
        // window over one never attached, the widest of two attached.
        let mut watched = stored[0].window;
        for result in &stored[1..] {
            match (watched.attached, result.window.attached) {
                (_, false) => {}
                (false, true) => watched = result.window,
                (true, true) => {
                    watched.opened_ns = watched.opened_ns.min(result.window.opened_ns);
                    watched.closed_ns = watched.closed_ns.max(result.window.closed_ns);
                }
            }
        }
        // As the run does, a source that stopped with the run watched to its
        // end, and one detached before it ended its states at the detach. The
        // stored result says which in its `stopped_because`.
        let detached = stored
            .iter()
            .any(|result| crate::collect::stopped_at_a_detach(&result.stopped_because));
        if watched.attached && !detached {
            watched.closed_ns = u64::MAX;
        }
        windows.insert(source.clone(), watched);
    }
    // Every stream of either side, in the stored order: one source's two
    // results stay two streams, as two collections of a run do.
    for result in results
        .iter()
        .filter(|result| result.source == rule.from || result.source == rule.to)
    {
        for output in &result.outputs {
            if let crate::emit::Output::Raw { events } = output {
                streams.push(events);
            }
        }
    }
    let mut correlation = apply_merging(rule, &streams, &windows)?;
    correlation.rule = rule.id.clone();
    Ok(correlation)
}

/// `apply_within` over several streams -- one per collection -- merged by the
/// engine as it walks them, rather than over one stream made by concatenating
/// them.
///
/// Two streams merge here, and at an equal timestamp the order decides the
/// answer: an allocation has to be open before a fault inside it can be
/// attributed, and a wakeup before the work it released. Sorting the opening
/// side first at each instant is the only ordering that cannot invent an
/// unmatched event; at an equal instant and side, the earlier stream and then
/// the earlier event come first, which is the order a stable sort of the
/// streams laid end to end gives.
///
/// A stream already in that order is walked where it lies; one that is not is
/// walked through an index of references to it, eight bytes an event, which is
/// the one cost of the engine that grows with the run once its attributions
/// are bounded (`Ranked`). The streams of a run are usually each in order and
/// their concatenation is not, so handing them here apart is what keeps a
/// run's joins from taking that index over the whole run. `woke` takes the
/// index regardless, because the pairs it hands a decomposition borrow the
/// events they pair.
pub fn apply_merging(
    rule: &Rule,
    streams: &[&[Event]],
    windows: &BTreeMap<String, Watched>,
) -> Result<Correlation, JoinError> {
    check(rule)?;
    let mut watch = Watch::of(rule, windows);
    let order = |event: &Event| (event.ts_ns, u8::from(event.kind != rule.from));

    // The streams out of order, indexed; the others walked where they lie.
    let indexed: Vec<Option<Vec<&Event>>> = streams
        .iter()
        .map(|events| {
            let in_order = events
                .windows(2)
                .all(|pair| order(&pair[0]) <= order(&pair[1]));
            (!in_order).then(|| {
                let mut ordered: Vec<&Event> = events.iter().collect();
                ordered.sort_by_key(|event| order(event));
                ordered
            })
        })
        .collect();
    let cursors: Vec<Cursor<'_>> = streams
        .iter()
        .zip(&indexed)
        .map(|(events, index)| match index {
            Some(index) => Cursor::Refs(index),
            None => Cursor::Events(events),
        })
        .collect();
    let merged = Merged {
        cursors,
        from: &rule.from,
    };

    let mut correlation = match rule.match_kind {
        MatchKind::Woke => {
            let ordered: Vec<&Event> = merged.collect();
            woke(rule, &ordered, &mut watch).map(|(correlation, _)| correlation)
        }
        match_kind => walk(match_kind, rule, merged, &mut watch),
    }?;
    correlation.outside_window = watch.outside;
    Ok(correlation)
}

/// What is left of one stream to walk, in the engine's order.
#[derive(Clone, Copy)]
enum Cursor<'a> {
    Events(&'a [Event]),
    Refs(&'a [&'a Event]),
}

impl<'a> Cursor<'a> {
    fn head(&self) -> Option<&'a Event> {
        match self {
            Cursor::Events(events) => events.first(),
            Cursor::Refs(refs) => refs.first().copied(),
        }
    }

    fn advance(&mut self) {
        match self {
            Cursor::Events(events) => *events = &events[1..],
            Cursor::Refs(refs) => *refs = &refs[1..],
        }
    }
}

/// Several streams, each in the engine's order, walked as one: at each step
/// the stream whose next event comes first, the earlier stream on a tie. Its
/// state is a cursor per stream, so a match that walks the input twice clones
/// it rather than the events.
#[derive(Clone)]
struct Merged<'a, 'r> {
    cursors: Vec<Cursor<'a>>,
    from: &'r str,
}

impl<'a> Iterator for Merged<'a, '_> {
    type Item = &'a Event;

    fn next(&mut self) -> Option<&'a Event> {
        let from = self.from;
        let order = |event: &Event| (event.ts_ns, u8::from(event.kind != from));
        let (at, event) = self
            .cursors
            .iter()
            .enumerate()
            .filter_map(|(at, cursor)| cursor.head().map(|event| (at, event)))
            .min_by_key(|(at, event)| (order(event), *at))?;
        self.cursors[at].advance();
        Some(event)
    }
}

/// One match over a stream already in the engine's order.
fn walk<'a>(
    match_kind: MatchKind,
    rule: &Rule,
    events: impl Iterator<Item = &'a Event> + Clone,
    watch: &mut Watch,
) -> Result<Correlation, JoinError> {
    match match_kind {
        MatchKind::Interval => interval(rule, events, watch),
        MatchKind::Eq | MatchKind::ScopedEq => keyed(rule, events, watch),
        MatchKind::Wakeup => wakeup(rule, events, watch),
        MatchKind::Between => between(rule, events, watch),
        MatchKind::Overlap => overlap(rule, events, watch),
        MatchKind::Woke => unreachable!("`woke` is walked over its references"),
    }
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
fn interval<'a>(
    rule: &Rule,
    events: impl Iterator<Item = &'a Event> + Clone,
    watch: &mut Watch,
) -> Result<Correlation, JoinError> {
    let mut keyless = 0u64;
    let mut states: States<u64> = States::new(rule);
    let (mut matched, mut unmatched) = (0u64, 0u64);

    for event in events {
        watch.advance(event.ts_ns, &mut states);
        states.expire(event.ts_ns);

        if event.kind == rule.from {
            let Some(KeyValue::Mem { base, len }) = event.key(keyed_by(rule)).cloned() else {
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
            let Some(KeyValue::Mem { base: address, .. }) = event.key(keyed_by(rule)) else {
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
fn keyed<'a>(
    rule: &Rule,
    events: impl Iterator<Item = &'a Event> + Clone,
    watch: &mut Watch,
) -> Result<Correlation, JoinError> {
    let mut keyless = 0u64;
    let scoped = rule.match_kind == MatchKind::ScopedEq;
    let mut states: States<String> = States::new(rule);
    let (mut matched, mut unmatched) = (0u64, 0u64);

    for event in events {
        watch.advance(event.ts_ns, &mut states);
        states.expire(event.ts_ns);

        if event.kind == rule.from {
            let Some(key) = event.key(keyed_by(rule)) else {
                return Err(missing_key(rule, event));
            };
            let id = identity(key);

            if closes(rule, event) {
                states.close(&id, event.ts_ns);
                continue;
            }
            if opens(rule, event) {
                let label = if scoped {
                    key.to_string()
                } else {
                    label_of(key)
                };
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
            let Some(key) = event.key(keyed_by(rule)) else {
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
fn wakeup<'a>(
    rule: &Rule,
    events: impl Iterator<Item = &'a Event> + Clone,
    watch: &mut Watch,
) -> Result<Correlation, JoinError> {
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
            let Some(KeyValue::Task { tid: wakee }) = event.key(keyed_by(rule)).cloned() else {
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
            let Some(KeyValue::Task { tid }) = event.key(keyed_by(rule)) else {
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
fn between<'a>(
    rule: &Rule,
    events: impl Iterator<Item = &'a Event> + Clone,
    watch: &mut Watch,
) -> Result<Correlation, JoinError> {
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
            let Some(key) = event.key(keyed_by(rule)) else {
                return Err(missing_key(rule, event));
            };
            let closing = closes(rule, event);
            if closing || opens(rule, event) {
                arrived += 1;
                states.insert(
                    (identity(key), arrived),
                    OpenState {
                        label: label_of(key),
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
            let Some(key) = event.key(keyed_by(rule)) else {
                // A downstream event without the key is outside every state by
                // construction -- a block request issued outside any request --
                // and is counted apart rather than refusing the whole rule.
                keyless += 1;
                continue;
            };
            // The states are filed under the identity, which is what the two
            // sides share; the reading's own baseline is its series, every
            // field of the key, so two series of one interface -- a rename,
            // or two counter kinds -- never take each other's previous value.
            let id = identity(key);
            let key = key.to_string();
            let value = event
                .attr(attr)
                .and_then(AttrValue::as_f64)
                .ok_or_else(|| JoinError::MissingValue {
                    id: rule.id.clone(),
                    attr: attr.to_owned(),
                    kind: event.kind.clone(),
                })?;

            let pending = states.take_key(&id);
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
            states.finished.push(Attribution {
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
    let mut correlation = assemble(rule, matched, unmatched, states);
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
            let Some(KeyValue::Task { tid }) = event.key(keyed_by(rule)).cloned() else {
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
                None => match event.key(keyed_by(rule)) {
                    Some(KeyValue::Task { tid }) => Some(*tid),
                    _ => None,
                },
            };
            let Some(tid) = woken else {
                keyless += 1;
                continue;
            };
            let Some(mut state) = states.remove(&tid) else {
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
            states.end(state, Some(event.ts_ns), Outcome::Closed);
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
/// same `via` key (of the one series, when the rule names none) -- or
/// `[ts - interval_ns, ts]` when the reading says so
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
fn overlap<'a>(
    rule: &Rule,
    events: impl Iterator<Item = &'a Event> + Clone,
    watch: &mut Watch,
) -> Result<Correlation, JoinError> {
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

    // With no key, every reading is one series, and a source that carries
    // several would have each series' reading close the other's interval:
    // refused by name before anything is credited.
    if rule.via.is_none() {
        let series: BTreeSet<String> = events
            .clone()
            .filter(|event| event.kind == rule.from && opens(rule, event))
            .map(all_keys)
            .collect();
        if series.len() > 1 {
            return Err(JoinError::SeveralSeries {
                id: rule.id.clone(),
                from: rule.from.clone(),
                series: series.len(),
                sample: series.into_iter().take(3).collect::<Vec<_>>().join(", "),
            });
        }
    }

    // The intervals each key's readings cover, in the order they ended, and
    // what a row calls each key.
    let mut last: BTreeMap<String, u64> = BTreeMap::new();
    let mut intervals: BTreeMap<String, Vec<Covered>> = BTreeMap::new();
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    for event in events.clone().filter(|event| event.kind == rule.from) {
        let Some(key) = series_of(rule, event) else {
            return Err(missing_key(rule, event));
        };
        if !opens(rule, event) {
            continue;
        }
        labels.entry(key.clone()).or_insert_with(|| {
            rule.via
                .and_then(|via| event.key(via))
                .map_or_else(|| key.clone(), label_of)
        });
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
    for event in events.filter(|event| event.kind == rule.to) {
        if watch.outside(event.ts_ns) {
            continue;
        }
        let Some(key) = series_of(rule, event) else {
            keyless += 1;
            continue;
        };
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
            let Some(enclosing) = rule.enclosing.as_deref() else {
                unmatched += 1;
                readings.uncredited += value;
                continue;
            };
            let length = event
                .attr(enclosing)
                .and_then(AttrValue::as_f64)
                .ok_or_else(|| JoinError::MissingSpan {
                    id: rule.id.clone(),
                    attr: enclosing.to_owned(),
                    kind: event.kind.clone(),
                })?
                .max(0.0) as u64;
            let (start_ns, end_ns) = match rule.stamped.unwrap_or(Stamped::End) {
                Stamped::End => (event.ts_ns.saturating_sub(length), event.ts_ns),
                Stamped::Start => (event.ts_ns, event.ts_ns.saturating_add(length)),
            };
            // The one interval that holds the whole window, `(start, end]`,
            // if any: intervals of one series do not overlap, so the first
            // that ends at or after the window's end is the only candidate.
            let holder = intervals.get_mut(&key).and_then(|list| {
                let first = list.partition_point(|covered| covered.end_ns < end_ns);
                list.get_mut(first)
                    .filter(|covered| covered.start_ns <= start_ns)
            });
            match holder {
                Some(covered) => {
                    covered.credited += value;
                    covered.events += 1;
                    matched += 1;
                    readings.credited += value;
                }
                None => {
                    unmatched += 1;
                    readings.uncredited += value;
                }
            }
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
    for (key, list) in intervals {
        states.stats.peak_open += 1;
        let label = labels.get(&key).cloned().unwrap_or(key);
        for covered in list {
            states.stats.opened += 1;
            states.stats.closed += 1;
            states.finished.push(Attribution {
                key: label.clone(),
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
    let mut correlation = assemble(rule, matched, unmatched, states);
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
    let (stats, ranked) = states.finish();
    let (finished, total_attributed) = ranked.into_top();

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

/// How the attributions of one rule are ranked, and how many are kept.
enum Ranking {
    /// `wakeup`: folded into one row per chain as they end, unmatched
    /// wakeups included, since a chain counts every wakeup.
    Chain,
    /// `woke`: folded into one row per waker, matched pairs only.
    Waker,
    /// `between` and `overlap`: the heaviest change first, either way.
    Magnitude,
    /// Every other match: the most matched first.
    Matched,
}

/// The attributions of one rule as its states end, bounded.
///
/// A folding match keeps one row per label, which is bounded by the labels
/// (chains, wakers) and not by the run. A ranking match keeps the rows that
/// can still be among the rule's `top`: once the buffer reaches twice that
/// (and at least [`RANKED_BUFFER`]), it is sorted and cut back to `top`.
/// That is exact: the final sort is stable, every cut is the same stable sort,
/// and a row cut once can never rise above the `top` rows that outranked it.
/// What `total_attributed` counts is summed as rows arrive, so the cut costs
/// it nothing.
struct Ranked {
    ranking: Ranking,
    top: usize,
    rows: Vec<Attribution>,
    folded: BTreeMap<String, Attribution>,
    attributed: u64,
}

/// The fewest rows a ranking match buffers before it cuts back to `top`.
const RANKED_BUFFER: usize = 1024;

impl Ranked {
    fn new(rule: &Rule) -> Self {
        Self {
            ranking: match rule.match_kind {
                MatchKind::Wakeup => Ranking::Chain,
                MatchKind::Woke => Ranking::Waker,
                MatchKind::Between | MatchKind::Overlap => Ranking::Magnitude,
                _ => Ranking::Matched,
            },
            top: rule.top,
            rows: Vec::new(),
            folded: BTreeMap::new(),
            attributed: 0,
        }
    }

    fn push(&mut self, attribution: Attribution) {
        match self.ranking {
            Ranking::Chain => fold_chain(&mut self.folded, attribution),
            Ranking::Waker if attribution.matched > 0 => {
                fold_waker(&mut self.folded, attribution);
            }
            Ranking::Waker => {}
            Ranking::Magnitude | Ranking::Matched if attribution.matched > 0 => {
                self.attributed += attribution.matched;
                self.rows.push(attribution);
                if self.rows.len() >= (2 * self.top).max(RANKED_BUFFER) {
                    self.cut();
                }
            }
            Ranking::Magnitude | Ranking::Matched => {}
        }
    }

    fn cut(&mut self) {
        match self.ranking {
            Ranking::Magnitude => {
                // The heaviest interval is the one the total moved most in,
                // either way: a free that gave 32 MiB back is as much the
                // answer as the allocation that took it.
                let size = |attribution: &Attribution| {
                    attribution
                        .change
                        .as_ref()
                        .map_or(0.0, |change| change.delta.abs())
                };
                self.rows.sort_by(|left, right| {
                    size(right)
                        .total_cmp(&size(left))
                        .then(left.opened_ns.cmp(&right.opened_ns))
                });
            }
            _ => self.rows.sort_by(|left, right| {
                right
                    .matched
                    .cmp(&left.matched)
                    .then(right.wakeups.cmp(&left.wakeups))
                    .then(left.base.cmp(&right.base))
                    .then_with(|| left.key.cmp(&right.key))
            }),
        }
        self.rows.truncate(self.top);
    }

    /// The rule's `top` rows, ranked, and what every row attributed.
    fn into_top(mut self) -> (Vec<Attribution>, u64) {
        if matches!(self.ranking, Ranking::Chain | Ranking::Waker) {
            self.rows = std::mem::take(&mut self.folded)
                .into_values()
                .filter(|attribution| attribution.matched > 0)
                .collect();
            self.attributed = self.rows.iter().map(|row| row.matched).sum();
        }
        self.cut();
        (self.rows, self.attributed)
    }
}

/// Folds one of a `woke` join's pairs into its label's row -- its waker's,
/// when the rule names one.
///
/// Counted over the pairs, spanning the earliest close to the latest wakeup,
/// and carrying in `change` what a reader of this match wants summed: the time
/// from each close to the wakeup that followed it, in `delta`, over `opened`
/// closes of which `closed` were woken -- the same number here, since a state
/// that no wakeup ended is not folded.
fn fold_waker(folded: &mut BTreeMap<String, Attribution>, attribution: Attribution) {
    let waited = attribution
        .closed_ns
        .map_or(0, |closed| closed.saturating_sub(attribution.opened_ns)) as f64;
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

/// Folds one wakeup's attribution into its chain's row.
///
/// Summed over the events, counted over the wakeups, spanning from the
/// earliest wakeup to the latest end, and ending the way the latest wakeup
/// did: the chain is the thing a reader ranks, and the individual wakeups are
/// what it is made of.
fn fold_chain(chains: &mut BTreeMap<String, Attribution>, attribution: Attribution) {
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
        MatchKind::Eq if rule.via.is_some_and(is_scoped) => Evidence::Temporal,
        _ if rule.via == Some(KeyKind::Req) => Evidence::Identity,
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
        KeyValue::Iface { .. } => key.iface_identity().expect("an iface key has an identity"),
        other => other.to_string(),
    }
}

/// What a row names a state by: the key as a person reads it, less what the
/// identity leaves out. A block request without its open timestamp, and an
/// interface by its name -- not the index the identity compares, and not the
/// sub-series a producer wrote after it -- with its network namespace when it
/// is not this process's.
fn label_of(key: &KeyValue) -> String {
    match key {
        KeyValue::Blk { .. } => identity(key),
        // Printed as the key prints, so another namespace's `lo` says which.
        KeyValue::Iface { netns, index, .. } => KeyValue::iface_in(
            *netns,
            *index,
            key.iface_name().expect("an iface key has a name"),
        )
        .to_string(),
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
        via: keyed_by(rule),
        kind: event.kind.clone(),
    }
}

/// The key a keyed match pairs on. Every match but `overlap` has one: [`check`]
/// refuses the rule otherwise, and [`apply_within`] asks it before any match
/// runs, so a rule reaching a keyed match without one is this module's defect.
fn keyed_by(rule: &Rule) -> KeyKind {
    rule.via
        .expect("`check` refuses a rule with no key on every match but `overlap`")
}

/// What a keyless `overlap` calls its one series, in the rows it writes.
pub const MACHINE: &str = "machine";

/// The series an `overlap` event belongs to: its `via` key, or the machine's
/// one series when the rule names none. `None` is an event without the key.
fn series_of(rule: &Rule, event: &Event) -> Option<String> {
    match rule.via {
        Some(via) => event.key(via).map(identity),
        None => Some(MACHINE.to_owned()),
    }
}

/// Every key an event carries, as one label: what tells two series of one
/// source apart when a rule names none of them.
fn all_keys(event: &Event) -> String {
    let keys: Vec<String> = event.keys.iter().map(ToString::to_string).collect();
    if keys.is_empty() {
        "no key".to_owned()
    } else {
        keys.join(" ")
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

mod decompose;
pub use self::decompose::*;

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
            via: Some(KeyKind::Mem),
            match_kind: MatchKind::Interval,
            opens: Some("op == 'alloc'".parse().unwrap()),
            closes: Some("op == 'free'".parse().unwrap()),
            waker: None,
            woken: None,
            span: None,
            stamped: None,
            enclosing: None,
            depth: None,
            value: None,
            reading: None,
            window: None,
            capacity: 1000,
            top: 10,
            tier: Tier::Agent,
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
        rule.via = Some(KeyKind::Task);
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
            via: Some(KeyKind::Task),
            match_kind: MatchKind::Eq,
            opens: None,
            closes: None,
            waker: None,
            woken: None,
            span: None,
            stamped: None,
            enclosing: None,
            depth: None,
            value: None,
            reading: None,
            window: None,
            capacity: 1000,
            top: 10,
            tier: Tier::Agent,
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

    /// The streams of a run walked apart, as `assemble` collects them: each
    /// in order, their concatenation not. The answer is the one worked out by
    /// hand -- a descriptor at the instant its thread's state opens is inside
    /// it, one before it is not -- whichever stream comes first, whether a
    /// stream is out of order itself, and the same as over one sorted stream.
    #[test]
    fn streams_walked_apart_meet_in_the_engines_order() {
        let opened = vec![threads(10, 7), threads(20, 9), threads(30, 7)];
        let looked_up = vec![
            descriptors(10, 7),
            descriptors(15, 9),
            descriptors(20, 9),
            descriptors(31, 7),
        ];
        let windows = BTreeMap::new();
        let merged = apply_merging(&eq_rule(), &[&opened, &looked_up], &windows).unwrap();
        assert_eq!((merged.matched, merged.unmatched), (3, 1));
        states_balance(&merged);

        let reversed = apply_merging(&eq_rule(), &[&looked_up, &opened], &windows).unwrap();
        assert_eq!(reversed, merged);
        let mut shuffled = looked_up.clone();
        shuffled.reverse();
        let indexed = apply_merging(&eq_rule(), &[&opened, &shuffled], &windows).unwrap();
        assert_eq!(indexed, merged);
        let concatenated: Vec<Event> = looked_up.iter().chain(&opened).cloned().collect();
        assert_eq!(apply(&eq_rule(), &concatenated).unwrap(), merged);
    }

    fn link(ts_ns: u64, kind: &str, key: KeyValue, value: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: kind.to_owned(),
            keys: vec![key],
            attrs: vec![Attr::uint("bytes", value), Attr::uint("frames", value)],
            collector_id: CollectorId::new(kind),
            seq: 0,
        }
    }

    /// An interface is one key by the kernel's index, whatever its name says:
    /// `net.iface`'s `lo/drops` is `lo`, a rename mid-run is still the same
    /// interface, and the same name on another index is another one. With no
    /// index known at the read, the name before its sub-series is compared.
    #[test]
    fn an_interface_is_one_key_by_its_index_across_a_rename_and_a_sub_series() {
        let mut rule = eq_rule();
        rule.via = Some(KeyKind::Iface);
        rule.from = "net.iface".to_owned();
        rule.to = "net.qdisc".to_owned();
        let events = vec![
            link(10, "net.iface", KeyValue::iface_as_given(1, "lo/drops"), 0),
            link(
                11,
                "net.iface",
                KeyValue::iface_as_given(0, "veth9/errors"),
                0,
            ),
            link(20, "net.qdisc", KeyValue::iface_as_given(1, "lo"), 1),
            // Renamed after the state opened: one interface, one key.
            link(21, "net.qdisc", KeyValue::iface_as_given(1, "renamed"), 1),
            // The same name on another index is another interface.
            link(22, "net.qdisc", KeyValue::iface_as_given(2, "lo"), 1),
            link(23, "net.qdisc", KeyValue::iface_as_given(0, "veth9"), 1),
        ];
        let correlation = apply(&rule, &events).unwrap();
        assert_eq!((correlation.matched, correlation.unmatched), (3, 1));
        let keys: Vec<(&str, u64)> = correlation
            .top
            .iter()
            .map(|row| (row.key.as_str(), row.matched))
            .collect();
        assert_eq!(keys, vec![("iface:lo", 2), ("iface:veth9", 1)]);
        states_balance(&correlation);
    }

    /// An index is only an index inside its network namespace: another
    /// namespace's `lo` -- index 1 there too -- meets nothing of this one's,
    /// is counted unmatched, and its row says which namespace it is in.
    #[test]
    fn another_namespaces_interface_of_the_same_index_is_not_credited_to_this_ones() {
        let here = crate::event::own_netns();
        let elsewhere = here + 1;
        let mut rule = eq_rule();
        rule.via = Some(KeyKind::Iface);
        rule.from = "net.iface".to_owned();
        rule.to = "net.qdisc".to_owned();
        let events = vec![
            link(10, "net.iface", KeyValue::iface(1, "lo/drops"), 0),
            link(20, "net.qdisc", KeyValue::iface_in(here, 1, "lo"), 1),
            link(21, "net.qdisc", KeyValue::iface_in(elsewhere, 1, "lo"), 1),
            link(22, "net.qdisc", KeyValue::iface_in(elsewhere, 1, "lo"), 1),
        ];
        let correlation = apply(&rule, &events).unwrap();
        assert_eq!((correlation.matched, correlation.unmatched), (1, 2));
        let keys: Vec<(&str, u64)> = correlation
            .top
            .iter()
            .map(|row| (row.key.as_str(), row.matched))
            .collect();
        assert_eq!(keys, vec![("iface:lo", 1)]);
        states_balance(&correlation);

        // Opened on the other namespace's side, its row names that namespace.
        let events = vec![
            link(
                10,
                "net.iface",
                KeyValue::iface_in(elsewhere, 1, "lo/drops"),
                0,
            ),
            link(20, "net.qdisc", KeyValue::iface_in(elsewhere, 1, "lo"), 1),
            link(21, "net.qdisc", KeyValue::iface(1, "lo"), 1),
        ];
        let correlation = apply(&rule, &events).unwrap();
        assert_eq!((correlation.matched, correlation.unmatched), (1, 1));
        assert_eq!(
            correlation.top[0].key,
            format!("iface:lo@netns:{elsewhere}")
        );
    }

    /// Under `between`, the states are the interface's and the baseline is the
    /// reading's own series: two series of one interface each start from their
    /// own previous value, and both are credited to the state of the interface.
    #[test]
    fn a_reading_of_an_interface_keeps_its_own_baseline_and_shares_the_interfaces_states() {
        let mut rule = between_rule();
        rule.via = Some(KeyKind::Iface);
        rule.from = "net.iface".to_owned();
        rule.to = "net.bytes".to_owned();
        rule.opens = None;
        rule.closes = None;
        rule.reading = Some(Reading::Level);
        let events = vec![
            link(0, "net.bytes", KeyValue::iface_as_given(1, "lo"), 100),
            link(
                0,
                "net.bytes",
                KeyValue::iface_as_given(1, "lo/other"),
                5000,
            ),
            link(50, "net.iface", KeyValue::iface_as_given(1, "lo/drops"), 0),
            link(100, "net.bytes", KeyValue::iface_as_given(1, "lo"), 130),
            link(
                100,
                "net.bytes",
                KeyValue::iface_as_given(1, "lo/other"),
                5007,
            ),
        ];
        let correlation = apply(&rule, &events).unwrap();
        let readings = correlation.readings.as_ref().unwrap();
        assert_eq!(readings.baselines, 2, "one baseline per series");
        // The first reading at 100 takes the state; the second finds none.
        assert_eq!(readings.credited, 30.0);
        assert_eq!(readings.uncredited, 7.0);
        assert_eq!(correlation.top[0].key, "iface:lo");
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
        rule.via = Some(KeyKind::Req);
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
            via: Some(KeyKind::Blk),
            match_kind: MatchKind::ScopedEq,
            opens: None,
            closes: None,
            waker: None,
            woken: None,
            span: None,
            stamped: None,
            enclosing: None,
            depth: None,
            value: None,
            reading: None,
            window: None,
            capacity: 1000,
            top: 10,
            tier: Tier::Agent,
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
        rule.via = Some(KeyKind::Task);
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
            via: Some(KeyKind::Task),
            match_kind: MatchKind::Wakeup,
            opens: None,
            closes: None,
            waker: Some("waker_tid".to_owned()),
            woken: None,
            span: None,
            stamped: None,
            enclosing: None,
            depth: None,
            value: None,
            reading: None,
            window: None,
            capacity: 1000,
            top: 10,
            tier: Tier::Agent,
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
        rule.via = Some(KeyKind::Sock);
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
            via: Some(KeyKind::Blk),
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

    /// A drained row of several episodes, stamped when its first began, with
    /// the sum of their durations and the window that encloses them all.
    fn drained(ts_ns: u64, tid: i32, count: u64, total_ns: u64, enclosing_ns: u64) -> Event {
        let mut event = blocked(ts_ns, tid, total_ns);
        event.attrs.push(Attr::uint("count", count));
        event.attrs.push(Attr::uint("enclosing_ns", enclosing_ns));
        event
    }

    /// The rule says which attribute of a drained row is a window rather
    /// than a sum: with `enclosing`, a row whose episodes all lie inside one
    /// interval is credited whole to it, and one whose window crosses an
    /// interval's end stays uncredited -- as every such row does without it.
    #[test]
    fn a_drained_row_is_credited_whole_to_the_one_interval_that_encloses_it() {
        let mut rule = overlap_rule();
        rule.stamped = Some(Stamped::Start);
        let events = vec![
            polled(100, 7),
            polled(200, 7),
            polled(300, 7),
            // Three episodes summing 30 inside [110, 190]: in (100, 200].
            drained(110, 7, 3, 30, 80),
            // Two summing 20 inside [180, 240]: across the reading at 200.
            drained(180, 7, 2, 20, 60),
            // One episode: its own span, as ever. [210, 220] in (200, 300].
            blocked(210, 7, 10),
        ];
        let summed = apply(&rule, &events).unwrap();
        let readings = summed.readings.as_ref().unwrap();
        assert_eq!((readings.credited, readings.uncredited), (10.0, 50.0));
        assert_eq!((summed.matched, summed.unmatched), (1, 2));

        rule.enclosing = Some("enclosing_ns".to_owned());
        let enclosed = apply(&rule, &events).unwrap();
        let readings = enclosed.readings.as_ref().unwrap();
        assert_eq!((readings.credited, readings.uncredited), (40.0, 20.0));
        assert_eq!((enclosed.matched, enclosed.unmatched), (2, 1));
        let credit = |start: u64| {
            enclosed
                .top
                .iter()
                .find(|row| row.opened_ns == start)
                .and_then(|row| row.change.as_ref())
                .map(|change| change.delta)
        };
        assert_eq!(credit(100), Some(30.0));
        assert_eq!(credit(200), Some(10.0));

        // A row without the attribute the rule names is the session's
        // mistake, as a missing `span` is.
        let mut bare = blocked(150, 7, 5);
        bare.attrs.push(Attr::uint("count", 2));
        let error = apply(&rule, &[polled(100, 7), polled(200, 7), bare]).unwrap_err();
        assert!(matches!(error, JoinError::MissingSpan { .. }), "{error}");

        // And a match that places no window refuses the field.
        let mut keyed = eq_rule();
        keyed.enclosing = Some("enclosing_ns".to_owned());
        let error = apply(&keyed, &[]).unwrap_err();
        assert!(
            matches!(error, JoinError::EnclosingOutsideOverlap { .. }),
            "{error}"
        );
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
            enclosing: None,
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

    /// A machine-wide reading, as `mem.pressure` for the system writes one:
    /// no key, and the interval it covers stated.
    fn pressure(ts_ns: u64, interval_ns: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.pressure".to_owned(),
            keys: vec![],
            attrs: vec![
                Attr::float("stall_ratio", 0.0),
                Attr::uint("interval_ns", interval_ns),
            ],
            collector_id: CollectorId::new("pressure"),
            seq: 0,
        }
    }

    fn machine_rule() -> Rule {
        Rule {
            id: "blocked-per-pressure-reading".to_owned(),
            from: "mem.pressure".to_owned(),
            via: None,
            ..overlap_rule()
        }
    }

    /// With no key, the machine's readings are one series and every task's
    /// window is laid across it: two tasks' blocks share one interval, which a
    /// keyed rule would have kept apart, and nothing is keyless.
    #[test]
    fn with_no_key_every_window_is_laid_across_the_machines_one_series() {
        let events = vec![
            pressure(100, 100),
            pressure(200, 100),
            pressure(300, 100),
            // [120, 370]: 80 of (100, 200], 100 of (200, 300], 70 unread.
            blocked(370, 7, 250),
            // Another task, [240, 250], wholly in (200, 300].
            blocked(250, 8, 10),
        ];
        let correlation = apply(&machine_rule(), &events).unwrap();
        assert_eq!(correlation.evidence, Evidence::Temporal);
        assert_eq!((correlation.matched, correlation.unmatched), (2, 0));
        assert_eq!(correlation.keyless, 0);
        let credit = |start: u64| {
            correlation
                .top
                .iter()
                .find(|row| row.opened_ns == start)
                .map(|row| (row.key.clone(), row.change.as_ref().map(|c| c.delta)))
        };
        assert_eq!(
            credit(0),
            None,
            "the first interval, (0, 100], holds nothing"
        );
        assert_eq!(credit(100), Some((MACHINE.to_owned(), Some(80.0))));
        assert_eq!(credit(200), Some((MACHINE.to_owned(), Some(100.0 + 10.0))));
        let readings = correlation.readings.as_ref().unwrap();
        assert_eq!(readings.baselines, 0, "every reading stated its interval");
        assert_eq!(readings.credited, 190.0);
        assert_eq!(readings.uncredited, 70.0, "the part after the last reading");
        states_balance(&correlation);
    }

    /// The same rule, keyed by task, finds no task on the machine's readings:
    /// the separation that says the keyless rule is not the keyed one with a
    /// default.
    #[test]
    fn a_keyed_rule_over_the_machines_readings_finds_no_key_and_says_so() {
        let rule = Rule {
            via: Some(KeyKind::Task),
            ..machine_rule()
        };
        let events = vec![pressure(100, 100), blocked(90, 7, 10)];
        let error = apply(&rule, &events).unwrap_err();
        assert!(matches!(error, JoinError::MissingKey { .. }), "{error}");
    }

    /// A source read as one series that carries several is refused by name:
    /// each series' reading would close the other's interval.
    #[test]
    fn with_no_key_a_source_of_several_series_is_refused_by_name() {
        let events = vec![polled(100, 7), polled(101, 8), blocked(150, 7, 10)];
        let rule = Rule {
            via: None,
            ..overlap_rule()
        };
        let error = apply(&rule, &events).unwrap_err();
        assert!(
            matches!(error, JoinError::SeveralSeries { series: 2, .. }),
            "{error}"
        );
        assert!(error.to_string().contains("task:7"), "{error}");
    }

    /// Every match but `overlap` pairs events by a key, and one with none is
    /// refused before any event is read.
    #[test]
    fn every_match_but_overlap_is_refused_without_a_key() {
        for rule in [
            eq_rule(),
            wakeup_rule(),
            woke_rule(),
            between_rule(),
            rule(),
        ] {
            let rule = Rule { via: None, ..rule };
            let error = check(&rule).unwrap_err();
            assert!(matches!(error, JoinError::NoKey { .. }), "{error}");
            assert!(error.to_string().contains("via"), "{error}");
        }
        let rule = Rule {
            value: None,
            ..machine_rule()
        };
        assert!(matches!(
            check(&rule).unwrap_err(),
            JoinError::NoShareValue { .. }
        ));
        assert_eq!(check(&machine_rule()), Ok(()));
    }

    /// A rule written without `via` reads back without it, and one with it
    /// reads back with it.
    #[test]
    fn a_rule_without_via_parses_and_round_trips() {
        let rule: Rule = serde_norway::from_str(
            "{id: m, from: mem.pressure, to: sched.offcpu, match: overlap, value: total_ns}",
        )
        .unwrap();
        assert_eq!(rule.via, None);
        assert!(!serde_json::to_string(&rule).unwrap().contains("via"));
        let rule: Rule = serde_norway::from_str(
            "{id: m, from: proc.threads, to: sched.offcpu, via: task, match: overlap, value: total_ns}",
        )
        .unwrap();
        assert_eq!(rule.via, Some(KeyKind::Task));
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
            via: Some(KeyKind::Task),
            match_kind: MatchKind::Between,
            opens: Some("op == 'alloc'".parse().unwrap()),
            closes: Some("op == 'free'".parse().unwrap()),
            waker: None,
            woken: None,
            span: None,
            stamped: None,
            enclosing: None,
            depth: None,
            value: Some("bytes".to_owned()),
            reading: Some(Reading::Level),
            window: None,
            capacity: 1000,
            top: 10,
            tier: Tier::Agent,
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

    /// A sample count adds the cpu the kernel counted for a task before the
    /// sampler found it (`unsampled_ns:task:<tid>` in the result's fields):
    /// `make`'s second compile, found on a late tick, was half a job's cpu in
    /// no row. A task found and never sampled is that time, not missing.
    #[test]
    fn a_samples_row_adds_the_cpu_before_the_task_was_found() {
        let spec = decomposition(
            "{id: t, whole: window, parts: [\
               {state: on-cpu, from: cpu.profile, value: samples},\
               {state: blocked, from: sched.offcpu, value: total_ns}]}",
        );
        let requests = [
            request("cpu.profile", "sample@1000hz"),
            request("sched.offcpu", "trace"),
        ];
        let mut profile = result("cpu.profile", Unit::Count, 10_000_000);
        profile
            .fields
            .insert(format!("{UNSAMPLED_NS}task:7"), 2_500_000);
        profile
            .fields
            .insert(format!("{UNSAMPLED_NS}task:8"), 3_000_000);
        // Task 7's ring lost 3 samples, all the run lost: the row adds them
        // and is no lower bound.
        profile.losses.dropped = 3;
        profile.by_ring = vec![crate::collect::RingLoss {
            key: KeyValue::task(7),
            losses: crate::collect::Losses {
                dropped: 3,
                ..Default::default()
            },
        }];
        let results = [profile, result("sched.offcpu", Unit::Ns, 9_000_000)];
        let mut events: Vec<Event> = (0..4)
            .map(|i| total(i, "cpu.profile", 7, vec![Attr::uint("count", 1)]))
            .collect();
        events.push(total(
            5,
            "sched.offcpu",
            8,
            vec![Attr::uint("total_ns", 1_000)],
        ));

        let output = decompose(&spec, &requests, &results, &events);
        let row = |key: &str| {
            output
                .tables
                .iter()
                .find(|table| table.key == key)
                .map(|table| table.rows[0].clone())
                .unwrap_or_else(|| panic!("no table for {key}"))
        };
        let sampled = row("task:7");
        assert_eq!(
            sampled.amount,
            Some(9_500_000.0),
            "4 periods kept, 3 lost, and 2.5 ms before"
        );
        assert!(
            sampled
                .note
                .as_deref()
                .is_some_and(|n| n.contains("schedstat") && n.contains("3 more")),
            "{sampled:?}"
        );
        assert!(sampled.short.is_none(), "{sampled:?}");
        let never = row("task:8");
        assert_eq!(never.missing, None);
        assert_eq!(never.amount, Some(3_000_000.0));
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

    /// What `main.rs` does since every window closes before the first stop:
    /// the joins get a source that ran to the end with `closed_ns =
    /// u64::MAX`, and the events its stop drained are stamped after the
    /// results' close. A paired part read over the results' windows cuts the
    /// open completion at that close and counts the drained wakeup as
    /// outside; read over the joins' windows, its pairs are the join's.
    #[test]
    fn a_paired_part_pairs_over_the_windows_the_joins_were_given() {
        let spec = decomposition(
            "{id: one-read, whole: {from: io.syscall, value: total_ns}, parts: [\
               {state: completion -> wakeup, from: completion-released, value: paired}]}",
        );
        let requests = [
            request("io.syscall", "trace"),
            request("io.latency", "trace"),
            request("sched.wakeup", "trace"),
        ];
        let joins = [woke_rule()];
        let closed = |mut results: Results| {
            results.window = Watched {
                run_opened_ns: 0,
                opened_ns: 0,
                closed_ns: 35,
                attached: true,
                attach_ns: None,
            };
            results
        };
        let results = [
            closed(result("io.syscall", Unit::Ns, 35)),
            closed(result("io.latency", Unit::Ns, 35)),
            closed(result("sched.wakeup", Unit::Count, 35)),
        ];
        let wakeup = |ts| {
            total(
                ts,
                "sched.wakeup",
                7,
                vec![Attr::uint("wakee_tid", 7), Attr::uint("waker_tid", 3)],
            )
        };
        // The second completion is inside the window; its wakeup, at 39, is
        // what the stop drained after the close at 35.
        let events = vec![
            total(9, "io.latency", 7, vec![Attr::uint("latency_ns", 5)]),
            wakeup(12),
            total(30, "io.latency", 7, vec![Attr::uint("latency_ns", 5)]),
            wakeup(39),
            total(50, "io.syscall", 7, vec![Attr::uint("total_ns", 50)]),
        ];
        let mut windows = windows_of(&results);
        for watched in windows.values_mut() {
            watched.closed_ns = u64::MAX;
        }
        let join = apply_within(&joins[0], &events, &windows).expect("the join applies");
        assert_eq!(join.matched, 2, "{join:?}");
        assert_eq!(join.outside_window, 0);

        let within = decompose_within(&spec, &requests, &results, &events, &joins, &windows);
        let row = &within.tables[0].rows[0];
        assert_eq!(row.readings, 2, "the join's two pairs: {row:?}");
        assert_eq!(row.amount, Some(3.0 + 9.0), "12 - 9 and 39 - 30");

        // Over the results' own windows the drained wakeup is lost to the
        // part, which is the inconsistency the windows passed beside fix.
        let with = decompose_with(&spec, &requests, &results, &events, &joins);
        let row = &with.tables[0].rows[0];
        assert_eq!(row.readings, 1, "{row:?}");
        assert_eq!(row.amount, Some(3.0));

        // An empty map watches everything, as `join::apply` does.
        let everything = decompose_within(
            &spec,
            &requests,
            &results,
            &events,
            &joins,
            &BTreeMap::new(),
        );
        assert_eq!(everything.tables[0].rows[0].readings, 2);
    }

    /// Every row the engine builds says what ties it to the key, whatever
    /// kind of number it is and whether or not it could be read; and a table
    /// read back without one is refused, not shown with a dash.
    #[test]
    fn a_row_without_an_evidence_level_is_never_built_and_never_read() {
        let spec = decomposition(
            "{id: t, whole: window, parts: [\
               {state: on-cpu, from: cpu.profile, value: samples},\
               {state: blocked, from: sched.offcpu, value: total_ns},\
               {state: unread, from: io.latency, value: latency_ns},\
               {state: joined, from: completion-released, value: paired}]}",
        );
        let requests = [
            request("cpu.profile", "sample@1000hz"),
            request("sched.offcpu", "trace"),
        ];
        let results = [
            result("cpu.profile", Unit::Count, 10_000_000),
            result("sched.offcpu", Unit::Ns, 9_000_000),
        ];
        let events = vec![
            total(1, "cpu.profile", 7, vec![Attr::uint("count", 1)]),
            total(2, "sched.offcpu", 8, vec![Attr::uint("total_ns", 5)]),
        ];
        // No joins, so the paired part cannot be read; `io.latency` was not
        // collected, so neither can that one; and task 8 has no sample.
        let output = decompose(&spec, &requests, &results, &events);
        assert_eq!(output.tables.len(), 2);
        for table in &output.tables {
            for row in &table.rows {
                assert!(row.evidence.is_some(), "{row:?}");
            }
            assert!(table.whole.kind.is_bracket() && table.whole.evidence.is_none());
            let json = serde_json::to_value(table).unwrap();
            let back: Breakdown = serde_json::from_value(json.clone()).expect("a valid table");
            assert_eq!(&back, table);

            let mut stripped = json.clone();
            stripped["rows"][1]
                .as_object_mut()
                .unwrap()
                .remove("evidence");
            let error = serde_json::from_value::<Breakdown>(stripped)
                .unwrap_err()
                .to_string();
            let state = json["rows"][1]["state"].as_str().unwrap().to_owned();
            assert!(
                error.contains(&format!(
                    "the `{state}` row of the table for `{}` carries no evidence level: \
                     every row says what ties it to the key",
                    table.key
                )),
                "{error}"
            );
            assert!(
                !error.contains("  "),
                "a run of spaces in the message: {error:?}"
            );

            let mut measured = json;
            measured["whole"]["kind"] = serde_json::json!("traced");
            let error = serde_json::from_value::<Breakdown>(measured)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(
                    "carries no evidence level: only a window or a life, which bracket the key, \
                     can go without one"
                ),
                "{error}"
            );
            assert!(
                !error.contains("  "),
                "a run of spaces in the message: {error:?}"
            );
        }
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

    // -- the tier a rule is applied at --

    fn raw_result(source: &str, events: Vec<Event>, window: Watched) -> Results {
        let mut result = watched_result(source, Unit::Count, window);
        result.outputs = vec![crate::emit::Output::Raw { events }];
        result
    }

    fn to_the_end() -> Watched {
        Watched {
            run_opened_ns: 0,
            opened_ns: 0,
            closed_ns: 1_000,
            attached: true,
            attach_ns: None,
        }
    }

    #[test]
    fn a_rule_is_applied_by_the_agent_unless_it_says_otherwise() {
        let rule: Rule = serde_norway::from_str(
            "{id: r, from: proc.threads, to: proc.fd, via: task, match: eq}",
        )
        .expect("a rule");
        assert_eq!(rule.tier, Tier::Agent);
        let written = serde_json::to_value(&rule).expect("serialisable");
        assert!(written.get("tier").is_none(), "the default is not written");
        for tier in ["query", "kernel"] {
            let rule: Rule = serde_norway::from_str(&format!(
                "{{id: r, from: proc.threads, to: proc.fd, via: task, match: eq, tier: {tier}}}"
            ))
            .expect("a rule");
            assert_eq!(rule.tier.as_str(), tier);
        }
        assert!(serde_norway::from_str::<Rule>(
            "{id: r, from: proc.threads, to: proc.fd, via: task, match: eq, tier: gpu}"
        )
        .is_err());
    }

    /// No match kind has a kernel program: the rule is refused with the
    /// reason and the two tiers that serve it, never applied by the agent
    /// under a name that says kernel.
    #[test]
    fn a_rule_at_the_kernel_tier_is_refused_with_the_tiers_that_serve_it() {
        let rule = Rule {
            tier: Tier::Kernel,
            ..eq_rule()
        };
        let error = check(&rule).unwrap_err();
        assert_eq!(
            error,
            JoinError::KernelTier {
                id: rule.id.clone()
            }
        );
        let said = error.to_string();
        assert!(
            said.contains("tier: agent") && said.contains("tier: query"),
            "{said}"
        );
        assert!(apply(&rule, &[]).is_err(), "the engine asks again");
    }

    /// The query tier asks of the session what the stored run must hold: both
    /// sides collected, with `raw`. A `window` is stored with its close.
    #[test]
    fn a_rule_at_the_query_tier_needs_both_sides_stored_in_full() {
        let rule = Rule {
            tier: Tier::Query,
            ..eq_rule()
        };
        let both = [
            request("proc.threads", "poll@10ms"),
            request("proc.fd", "poll@10ms"),
        ];
        check_in(&rule, &both, &catalog()).expect("both sides, raw, the whole run");

        let missing = [request("proc.threads", "poll@10ms")];
        let error = check_in(&rule, &missing, &catalog()).unwrap_err();
        assert!(
            matches!(&error, JoinError::NotStored { side, .. } if side == "proc.fd"),
            "{error}"
        );

        let mut summed = both.clone();
        summed[1] = serde_norway::from_str(
            "{source: proc.fd, target: {launch: [/bin/true]}, mode: 'poll@10ms', emit: [summary]}",
        )
        .expect("a request");
        let error = check_in(&rule, &summed, &catalog()).unwrap_err();
        assert!(error.to_string().contains("without `raw`"), "{error}");

        let mut windowed = both.clone();
        windowed[0] = serde_norway::from_str(
            "{source: proc.threads, target: {launch: [/bin/true]}, mode: 'poll@10ms', \
             emit: [raw], window: '1s..'}",
        )
        .expect("a request");
        check_in(&rule, &windowed, &catalog())
            .expect("a stored result says where its window closed, and whether at a detach");

        // The agent tier asks none of it.
        check_in(&eq_rule(), &summed, &catalog()).expect("the agent reads the run's events");
    }

    /// The same rule over the same events answers the same at both tiers:
    /// the query tier is the engine over the stored `raw` outputs, with the
    /// run's own merge of two results of one source.
    #[test]
    fn a_rule_over_a_stored_run_answers_what_the_run_did() {
        let opened = vec![threads(10, 7), threads(11, 9), threads(40, 7)];
        let looked_up = vec![
            descriptors(20, 7),
            descriptors(21, 9),
            descriptors(22, 7),
            descriptors(23, 11),
            descriptors(41, 7),
        ];
        let more = vec![descriptors(42, 9)];
        let mut windows = BTreeMap::new();
        let mut watched = to_the_end();
        watched.closed_ns = u64::MAX;
        windows.insert("proc.threads".to_owned(), watched);
        windows.insert("proc.fd".to_owned(), watched);
        let ran = apply_merging(&eq_rule(), &[&opened, &looked_up, &more], &windows).unwrap();

        let unrelated = vec![total(15, "cpu.profile", 7, vec![])];
        let stored = [
            raw_result("proc.threads", opened, to_the_end()),
            raw_result("cpu.profile", unrelated, to_the_end()),
            raw_result("proc.fd", looked_up, to_the_end()),
            raw_result("proc.fd", more, to_the_end()),
        ];
        let rule = Rule {
            tier: Tier::Query,
            ..eq_rule()
        };
        let queried = apply_stored(&rule, &stored).unwrap();
        assert_eq!(queried, ran);
        assert_eq!((queried.matched, queried.unmatched), (5, 1));

        // A stored run without one side's events is refused by name.
        let error = apply_stored(&rule, &stored[..2]).unwrap_err();
        assert!(error.to_string().contains("no result"), "{error}");
        let mut summed = stored.clone();
        summed[2].outputs.clear();
        let error = apply_stored(&rule, &summed).unwrap_err();
        assert!(error.to_string().contains("without `raw`"), "{error}");
    }

    /// A side whose stored result says it was detached is read as the run
    /// read it: its window ends at the detach, not at the end of the run.
    #[test]
    fn a_stored_side_detached_mid_run_is_closed_where_it_was_detached() {
        let opened = vec![threads(10, 7), threads(40, 7)];
        let looked_up = vec![descriptors(20, 7), descriptors(41, 7)];
        let mut detached_at_30 = to_the_end();
        detached_at_30.closed_ns = 30;
        let mut ended = to_the_end();
        ended.closed_ns = u64::MAX;
        let mut as_run = BTreeMap::new();
        as_run.insert("proc.threads".to_owned(), detached_at_30);
        as_run.insert("proc.fd".to_owned(), ended);
        let ran = apply_merging(&eq_rule(), &[&opened, &looked_up], &as_run).unwrap();
        let mut guessed = as_run.clone();
        guessed.insert("proc.threads".to_owned(), ended);
        let to_the_end_of_the_run =
            apply_merging(&eq_rule(), &[&opened, &looked_up], &guessed).unwrap();
        assert_ne!(
            ran, to_the_end_of_the_run,
            "the detach changes the answer, so reading it is what is tested"
        );

        let mut threads_side = raw_result("proc.threads", opened, detached_at_30);
        threads_side.stopped_because =
            "its window closed: detached 0.000 s into the run, as asked".to_owned();
        let stored = [threads_side, raw_result("proc.fd", looked_up, to_the_end())];
        let rule = Rule {
            tier: Tier::Query,
            ..eq_rule()
        };
        assert_eq!(apply_stored(&rule, &stored).unwrap(), ran);
    }

    /// The events a run keeps for its joins and decompositions are those of
    /// the sources they read: the table read from those alone is the table
    /// read from all of them, and a source nothing reads is not among them.
    #[test]
    fn a_decomposition_over_the_sources_it_reads_is_the_one_over_every_source() {
        let spec = decomposition(
            "{id: one-read, whole: {from: io.syscall, value: total_ns}, parts: [\
               {state: device, from: io.latency, value: latency_ns},\
               {state: completion -> wakeup, from: completion-released, value: paired}]}",
        );
        let requests = [
            request("io.syscall", "trace"),
            request("io.latency", "trace"),
            request("sched.wakeup", "trace"),
            request("cpu.profile", "sample@1000hz"),
        ];
        let joins = [woke_rule()];
        let results = [
            result("io.syscall", Unit::Ns, 100),
            result("io.latency", Unit::Ns, 100),
            result("sched.wakeup", Unit::Count, 100),
            result("cpu.profile", Unit::Count, 100),
        ];
        let wakeup = |ts_ns| {
            total(
                ts_ns,
                "sched.wakeup",
                7,
                vec![Attr::uint("wakee_tid", 7), Attr::uint("waker_tid", 3)],
            )
        };
        let events = vec![
            total(9, "io.latency", 7, vec![Attr::uint("latency_ns", 5)]),
            total(10, "cpu.profile", 7, vec![Attr::uint("samples", 1)]),
            wakeup(12),
            total(30, "io.latency", 7, vec![Attr::uint("latency_ns", 5)]),
            wakeup(39),
            total(50, "io.syscall", 7, vec![Attr::uint("total_ns", 50)]),
        ];
        let read = sources_read(&joins, std::slice::from_ref(&spec));
        assert!(read.contains("io.syscall") && read.contains("io.latency"));
        assert!(
            read.contains("sched.wakeup"),
            "the paired part's join reads it"
        );
        assert!(!read.contains("cpu.profile"), "{read:?}");
        let kept: Vec<Event> = events
            .iter()
            .filter(|event| read.contains(&event.kind))
            .cloned()
            .collect();
        assert_eq!(kept.len(), events.len() - 1);
        assert_eq!(
            decompose_with(&spec, &requests, &results, &kept, &joins),
            decompose_with(&spec, &requests, &results, &events, &joins)
        );
        assert!(
            sources_read(&[], &[]).is_empty(),
            "a run with no join keeps nothing"
        );
        let queried = [Rule {
            tier: Tier::Query,
            ..eq_rule()
        }];
        assert!(
            sources_read(&queried, &[]).is_empty(),
            "a query-tier rule reads the stored run, not the run's events"
        );
    }
}
