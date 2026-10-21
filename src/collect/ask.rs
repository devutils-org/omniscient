//! What the runner asks of its caller and hands back: a cheaper rung, an
//! escalation between rounds, a recorder and what it recorded.

use super::*;

/// Where a run turns when it costs more than it was allowed.
///
/// The runner can tell that it is over budget. It cannot tell what is cheaper:
/// that is a question about the catalog and about this machine, and answering
/// it here would mean the collector layer reaching back into the resolver. So
/// the answer is handed in, and a run given no answer stops instead of
/// degrading.
pub trait Cheaper {
    /// The next backend to serve request `index` with -- cheaper than the one
    /// in use, and already resolved against this machine. `None` means the
    /// chain has nothing left, which is what ends the run.
    fn next(&mut self, index: usize, spent: &Overhead, ceiling: f64) -> Option<ResolvedRequest>;
}

/// The policy of a run that was given nowhere to degrade to: over budget, it
/// stops and says so.
pub struct StopWhenOverBudget;

impl Cheaper for StopWhenOverBudget {
    fn next(&mut self, _: usize, _: &Overhead, _: f64) -> Option<ResolvedRequest> {
        None
    }
}

/// Where a run turns when its own numbers name a suspect.
///
/// The runner can attach and detach a request between two rounds -- a window
/// does it at times the session wrote. What it cannot know is *which* request
/// the run's numbers so far call for: that is a question about what a task's
/// time went to, and answering it here would put triage inside the collector
/// layer. So the question is handed out, the way [`Cheaper`] is, and the
/// runner only applies the answer: requests held for the escalation (a window
/// written `on <layer>`) are prepared before the run and wait, and this is
/// asked once a round which of them to attach and which attached ones to let
/// go.
///
/// Its only power is over requests the session declared: every one was
/// validated, resolved and prepared before the program was launched, so an
/// escalation can never attach a backend the resolver did not choose, or a
/// source that answers a question the session did not ask.
pub trait Escalate {
    /// What to attach and detach now, from what the run has collected so far.
    /// Asked after the round's reads and after the stop condition, so a run
    /// that is ending attaches nothing. An empty answer leaves it as it is.
    fn decide(&mut self, run: &Progress<'_>) -> Vec<Switch>;

    /// A request this escalation attached is over its budget, and nothing
    /// cheaper serves it. Answers why to detach it and go on with the cheap
    /// pass -- the budget holds and the cheap answer stands -- or `None` to end
    /// the run, as any other run over its budget ends.
    fn shed(&mut self, index: usize, over: &str) -> Option<String>;

    /// What each request's collection says about the escalation once the run
    /// is over: the decisions taken, and the layers named that could not be
    /// served. Pairs of a request's index and a sentence.
    fn close(&mut self, run: &Progress<'_>) -> Vec<(usize, String)>;

    /// What each request's collector says of its own events so far, shown
    /// before [`Escalate::decide`] each round: what a live view reads a value
    /// by, how a drained row is stamped, and the drops placed at their drains.
    /// Ignored unless an escalation draws the run as it goes.
    fn described(&mut self, _said: &[Described<'_>]) {}
}

/// What one request's collector says of its events while the run goes: the
/// fields the run's results carry at the end ([`Collection::value_attr`],
/// [`Collection::row_stamp`], [`Collection::drains`]), as they stand now.
/// Empty and `None` for a request not attached yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Described<'a> {
    pub value_attr: &'static str,
    pub row_stamp: Option<RowStamp>,
    pub drains: &'a [DrainDrop],
}

/// What the escalation is shown of a run in progress.
pub struct Progress<'a> {
    /// When the run opened, on `CLOCK_MONOTONIC`.
    pub opened_ns: u64,
    /// Now, on the same clock.
    pub now_ns: u64,
    /// Every event each request has produced so far, in the order they
    /// arrived. A drained source's arrive on its drain tick, not as they
    /// happen.
    pub events: &'a [Vec<Event>],
    /// Where each request is.
    pub standing: &'a [Standing],
    /// The target each request is bound to: a launched program is its pid.
    pub bound: &'a [Target],
}

/// Where one request is in the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Prepared, and not attached yet.
    Waiting,
    Attached,
    /// Attached once and let go: a request has one window, so it is not
    /// attached again.
    Detached,
    /// Never attached, and why.
    Missed(String),
}

/// One thing the escalation asks of the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switch {
    pub index: usize,
    /// Attach when true, detach when false.
    pub attach: bool,
    /// Why, in the words the request's collection will carry.
    pub why: String,
}

pub(super) fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// A request held for the escalation that this machine cannot serve: the
/// resolver found no backend for it.
///
/// Not a reason to refuse the run. The session asked for it only in case its
/// layer were named, and the cheap pass does not need it; the resolver's
/// reason is what the escalation says if it does name that layer, and until
/// then the run goes on as if the request were a layer this machine lacks --
/// which it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Unserved {
    pub request: crate::request::Request,
    /// Why the resolver could not serve it, in its own words.
    pub reason: String,
}

/// What a recording asks of the runner: a trigger, a retention bound, and
/// where each document goes as it is cut.
///
/// A recording is a mode of a run, not a second runner. The run is the one
/// [`Run`] drives -- the same collectors, the same rounds, the same
/// budget -- with three differences: it keeps only what the retention holds,
/// evicting and counting the rest; it reads one request's rounds against a
/// predicate; and when the predicate fires it hands the retained window out
/// as a [`Document`] and goes on. Its buffers are the events a round has
/// already read into memory, so nothing new is kept in the kernel.
pub struct Recorder<'a> {
    /// The request whose rounds the trigger reads.
    pub on: usize,
    /// The predicate, in the filter grammar. `value` in it is read as the
    /// `on` request's measured value, whatever its collector names it.
    pub trigger: crate::filter::FilterExpr,
    /// How far back a document reaches from its trigger.
    pub retain_ns: u64,
    /// How long a document goes on recording after its trigger.
    pub after_ns: u64,
    /// The most documents the run writes; it ends with the last.
    pub documents: u32,
    /// Where each document goes, as soon as it is cut. An error ends the
    /// run, with the reason: a recorder that cannot write what it recorded
    /// is not recording.
    pub sink: &'a mut dyn FnMut(Document) -> Result<(), String>,
}

/// One document of a recording: the window the retention held when the
/// trigger fired, and the trigger itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub trigger: Fired,
    /// One collection per request, each holding the events of this window
    /// only, with `window` saying where it was.
    pub collections: Vec<Collection>,
}

/// Why a document was written: which predicate fired, on what, when, with
/// which value, and the window it covers.
///
/// Every timestamp is `CLOCK_MONOTONIC`, the clock of every event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fired {
    /// Which document of the run this is, from one.
    pub number: u32,
    /// The most the run was allowed to write.
    pub of_at_most: u32,
    /// The predicate that fired, as the session wrote it.
    pub predicate: String,
    /// The request it read, by label or by source.
    pub on: String,
    /// When the round that fired had read, which is when the run knew.
    pub fired_ns: u64,
    /// The event that made the predicate true, on its own timestamp.
    pub event_ts_ns: u64,
    /// The name the `on` request's collector gives its value, which is what
    /// `value` in the predicate read.
    pub value_attr: String,
    /// That event's value, the one that fired it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Every field the predicate reads, as that event carried it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, crate::event::AttrValue>,
    /// When the run opened, which the window is inside.
    pub run_opened_ns: u64,
    /// Where the window opens: `retain` before the trigger, or the run's
    /// open when the run was younger than that.
    pub opened_ns: u64,
    /// Where it closes: the round the document was cut at, `after` past the
    /// trigger or the end of the run, whichever came first.
    pub closed_ns: u64,
    pub retain_ns: u64,
    pub after_ns: u64,
    /// Set when the run ended before `after` had passed, with why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_short: Option<String>,
    /// Rounds in which the predicate became true again while this document
    /// was open. Their events are in it; they wrote no document of their own.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fired_again: u64,
    /// What the retention evicted since the document before this one, per
    /// request: read by the run and in no document.
    pub evicted: Vec<Evicted>,
}

/// What a recording read and let go for one request, in no document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evicted {
    /// The request, by label or by source.
    pub request: String,
    /// Its events.
    pub events: u64,
    /// The rounds that read them: a poll's reads, a stream's drains.
    pub rounds: u64,
}

/// What a recording did over the whole of its run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Recorded {
    pub predicate: String,
    pub on: String,
    pub retain_ns: u64,
    pub after_ns: u64,
    pub documents_at_most: u32,
    /// Every document written, in order: the trigger half of each.
    pub documents: Vec<Fired>,
    /// Why the run ended: its longest duration, its bound on documents,
    /// the program's exit.
    pub stopped_because: String,
    /// When it opened and closed, on `CLOCK_MONOTONIC`.
    pub opened_ns: u64,
    pub closed_ns: u64,
    /// The span the budget is weighed over while a recording runs, instead
    /// of the whole run: a recorder that idled for an hour must not have
    /// banked an hour of budget for its burst.
    pub budget_interval_ns: u64,
    /// What was read after the last document -- or over the whole run, when
    /// there was none -- and is in no document: evicted, or still retained
    /// when the run ended.
    pub unrecorded: Vec<Evicted>,
}
