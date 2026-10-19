//! Collectors: where events come from.
//!
//! A collector turns one resolved backend into a stream of events. It knows its
//! backend and nothing else -- not the emit, not the output format, not the
//! other collectors. Everything above it works on `Event`, which is what makes
//! adding a source an additive change.
//!
//! Two shapes live behind the same trait. A *polling* collector reads a value
//! whenever it is asked, at the interval the request names. A *streaming* one
//! has the kernel push events into a buffer and is drained often enough that
//! the buffer does not overflow. The runner asks which it is dealing with
//! rather than assuming, because draining a ring once a second loses events and
//! polling procfs every ten milliseconds is just expensive.
//!
//! Loss accounting is part of the interface rather than an afterthought. A
//! collection that loses three percent of its events without saying so produces
//! wrong numbers that look right, and by the time anyone notices the run is
//! gone. So a `Collection` always carries what was lost and why, and the count
//! travels all the way to the user-visible output.

mod cpu;
pub mod gpu;
pub(crate) mod io;
mod launch;
pub(crate) use launch::helper_output;
/// A run's events handed out while it goes: what `omn top` draws from, and
/// what a program embedding `omn` reacts to.
pub mod live;
pub use launch::{adopt_orphans, PROGRAM_STDERR_VARIABLE, PROGRAM_STDOUT_VARIABLE};
pub mod mem;
/// Public for its probes: `net::probe` answers the PTP-clock and ethtool
/// requirements, which the capability layer asks.
pub mod net;
mod power;
mod proc;
mod runtime;
mod sched;
mod task;

/// The one pressure-stall reader, which `cpu`, `io` and `mem` all read through.
pub mod psi;

use std::any::Any;
use std::collections::BTreeMap;
use std::os::fd::RawFd;
use std::time::Duration as StdDuration;

use serde::{Deserialize, Serialize};

use crate::clock::monotonic_ns;
use crate::event::{CollectorId, Event};
use crate::request::{Mode, Target, Window};
use crate::resolve::{FilterPlacement, ResolvedRequest};

pub use launch::Lifetime;
pub use mem::heap::find_shim;

/// Whose gap a refusal is, and therefore what the caller should do about it.
///
/// Three answers that lead to three different next steps -- rewrite the
/// request, run it somewhere else, or wait for someone to write it -- and a
/// caller scripting around them reads the exit code rather than the sentence.
/// Every refusal this layer produces names one of these; nothing is left to be
/// inferred from the wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Whose {
    /// The request cannot mean what it asks for. Rewriting it is the only way
    /// out, and it will fail the same way on every machine and every build.
    Request,
    /// The request is fine and this machine cannot serve it: a counter that
    /// would not open, a file that is not there, a target that went away.
    /// Another machine, or another privilege, may well serve it.
    Machine,
    /// The request is fine, this machine could serve it, and this build has not
    /// written the part that would. Retrying, raising privileges and changing
    /// the machine all change nothing.
    Build,
}

/// How a collector marks whose gap its refusal is.
///
/// A factory's build function, [`Prepared::attach`] and [`Collector::start`]
/// all answer with a `String`, which every collector in nine families returns
/// from a few dozen places. Widening that to a typed error would be a change in
/// fifty files to carry two bits, so the bits travel in front of the sentence
/// instead: one control character no message would ever contain, put there by
/// [`not_built`] or [`cannot_mean_anything`] and taken off again by
/// [`CollectError::start`] before the reason is shown to anyone.
///
/// A refusal that carries no mark is classified by *where* it was raised, which
/// is what [`CollectError::start`] documents.
const MARK_BUILD: char = '\u{1}';
const MARK_REQUEST: char = '\u{2}';

/// Marks a refusal as a gap in this build rather than a failure of this run.
///
/// For the collector that refuses a mode, an emit or a target shape it has not
/// implemented, from somewhere the default classification cannot see -- that
/// is, from `attach` or `start` rather than from the factory. The reason
/// reaches the user unchanged; the exit code becomes 3 rather than 1.
pub fn not_built(reason: impl std::fmt::Display) -> String {
    format!("{MARK_BUILD}{reason}")
}

/// Marks a refusal as a request that cannot mean anything, wherever it is
/// raised.
///
/// A queue depth counted over a window and a since-boot counter read once are
/// not gaps anybody can fill: no build and no machine makes them mean
/// something. The exit code becomes 2, the validator's.
pub fn cannot_mean_anything(reason: impl std::fmt::Display) -> String {
    format!("{MARK_REQUEST}{reason}")
}

/// Why a collection could not start, or could not continue.
#[derive(Debug, thiserror::Error)]
pub enum CollectError {
    #[error("no collector is written for `{source_id}` on backend `{backend}` yet")]
    NoCollector { source_id: String, backend: String },
    #[error("`{source_id}` cannot be collected for a {target} target yet")]
    UnsupportedTarget { source_id: String, target: String },
    #[error(
        "a poll needs to know when to stop: pass --duration, or launch a program with `-- prog`"
    )]
    NoStopCondition,
    #[error("cannot launch {program}: {error}")]
    Launch { program: String, error: String },
    #[error("cannot start collecting `{source_id}`: {reason}")]
    Start {
        source_id: String,
        reason: String,
        /// Whose gap this is. Not in the message: the sentence already says it
        /// in words, and this is the same statement in the shape a script
        /// reads.
        whose: Whose,
    },
    #[error(
        "one run observes one workload -- a program, a process, a cgroup -- beside any number \
         of machine targets (`system`, `cpu`, `iface`), but this session asks for `{first}` and \
         `{second}`; use one session per workload"
    )]
    MixedTargets { first: String, second: String },
    #[error(
        "two collectors disagree about `{variable}` in the launched program's environment \
         (`{first}` and `{second}`)"
    )]
    ConflictingEnvironment {
        variable: String,
        first: String,
        second: String,
    },
    /// Several requests of one session could not be served, each for its own
    /// reason.
    ///
    /// Reported together, because a reader fixing a session should learn about
    /// all of its problems in one run: the validator already answers for every
    /// rejection at once, and stopping at the first missing collector made the
    /// same file take five runs to fix.
    #[error("{}", .refusals.iter().map(CollectError::to_string).collect::<Vec<_>>().join("\n"))]
    Several { refusals: Vec<CollectError> },
    /// A request's window cannot be honoured: by this collector, by this mode
    /// or by this run. Refused before anything is launched, because a window
    /// quietly widened to the whole run is a different measurement under the
    /// same label.
    #[error("`{source_id}` cannot be watched over the window `{window}`: {reason}")]
    Window {
        source_id: String,
        window: String,
        reason: String,
        whose: Whose,
    },
    /// The session's escalation cannot decide anything from what it runs:
    /// no cheap pass to read, or a target it cannot follow. Refused before
    /// the launch, for the reason a window is.
    #[error("the escalation cannot run: {reason}")]
    Escalation { reason: String },
    /// A recording this runner cannot hold as it was handed over. The session
    /// refuses every one of these in its own words before anything is
    /// resolved; this is the same refusal for a caller that built the
    /// requests itself.
    #[error("the recording cannot run: {reason}")]
    Recording { reason: String },
}

impl CollectError {
    /// Builds a `Start` refusal, reading the collector's mark when it left one
    /// and falling back to where the refusal was raised when it did not.
    ///
    /// The fallback is the whole of the classification for most collectors, and
    /// it is not a guess. A factory's build function is a pure function of the
    /// resolved request: nothing has been launched, no counter has been opened
    /// and no file has been read, so a refusal it produces cannot be about this
    /// machine or this run -- it is this build's answer to the request as
    /// written, which is exit 3. A refusal from `attach` or `start` happens
    /// against a real target and is a failure of this run until the collector
    /// says otherwise with [`not_built`].
    fn start(source_id: impl Into<String>, reason: impl Into<String>, unmarked: Whose) -> Self {
        let reason = reason.into();
        let (whose, reason) = match reason.chars().next() {
            Some(MARK_BUILD) => (Whose::Build, reason[MARK_BUILD.len_utf8()..].to_owned()),
            Some(MARK_REQUEST) => (Whose::Request, reason[MARK_REQUEST.len_utf8()..].to_owned()),
            _ => (unmarked, reason),
        };
        CollectError::Start {
            source_id: source_id.into(),
            reason,
            whose,
        }
    }

    /// Whose gap this refusal is, which is what the exit code is derived from.
    ///
    /// The order for a session that failed several ways at once is the order of
    /// the work the reader has to do: a request that cannot mean anything has
    /// to be rewritten before a build that lacks a collector is worth chasing,
    /// and both come before moving to another machine.
    pub fn whose(&self) -> Whose {
        match self {
            CollectError::NoCollector { .. } | CollectError::UnsupportedTarget { .. } => {
                Whose::Build
            }
            CollectError::Start { whose, .. } | CollectError::Window { whose, .. } => *whose,
            // Two collectors wanting different things of one launch is not a
            // request that cannot mean anything: each asked for something real,
            // and it is this build that cannot give a program two answers in
            // one variable. Collectors that can share what they need say so
            // through `Prepared::share_launch`, and what is left here is a gap
            // in that, which is exit 3.
            CollectError::ConflictingEnvironment { .. } => Whose::Build,
            CollectError::NoStopCondition
            | CollectError::MixedTargets { .. }
            | CollectError::Escalation { .. }
            | CollectError::Recording { .. } => Whose::Request,
            CollectError::Launch { .. } => Whose::Machine,
            CollectError::Several { refusals } => refusals
                .iter()
                .map(CollectError::whose)
                .min_by_key(|whose| match whose {
                    Whose::Request => 0,
                    Whose::Build => 1,
                    Whose::Machine => 2,
                })
                .unwrap_or(Whose::Machine),
        }
    }

    /// Every refusal behind this one, which is more than one only for
    /// [`CollectError::Several`].
    pub fn each(&self) -> Vec<&CollectError> {
        match self {
            CollectError::Several { refusals } => refusals.iter().collect(),
            single => vec![single],
        }
    }
}

/// What a run produced, and what it lost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub events: Vec<Event>,
    /// The attribute holding the measured value, named by the collector rather
    /// than guessed from the unit: a source may carry several numbers.
    pub value_attr: String,
    pub losses: Losses,
    /// Wall time actually observed, which is not the requested duration when a
    /// launched program exits early or a target disappears.
    pub observed_ns: u64,
    /// Why the run stopped.
    pub stopped_because: String,
    /// The frames behind each stack identity this run saw, root first.
    ///
    /// Carried here because a stack is interned by the collector that captured
    /// it and rendered by the layer that aggregates it, and neither should have
    /// to know about the other's types. The events name ids; this says what
    /// they were.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stacks: BTreeMap<u64, Vec<String>>,
    /// What this collector had to give up, discovered while running rather than
    /// while resolving: a counter the machine would only open for user mode, a
    /// group the hardware had to time-share. The resolver cannot know these
    /// until the event is opened, and they change what the number means, so
    /// they travel with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// What the request's filter removed, when it had to run here. `None` when
    /// the request carried no filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered: Option<Filtered>,
    /// The backends that produced these events, in the order they served.
    ///
    /// One entry, the resolver's choice, unless a budget moved the collection
    /// onto a cheaper backend part way through; then the series is two
    /// mechanisms end to end and `notes` says when it changed. Recorded here
    /// because the resolved request is written down before the run and cannot
    /// know.
    #[serde(default)]
    pub served_by: Vec<String>,
    /// What this collection cost, measured rather than declared.
    #[serde(default)]
    pub overhead: Overhead,
    /// When the run's launched program ran, from the gate to the reap, on
    /// every collection of a run that launched one -- including those of
    /// machine targets beside it, since they share its window and it is the
    /// whole they would be read against. `None` when nothing was launched.
    ///
    /// Not `observed_ns`, and deliberately beside it: the window opens once
    /// every collector is attached and closes after the last drain, so it runs
    /// past the program by the length of a drain, and a residue smaller than
    /// that difference cannot be told from zero against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime: Option<Lifetime>,
    /// When this collection was watching, which is its own and not the run's
    /// when its request carried a `window`.
    ///
    /// `observed_ns` is the length of this span, so a rate over a source
    /// attached for two seconds of a ten-second run is a rate over two.
    #[serde(default)]
    pub window: Watched,
    /// Whether that window closed at a detach rather than with the run.
    /// Every window closes where its own collector stopped, and the run stops
    /// them in turn, so a close earlier than another's is not by itself a
    /// detach: a join across a source that ran to the end treats it as
    /// watching to the end, and one across a detached source ends its states
    /// at the detach.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    /// The part of `losses` each ring lost, for a collector that reads one
    /// ring per key; empty when the losses belong to the whole alike.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_ring: Vec<RingLoss>,
    /// What the collector states about the run that is not a concession:
    /// [`Collector::facts`]. `notes` is what it had to give up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<String>,
    /// How the collector stamps a row it read from a table the kernel filled
    /// between drains: [`Collector::row_stamp`]. `None` when it does not say,
    /// which is not a claim that each row is stamped at its own event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_stamp: Option<RowStamp>,
    /// What the kernel dropped, placed in time: one entry per read of a
    /// drained collector that found its drop count had grown, with that
    /// read's time. Their sum is `losses.dropped`, so a run that lost a burst
    /// in its first second reads apart from one that lost steadily. Empty for
    /// a collector that is not drained, or that dropped nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drains: Vec<DrainDrop>,
    /// The collector broke the program it measured: [`Collector::failure`].
    /// The run's outcome reads it and exits non-zero with the sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// What the collector counted about the run as numbers a program can
    /// read: [`Collector::fields`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: crate::emit::Fields,
    /// What the stacks' labels cannot say: [`Collector::provenance`].
    #[serde(default, skip_serializing_if = "crate::stack::Provenance::is_empty")]
    pub provenance: crate::stack::Provenance,
}

/// How a drained collector stamps a row it read from a table the kernel
/// filled between two drains -- a row that stands for many events, not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowStamp {
    /// At the close of the drain that read it: the row covers the
    /// `window_ns` before its timestamp.
    DrainClose,
    /// Where the row's first event began, and at the close of the drain that
    /// read it when the kernel recorded no beginning: the row covers from its
    /// timestamp to that close.
    FirstEvent,
}

/// What one read of a drained collector found the kernel had dropped since
/// the read before it, and when that read was taken, on `CLOCK_MONOTONIC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainDrop {
    pub ts_ns: u64,
    pub dropped: u64,
}

/// What one ring of a collection lost: the key the ring belongs to -- the
/// thread a per-thread profile opened it for -- and its share of the
/// collection's losses.
///
/// A series keyed by that key is short by this and by nothing else, which is
/// the point: a profile keyed by `task` whose one thread overflowed its ring
/// used to mark every thread's series with the run's whole drop count, and a
/// reader could not tell which thread was missing samples (`STATE.md`, the
/// note on `cross_compute`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingLoss {
    pub key: crate::event::KeyValue,
    pub losses: Losses,
}

/// The span one collection watched, on `CLOCK_MONOTONIC`.
///
/// Every collection carries one. For a request without a window it is the
/// run's own, from the moment the accounting opened to the moment the last
/// collector was stopped; for a request attached or detached mid-run it is the
/// part of the run it was attached for, and an event of another source outside
/// it was outside *this source's* view rather than outside everything the run
/// saw -- which is what a join across it needs to know.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watched {
    /// When the run itself opened: what the request's window is relative to.
    pub run_opened_ns: u64,
    /// When this collector was attached and first read. Equal to the run's
    /// open for a request attached with the rest.
    pub opened_ns: u64,
    /// When it was stopped: its detach, or the end of the run.
    pub closed_ns: u64,
    /// Whether it was ever attached. A request whose window opened after the
    /// run had ended never was, and its empty series is not a measurement of
    /// nothing.
    pub attached: bool,
    /// Wall time spent attaching and starting it mid-run, while the target
    /// ran. Absent for a collector attached before the launch gate opened,
    /// whose attach is this tool's startup rather than part of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_ns: Option<u64>,
}

/// How a collection's `stopped_because` says it was detached `at_s` seconds
/// into the run, whatever detached it: its window, the budget or the
/// escalation. One spelling, so [`stopped_at_a_detach`] can read it back.
fn detached_at(at_s: f64) -> String {
    format!("detached {at_s:.3} s into the run")
}

/// Whether a stored `stopped_because` says its collection was detached before
/// the run ended, rather than stopped with it: what a join over a stored run
/// needs to end the states still open at its close where the agent tier did.
pub fn stopped_at_a_detach(stopped_because: &str) -> bool {
    let Some((_, after)) = stopped_because.split_once("detached ") else {
        return false;
    };
    after
        .split_once(" s into the run")
        .is_some_and(|(seconds, _)| seconds.parse::<f64>().is_ok())
}

impl Watched {
    /// Whether an event stamped at `ts_ns` fell inside this collection's view.
    pub fn contains(&self, ts_ns: u64) -> bool {
        self.attached && self.opened_ns <= ts_ns && ts_ns <= self.closed_ns
    }

    /// How long this collection watched.
    pub fn span_ns(&self) -> u64 {
        self.closed_ns.saturating_sub(self.opened_ns)
    }
}

/// What a filter the resolver could not push down did to a collection.
///
/// Events a filter removed are not losses: they were asked to go. But they were
/// collected, paid for, and are absent from every number that follows, so the
/// count belongs beside the results for the same reason the loss count does --
/// a series that quietly holds three of forty events looks exactly like a
/// series of three.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filtered {
    /// The predicate, as the request wrote it.
    pub predicate: String,
    pub kept: u64,
    pub dropped: u64,
    /// Fields the predicate names that no event carried. A predicate over a
    /// field nothing produces is false everywhere, which empties the series and
    /// looks exactly like a workload that did nothing -- so the reason is
    /// recorded rather than left to be guessed at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unseen_fields: Vec<String>,
}

/// Everything that did not make it into `events`.
///
/// Zero everywhere is the only reading that means "these numbers are complete".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Losses {
    /// Events the kernel had nowhere to put. Reported by the ring itself.
    pub dropped: u64,
    /// Reads that returned nothing.
    pub failed_reads: u64,
    /// Ticks that started late enough to shift the series.
    pub late_ticks: u64,
    /// Records the reader did not know how to decode. Counted rather than
    /// skipped, because a record shape nobody handles is a gap in the data.
    pub undecoded: u64,
    /// Periods in which the kernel throttled the event because it was costing
    /// too much.
    ///
    /// Deliberately not folded into `dropped`. A drop count is a number of
    /// events the ring reported losing; a throttle is the kernel declining to
    /// deliver anything for a while, and how many events happened inside that
    /// window is not knowable from userspace. Counting one throttle as one
    /// drop claims a precision that does not exist, and is the reason a
    /// throttled run of `sched.switches` under-reported its own loss rather
    /// than saying it could not tell.
    ///
    /// `kernel.perf_event_max_sample_rate` is what decides this, and a run
    /// with a non-zero count here is not comparable to one without.
    pub throttled: u64,
    /// Events a recording read and evicted for its retention bound, before
    /// any document held them: observed, paid for, and in no document.
    ///
    /// Only a recording evicts, and only its documents carry this: each one
    /// counts what was evicted since the document before it. It does not make
    /// the document's distribution a lower bound -- every event of the
    /// document's window is in it -- but it is what the run saw and let go,
    /// and a count nobody is shown is a silent loss.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub evicted: u64,
    /// The reasons behind the counts above, with how often each occurred.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reasons: BTreeMap<String, u64>,
    /// By how much the late ticks were late, when any were.
    ///
    /// Separate from the reason because they are two different things. The
    /// reason is a *kind* -- "a round of reads outran its tick" -- and the map
    /// above counts kinds; the duration is a magnitude, and putting it in the
    /// key made every occurrence its own kind with a count of one. A run that
    /// was late two hundred times then printed two hundred lines, aggregated
    /// none of them, and carried two hundred keys into every structured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrun: Option<Overrun>,
}

/// How far a poll fell behind the cadence it was asked for.
///
/// Three numbers rather than one: the worst is what tells a reader whether the
/// series is usable at all, the total is what says whether being late was the
/// exception or the shape of the whole run, and the tick is what both are
/// against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overrun {
    /// The interval the reads were supposed to keep.
    pub tick_ns: u64,
    /// The longest single round of reads.
    pub worst_ns: u64,
    /// Every late round added up, so a mean can be taken against the count.
    pub total_ns: u64,
    /// The readings that fell due while a late round held the run and were
    /// never taken: the schedule is re-based on the late read, so a round
    /// that held a 250 ms poll for three seconds is one late tick and eleven
    /// readings that never ran. A machine-wide `omn top` starving its
    /// pollers showed as one late tick a poller, and these were counted
    /// nowhere.
    #[serde(default)]
    pub skipped: u64,
}

impl Losses {
    pub fn is_clean(&self) -> bool {
        self.dropped == 0
            && self.failed_reads == 0
            && self.late_ticks == 0
            && self.undecoded == 0
            && self.throttled == 0
            && self.evicted == 0
    }

    /// What was lost since `earlier`, a reading of the same account taken
    /// before: the counts and the reasons by difference. The overrun's
    /// magnitudes cannot be taken apart, so the later ones are kept whole,
    /// which never understates the worst round.
    pub fn since(&self, earlier: &Losses) -> Losses {
        let mut reasons = BTreeMap::new();
        for (reason, count) in &self.reasons {
            let before = earlier.reasons.get(reason).copied().unwrap_or(0);
            if *count > before {
                reasons.insert(reason.clone(), count - before);
            }
        }
        let late_ticks = self.late_ticks.saturating_sub(earlier.late_ticks);
        Losses {
            dropped: self.dropped.saturating_sub(earlier.dropped),
            failed_reads: self.failed_reads.saturating_sub(earlier.failed_reads),
            late_ticks,
            undecoded: self.undecoded.saturating_sub(earlier.undecoded),
            throttled: self.throttled.saturating_sub(earlier.throttled),
            evicted: self.evicted.saturating_sub(earlier.evicted),
            reasons,
            overrun: self.overrun.filter(|_| late_ticks > 0),
        }
    }

    pub fn note(&mut self, reason: impl Into<String>) {
        self.note_times(reason, 1);
    }

    /// Records `reason` as the cause of `times` losses: the call beside a
    /// counter's `+= n`, so that the reasons count what the counters count
    /// and the summary's `{n}x {reason}` reads as that many lost, not as the
    /// number of drains that lost some.
    pub fn note_times(&mut self, reason: impl Into<String>, times: u64) {
        *self.reasons.entry(reason.into()).or_insert(0) += times;
    }

    /// Records one round of reads that outran the interval it was given.
    ///
    /// The count and the magnitudes are kept apart here and joined again in
    /// [`Losses::summarize`], once, when the run is over. Formatting the
    /// duration into the reason at this point is what made a late run print one
    /// line per late tick -- see [`Losses::overrun`].
    pub fn late_tick(&mut self, spent_ns: u64, tick_ns: u64) {
        self.late_ticks += 1;
        let overrun = self.overrun.get_or_insert(Overrun {
            tick_ns,
            ..Overrun::default()
        });
        overrun.tick_ns = tick_ns;
        overrun.worst_ns = overrun.worst_ns.max(spent_ns);
        overrun.total_ns += spent_ns;
        // The readings due at one tick, two ticks, ... after the late one's
        // own due time and before it finished.
        overrun.skipped += spent_ns.saturating_sub(1) / tick_ns.max(1);
    }

    /// Turns the accumulated magnitudes into the one reason a reader sees.
    ///
    /// Called once, after everything has been absorbed, so that the reason is a
    /// single key carrying the count it explains -- `77x a round of reads
    /// outran the 1ms tick` -- with the worst and the average beside it rather
    /// than in place of it.
    pub fn summarize(&mut self) {
        let Some(overrun) = self.overrun else {
            return;
        };
        if self.late_ticks == 0 {
            return;
        }
        let mean_ns = overrun.total_ns / self.late_ticks;
        let mut reason = format!(
            "a round of reads outran the {} tick: worst {}, {} on average",
            span_ns(overrun.tick_ns),
            span_ns(overrun.worst_ns),
            span_ns(mean_ns),
        );
        if overrun.skipped > 0 {
            reason.push_str(&format!(
                "; {} more reading{} fell due meanwhile and never ran",
                overrun.skipped,
                if overrun.skipped == 1 { "" } else { "s" },
            ));
        }
        self.note_times(reason, self.late_ticks);
    }

    /// Records one throttled period, with what the reader needs to know about
    /// it. The count is of periods, not of events: the events are the part
    /// nobody can count.
    pub fn throttle(&mut self, reason: impl Into<String>) {
        self.throttled += 1;
        self.note(reason);
    }

    fn absorb(&mut self, other: Losses) {
        self.dropped += other.dropped;
        self.failed_reads += other.failed_reads;
        self.late_ticks += other.late_ticks;
        self.undecoded += other.undecoded;
        self.throttled += other.throttled;
        self.evicted += other.evicted;
        self.overrun = match (self.overrun, other.overrun) {
            (Some(mine), Some(theirs)) => Some(Overrun {
                // The two halves of one collection are driven at one cadence,
                // so the ticks agree; the larger is kept rather than averaged
                // so that the reason never understates what it is against.
                tick_ns: mine.tick_ns.max(theirs.tick_ns),
                worst_ns: mine.worst_ns.max(theirs.worst_ns),
                total_ns: mine.total_ns + theirs.total_ns,
                skipped: mine.skipped + theirs.skipped,
            }),
            (mine, theirs) => mine.or(theirs),
        };
        for (reason, count) in other.reasons {
            *self.reasons.entry(reason).or_insert(0) += count;
        }
    }
}

/// Where a budget moved a collection, and the span in which the backend it
/// left and the one it moved onto were both observing the target.
struct Moved {
    onto: String,
    /// The mode it was moved into, when the move kept the backend and changed
    /// the mode (`trace` -> `sample`): then the backend alone does not say
    /// what changed.
    into_mode: Option<String>,
    overlap_from_ns: u64,
    overlap_to_ns: u64,
}

/// Nanoseconds a reader can take in at a glance, for the loss reasons.
///
/// Deliberately not the raw count. "a round of reads took 104754353ns" is a
/// number nobody reads as a tenth of a second, and the reason line exists to be
/// read rather than parsed -- the structured output carries the nanoseconds.
fn span_ns(ns: u64) -> String {
    match ns {
        ns if ns < 1_000 => format!("{ns}ns"),
        ns if ns < 1_000_000 => format!("{:.1}us", ns as f64 / 1e3),
        ns if ns < 1_000_000_000 => format!("{:.1}ms", ns as f64 / 1e6),
        ns => format!("{:.2}s", ns as f64 / 1e9),
    }
}

/// What a run cost, measured rather than declared.
///
/// The catalog declares a cost *band* per backend -- `free`, `low`, `med` --
/// and until now that was a claim nobody had checked. This is the measurement
/// behind it: the cpu time this process burned collecting, against the cpu time
/// the thing being observed burned over the same window.
///
/// The denominator is the target's own cpu time on purpose. "Two percent
/// overhead" means "the measurement cost two percent of what the workload
/// cost"; a fraction of wall time would answer a different question and would
/// let the same collector read as cheap because the machine happened to be
/// idle. A target that burned no cpu, or that has no cpu account at all like a
/// network interface, has no denominator -- and that is said rather than
/// divided by.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Overhead {
    /// User plus system cpu time this whole process spent between the moment
    /// the collectors were attached and the moment the window closed.
    /// `getrusage(RUSAGE_SELF)`, differenced.
    pub collector_cpu_ns: u64,
    /// The share of `collector_cpu_ns` this collection is answerable for.
    ///
    /// One process serves every request in a session and `getrusage` cannot say
    /// which of them spent what, so the share is the fraction of the time spent
    /// inside `sample` that went to this collector. That is wall time, and
    /// therefore an approximation -- exactly equal to the whole when a session
    /// carries one request, which is the common case, and reported as a share
    /// when it does not.
    pub attributed_cpu_ns: u64,
    /// Cpu time the observed target spent over the same window.
    pub observed_cpu_ns: Option<u64>,
    /// Where the number above came from, or why there is none. It travels with
    /// the number because `getrusage(RUSAGE_CHILDREN)` and `/proc/<pid>/stat`
    /// and `/proc/stat` are three different accounts, and a reader comparing
    /// two runs needs to know which one they are reading.
    pub observed_from: String,
    /// Wall time of the window.
    pub wall_ns: u64,
    /// What measuring all of this cost: the `getrusage` calls, the target's
    /// procfs reads, the clock reads that share time out per collector, and --
    /// when a ceiling was exceeded -- moving the collection onto a cheaper
    /// backend. Enforcing a budget is part of the price of having one. Cpu
    /// time of the thread that did it, not wall time: a weighing preempted on a
    /// loaded machine did not cost the wait.
    ///
    /// Reported rather than subtracted. A measurement that hides its own price
    /// is exactly the thing this field exists to stop, and a budget check that
    /// ran often enough to dominate the answer would show up here first.
    pub measurement_ns: u64,
    /// How many times the cost was read during the run.
    pub measurements: u64,
    /// Cpu time the collector's own code spent inside the target's context,
    /// where the tool's own account cannot see it: an eBPF program run on
    /// every syscall the target makes, the preload shim's record on every
    /// allocation (finding 47). `Some(0)` for a collector that reads from
    /// outside the target; `None` when it runs inside and what it spent there
    /// is not knowable on this run, which `in_target_from` says why --
    /// **unknown, never zero**.
    #[serde(default)]
    pub in_target_cpu_ns: Option<u64>,
    /// How many times that code ran inside the target, when it is counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_target_runs: Option<u64>,
    /// How many of those runs are in `in_target_cpu_ns` at the mean of the
    /// others rather than as timed: the preload shim's calls a preemption fell
    /// in, whose spans are the scheduler's slice and not the hook's. Counted
    /// and said, because an account that silently left them out would read
    /// low with nothing to show it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub in_target_left_out: u64,
    /// Where `in_target_cpu_ns` came from, or why there is none.
    #[serde(default)]
    pub in_target_from: String,
    /// This process's cpu before this collection's span opened -- the span
    /// `collector_cpu_ns` weighs -- and not before the target ran.
    /// `getrusage(RUSAGE_SELF)` at the span's opening. For a collection open
    /// from the target's start that is the tool's own preparation: its start,
    /// the catalog, the probes, the attach, and whatever table the run was
    /// going to need and read then so that no drain would pay for it -- the
    /// kernel's symbols, for a profile with stacks, which a debug build parses
    /// in the better part of a second. For one attached mid-run, or detached
    /// and reopened, it also holds what every other collection's window cost
    /// before this one's opened.
    ///
    /// Outside the percentage on purpose: none of it is this collection's
    /// dilation. Reported, because a reader told a profile cost 3 % of its
    /// target, where the run cost the machine more than the target did, has
    /// been told something false (finding U-cost, fourteenth wave).
    #[serde(default)]
    pub before_window_cpu_ns: u64,
    /// This process's cpu after the span `collector_cpu_ns` weighs, up to the
    /// collection being handed back: what the runner did once the window was
    /// weighed -- the stacks, the filter, the other requests' tails. What
    /// formatting the results costs after that is not in it.
    #[serde(default)]
    pub after_window_cpu_ns: u64,
    /// The part of `attributed_cpu_ns` spent once rather than at a rate:
    /// loading a symbol table the first time a frame landed in its module.
    /// Weighed apart -- outside [`Overhead::percent`] and the budget's checks
    /// -- and said apart, because a load of tens of milliseconds read against
    /// the few milliseconds a waiting program burns by the first check is a
    /// percentage of how early the check came, not of what running costs.
    /// Still in `collector_cpu_ns` and [`Overhead::tool_cpu_ns`]: what the
    /// run cost is what it cost.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub one_time_cpu_ns: u64,
}

impl Overhead {
    /// What this collection cost, as a percentage of the work the target did
    /// of its own: the tool's cpu and what the collector spent inside the
    /// target, over the target's cpu less that second part -- which the
    /// target's account already holds, since it ran in the target's context.
    /// That is the dilation a program that times itself would see.
    ///
    /// Where the in-target part is unknown this is the tool's share alone, and
    /// [`Overhead::in_target_known`] is what says it is incomplete. Where the
    /// in-target part is larger than everything the target spent -- a program
    /// that ran for every task on the machine and filtered most of them out --
    /// the target's whole account is the denominator, which never understates.
    pub fn percent(&self) -> Option<f64> {
        let observed = self.observed_cpu_ns.filter(|observed| *observed > 0)?;
        let inside = self.in_target_cpu_ns.unwrap_or(0);
        let work = match observed.checked_sub(inside) {
            Some(work) if work > 0 => work,
            _ => observed,
        };
        Some((self.running_cpu_ns() + inside) as f64 * 100.0 / work as f64)
    }

    /// The tool's share less what it spent once: the rate a budget weighs.
    pub fn running_cpu_ns(&self) -> u64 {
        self.attributed_cpu_ns.saturating_sub(self.one_time_cpu_ns)
    }

    /// Everything this process spent, by its own kernel account, from its
    /// start to the collection being handed back: before the window, in it,
    /// and after. What a `wait4` over the tool reports, less its target and
    /// the formatting of the results.
    ///
    /// A process total, not this collection's share: every collection of one
    /// run carries the same figure, up to when each was handed back, so the
    /// figures of a run's collections are compared, never summed.
    pub fn tool_cpu_ns(&self) -> u64 {
        self.before_window_cpu_ns + self.collector_cpu_ns + self.after_window_cpu_ns
    }

    /// The tool's own share of the target's cpu, as every run before finding
    /// 47 reported it.
    pub fn own_percent(&self) -> Option<f64> {
        let observed = self.observed_cpu_ns.filter(|observed| *observed > 0)?;
        Some(self.running_cpu_ns() as f64 * 100.0 / observed as f64)
    }

    /// Whether what the collector spent inside the target is part of
    /// [`Overhead::percent`].
    pub fn in_target_known(&self) -> bool {
        self.in_target_cpu_ns.is_some()
    }
}

/// What a collector's own code has spent inside its target so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Inside {
    pub cpu_ns: u64,
    pub runs: u64,
    /// How many of those runs are in `cpu_ns` at an estimate rather than as
    /// timed: the preload shim's calls a preemption fell in, whose spans were
    /// the scheduler's, priced at the mean of the others.
    pub left_out: u64,
}

impl std::ops::Add for Inside {
    type Output = Inside;
    fn add(self, other: Inside) -> Inside {
        Inside {
            cpu_ns: self.cpu_ns + other.cpu_ns,
            runs: self.runs + other.runs,
            left_out: self.left_out + other.left_out,
        }
    }
}

/// A collector's account of what it spends inside its target, for a
/// collector whose code runs there and that keeps one: the preload shim
/// publishes its own time in the ring it shares with `omn`.
///
/// Cumulative since the target started: the runner differences it.
pub trait InsideAccount {
    /// What has been spent so far, or why it cannot be said.
    fn read(&self) -> Result<Inside, String>;
    /// Where the number comes from, for the record beside it.
    fn describes(&self) -> String;
}

/// Whether the kernel is counting eBPF programs' run time for this run.
///
/// It counts only while `bpf_stats_enabled_key` is on -- the
/// `kernel.bpf_stats_enabled` sysctl, or a descriptor from `BPF_ENABLE_STATS`
/// held open -- because timing each run costs two clock reads. Off by default,
/// and turning it on needs `CAP_SYS_ADMIN`, which the `CAP_BPF` this tool is
/// granted does not include.
enum ProgramStats {
    /// Nothing in the run loads a program.
    NotAsked,
    /// Counting, with the descriptor that keeps it on when this run turned it
    /// on, and without one when the sysctl already was.
    On(Option<std::os::fd::OwnedFd>),
    /// Not counting, with why and what would change it.
    Off(String),
}

fn bpf_stats_sysctl() -> Option<bool> {
    std::fs::read_to_string("/proc/sys/kernel/bpf_stats_enabled")
        .ok()
        .map(|text| text.trim() != "0")
}

impl ProgramStats {
    /// Whether a program loaded now is timed, and why not when it is not.
    fn counting(&self) -> Result<(), String> {
        match self {
            ProgramStats::On(_) => Ok(()),
            ProgramStats::Off(why) => Err(why.clone()),
            ProgramStats::NotAsked => Err(
                "this run did not ask the kernel to time eBPF programs, because none of its \
                 requests started on a backend that loads one"
                    .to_owned(),
            ),
        }
    }

    /// Whether the sysctl this run relied on was turned off under it.
    fn lost(&self) -> Option<String> {
        match self {
            ProgramStats::On(None) if bpf_stats_sysctl() == Some(false) => Some(
                "`kernel.bpf_stats_enabled` was turned off during the run, so the kernel stopped \
                 timing the programs part way through and their run time is incomplete"
                    .to_owned(),
            ),
            _ => None,
        }
    }

    fn for_run(resolved: &[ResolvedRequest]) -> ProgramStats {
        if !resolved.iter().any(|one| loads_programs(&one.backend.id)) {
            return ProgramStats::NotAsked;
        }
        if bpf_stats_sysctl() == Some(true) {
            return ProgramStats::On(None);
        }
        // The layer's own call (`src/bpf/mod.rs`), not a copy of it here.
        let error = match crate::bpf::enable_run_time_stats() {
            Ok(fd) => return ProgramStats::On(Some(fd)),
            Err(error) => error,
        };
        let sysctl = match bpf_stats_sysctl() {
            Some(_) => "`kernel.bpf_stats_enabled` is 0",
            None => "this kernel has no `kernel.bpf_stats_enabled`",
        };
        let errno = match error.raw_os_error() {
            Some(libc::EPERM) => "EPERM: it needs CAP_SYS_ADMIN".to_owned(),
            _ => error.to_string(),
        };
        ProgramStats::Off(format!(
            "the kernel is not timing eBPF programs on this run: {sysctl}, and BPF_ENABLE_STATS \
             was refused ({errno}); `sysctl kernel.bpf_stats_enabled=1` before the run makes \
             what the programs spend inside the target measurable"
        ))
    }
}

/// Whether a backend works by loading eBPF programs, whose run time is then
/// the kernel's to report.
fn loads_programs(backend: &str) -> bool {
    backend == "bpf" || backend.starts_with("bpf:")
}

/// An eBPF collector's task ids, written by the kernel in its own numbering,
/// rewritten into the one every other collector uses (`bpf::pidns`), and what
/// could not be said so.
fn into_our_ids(serving: Option<&ResolvedRequest>, events: &mut [Event], losses: &mut Losses) {
    if !serving.is_some_and(|serving| loads_programs(&serving.backend.id)) {
        return;
    }
    // Nothing handed over may be rows still in the kernel, whose tasks must
    // be seen alive to be translated when they come (`pidns::remember`).
    if events.is_empty() {
        crate::bpf::pidns::remember();
        return;
    }
    let missed = crate::bpf::pidns::translate(events);
    if missed > 0 {
        losses.note(crate::bpf::pidns::untranslated_note(missed));
    }
}

/// The eBPF programs this process holds a descriptor to, by descriptor, with
/// their id and what the kernel has counted of each.
///
/// Read from `/proc/self/fdinfo`, which prints `prog_id`, `run_time_ns` and
/// `run_cnt` for a program and no run time for a link or a map -- so whoever
/// loaded the program does not matter: the fentry trampolines load their own,
/// outside `src/bpf/syscall.rs`, and are found the same way.
fn programs_held() -> BTreeMap<RawFd, (u32, Inside)> {
    let Ok(entries) = std::fs::read_dir("/proc/self/fdinfo") else {
        return BTreeMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<RawFd>().ok())
        .filter_map(|fd| program_run(fd).map(|run| (fd, run)))
        .collect()
}

/// What the kernel has counted of the program behind `fd`, with its id; `None`
/// when the descriptor is closed or is not a program.
fn program_run(fd: RawFd) -> Option<(u32, Inside)> {
    let text = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).ok()?;
    parse_program_fdinfo(&text)
}

fn parse_program_fdinfo(text: &str) -> Option<(u32, Inside)> {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim().parse::<u64>().ok())?
        })
    };
    // A link prints the id of the program it holds; only a program prints its
    // own run time.
    let cpu_ns = field("run_time_ns")?;
    Some((
        field("prog_id")? as u32,
        Inside {
            cpu_ns,
            runs: field("run_cnt").unwrap_or(0),
            left_out: 0,
        },
    ))
}

/// The programs that appeared among this process's descriptors since
/// `before`: what one collector's prepare or attach loaded.
fn programs_since(before: Option<&BTreeMap<RawFd, (u32, Inside)>>) -> Vec<(RawFd, u32)> {
    let Some(before) = before else {
        return Vec::new();
    };
    let known: std::collections::BTreeSet<u32> = before.values().map(|(id, _)| *id).collect();
    let mut seen = std::collections::BTreeSet::new();
    programs_held()
        .into_iter()
        .filter(|(_, (id, _))| !known.contains(id) && seen.insert(*id))
        .map(|(fd, (id, _))| (fd, id))
        .collect()
}

/// Where a collection's in-target cost is read from.
enum InsideMeter {
    /// Nothing of the collector runs in the target's context.
    Outside(&'static str),
    /// It does, and what it spends there cannot be known on this run.
    Unknown(String),
    /// The eBPF programs it loaded, read through the kernel's own account.
    Programs {
        held: Vec<(RawFd, u32)>,
        /// The last reading of each, kept for when its descriptor has closed.
        last: Vec<Inside>,
        /// What the backends before a degradation spent.
        sunk: Inside,
    },
    /// An account the collector keeps itself.
    Reported {
        account: Box<dyn InsideAccount>,
        last: Inside,
        sunk: Inside,
    },
}

const OUTSIDE: &str = "nothing of this collector runs in the target's context: it reads the \
                       kernel's account, or a file, from its own";

const PMU_INSIDE: &str = "a perf event's counter is saved and restored on every context switch \
                          of the target, and a sampling event interrupts it, in the target's own \
                          context; the kernel does not account that time apart from the target's";

impl InsideMeter {
    /// How a collection's in-target cost is read, decided by its mechanism:
    /// the account the collector keeps, the programs it loaded, or what its
    /// backend is known to do.
    fn of(
        backend: &str,
        account: Option<Box<dyn InsideAccount>>,
        programs: Vec<(RawFd, u32)>,
        stats: &Result<(), String>,
    ) -> InsideMeter {
        if let Some(account) = account {
            return InsideMeter::Reported {
                account,
                last: Inside::default(),
                sunk: Inside::default(),
            };
        }
        if loads_programs(backend) {
            return match stats {
                Err(why) => InsideMeter::Unknown(why.clone()),
                Ok(()) if programs.is_empty() => InsideMeter::Unknown(
                    "this collector's eBPF programs were not among the descriptors this process \
                     holds, so the kernel's account of them cannot be read"
                        .to_owned(),
                ),
                _ => InsideMeter::Programs {
                    last: vec![Inside::default(); programs.len()],
                    held: programs,
                    sunk: Inside::default(),
                },
            };
        }
        let unknown = |why: &str| InsideMeter::Unknown(why.to_owned());
        match backend.split(':').next().unwrap_or(backend) {
            "pmu" => unknown(PMU_INSIDE),
            "preload" => unknown(
                "a preloaded library runs inside the target, and this one keeps no account of \
                 its own time",
            ),
            "af_packet" => unknown(
                "the kernel copies each packet into the capture ring in whichever context \
                 received or sent it, which for a local program is often its own",
            ),
            "so_timestamping" => unknown(
                "a timestamping socket option makes the kernel stamp the target's own packets \
                 in its context",
            ),
            "vendor" if backend != "vendor:nvml" => unknown(
                "a vendor profiler injects into or replays the target, and says nothing of what \
                 that cost it",
            ),
            _ if backend == "fs:proc:clear_refs" || backend == "fs:sys:page_idle" => unknown(
                "clearing the pages' referenced bits makes the target take the faults or the \
                 TLB refills that set them again, in its own context",
            ),
            _ => InsideMeter::Outside(OUTSIDE),
        }
    }

    /// What has been spent inside the target so far, or why it cannot be
    /// said. Keeps each reading, so a program whose descriptor has since
    /// closed still counts what it last reported.
    fn read(&mut self) -> Result<Inside, String> {
        match self {
            InsideMeter::Outside(_) => Ok(Inside::default()),
            InsideMeter::Unknown(why) => Err(why.clone()),
            InsideMeter::Programs { held, last, sunk } => {
                for ((fd, id), last) in held.iter().zip(last.iter_mut()) {
                    if let Some((now_id, run)) = program_run(*fd) {
                        if now_id == *id {
                            *last = run;
                        }
                    }
                }
                Ok(last.iter().fold(*sunk, |total, one| total + *one))
            }
            InsideMeter::Reported {
                account,
                last,
                sunk,
            } => {
                *last = account.read()?;
                Ok(*sunk + *last)
            }
        }
    }

    /// The same reading, without keeping it.
    fn peek(&self) -> Result<Inside, String> {
        match self {
            InsideMeter::Programs { held, last, sunk } => {
                Ok(held
                    .iter()
                    .zip(last)
                    .fold(*sunk, |total, ((fd, id), last)| {
                        let now = program_run(*fd)
                            .filter(|(now_id, _)| now_id == id)
                            .map_or(*last, |(_, run)| run);
                        total + now
                    }))
            }
            InsideMeter::Reported { account, sunk, .. } => account.read().map(|now| *sunk + now),
            InsideMeter::Outside(_) => Ok(Inside::default()),
            InsideMeter::Unknown(why) => Err(why.clone()),
        }
    }

    /// Where the number comes from.
    fn describes(&self) -> String {
        match self {
            InsideMeter::Outside(why) => (*why).to_owned(),
            InsideMeter::Unknown(why) => why.clone(),
            InsideMeter::Programs { held, .. } => format!(
                "the kernel's own run time of the {} eBPF program(s) this collector loaded \
                 (bpf_prog_info run_time_ns and run_cnt, through /proc/self/fdinfo). A lower \
                 bound of what the target paid: the kernel times the program and not the \
                 tracepoint, trampoline or probe that calls it, nor the slower path an attached \
                 syscall tracepoint puts every task on. And it counts every run, including the \
                 ones for other tasks that the program filtered out",
                held.len()
            ),
            InsideMeter::Reported { account, .. } => account.describes(),
        }
    }

    /// The meter of the backend a budget moved this collection onto, carrying
    /// what the one before it spent.
    fn moved_onto(&mut self, mut next: InsideMeter) {
        let spent = self.read();
        let before = match self {
            InsideMeter::Outside(_) => Ok(Inside::default()),
            _ => spent,
        };
        match (&mut next, before) {
            (InsideMeter::Programs { sunk, .. } | InsideMeter::Reported { sunk, .. }, Ok(v)) => {
                *sunk = v;
            }
            (InsideMeter::Outside(_), Ok(v)) if v.cpu_ns > 0 || v.runs > 0 => {
                next = InsideMeter::Unknown(
                    "the backend this collection started on spent time inside the target that \
                     the one it moved onto has no account to add to"
                        .to_owned(),
                );
            }
            (_, Err(why)) => {
                next = InsideMeter::Unknown(format!(
                    "before a budget moved this collection, its first backend's cost inside the \
                     target was unknown: {why}"
                ));
            }
            _ => {}
        }
        *self = next;
    }
}

/// The cpu time the calling thread has been charged, in nanoseconds.
///
/// What weighing a run costs is timed on this clock rather than the monotonic
/// one: the price of a `getrusage` and a procfs read is the cpu they take, and
/// a weighing that is preempted half way through did not cost the time it
/// spent waiting for a core -- which on a loaded machine is what the monotonic
/// clock mostly measured (the flaky tally, #12: 12.3 ms of "weighing" in a
/// 605 ms run).
fn thread_cpu_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes a `timespec` this function owns.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
    (time.tv_sec as u64) * 1_000_000_000 + (time.tv_nsec as u64)
}

/// User plus system cpu time of one `getrusage` account, in nanoseconds.
fn rusage_ns(who: libc::c_int) -> u64 {
    // SAFETY: `getrusage` fills a struct this function owns, and writes nothing
    // else.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(who, &mut usage) } != 0 {
        return 0;
    }
    let ns =
        |time: libc::timeval| (time.tv_sec as u64) * 1_000_000_000 + (time.tv_usec as u64) * 1_000;
    ns(usage.ru_utime) + ns(usage.ru_stime)
}

/// The kernel's tick, which is what `/proc` reports cpu time in.
fn ticks_per_second() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one.
    match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
        ticks if ticks > 0 => ticks as u64,
        _ => 100,
    }
}

/// Cpu time one task has consumed, its reaped children included.
///
/// `task::process_cpu`: the process cpu clock in nanoseconds plus `cutime` and
/// `cstime` -- a shell that forks is still the workload that was asked for,
/// and leaving its children out would make every collector look expensive
/// against it. `/proc/<pid>/stat`'s ticks only where the clock cannot be read.
fn task_cpu_ns(pid: i32) -> Option<u64> {
    task::process_cpu(pid).map(|read| read.ns)
}

/// The cpu time one thread has been charged, its own and no other's, from
/// its `schedstat` in nanoseconds (`task::thread_cpu`).
fn thread_cpu_of(tid: i32) -> Option<u64> {
    task::thread_cpu(tid).map(|read| read.ns)
}

/// Cpu time the machine, or a set of cpus, spent on anything but idling.
///
/// `iowait` is idle: a cpu waiting for a disk is available to anything else
/// that wants it, and counting it as work would inflate the denominator and
/// make every collector look cheap on a machine that is mostly blocked.
fn cpus_busy_ns(wanted: Option<&[u32]>) -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let mut total = 0u64;
    let mut matched = false;
    for line in stat.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            break;
        };
        let (label, numbers) = match rest.split_once(char::is_whitespace) {
            Some(split) => split,
            None => continue,
        };
        match (wanted, label.parse::<u32>()) {
            // The summary line, which is what a machine-wide target wants.
            (None, Err(_)) => {}
            (Some(wanted), Ok(cpu)) if wanted.contains(&cpu) => {}
            _ => continue,
        }
        let fields: Vec<u64> = numbers
            .split_whitespace()
            .map(|field| field.parse().unwrap_or(0))
            .collect();
        // user nice system idle iowait irq softirq steal guest guest_nice.
        // The two guest columns are already counted inside user and nice, so
        // summing the line and taking the idle columns off would count a
        // virtualised machine's work twice; the busy columns are named instead.
        const BUSY: [usize; 6] = [0, 1, 2, 5, 6, 7];
        total += BUSY
            .iter()
            .filter_map(|column| fields.get(*column))
            .sum::<u64>();
        matched = true;
    }
    matched.then(|| total * 1_000_000_000 / ticks_per_second())
}

/// Cpu time a cgroup has been charged, from `cpu.stat`.
fn cgroup_cpu_ns(directory: &std::path::Path) -> Option<u64> {
    let stat = std::fs::read_to_string(directory.join("cpu.stat")).ok()?;
    stat.lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|usec| usec * 1_000)
}

/// Resolves a cgroup target to the directory that holds its `cpu.stat`.
///
/// A target may be written the way `/proc/self/cgroup` writes it, rooted at the
/// hierarchy, or as the mounted path; both name the same cgroup. The same
/// two-way reading is in `cpu/throttle.rs`, which is where it was worked out.
fn cgroup_directory(path: &std::path::Path) -> std::path::PathBuf {
    const ROOT: &str = "/sys/fs/cgroup";
    if path.starts_with(ROOT) {
        path.to_path_buf()
    } else {
        std::path::Path::new(ROOT).join(path.strip_prefix("/").unwrap_or(path))
    }
}

/// The highest cumulative reading a meter has given, so that it never goes
/// back: a cumulative account read as a sum over several processes can come
/// out short for one read (`task::tree_cpu`), and a denominator that shrank
/// would make the tool's cost jump for no reason of its own.
///
/// Equal to every other: two meters of the same target are the same meter
/// whatever each has read so far (`Meter::read_each`).
#[derive(Default)]
struct HighWater(std::cell::Cell<u64>);

impl HighWater {
    fn keep(&self, read: u64) -> u64 {
        let kept = self.0.get().max(read);
        self.0.set(kept);
        kept
    }
}

impl PartialEq for HighWater {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// Reads the cpu time of whatever is being observed, so that the tool's own
/// cost can be a fraction of something real.
#[derive(PartialEq)]
enum Meter {
    /// A program this tool launched, and the processes it started. While it
    /// runs, its own `/proc` entry and its descendants' say what the tree has
    /// spent (`task::tree_cpu`); once it has been reaped its entry is gone and
    /// the kernel has rolled its usage, and that of every child it reaped,
    /// into this process's children account, which is the same number `time`
    /// prints and is read from there instead.
    ///
    /// The tree, not the launched process alone: a compiler driver that spent
    /// 4 ms in `wait4` while `cc1` spent seconds made a budget read 8581 % of
    /// "the target's cpu" (`docs/fixes/w17-build.md`), and the collectors that
    /// follow a target's children were watching `cc1` the whole time. A
    /// child's cpu was already in the denominator once it was reaped; this
    /// counts it while it runs.
    Launched(i32, HighWater),
    /// Processes the request named.
    Tasks(Vec<i32>),
    /// Threads the request named, each weighed on its own account.
    Threads(Vec<i32>),
    /// The machine, or the cpus the request named.
    Cpus(Option<Vec<u32>>),
    Cgroup(std::path::PathBuf),
    /// Nothing here keeps a cpu account, with the reason.
    Nothing(&'static str),
}

impl Meter {
    fn of(target: &Target, launched: Option<i32>) -> Meter {
        match target {
            Target::Launch(_) => match launched {
                Some(pid) => Meter::Launched(pid, HighWater::default()),
                None => Meter::Nothing("the program was never launched"),
            },
            Target::Pid(pids) => Meter::Tasks(pids.clone()),
            Target::Tid(tids) => Meter::Threads(tids.clone()),
            Target::Cgroup(path) => Meter::Cgroup(cgroup_directory(path)),
            Target::Cpu(set) => Meter::Cpus(Some(set.cpus().to_vec())),
            Target::System => Meter::Cpus(None),
            Target::Iface(_) => Meter::Nothing(
                "a network interface consumes no cpu time of its own, so there is nothing \
                 for an overhead to be a percentage of",
            ),
            Target::Container(_) => Meter::Nothing(
                "a container has to be resolved to its cgroup before its cpu time can be \
                 read, which is not built yet",
            ),
        }
    }

    /// Every meter's [`Meter::read`], a meter equal to an earlier one taking
    /// that one's reading rather than its own. Two collections of one target
    /// are weighed against one denominator: read twice, a running target's
    /// nanosecond clock (`task::process_cpu`) moves between the two reads.
    ///
    /// The meter that took an earlier one's reading keeps it as its own high
    /// water too, so that a later [`Meter::read`] of it alone (a collection
    /// detached, or read after the program was killed) is not short of what
    /// the first one saw.
    fn read_each(meters: &[Meter]) -> Vec<Option<u64>> {
        let mut read: Vec<Option<u64>> = Vec::with_capacity(meters.len());
        for (index, meter) in meters.iter().enumerate() {
            let earlier = meters[..index].iter().position(|other| other == meter);
            read.push(match earlier {
                Some(earlier) => meter.agree(read[earlier]),
                None => meter.read(),
            });
        }
        read
    }

    /// Takes `read`, an equal meter's reading, as this one's: a launched
    /// program's meter keeps it as its high water and gives the kept value.
    fn agree(&self, read: Option<u64>) -> Option<u64> {
        match (self, read) {
            (Meter::Launched(_, high), Some(read)) => Some(high.keep(read)),
            _ => read,
        }
    }

    /// The cumulative cpu time of the target, to be differenced against a
    /// baseline taken when the window opened.
    fn read(&self) -> Option<u64> {
        match self {
            Meter::Launched(pid, high) => {
                // Adopting, every descendant of this process is the program's
                // -- its tree, and the orphans that left it while they live --
                // and every child reaped is in this process's children
                // account: one walk, and no orphan's work missing from the
                // denominator while it runs.
                // The helpers `omn` runs itself (`launch::helper_output`) are
                // its children too, and are no part of the program: the live
                // ones are left out of the walk, and what the reaped ones spent
                // out of the children account -- read after the account, so
                // that a helper reaped between the two reads is not counted.
                let children = || {
                    let account = rusage_ns(libc::RUSAGE_CHILDREN);
                    account.saturating_sub(launch::helpers_spent_ns())
                };
                let spent = if launch::adopting() {
                    // SAFETY: getpid cannot fail.
                    children()
                        + task::descendants_cpu(
                            unsafe { libc::getpid() },
                            &launch::helpers_running(),
                        )
                } else {
                    task::tree_cpu(*pid)
                        .map(|read| read.ns)
                        .unwrap_or_else(children)
                };
                Some(high.keep(spent))
            }
            Meter::Tasks(pids) => {
                let mut total = 0;
                let mut any = false;
                for pid in pids {
                    if let Some(cpu_ns) = task_cpu_ns(*pid) {
                        total += cpu_ns;
                        any = true;
                    }
                }
                any.then_some(total)
            }
            Meter::Threads(tids) => {
                let times: Vec<u64> = tids.iter().filter_map(|tid| thread_cpu_of(*tid)).collect();
                (!times.is_empty()).then(|| times.iter().sum())
            }
            Meter::Cpus(set) => cpus_busy_ns(set.as_deref()),
            Meter::Cgroup(directory) => cgroup_cpu_ns(directory),
            Meter::Nothing(_) => None,
        }
    }

    /// What to record beside the number, so a reader knows which account it is.
    fn describes(&self) -> String {
        match self {
            Meter::Launched(pid, _) => format!(
                "the launched program's cpu time and its descendants' ({}, of {pid} and of \
                 every process it started while they ran, its orphans adopted by omn \
                 included, and getrusage(RUSAGE_CHILDREN) in microseconds once it was \
                 reaped, the helpers omn ran itself left out)",
                task::tree_cpu(*pid).map_or(
                    "the scheduler's own nanoseconds, or clock ticks where it could not be read",
                    |read| read.grain.says()
                ),
            ),
            Meter::Tasks(pids) => format!(
                "the cpu time of {} ({}, children reaped included)",
                pids.iter()
                    .map(|pid| pid.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                pids.first()
                    .and_then(|pid| task::process_cpu(*pid))
                    .map_or(task::CpuGrain::Ticks, |read| read.grain)
                    .says(),
            ),
            Meter::Threads(tids) => format!(
                "the cpu time of thread {} (/proc/<tid>/task/<tid>/schedstat, its own)",
                tids.iter()
                    .map(|tid| tid.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Meter::Cpus(None) => "everything but idle on every cpu (/proc/stat)".to_owned(),
            Meter::Cpus(Some(cpus)) => format!(
                "everything but idle on cpu {} (/proc/stat)",
                cpus.iter()
                    .map(|cpu| cpu.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Meter::Cgroup(directory) => {
                format!("{}/cpu.stat usage_usec", directory.display())
            }
            Meter::Nothing(reason) => (*reason).to_owned(),
        }
    }
}

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

fn is_zero(value: &u64) -> bool {
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
/// [`run_all_within`] drives -- the same collectors, the same rounds, the same
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

/// One source of events.
pub trait Collector {
    fn id(&self) -> &CollectorId;

    /// The attribute that holds this collector's measured value.
    fn value_attr(&self) -> &'static str;

    /// Opens whatever the collector needs, before the observed program runs.
    fn start(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Produces the events observable right now.
    ///
    /// An error is a failed read, not the end of the run: a process that
    /// vanished mid-poll ends the collection, a transient read failure is
    /// counted and the run goes on.
    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError>;

    /// Closes the collector and returns anything still buffered.
    ///
    /// **This is not where a polled counter takes a closing read.** The runner
    /// closes the window itself: its last round of `sample` calls happens at
    /// the moment the stop condition is met, so the last interval of a series
    /// is already measured by the time this is called. A further read here
    /// covers the microseconds since -- `the_runner_closes_the_window` below
    /// measures them -- and produces a second point at the same offset with a
    /// value near zero, which drags every summary computed over the series.
    ///
    /// What does belong here: events a ring or a shim published since the last
    /// drain, and the single measurement of a `count` window, which has no
    /// series and whose closing read *is* the result.
    fn stop(&mut self) -> Vec<Event> {
        Vec::new()
    }

    /// How often this collector needs to be asked, when that is dictated by the
    /// collector rather than by the request. A streaming collector answers with
    /// the interval that keeps its buffer from overflowing.
    fn tick(&self) -> Option<StdDuration> {
        None
    }

    /// What the collector itself knows it lost.
    fn losses(&self) -> Losses {
        Losses::default()
    }

    /// The same losses, per ring, for a collector that reads one ring per
    /// key: a profile that follows each thread opens a ring for each, and each
    /// ring's drops are that thread's (`docs/fixes/silent.md`). Every entry is
    /// a part of [`Collector::losses`], not an addition to it; what no ring
    /// owns stays in the whole alone. Empty for a collector with one ring, or
    /// none, whose losses belong to every series alike.
    fn losses_by_ring(&self) -> Vec<RingLoss> {
        Vec::new()
    }

    /// What the collector states about its run that is not something it gave
    /// up -- the kernel's own count of what it bucketed, another party's
    /// account of the same thing -- which reaches the results as `notes`
    /// rather than as `degraded:`. [`Collector::notes`] stays the channel for
    /// concessions.
    fn facts(&self) -> Vec<String> {
        Vec::new()
    }

    /// How this collector stamps a row it read from a table the kernel
    /// filled between drains, for a collector that reads such rows. `None`
    /// says nothing, and a reader must not take it to mean each row is
    /// stamped at its own event.
    fn row_stamp(&self) -> Option<RowStamp> {
        None
    }

    /// The sentence saying this collector broke the program it measured --
    /// the GPU's injection library killing its target in its exit handler --
    /// read after `stop`. `None` is the normal case.
    ///
    /// Not a loss: a lost read leaves the program's own outcome standing, and
    /// this does not, so the run carries it to its outcome and `omn` exits
    /// non-zero with it rather than exit 0 over a program the tool ended.
    fn failure(&self) -> Option<String> {
        None
    }

    /// What this collector counted about the run, as numbers a program reads
    /// by name: what a caveat or a fact says in a sentence (`net.request`'s
    /// requests that never ended, or began before the run). Read after
    /// `stop`; empty for most collectors, and then absent from the results.
    fn fields(&self) -> crate::emit::Fields {
        crate::emit::Fields::new()
    }

    /// Who named each frame of [`Collector::stacks`], the perf maps that
    /// did, and the file each task it keys executed, for a collector that
    /// symbolises while its targets are alive. Read after `stop`, beside
    /// `stacks`; empty for everything else.
    fn provenance(&self) -> crate::stack::Provenance {
        crate::stack::Provenance::default()
    }

    /// The frames behind each stack identity this collector interned.
    ///
    /// Empty for everything that does not capture stacks, which is most of
    /// them. Read after `stop`, so a collector that finishes its symbolisation
    /// at the end has somewhere to do it.
    fn stacks(&self) -> BTreeMap<u64, Vec<String>> {
        BTreeMap::new()
    }

    /// What the collector had to give up to produce these numbers. Empty is the
    /// normal case; anything here reaches the user-visible output.
    fn notes(&self) -> Vec<String> {
        Vec::new()
    }
}

/// One collector this build knows how to create.
///
/// Families register through this list rather than through a central `match`,
/// so adding a source touches only that family's directory. Nine agents editing
/// one match arm is nine merge conflicts on the same three lines.
pub struct Factory {
    /// The catalog source this builds, such as `mem.rss`.
    pub source: &'static str,
    /// The backend id within that source's chain, such as `fs:proc:statm`.
    /// Backend ids are unique within a chain, which is what makes this pair a
    /// key.
    pub backend: &'static str,
    build: fn(&ResolvedRequest) -> Result<Box<dyn Prepared>, String>,
    /// Each event it emits in `sample` mode carries the period it stands for
    /// as [`crate::emit::WEIGHT_ATTR`] ([`Factory::weighing_its_samples`]).
    weighs_its_samples: bool,
}

impl Factory {
    pub fn new(
        source: &'static str,
        backend: &'static str,
        build: fn(&ResolvedRequest) -> Result<Box<dyn Prepared>, String>,
    ) -> Self {
        Self {
            source,
            backend,
            build,
            weighs_its_samples: false,
        }
    }

    /// Declares that this collector weighs each sampled event by the period it
    /// stands for, so a budget may move its series from `trace` to `sample`
    /// and the totals after the move are still the task's ([`weighs_its_samples`]).
    ///
    /// Declared by the collector, beside the code that attaches the weight,
    /// rather than listed where the budget is built: a list kept elsewhere is
    /// one nobody updates when a collector starts or stops weighing. A
    /// collector that declares it is held to a workload's own count across
    /// the move (`tests/budget.rs`).
    pub fn weighing_its_samples(mut self) -> Self {
        self.weighs_its_samples = true;
        self
    }
}

/// Whether the collector of `source` on `backend` weighs each sampled event
/// by its period ([`Factory::weighing_its_samples`]).
///
/// A series the budget moves from `trace` to `sample` is one answer only if
/// every sampled event after the move is weighed by its period; one that
/// counts once where it stood for a hundred makes every total after the move
/// short by that factor, so the budget's ladder passes over a rung whose
/// collector does not.
pub fn weighs_its_samples(source_id: &str, backend: &str) -> bool {
    registry().iter().any(|factory| {
        factory.source == source_id && factory.backend == backend && factory.weighs_its_samples
    })
}

/// Every collector in this build.
///
/// The nine families are listed once. There will never be a tenth: the catalog
/// families are the vocabulary, not an extension point.
fn registry() -> Vec<Factory> {
    let families = [
        cpu::factories,
        gpu::factories,
        io::factories,
        mem::factories,
        net::factories,
        power::factories,
        proc::factories,
        runtime::factories,
        sched::factories,
    ];
    families.into_iter().flat_map(|family| family()).collect()
}

/// A collector before the thing it observes exists.
///
/// Two phases are needed because some collectors have to act on the target
/// before it runs: a preloaded shim is given at exec and cannot be handed to a
/// process that is already going. Splitting construction from attachment lets
/// the launch carry whatever the collector needs, without the runner knowing
/// what any particular collector wants.
pub trait Prepared {
    /// Environment the launched program must be given.
    fn launch_environment(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Offered a collector prepared earlier for the same launch; answers
    /// whether this one now reads through what that one set up, so that the
    /// program is given it once.
    ///
    /// For collectors that hand the program a channel at exec -- a descriptor
    /// in its environment -- and can read one channel between several of them.
    /// Two such collectors that did not share would each want the variable to
    /// name their own, and a program has one environment. The runner offers
    /// every earlier collector of the launch in order and stops at the first
    /// that is taken; what "the same" means is the collector's to decide, which
    /// is why `earlier` arrives as itself rather than as a descriptor.
    fn share_launch(&mut self, earlier: &mut dyn Prepared) -> bool {
        let _ = earlier;
        false
    }

    /// This collector as something [`Prepared::share_launch`] can downcast,
    /// for the collectors that share. Opt-in rather than a supertrait, because
    /// upcasting a trait object to `dyn Any` is newer than this crate's
    /// declared toolchain.
    fn shareable(&mut self) -> Option<&mut dyn Any> {
        None
    }

    /// Set when this collector can only observe a program the tool launches,
    /// with the reason, so the refusal explains itself.
    fn requires_launch(&self) -> Option<&'static str> {
        None
    }

    /// Set when this collector, attached to a process that is already
    /// running, would follow only the thread it is opened on and the threads
    /// started after it -- with the reason, so the loss explains itself.
    ///
    /// A per-task perf counter is the one that does: it follows a process by
    /// inheritance, and inheritance is from the moment of the open. Attached at
    /// the launch gate that is every thread there will ever be; attached by a
    /// window to a program that has already started a thread pool, it counts
    /// the main thread and nothing of the pool, which is a number that looks
    /// right. The runner asks this before a late attach and, when the target
    /// has threads the collector would not see, does not attach it.
    fn follows_only_later_threads(&self) -> Option<&'static str> {
        None
    }

    /// The account this collector will keep of what it spends inside its
    /// target, when it keeps one. Asked before the attach, which consumes the
    /// prepared collector, and after the launch is shared.
    ///
    /// A collector that keeps none is weighed by its mechanism: the programs
    /// it loaded when it loads eBPF, and otherwise what its backend is known
    /// to do (`InsideMeter::of`).
    fn inside(&self) -> Option<Box<dyn InsideAccount>> {
        None
    }

    /// Binds to the target now that it exists.
    ///
    /// The target is passed whole rather than as a pid, because not every
    /// source observes a process: an interface counter and a machine-wide file
    /// have no pid to be given, and inventing one for them would push the lie
    /// into every collector.
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String>;
}

/// What a single sample can go wrong with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleError {
    /// The thing being observed no longer exists. The run ends, cleanly.
    TargetGone(String),
    /// This read failed; another one may not.
    Transient(String),
}

/// Collects every result, or every refusal.
///
/// `collect::<Result<_, _>>()` stops at the first error and throws away the
/// questions it never asked, which is why a session used to report one missing
/// collector at a time. One refusal is passed through as itself so that the
/// common case reads unchanged.
fn all_or_nothing<T>(
    answers: impl Iterator<Item = Result<T, CollectError>>,
) -> Result<Vec<T>, CollectError> {
    let mut collected = Vec::new();
    let mut refusals = Vec::new();
    for answer in answers {
        match answer {
            Ok(one) => collected.push(one),
            Err(refusal) => refusals.push(refusal),
        }
    }
    match refusals.len() {
        0 => Ok(collected),
        1 => Err(refusals.remove(0)),
        _ => Err(CollectError::Several { refusals }),
    }
}

/// Runs one resolved request to completion.
pub fn run(resolved: &ResolvedRequest) -> Result<Collection, CollectError> {
    let mut collections = run_all(std::slice::from_ref(resolved))?;
    Ok(collections.remove(0))
}

/// Runs several resolved requests over one target, in one window.
///
/// They share a window deliberately. Running them one after another would
/// measure several different moments and present them as one, and a join across
/// them would then be correlating a program with its own past. One launch, one
/// set of collectors, one clock.
///
/// A run started this way has nowhere to degrade to, so a `--budget` it exceeds
/// ends it. [`run_all_within`] is the same run with a ladder underneath.
pub fn run_all(resolved: &[ResolvedRequest]) -> Result<Vec<Collection>, CollectError> {
    run_all_within(resolved, &mut StopWhenOverBudget)
}

/// The same run, with somewhere to turn when a budget is exceeded.
///
/// The split keeps the collector layer from knowing about the catalog. The
/// runner can measure what it is spending and compare it against the ceiling;
/// *which backend is cheaper than this one and reachable here* is a question
/// about the catalog and about this machine, and it is answered by whoever
/// holds both.
pub fn run_all_within(
    resolved: &[ResolvedRequest],
    cheaper: &mut dyn Cheaper,
) -> Result<Vec<Collection>, CollectError> {
    run_all_within_unserved(resolved, cheaper, &[])
}

/// The same run, told which of the session's held requests this machine
/// could not serve.
///
/// They are not in `resolved` and are never attached; the escalation is
/// told of them, and if it names their layer it says that the layer could
/// not be served and why, in the resolver's words. A held request that does
/// not resolve is a layer this machine cannot serve, not a failed run.
pub fn run_all_within_unserved(
    resolved: &[ResolvedRequest],
    cheaper: &mut dyn Cheaper,
    unserved: &[Unserved],
) -> Result<Vec<Collection>, CollectError> {
    run_all_bounded(resolved, cheaper, unserved, None)
}

/// The same run, lasting as long as `bound` -- a program it launches and no
/// request targets -- when one is given: the window of a machine, a core, a
/// cgroup or a pid measured for a program's life, as `perf stat -a -- prog`
/// and `turbostat prog` measure. A request whose target is a launch beside
/// it is refused: the program is launched once, and is either the run's
/// bound or a request's target.
pub fn run_all_bounded(
    resolved: &[ResolvedRequest],
    cheaper: &mut dyn Cheaper,
    unserved: &[Unserved],
    bound: Option<&[String]>,
) -> Result<Vec<Collection>, CollectError> {
    let mut escalation = escalation_for(resolved, unserved)?;
    run_core(
        resolved,
        cheaper,
        escalation
            .as_deref_mut()
            .map(|escalation| escalation as &mut dyn Escalate),
        None,
        bound,
    )
    .map(|(collections, _)| collections)
}

/// A recording: the same run, with no fixed end but its longest duration,
/// keeping only what `recorder.retain_ns` holds and handing a [`Document`]
/// to `recorder.sink` each time its trigger fires.
///
/// Returns the run's collections as an ordinary run would -- what they cost,
/// what they lost, what their collectors noted -- holding the events still
/// retained at the end, and what the recording did.
pub fn record_within(
    resolved: &[ResolvedRequest],
    cheaper: &mut dyn Cheaper,
    recorder: &mut Recorder<'_>,
) -> Result<(Vec<Collection>, Recorded), CollectError> {
    record_bounded(resolved, cheaper, recorder, None)
}

/// [`record_within`], bounded by a launched program as [`run_all_bounded`]
/// is.
pub fn record_bounded(
    resolved: &[ResolvedRequest],
    cheaper: &mut dyn Cheaper,
    recorder: &mut Recorder<'_>,
    bound: Option<&[String]>,
) -> Result<(Vec<Collection>, Recorded), CollectError> {
    recording_holds(resolved, recorder)?;
    run_core(resolved, cheaper, None, Some(recorder), bound)
        .map(|(collections, recorded)| (collections, recorded.unwrap_or_default()))
}

/// Refuses a recording the runner cannot hold, for a caller that built its
/// requests without a session. `Session::check_recording` says the same
/// things first, before anything is resolved.
fn recording_holds(
    resolved: &[ResolvedRequest],
    recorder: &Recorder<'_>,
) -> Result<(), CollectError> {
    let refuse = |reason: String| Err(CollectError::Recording { reason });
    if recorder.on >= resolved.len() {
        return refuse(format!(
            "its trigger reads request {} of a run of {}",
            recorder.on,
            resolved.len()
        ));
    }
    if recorder.retain_ns == 0 || recorder.documents == 0 {
        return refuse(
            "it needs a retention bound and a bound on documents, both above zero".to_owned(),
        );
    }
    if !resolved.iter().any(|one| one.request.duration.is_some()) {
        return refuse("it needs a longest duration, and no request has one".to_owned());
    }
    for one in resolved {
        if one.request.window.is_some() {
            return refuse(format!(
                "`{}` has a window, and a request in a recording is watched for the whole run",
                one.request.source
            ));
        }
        if matches!(one.request.mode, Mode::Count | Mode::Snapshot) {
            return refuse(format!(
                "`{}` is a single reading, and a recording retains a series",
                one.request.source
            ));
        }
    }
    Ok(())
}

/// Where a session's escalation is answered from: the one line of the
/// collector layer that names triage.
///
/// The runner knows [`Escalate`] and nothing behind it, the way it knows
/// [`Cheaper`]; this is the registration, as `registry()` is for the
/// families. A session holds a request for the escalation with a window
/// written `on <layer>`, and one that holds none is run without asking
/// anything.
fn escalation_for(
    resolved: &[ResolvedRequest],
    unserved: &[Unserved],
) -> Result<Option<Box<dyn Escalate>>, CollectError> {
    if !resolved
        .iter()
        .any(|one| one.request.window.is_some_and(|window| window.on.is_some()))
        && unserved.is_empty()
    {
        return Ok(None);
    }
    crate::triage::escalate::Policy::for_run_with(resolved, unserved)
        .map(|policy| Some(Box::new(policy) as Box<dyn Escalate>))
        .map_err(|refusal| CollectError::Escalation {
            reason: refusal.to_string(),
        })
}

/// The same run, asking `escalation` between rounds which held requests to
/// attach and detach. `None` runs it as [`run_all_within`] would a session
/// that holds nothing; a held request is then never attached, and says so.
pub fn run_all_escalating(
    resolved: &[ResolvedRequest],
    cheaper: &mut dyn Cheaper,
    escalation: Option<&mut dyn Escalate>,
) -> Result<Vec<Collection>, CollectError> {
    run_core(resolved, cheaper, escalation, None, None).map(|(collections, _)| collections)
}

/// The one runner behind every entry point: an ordinary run, an escalating
/// one, and a recording.
fn run_core(
    resolved: &[ResolvedRequest],
    cheaper: &mut dyn Cheaper,
    escalation: Option<&mut dyn Escalate>,
    recorder: Option<&mut Recorder<'_>>,
    bound: Option<&[String]>,
) -> Result<(Vec<Collection>, Option<Recorded>), CollectError> {
    assert!(!resolved.is_empty(), "a run needs at least one request");
    let workload = workload_of(resolved)?;
    let bound = bound.filter(|argv| !argv.is_empty());
    // Whether each request is about the workload, which decides what it is
    // bound to, what its overhead is a percentage of, and whether its target
    // going away ends the run. With no workload in the session every target
    // is the run's own, as it was when a run had exactly one.
    let about_workload: Vec<bool> = resolved
        .iter()
        .map(|resolved| workload.is_none_or(|workload| &resolved.request.target == workload))
        .collect();
    let launched_argv: Option<&[String]> = match (workload, bound) {
        (Some(Target::Launch(argv)), None) => Some(argv),
        (Some(Target::Launch(argv)), Some(bound)) => {
            return Err(CollectError::MixedTargets {
                first: format!("a run bounded by `{}`", bound.join(" ")),
                second: Target::Launch(argv.clone()).to_string(),
            })
        }
        (_, bound) => bound,
    };
    // A process the run did not start, whose exit ends it as a launched
    // program's does: `--pid` alone, as `perf stat -p` ends with its
    // process. Not beside a bound, which is then the run's end.
    let exits = match (workload, launched_argv) {
        (Some(Target::Pid(pids)), None) => {
            let exits = launch::Exits::of(pids);
            exits.watches_all(pids).then_some(exits)
        }
        _ => None,
    };

    // Asked first, because "no collector exists for this backend" is a
    // different answer from "this target is not wired up yet", and reporting
    // the second when the first is true sends the reader to the wrong place.
    //
    // Every request is asked, not just requests up to the first refusal. A
    // session with five unbuilt sources used to report one of them and stop,
    // so fixing it took five runs; the validator has always answered for every
    // rejection at once, and this is the same courtesy one layer down.
    //
    // The kernel is asked to time eBPF programs first, so that it counts every
    // run of the ones about to be loaded; and each request's programs are the
    // ones that appear among this process's descriptors while it is prepared
    // and attached, which is what its cost inside the target is read from.
    let program_stats = ProgramStats::for_run(resolved);
    // In a container the ids every program writes are translated: the table
    // starts now, and with it the program that remembers each task's birth,
    // so that a task born and gone between two reads is still named.
    if resolved.iter().any(|one| loads_programs(&one.backend.id)) {
        crate::bpf::pidns::remember();
    }
    let mut programs: Vec<Vec<(RawFd, u32)>> = vec![Vec::new(); resolved.len()];
    let mut prepared: Vec<Box<dyn Prepared>> =
        all_or_nothing(resolved.iter().enumerate().map(|(index, resolved)| {
            let before = loads_programs(&resolved.backend.id).then(programs_held);
            Target::narrowed_to_one();
            let prepared = prepare(resolved);
            programs[index].extend(programs_since(before.as_ref()));
            match Target::narrowed_to_one() {
                Some(named) => Err(one_process_of_several(resolved, named)),
                None => prepared,
            }
        }))?;

    // A window this collector or this mode cannot honour is refused while
    // nothing is running, and before the environment is merged: a collector
    // given to the program at exec is exactly the one a window cannot move.
    let unheld: Vec<Option<String>> = all_or_nothing(
        resolved
            .iter()
            .zip(&prepared)
            .map(|(resolved, prepared)| window_honoured(resolved, prepared.as_ref())),
    )?;
    let mut plans: Vec<Plan> = resolved
        .iter()
        .map(|resolved| Plan::of(resolved.request.window))
        .collect();
    for (plan, unheld) in plans.iter_mut().zip(&unheld) {
        plan.unheld.clone_from(unheld);
    }

    // Only the collectors of the launch act on it. A machine target beside it
    // is not given to the program, so what its collector would want of the
    // program's environment -- nothing, for every collector written -- is not
    // the program's business. Nor is a held request's that this run cannot
    // hold: it is never attached.
    let launching: Vec<bool> = resolved
        .iter()
        .zip(&unheld)
        .map(|(resolved, unheld)| {
            matches!(resolved.request.target, Target::Launch(_)) && unheld.is_none()
        })
        .collect();
    share_launches(&mut prepared, &launching);
    // Asked once the launch is shared: the account a shared channel keeps is
    // the one the program was given.
    let mut accounts: Vec<Option<Box<dyn InsideAccount>>> =
        prepared.iter().map(|prepared| prepared.inside()).collect();
    let environment = merge_environments(
        prepared
            .iter()
            .zip(&launching)
            .filter_map(|(prepared, launching)| launching.then_some(prepared.as_ref())),
    )?;

    // A collector that can only be given to a program at exec is asked about
    // next, and before the stop condition, because the two refusals send the
    // reader to different places: "pass --duration" is advice for a request
    // that can work, and this one cannot. Answering the shallower question
    // first sent a caller off to add a flag that would change nothing.
    for (index, prepared) in prepared.iter().enumerate() {
        let target = &resolved[index].request.target;
        if matches!(target, Target::Launch(_)) || unheld[index].is_some() {
            continue;
        }
        if let Some(reason) = prepared.requires_launch() {
            return Err(CollectError::UnsupportedTarget {
                source_id: resolved[index].request.source.to_string(),
                target: format!("{} ({reason})", target.kind()),
            });
        }
    }

    // A run that nothing will ever stop is refused before the target is
    // launched or anything is attached to it -- but after the collectors have
    // been asked for, because "no collector is written for this" is the deeper
    // answer and the one worth giving first. A duration, a program whose exit
    // ends the run, a single read, or a window closing on every request: with
    // none of them there is no window, and observing a target that nothing
    // will stop watching is worse than refusing.
    let duration_ns = resolved
        .iter()
        .filter_map(|resolved| resolved.request.duration)
        .map(|duration| duration.as_nanos())
        .max();
    let snapshot = resolved
        .iter()
        .all(|resolved| resolved.request.mode == Mode::Snapshot);
    let every_window_closes = resolved.iter().all(|resolved| {
        resolved
            .request
            .window
            .is_some_and(|window| window.stop.is_some())
    });
    if duration_ns.is_none()
        && launched_argv.is_none()
        && exits.is_none()
        && !snapshot
        && !every_window_closes
    {
        return Err(CollectError::NoStopCondition);
    }
    if let Some(duration_ns) = duration_ns {
        all_or_nothing(
            resolved
                .iter()
                .map(|resolved| window_inside_the_run(resolved, duration_ns)),
        )?;
    }

    // A collector attached mid-run pays for its attach inside the run, while
    // the target runs and while every other collector waits for the round to
    // finish. The one fixed cost there known to be large is the kernel's
    // symbol table, which a profile that walks into the kernel reads on its
    // first start: measured at 962 ms for a `cpu.profile` the escalation
    // attached, during which nothing else was read and which the profile's
    // own cost then carried as 71 % of the target's cpu. It describes the
    // machine rather than the target, so it is paid here, before the launch,
    // whenever a request attached late asks for stacks.
    let late_stacks = resolved.iter().any(|resolved| {
        let request = &resolved.request;
        request.window.is_some_and(|window| window.attaches_late())
            && (request.stack.is_some() || request.emit.contains(&crate::request::Emit::Stack))
            // A user-only stack never walks into the kernel, so it never
            // reads the table this pays for (`profile.rs` primes it only for
            // a callchain with a kernel half).
            && resolved.stack.as_ref().is_none_or(|choice| !choice.user_only)
    });
    if late_stacks {
        crate::stack::prime_kernel_symbols();
    }

    // A launched program is both the target and the clock: the run ends when it
    // does, which is what makes `omn <source> -- ./prog` a complete measurement
    // rather than a sample of one. It is held at a gate so that collectors are
    // attached before its first instruction; otherwise the interesting part of
    // a short program is over before anything is watching.
    let mut child = match launched_argv {
        Some(argv) => {
            Some(
                launch::gated(argv, &environment).map_err(|error| CollectError::Launch {
                    program: argv.first().cloned().unwrap_or_default(),
                    error: error.to_string(),
                })?,
            )
        }
        None => None,
    };

    // A launched program replaces the target's own identity: the collectors
    // observe the process that was just created, not a pid the request named.
    // A target that names no process -- an interface, the machine -- is passed
    // through, and a collector that needs a pid refuses it by name. Each
    // request is bound to its own target: a core beside a program is the core.
    let bound: Vec<Target> = resolved
        .iter()
        .map(|resolved| match (&child, &resolved.request.target) {
            (Some(child), Target::Launch(_)) => Target::Pid(vec![child.pid()]),
            (_, target) => target.clone(),
        })
        .collect();

    // Everything is attached now except what a window asks to attach later,
    // which waits prepared: its factory has answered, so the only refusal left
    // for it is the one a running target can give.
    let mut slots: Vec<Slot> = Vec::with_capacity(prepared.len());
    for (index, prepared) in prepared.into_iter().enumerate() {
        if plans[index].unheld.is_some() {
            slots.push(Slot::Missed);
            continue;
        }
        if resolved[index]
            .request
            .window
            .is_some_and(|window| window.attaches_late())
        {
            slots.push(Slot::Waiting(prepared));
            continue;
        }
        let source_id = resolved[index].request.source.to_string();
        let before = loads_programs(&resolved[index].backend.id).then(programs_held);
        Target::narrowed_to_one();
        // These two happen against a real target -- a counter that would not
        // open, a file that is not readable, a process that has gone -- so an
        // unmarked refusal here is a failure of this run. A collector refusing
        // a shape it has not implemented says so with `not_built`.
        let attached = prepared.attach(&bound[index]);
        if let Some(named) = Target::narrowed_to_one() {
            if let Some(child) = child.as_mut() {
                child.terminate();
            }
            return Err(one_process_of_several(&resolved[index], named));
        }
        let mut collector =
            attached.map_err(|reason| CollectError::start(&source_id, reason, Whose::Machine))?;
        if let Err(reason) = collector.start() {
            if let Some(child) = child.as_mut() {
                child.terminate();
            }
            return Err(CollectError::start(source_id, reason, Whose::Machine));
        }
        programs[index].extend(programs_since(before.as_ref()));
        slots.push(Slot::Attached(collector));
    }

    // Everything is attached; let the program run.
    if let Some(child) = child.as_mut() {
        child.release();
    }

    // Each request at its own pace, from its own mode and its own collector.
    // A request still waiting for its window is paced when it is attached.
    let mut paces: Vec<Pace> = slots
        .iter()
        .zip(resolved)
        .map(|(slot, resolved)| match slot {
            Slot::Attached(collector) => pace_of(collector.as_ref(), resolved.request.mode),
            _ => Pace::UNSET,
        })
        .collect();

    let mut degradations: Vec<Vec<String>> = slots
        .iter()
        .zip(resolved)
        .zip(&paces)
        .map(|((slot, resolved), pace)| match slot {
            Slot::Attached(collector) => unpaced(resolved, collector.as_ref(), *pace)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        })
        .collect();

    let deadline_ns = duration_ns.map(|longest| monotonic_ns() + longest);

    // The cost accounting opens here, with everything attached and the program
    // released: what was spent loading the catalog, probing the machine and
    // opening counters is this tool's startup, not the price of the
    // measurement, and charging the measurement for it would make every short
    // run look ruinous. A collector attached later is attached inside this
    // window, and pays for its own attach.
    let launched_pid = child.as_ref().map(|child| child.pid());
    let meters = resolved
        .iter()
        .map(|resolved| Meter::of(&resolved.request.target, launched_pid))
        .collect();
    let counting = program_stats.counting();
    let inside = resolved
        .iter()
        .zip(accounts.iter_mut())
        .zip(programs)
        .map(|((resolved, account), programs)| {
            InsideMeter::of(&resolved.backend.id, account.take(), programs, &counting)
        })
        .collect();
    let mut spending = Spending::open(meters, inside, bound, resolved.to_vec(), cheaper);
    spending.program_stats = counting;
    spending.deadline_ns = deadline_ns;
    spending.sheddable = plans.iter().map(|plan| plan.held).collect();
    let started_ns = spending.opened_ns;
    let wake = child
        .as_ref()
        .and_then(|child| child.exit_fd())
        .or_else(|| exits.as_ref().and_then(launch::Exits::wake_fd));
    // A launched program the kernel gives no pidfd for (before Linux 5.3) is
    // asked between slices of the run's sleep instead.
    let notice_exit = child
        .as_ref()
        .is_some_and(|child| child.exit_fd().is_none());
    // A recording weighs its budget over a rolling span rather than the
    // whole run, which has no fixed end to be a share of.
    let budget_interval_ns = recorder.as_ref().map(|recorder| {
        recorder
            .retain_ns
            .max(RECORDING_BUDGET_FLOOR.as_nanos() as u64)
    });
    spending.rolling_ns = budget_interval_ns;
    let mut recording =
        recorder.map(|recorder| Recording::new(recorder, resolved, &slots, started_ns));
    let Driven {
        mut events,
        mut losses,
        mut watched,
        mut closed_because,
        stopped_because,
        mut escalated,
        mut drains,
    } = drive(
        &mut slots,
        &mut paces,
        &plans,
        &about_workload,
        wake,
        notice_exit,
        &mut spending,
        escalation,
        recording.as_mut(),
        || {
            if let Some(child) = child.as_mut() {
                if let Some(status) = child.finished() {
                    return Some(format!("the launched program exited with {status}"));
                }
            }
            if let Some(reason) = exits.as_ref().and_then(launch::Exits::all_exited) {
                return Some(reason);
            }
            match deadline_ns {
                Some(deadline_ns) if monotonic_ns() >= deadline_ns => {
                    Some("the requested duration elapsed".to_owned())
                }
                _ => None,
            }
        },
    );

    // Whatever the kernel or the shim published between the last tick and the
    // stop still belongs to this run.
    //
    // The window closes before any of them is stopped. A drained collector's
    // last read is inside its own `stop()`, and the run stops them one after
    // another, so a window closed after the last stop claimed the span after
    // each earlier collector's last read, where nothing of it could be in a
    // row: stream S measured 6 of 930 blocks with `sched.offcpu` alone and 71
    // with five collectors stopping after it. One instant before the first
    // stop is before every last read, and it is one window for every
    // collector the run held to the end, as a session's sources share one.
    let closing_ns = monotonic_ns();
    let ended_s = closing_ns.saturating_sub(started_ns) as f64 / 1e9;
    spending.read_in_target_before_the_stops();
    for (index, slot) in slots.iter_mut().enumerate() {
        match slot {
            Slot::Attached(collector) => {
                watched[index].closed_ns = closing_ns;
                let mut trailing = collector.stop();
                into_our_ids(
                    spending.serving.get(index),
                    &mut trailing,
                    &mut losses[index],
                );
                events[index].append(&mut trailing);
                let collector_losses = collector.losses();
                losses[index].absorb(collector_losses);
                drains[index].grew_to(losses[index].dropped, closing_ns);
                if let Some(stop) = resolved[index]
                    .request
                    .window
                    .and_then(|window| window.stop)
                {
                    degradations[index].push(format!(
                        "the run ended {ended_s:.3} s in, before this request's window closed at \
                         {stop}: it was watched for less than it asked"
                    ));
                }
            }
            Slot::Waiting(_) if plans[index].held => {
                let layer = resolved[index]
                    .request
                    .window
                    .and_then(|window| window.layer())
                    .map_or_else(String::new, |layer| format!(" `{layer}`"));
                let why = format!(
                    "never attached: the escalation did not name its layer{layer} in the {ended_s:.3} \
                     s the run lasted"
                );
                degradations[index].push(format!("{why}, so it measured nothing and cost nothing"));
                closed_because[index] = Some(why);
                *slot = Slot::Missed;
            }
            Slot::Waiting(_) => {
                let window = resolved[index].request.window.unwrap_or(Window {
                    start: None,
                    stop: None,
                    on: None,
                });
                let why = format!(
                    "never attached: the run ended {ended_s:.3} s in, before this request's \
                     window `{window}` opened"
                );
                degradations[index].push(format!("{why}, so its empty series measured nothing"));
                closed_because[index] = Some(why);
                *slot = Slot::Missed;
            }
            Slot::Detached(_) | Slot::Missed => {}
        }
    }

    // A launched program that outlived the duration is stopped: the tool does
    // not leave behind processes it started. Its tree is read first, the last
    // moment the processes it started can be: what they spent is in nobody's
    // account once they are killed, since whoever inherits them reaps them.
    if let Some(child) = child.as_mut() {
        if child.finished().is_none() {
            spending.read_before_the_stop();
            child.terminate();
        }
        // A program that exited on its own may have left processes running
        // outside its tree -- a daemon's double fork -- which this process
        // adopted: they end with the run as they would at a stop.
        child.end_orphans();
    }

    let closed_ns = monotonic_ns();
    // An attached collector's window closed before the first stop, above; a
    // detached one's at its detach.
    for window in watched.iter_mut().filter(|window| !window.attached) {
        // Never attached: a span of nothing, at the end.
        window.opened_ns = closed_ns;
        window.closed_ns = closed_ns;
    }
    let lifetime = child.as_ref().and_then(|child| child.lifetime());
    // Read once the child has been reaped: `getrusage(RUSAGE_CHILDREN)` is what
    // `time` prints, and it only prints it after the wait.
    spending.stats_lost = program_stats.lost();
    let overheads = spending.close();
    drop(program_stats);
    let recorded = recording.map(|recording| {
        recording.recorded(&stopped_because, closed_ns, budget_interval_ns.unwrap_or(0))
    });
    let mut collections = slots
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let (value_attr, stacks, collector_notes, by_ring, facts, row_stamp, failure) =
                match slot {
                    Slot::Attached(collector) => (
                        collector.value_attr(),
                        collector.stacks(),
                        collector.notes(),
                        collector.losses_by_ring(),
                        collector.facts(),
                        collector.row_stamp(),
                        collector.failure(),
                    ),
                    Slot::Detached(record) => (
                        record.value_attr,
                        record.stacks.clone(),
                        record.notes.clone(),
                        record.by_ring.clone(),
                        record.facts.clone(),
                        record.row_stamp,
                        record.failure.clone(),
                    ),
                    Slot::Waiting(_) | Slot::Missed => (
                        "",
                        BTreeMap::new(),
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        None,
                        None,
                    ),
                };
            let fields = slot_fields(slot);
            let provenance = slot_provenance(slot);
            let mut collected = std::mem::take(&mut events[index]);
            collected.sort_by_key(|event| event.ts_ns);
            // The filter is the request's, and a degradation does not change
            // the request -- only how it was served.
            let filtered = apply_filter(&resolved[index], &mut collected);
            let mut notes = std::mem::take(&mut degradations[index]);
            notes.extend(std::mem::take(&mut spending.notes[index]));
            notes.extend(std::mem::take(&mut escalated[index]));
            notes.extend(collector_notes);
            // Everything is in: the runner's counts, what the collector knew it
            // lost, and anything a budget added. The magnitudes become the one
            // reason that carries them now, and not one reason each.
            let mut losses = std::mem::take(&mut losses[index]);
            losses.summarize();
            Collection {
                observed_ns: watched[index].span_ns(),
                value_attr: value_attr.to_owned(),
                events: collected,
                losses,
                stopped_because: closed_because[index]
                    .take()
                    .unwrap_or_else(|| stopped_because.clone()),
                notes,
                stacks,
                filtered,
                served_by: std::mem::take(&mut spending.served_by[index]),
                overhead: overheads[index].clone(),
                lifetime,
                window: watched[index],
                detached: matches!(slot, Slot::Detached(_)),
                by_ring,
                facts,
                row_stamp,
                drains: std::mem::take(&mut drains[index].placed),
                failure,
                fields,
                provenance,
            }
        })
        .collect::<Vec<Collection>>();
    // What the runner spent once each span was weighed, read last so that
    // before, during and after add up to this process's account.
    let handed_back_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
    for collection in &mut collections {
        let overhead = &mut collection.overhead;
        overhead.after_window_cpu_ns = handed_back_cpu_ns
            .saturating_sub(overhead.before_window_cpu_ns + overhead.collector_cpu_ns);
    }
    Ok((collections, recorded))
}

/// The refusal for a collector that observes one process of the several its
/// request named: it would measure the first and drop the rest without a
/// word, which is a number that looks like the whole.
fn one_process_of_several(resolved: &ResolvedRequest, named: usize) -> CollectError {
    let target = &resolved.request.target;
    let first = target
        .first_pid()
        .map_or_else(String::new, |pid| format!(" -- only {pid} --"));
    // The call above narrowed the target again; it is this refusal's own.
    Target::narrowed_to_one();
    CollectError::UnsupportedTarget {
        source_id: resolved.request.source.to_string(),
        target: format!(
            "{} (`{}` through `{}` observes one process, and would have measured one \
             of the {named} this request named{first} and dropped the others without saying so; \
             ask for each in a request of its own)",
            target.kind(),
            resolved.request.source,
            resolved.backend.id,
        ),
    }
}

/// Whether a request's window is one its collector and its mode can honour.
///
/// Two things cannot be moved by a window. A collector given to the program at
/// exec -- the preload shim -- exists only from the program's first
/// instruction to its last: it cannot be attached to a program that is already
/// running, and detaching it early would leave the program writing into a
/// channel nobody reads, which blocks it or kills it and in either case
/// changes the thing being measured. And a snapshot is a single read, which
/// can be taken late but has nothing to stop.
fn window_honoured(
    resolved: &ResolvedRequest,
    prepared: &dyn Prepared,
) -> Result<Option<String>, CollectError> {
    let Some(window) = resolved.request.window else {
        return Ok(None);
    };
    let refuse = |reason: String, whose: Whose| CollectError::Window {
        source_id: resolved.request.source.to_string(),
        window: window.to_string(),
        reason,
        whose,
    };
    let at_exec = prepared.requires_launch().is_some() || !prepared.launch_environment().is_empty();
    if at_exec {
        // This build's gap rather than the request's: the catalog has
        // backends for these sources -- a uprobe, a USDT probe -- that attach
        // to a running process, and a build that had them could honour this.
        let what = if window.attaches_late() {
            "attached once the program is running"
        } else {
            "detached while the program is still running"
        };
        // Held for the escalation, it is not refused: the session asked for
        // it only in case its layer were named, and the cheap answer does not
        // need it. The escalation says so when it names that layer.
        if window.on.is_some() {
            return Ok(Some(format!(
                "it is served through `{}`, which is given to the program at exec and read \
                 until it exits, so it cannot be {what}{}",
                resolved.backend.id,
                passed_over(resolved),
            )));
        }
        return Err(refuse(
            format!(
                "it is served through `{}`, which is given to the program at exec and read until \
                 it exits, so it cannot be {what}; leave the window out, or force a backend that \
                 attaches to a running process",
                resolved.backend.id
            ),
            Whose::Build,
        ));
    }
    if resolved.request.mode == Mode::Snapshot && window.stop.is_some() {
        return Err(refuse(
            "a snapshot is a single read and has nothing to detach: give it a start and no \
             stop, such as `2s..`"
                .to_owned(),
            Whose::Request,
        ));
    }
    Ok(None)
}

/// What the resolver said of the backends it did not choose, for a request
/// held for a layer it cannot serve: the probes' own reasons, so the reader
/// learns what this machine lacks rather than only that it lacks something.
fn passed_over(resolved: &ResolvedRequest) -> String {
    use crate::resolve::Verdict;
    let reasons: Vec<String> = resolved
        .chain
        .iter()
        .filter_map(|considered| match &considered.verdict {
            Verdict::Rejected(why) => Some(format!("`{}`: {}", considered.id, why.join("; "))),
            Verdict::Unimplemented => Some(format!(
                "`{}`: this build has no collector for it",
                considered.id
            )),
            Verdict::Selected | Verdict::NotReached => None,
        })
        .collect();
    if reasons.is_empty() {
        String::new()
    } else {
        format!(
            "; the rest of its chain was passed over -- {}",
            reasons.join(", ")
        )
    }
}

/// Whether a request's window lies inside the run a duration bounds.
///
/// A window that opens or closes after the run has ended would quietly become
/// a shorter one, or none, under the label that asked for more.
fn window_inside_the_run(resolved: &ResolvedRequest, duration_ns: u64) -> Result<(), CollectError> {
    let Some(window) = resolved.request.window else {
        return Ok(());
    };
    let past = [window.start, window.stop]
        .into_iter()
        .flatten()
        .find(|end| end.as_nanos() > duration_ns);
    match past {
        Some(end) => Err(CollectError::Window {
            source_id: resolved.request.source.to_string(),
            window: window.to_string(),
            reason: format!(
                "the run lasts at most {}, and this window reaches {end} past its start",
                crate::scalar::Duration::from_nanos(duration_ns)
            ),
            whose: Whose::Request,
        }),
        None => Ok(()),
    }
}

/// The note for a request asked to be sampled and served by a collector with
/// no cadence of its own.
///
/// It is being read at a poll interval nobody asked for. The number that comes
/// back is a real measurement of something; it is simply not a thousand
/// samples a second, and the two are told apart here rather than left to be
/// guessed at from a plausible figure.
fn unpaced(resolved: &ResolvedRequest, collector: &dyn Collector, pace: Pace) -> Option<String> {
    let mode = resolved.request.mode;
    (matches!(mode, Mode::Sample { .. } | Mode::Trace) && collector.tick().is_none()).then(|| {
        format!(
            "`{}` was asked for in `{mode}` mode and `{}` has no sampling of its own: it was \
             read every {}, which is a poll and not the rate that was requested",
            resolved.request.source,
            resolved.backend.id,
            span_ns(pace.interval.as_nanos() as u64),
        )
    })
}

/// Why a collector must not be attached late to this target, when it must
/// not: it would follow only threads started after it, and the target already
/// has others.
///
/// Asked at the attach rather than before the run, because how many threads a
/// program has by then is the program's business: a single-threaded one is
/// followed whole by inheritance whenever the collector is opened on it.
fn blind_to_running_threads(prepared: &dyn Prepared, target: &Target) -> Option<String> {
    let reason = prepared.follows_only_later_threads()?;
    let pid = target.first_pid()?;
    let threads = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?.count();
    (threads > 1).then(|| {
        format!(
            "{reason}, and process {pid} was already running {threads} threads, so a count \
             from here would be of one of them"
        )
    })
}

/// Where one request is in its window.
///
/// A request without a window is attached before the gate and stays
/// `Attached` to the end, which is every request there was before hot attach.
enum Slot {
    /// Prepared, and waiting for its window to open.
    Waiting(Box<dyn Prepared>),
    Attached(Box<dyn Collector>),
    /// Its window closed, and the collector with it: what is left is what the
    /// results need of it.
    Detached(Box<Detached>),
    /// Never attached. Why is in the collection's `stopped_because`.
    Missed,
}

/// What a detached collector leaves behind. Taken at the detach, because the
/// collector itself is dropped then: releasing what it held in the kernel is
/// the point of detaching.
struct Detached {
    value_attr: &'static str,
    stacks: BTreeMap<u64, Vec<String>>,
    notes: Vec<String>,
    by_ring: Vec<RingLoss>,
    facts: Vec<String>,
    row_stamp: Option<RowStamp>,
    failure: Option<String>,
    fields: crate::emit::Fields,
    provenance: crate::stack::Provenance,
}

/// What a collector's slot says of [`Collector::fields`]: the live
/// collector's count, what a detached one said at its detach, nothing for
/// one that never attached.
fn slot_fields(slot: &Slot) -> crate::emit::Fields {
    match slot {
        Slot::Attached(collector) => collector.fields(),
        Slot::Detached(record) => record.fields.clone(),
        Slot::Waiting(_) | Slot::Missed => crate::emit::Fields::new(),
    }
}

/// What a collector's slot says of [`Collector::provenance`], as
/// [`slot_fields`] reads its fields.
fn slot_provenance(slot: &Slot) -> crate::stack::Provenance {
    match slot {
        Slot::Attached(collector) => collector.provenance(),
        Slot::Detached(record) => record.provenance.clone(),
        Slot::Waiting(_) | Slot::Missed => crate::stack::Provenance::default(),
    }
}

/// One request's drops placed in time, as the runner reads them: each read
/// that found the collection's total grown adds the growth at its time.
#[derive(Debug, Clone, Default)]
struct Drains {
    placed: Vec<DrainDrop>,
    seen: u64,
}

impl Drains {
    /// The collection has dropped `total` in all by `ts_ns`.
    fn grew_to(&mut self, total: u64, ts_ns: u64) {
        if total > self.seen {
            self.placed.push(DrainDrop {
                ts_ns,
                dropped: total - self.seen,
            });
            self.seen = total;
        }
    }
}

/// When one request is attached and detached, in nanoseconds after the run
/// opened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Plan {
    attach_ns: u64,
    detach_ns: Option<u64>,
    /// Held for the escalation, which decides both ends.
    held: bool,
    /// Held, and this run cannot hold it: why. It is never attached, and the
    /// escalation is told so rather than the run refused, because the cheap
    /// answer does not need it.
    unheld: Option<String>,
}

impl Plan {
    fn of(window: Option<Window>) -> Plan {
        window.map_or(Plan::default(), |window| Plan {
            attach_ns: window.start_ns(),
            detach_ns: window.stop.map(crate::scalar::Duration::as_nanos),
            held: window.on.is_some(),
            unheld: None,
        })
    }
}

/// The one workload target of a session, if it has one.
///
/// Any number of machine targets may sit beside it, because that is the shape
/// of every confounder check: the machine's state is read at `system` or
/// `cpu`, and the thing it might explain is measured on the program. Two
/// workloads are still refused. They would have to share a launch, a stop
/// condition and an overhead denominator, and each of those is a question with
/// no single answer -- whose exit ends the run is the first of them.
fn workload_of(resolved: &[ResolvedRequest]) -> Result<Option<&Target>, CollectError> {
    let mut workload: Option<&Target> = None;
    for resolved in resolved {
        let target = &resolved.request.target;
        if target.is_machine() {
            continue;
        }
        match workload {
            None => workload = Some(target),
            Some(first) if first == target => {}
            Some(first) => {
                return Err(CollectError::MixedTargets {
                    first: first.to_string(),
                    second: target.to_string(),
                })
            }
        }
    }
    Ok(workload)
}

/// Lets collectors of one launch read one channel between them, where they
/// can.
///
/// Each is offered every earlier one in order, and the first that takes it
/// wins; what "can" means is the collector's own answer, in
/// [`Prepared::share_launch`].
fn share_launches(prepared: &mut [Box<dyn Prepared>], launching: &[bool]) {
    for later in 1..prepared.len() {
        if !launching[later] {
            continue;
        }
        let (earlier, rest) = prepared.split_at_mut(later);
        for (index, earlier) in earlier.iter_mut().enumerate() {
            if launching[index] && rest[0].share_launch(earlier.as_mut()) {
                break;
            }
        }
    }
}

/// How often a run carrying a budget looks at what it is spending.
///
/// Every check is two cheap reads -- `getrusage` and one `/proc` file -- and
/// `Overhead::measurement_ns` reports what they came to over the run. Ten times
/// a second is often enough to stop a collection that is running away, and rare
/// enough that the answer is not mostly itself: at roughly ten microseconds a
/// check this is about a thousandth of a percent of one cpu.
const BUDGET_CHECK_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Where one collection's accounting starts.
///
/// The whole run, normally. A collection a budget moved onto a cheaper backend
/// starts again from the moment it moved: what the backend before it spent is
/// sunk, and charging the replacement for its predecessor would condemn it
/// before it had read anything.
#[derive(Debug, Clone, Copy, Default)]
struct Since {
    self_cpu_ns: u64,
    observed_cpu_ns: u64,
    inside_ns: u64,
    /// Everything spent inside every collector by then, so a share is a share
    /// of the same span as the cpu time it divides.
    total_inside_ns: u64,
    wall_opened_ns: u64,
    /// What the collector had spent inside the target by then.
    in_target: Inside,
    /// This process's one-time loads by then ([`crate::stack::one_time_cpu_ns`]).
    one_time_ns: u64,
}

/// The cost half of a run: what it is spending, what it was allowed to spend,
/// and where it turns when the two disagree.
struct Spending<'a> {
    /// What each collection's overhead is a percentage of: its own target's
    /// cpu account, since a core beside a program is weighed against the core.
    meters: Vec<Meter>,
    /// Where each collection's cost inside its target is read, which the
    /// tool's own cpu does not include (finding 47).
    inside: Vec<InsideMeter>,
    /// What each had spent inside its target when the accounting opened.
    inside_opened: Vec<Inside>,
    /// Set at the close when the kernel stopped timing programs part way
    /// through a run that did not hold them on itself.
    stats_lost: Option<String>,
    /// Whether a program loaded during the run is timed by the kernel.
    program_stats: Result<(), String>,
    /// The target each collector is bound to, which a replacement has to be
    /// attached to as well.
    bound: Vec<Target>,
    /// The request each collector is serving *now*. A degradation changes it,
    /// which is why the resolved request written before the run cannot be the
    /// record of what produced the numbers.
    serving: Vec<ResolvedRequest>,
    ceilings: Vec<Option<f64>>,
    cheaper: &'a mut dyn Cheaper,
    /// Wall time spent inside each collector's own calls, which is how one
    /// process's cpu time is shared out between several collectors. Left at
    /// zero when a run has a single collector, because then the share is the
    /// whole and two clock reads per tick would buy nothing.
    inside_ns: Vec<u64>,
    since: Vec<Since>,
    opened_ns: u64,
    self_cpu_opened_ns: u64,
    /// [`crate::stack::one_time_cpu_ns`] when the run opened.
    one_time_opened_ns: u64,
    observed_opened: Vec<u64>,
    last_check_ns: u64,
    measurement_ns: u64,
    measurements: u64,
    /// What a budget did to each collection, in the words the user will read.
    notes: Vec<Vec<String>>,
    /// The backends each collection has run through, in order.
    served_by: Vec<Vec<String>>,
    /// Where a collection attached mid-run started paying. Its overhead is
    /// over its own window, because that is when it was running.
    window_since: Vec<Option<Since>>,
    /// The overhead of a collection that was detached, taken at the detach:
    /// what the run spent after it is not its cost.
    frozen: Vec<Option<Overhead>>,
    /// Which collections the escalation attached, and may detach rather than
    /// see the run end when they are over budget.
    sheddable: Vec<bool>,
    /// Collections found over budget with nowhere cheaper to go that the
    /// escalation may shed, with the reason the run would otherwise stop for.
    over: Vec<(usize, String)>,
    /// Set for a run with no fixed end: each collection is weighed over the
    /// last this many nanoseconds rather than since the run opened. A share
    /// of a run that idled for an hour is a share of that hour, and a burst
    /// would be under any ceiling against it.
    rolling_ns: Option<u64>,
    /// Where each account stood at every check within the rolling span, the
    /// oldest first: the baseline a rolling weighing is against.
    snapshots: Vec<std::collections::VecDeque<Since>>,
    /// When the run's duration elapses, which the drive loop wakes for: a
    /// counted request is read at a slow cadence, and a run that slept to its
    /// next read ended there instead of at its duration -- `-m count -d 600ms`
    /// observed a second (`open-requests 2947`, `6181`).
    deadline_ns: Option<u64>,
}

impl<'a> Spending<'a> {
    /// Opens the accounting, with the collectors attached and the program
    /// running: what was spent resolving, probing and attaching is this tool's
    /// startup, not the price of the measurement.
    fn open(
        meters: Vec<Meter>,
        mut inside: Vec<InsideMeter>,
        bound: Vec<Target>,
        serving: Vec<ResolvedRequest>,
        cheaper: &'a mut dyn Cheaper,
    ) -> Self {
        let count = serving.len();
        inside.resize_with(count, || {
            InsideMeter::Unknown("this run was not weighed".to_owned())
        });
        let inside_opened: Vec<Inside> = inside
            .iter_mut()
            .map(|meter| meter.read().unwrap_or_default())
            .collect();
        let inside_since = inside_opened.clone();
        let serving_ids = serving
            .iter()
            .map(|resolved| vec![resolved.backend.id.clone()])
            .collect();
        let ceilings = serving
            .iter()
            .map(|resolved| resolved.request.budget.map(|budget| budget.percent()))
            .collect();
        let opened_ns = monotonic_ns();
        let self_cpu_opened_ns = rusage_ns(libc::RUSAGE_SELF);
        let one_time_opened_ns = crate::stack::one_time_cpu_ns();
        let observed_opened: Vec<u64> = Meter::read_each(&meters)
            .into_iter()
            .map(|read| read.unwrap_or(0))
            .collect();
        Self {
            meters,
            inside,
            inside_opened,
            stats_lost: None,
            program_stats: Err("this run was not weighed".to_owned()),
            bound,
            serving,
            ceilings,
            cheaper,
            inside_ns: vec![0; count],
            since: (0..count)
                .map(|index| Since {
                    self_cpu_ns: self_cpu_opened_ns,
                    observed_cpu_ns: observed_opened.get(index).copied().unwrap_or(0),
                    inside_ns: 0,
                    total_inside_ns: 0,
                    wall_opened_ns: opened_ns,
                    in_target: inside_since.get(index).copied().unwrap_or_default(),
                    one_time_ns: one_time_opened_ns,
                })
                .collect(),
            opened_ns,
            self_cpu_opened_ns,
            one_time_opened_ns,
            observed_opened,
            last_check_ns: opened_ns,
            measurement_ns: 0,
            measurements: 0,
            notes: vec![Vec::new(); count],
            served_by: serving_ids,
            window_since: vec![None; count],
            frozen: vec![None; count],
            sheddable: vec![false; count],
            over: Vec::new(),
            rolling_ns: None,
            snapshots: vec![std::collections::VecDeque::new(); count],
            deadline_ns: None,
        }
    }

    /// What collection `index` is weighed from now: the whole run, or since
    /// it last moved -- or, in a rolling run, the oldest reading of its
    /// account inside the rolling span, whichever is later. Records this
    /// reading for the checks to come.
    fn baseline(&mut self, index: usize, now: Since) -> Since {
        let since = self.since[index];
        let Some(rolling_ns) = self.rolling_ns else {
            return since;
        };
        // The latest reading at or before the horizon is the baseline, so
        // the span weighed is the rolling one or a check interval more; none
        // that old yet is the run's first span, weighed from its start.
        let horizon_ns = now.wall_opened_ns.saturating_sub(rolling_ns);
        let snapshots = &mut self.snapshots[index];
        while snapshots.len() >= 2 && snapshots[1].wall_opened_ns <= horizon_ns {
            snapshots.pop_front();
        }
        let baseline = match snapshots.front() {
            Some(oldest)
                if oldest.wall_opened_ns <= horizon_ns
                    && oldest.wall_opened_ns >= since.wall_opened_ns =>
            {
                *oldest
            }
            _ => since,
        };
        snapshots.push_back(now);
        baseline
    }

    /// Whether anything here has a ceiling at all. A run with no budget is
    /// never interrupted to be weighed.
    fn enforcing(&self) -> bool {
        self.ceilings.iter().any(Option::is_some)
    }

    /// What one collection has cost since a baseline.
    fn spent(
        &self,
        index: usize,
        since: Since,
        self_cpu_ns: u64,
        observed_cpu_ns: Option<u64>,
        in_target: &Result<Inside, String>,
        now_ns: u64,
    ) -> Overhead {
        let collector_cpu_ns = self_cpu_ns.saturating_sub(since.self_cpu_ns);
        // One process serves every request, so `getrusage` answers for all of
        // them at once. With one collector the share is the whole and there is
        // nothing to approximate; with several it is the fraction of the time
        // spent inside collectors that went to this one.
        let total_inside_ns: u64 = self
            .inside_ns
            .iter()
            .sum::<u64>()
            .saturating_sub(since.total_inside_ns);
        let attributed_cpu_ns = match (self.inside_ns.len(), total_inside_ns) {
            (1, _) => collector_cpu_ns,
            (count, 0) => collector_cpu_ns / count as u64,
            (_, total) => {
                let mine = self.inside_ns[index].saturating_sub(since.inside_ns);
                (collector_cpu_ns as u128 * mine as u128 / total as u128) as u64
            }
        };
        // A one-time load is in the process's account; this collection's
        // share of it is its share of that account, all of it when it is
        // the only one.
        let one_time_ns = crate::stack::one_time_cpu_ns().saturating_sub(since.one_time_ns);
        let one_time_cpu_ns = if collector_cpu_ns == 0 {
            0
        } else {
            ((one_time_ns as u128 * attributed_cpu_ns as u128 / collector_cpu_ns as u128) as u64)
                .min(attributed_cpu_ns)
        };
        let in_target = match (&self.stats_lost, in_target) {
            (Some(lost), Ok(_)) if matches!(self.inside[index], InsideMeter::Programs { .. }) => {
                Err(lost.clone())
            }
            (_, reading) => reading.clone(),
        };
        let (in_target_cpu_ns, in_target_runs, in_target_left_out, in_target_from) = match in_target
        {
            Ok(now) => {
                let counted = !matches!(self.inside[index], InsideMeter::Outside(_));
                (
                    Some(now.cpu_ns.saturating_sub(since.in_target.cpu_ns)),
                    counted.then(|| now.runs.saturating_sub(since.in_target.runs)),
                    now.left_out.saturating_sub(since.in_target.left_out),
                    self.inside[index].describes(),
                )
            }
            Err(why) => (None, None, 0, why),
        };
        Overhead {
            before_window_cpu_ns: since.self_cpu_ns,
            after_window_cpu_ns: 0,
            collector_cpu_ns,
            attributed_cpu_ns,
            observed_cpu_ns: observed_cpu_ns
                .map(|observed| observed.saturating_sub(since.observed_cpu_ns)),
            observed_from: self.meters[index].describes(),
            wall_ns: now_ns.saturating_sub(since.wall_opened_ns),
            measurement_ns: self.measurement_ns,
            measurements: self.measurements,
            in_target_cpu_ns,
            in_target_runs,
            in_target_left_out,
            in_target_from,
            one_time_cpu_ns,
        }
    }

    /// Looks at what the run has cost, and acts when a collection is over its
    /// ceiling. Returns the reason the run must stop, when there is one.
    fn check(
        &mut self,
        slots: &mut [Slot],
        events: &mut [Vec<Event>],
        losses: &mut [Losses],
    ) -> Option<String> {
        if !self.enforcing() {
            return None;
        }
        let entered_ns = monotonic_ns();
        if entered_ns.saturating_sub(self.last_check_ns) < BUDGET_CHECK_INTERVAL.as_nanos() as u64 {
            return None;
        }
        let entered_cpu_ns = thread_cpu_ns();
        self.last_check_ns = entered_ns;
        let self_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
        self.measurements += 1;

        let mut stop = None;
        for index in 0..slots.len() {
            let Some(ceiling) = self.ceilings[index] else {
                continue;
            };
            // Only what is running is spending: a request waiting for its
            // window, or past it, has nothing here to weigh.
            if !matches!(slots[index], Slot::Attached(_)) {
                continue;
            }
            // A collection attached mid-run is weighed once it has run for
            // one check interval. Before that its attach -- loading a program,
            // reading the target's maps -- is most of its account, and a
            // percentage taken a few milliseconds in says how recently it was
            // attached rather than what it costs to run.
            if self.window_since[index].is_some_and(|since| {
                entered_ns.saturating_sub(since.wall_opened_ns)
                    < BUDGET_CHECK_INTERVAL.as_nanos() as u64
            }) {
                continue;
            }
            let observed_cpu_ns = self.meters[index].read();
            let in_target = self.inside[index].read();
            let baseline = self.baseline(
                index,
                Since {
                    self_cpu_ns,
                    observed_cpu_ns: observed_cpu_ns.unwrap_or(0),
                    inside_ns: self.inside_ns[index],
                    total_inside_ns: self.inside_ns.iter().sum(),
                    wall_opened_ns: entered_ns,
                    in_target: in_target.clone().unwrap_or_default(),
                    one_time_ns: crate::stack::one_time_cpu_ns(),
                },
            );
            let spent = self.spent(
                index,
                baseline,
                self_cpu_ns,
                observed_cpu_ns,
                &in_target,
                entered_ns,
            );
            // No denominator yet is not "within budget": it is "not knowable
            // yet", and a target that has burned no cpu at all has nothing for
            // an overhead to be a percentage of.
            let Some(percent) = spent.percent() else {
                continue;
            };

            if percent <= ceiling {
                continue;
            }
            let source = self.serving[index].request.source.to_string();
            let was = self.serving[index].backend.id.clone();
            // What the collector spent inside the target is in the percentage,
            // and the reason says how much of it, since the tool's own cpu
            // alone would read as far under the ceiling.
            let inside = match spent.in_target_cpu_ns {
                Some(ns) if ns > 0 => format!(" ({} of it inside the target)", span_ns(ns)),
                _ => String::new(),
            };
            let inside = if spent.one_time_cpu_ns > 0 {
                format!(
                    "{inside} (and {} once, loading symbol tables, weighed apart)",
                    span_ns(spent.one_time_cpu_ns)
                )
            } else {
                inside
            };
            // A collection a budget has already moved once is weighed on the
            // window since it moved, so the reason has to say which window it
            // is talking about or it reads as disagreeing with the run's own
            // total.
            let window = match self.rolling_ns {
                Some(rolling_ns) => format!(
                    " over the last {}, {ROLLING_BUDGET}",
                    span_ns(
                        entered_ns
                            .saturating_sub(baseline.wall_opened_ns)
                            .min(rolling_ns)
                    )
                ),
                None if self.served_by[index].len() > 1 => {
                    format!(" since it was moved onto `{was}`")
                }
                None => String::new(),
            };
            match self.cheaper.next(index, &spent, ceiling) {
                Some(next) => match self.degrade(index, next, slots, events, losses) {
                    Ok(moved) => self.notes[index].push(format!(
                        "over budget at {:.2} ms into the run: `{source}` through `{was}` spent \
                         {percent:.2} % of the target's cpu time{inside} against a ceiling of \
                         {ceiling} %, so it was moved onto `{}`{} for the rest of it; both \
                         counted from {:.3} ms to {:.3} ms into the run, and what the target did \
                         in that span can be in both",
                        entered_ns.saturating_sub(self.opened_ns) as f64 / 1e6,
                        moved.onto,
                        moved
                            .into_mode
                            .as_ref()
                            .map_or(String::new(), |mode| format!(" in `{mode}`")),
                        moved.overlap_from_ns.saturating_sub(self.opened_ns) as f64 / 1e6,
                        moved.overlap_to_ns.saturating_sub(self.opened_ns) as f64 / 1e6,
                    )),
                    Err(why) => {
                        stop = Some(format!(
                            "`{source}` spent {percent:.2} % of the target's cpu time{inside}\
                             {window} {OVER_ITS_CEILING} {ceiling} %, and the cheaper backend \
                             could not be started: {why}"
                        ));
                        break;
                    }
                },
                None => {
                    let reason = format!(
                        "`{source}` spent {percent:.2} % of the target's cpu time{inside}{window} \
                         {OVER_ITS_CEILING} {ceiling} %, and nothing cheaper than `{was}` is \
                         available"
                    );
                    if self.sheddable[index] {
                        self.over.push((index, reason));
                        continue;
                    }
                    stop = Some(reason);
                    break;
                }
            }
        }

        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);
        stop
    }

    /// Moves one collection onto a cheaper backend without ending the run.
    ///
    /// Everything the outgoing collector still held is drained into the run
    /// first: those events were paid for and are as real as the ones that come
    /// after. What changes from here is how the next ones are obtained, which
    /// is why the note says *when* it changed.
    fn degrade(
        &mut self,
        index: usize,
        next: ResolvedRequest,
        slots: &mut [Slot],
        events: &mut [Vec<Event>],
        losses: &mut [Losses],
    ) -> Result<Moved, String> {
        let onto = next.backend.id.clone();
        let Slot::Attached(current) = &mut slots[index] else {
            return Err("it is not attached".to_owned());
        };
        let before = loads_programs(&onto).then(programs_held);
        let prepared = prepare(&next).map_err(|error| error.to_string())?;
        let account = prepared.inside();
        Target::narrowed_to_one();
        // The replacement may be counting from its attach, and the outgoing
        // collector counts until its last read inside `stop`: between the two
        // both see the target. Stopping first would trade that overlap for a
        // gap, whose events nobody counts and nothing could show; an overlap
        // is bounded, and said. Nothing else runs between the replacement's
        // start and the outgoing's stop, which keeps it to the two calls.
        let overlap_from_ns = monotonic_ns();
        let mut replacement = prepared.attach(&self.bound[index])?;
        if let Some(named) = Target::narrowed_to_one() {
            return Err(one_process_of_several(&next, named).to_string());
        }
        replacement.start()?;
        // What the outgoing collector spent inside the target, read while its
        // programs are open: a `stop` that drops them leaves nothing to read.
        let _ = self.inside[index].read();
        let mut trailing = current.stop();
        let overlap_to_ns = monotonic_ns();
        into_our_ids(self.serving.get(index), &mut trailing, &mut losses[index]);
        events[index].append(&mut trailing);
        losses[index].absorb(current.losses());

        let programs = programs_since(before.as_ref());
        let meter = InsideMeter::of(&onto, account, programs, &self.program_stats);
        self.inside[index].moved_onto(meter);
        let was = &self.serving[index];
        let into_mode = (was.backend.id == onto).then(|| next.request.mode.to_string());
        losses[index].note(if was.backend.id == onto {
            format!(
                "the budget moved this collection from `{}` to `{}` on `{onto}` mid-run; the \
                 series is two modes end to end",
                was.request.mode, next.request.mode
            )
        } else {
            format!(
                "the budget moved this collection from `{}` to `{onto}` mid-run; the series is \
                 two backends end to end",
                was.backend.id
            )
        });
        *current = replacement;
        self.serving[index] = next;
        self.served_by[index].push(onto.clone());
        // The replacement is weighed on its own account.
        self.since[index] = Since {
            wall_opened_ns: self.opened_ns,
            ..self.now(index)
        };
        Ok(Moved {
            onto,
            into_mode,
            overlap_from_ns,
            overlap_to_ns,
        })
    }

    /// Where one collection's account stands at this instant.
    fn now(&mut self, index: usize) -> Since {
        Since {
            self_cpu_ns: rusage_ns(libc::RUSAGE_SELF),
            observed_cpu_ns: self.meters[index].read().unwrap_or(0),
            inside_ns: self.inside_ns[index],
            total_inside_ns: self.inside_ns.iter().sum(),
            wall_opened_ns: monotonic_ns(),
            in_target: self.inside[index].read().unwrap_or_default(),
            one_time_ns: crate::stack::one_time_cpu_ns(),
        }
    }

    /// Opens the account of a collection attached mid-run: its budget, and
    /// the overhead its results report, are over its own window.
    ///
    /// Taken *before* the attach, so that what attaching cost -- loading a
    /// program into the kernel while the target runs -- is in its account
    /// rather than in nobody's.
    fn attaching(&mut self, index: usize) {
        let entered_cpu_ns = thread_cpu_ns();
        let since = self.now(index);
        self.since[index] = since;
        self.window_since[index] = Some(since);
        self.measurements += 1;
        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);
    }

    /// Closes the account of a collection detached mid-run.
    fn detached(&mut self, index: usize, closed_ns: u64) {
        let entered_cpu_ns = thread_cpu_ns();
        let since = self.window_since[index].unwrap_or(self.whole_run(index));
        let self_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
        let observed_cpu_ns = self.meters[index].read();
        let in_target = self.inside[index].read();
        self.measurements += 1;
        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);
        self.frozen[index] = Some(self.spent(
            index,
            since,
            self_cpu_ns,
            observed_cpu_ns,
            &in_target,
            closed_ns,
        ));
    }

    /// Reads every meter once more, just before the launched program and the
    /// processes it started are killed, so that a meter keeping its highest
    /// reading holds what the tree spent up to the stop (`HighWater`): read
    /// after the kill, a launched program's account is only what it reaped.
    fn read_before_the_stop(&mut self) {
        let entered_cpu_ns = thread_cpu_ns();
        let _ = Meter::read_each(&self.meters);
        self.measurements += 1;
        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);
    }

    /// Reads every collection's in-target meter just before the collectors
    /// are stopped. A meter keeps its last reading, and a collector whose
    /// `stop` drops its programs closes the descriptors the kernel's account
    /// is read through: read only at the close, such a run without a budget
    /// -- never checked, so never read -- said zero spent inside the target
    /// (the block programs read "0 runs for 0 ns" beside 26.8 M requests).
    fn read_in_target_before_the_stops(&mut self) {
        let entered_cpu_ns = thread_cpu_ns();
        for meter in &mut self.inside {
            let _ = meter.read();
        }
        self.measurements += 1;
        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);
    }

    /// The account of a collection that ran for the whole of the run.
    fn whole_run(&self, index: usize) -> Since {
        Since {
            self_cpu_ns: self.self_cpu_opened_ns,
            observed_cpu_ns: self.observed_opened.get(index).copied().unwrap_or(0),
            inside_ns: 0,
            total_inside_ns: 0,
            wall_opened_ns: self.opened_ns,
            in_target: self.inside_opened.get(index).copied().unwrap_or_default(),
            one_time_ns: self.one_time_opened_ns,
        }
    }

    /// Closes the accounting and shares the run's cost out between the
    /// collections. This is the whole window, not the part since a degradation:
    /// what the run cost is what it cost.
    fn close(&mut self) -> Vec<Overhead> {
        let entered_cpu_ns = thread_cpu_ns();
        let self_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
        let observed_cpu_ns = Meter::read_each(&self.meters);
        let in_target: Vec<Result<Inside, String>> =
            self.inside.iter_mut().map(InsideMeter::read).collect();
        self.measurements += 1;
        let closed_ns = monotonic_ns();
        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);

        (0..self.ceilings.len())
            .map(|index| {
                if let Some(frozen) = &self.frozen[index] {
                    return frozen.clone();
                }
                let since = self.window_since[index].unwrap_or(self.whole_run(index));
                self.spent(
                    index,
                    since,
                    self_cpu_ns,
                    observed_cpu_ns[index],
                    &in_target[index],
                    closed_ns,
                )
            })
            .collect()
    }
}

#[cfg(test)]
impl Spending<'static> {
    /// A run with no ceiling and nothing to weigh it against, for the tests
    /// that are about the pacing rather than the price.
    fn unweighed(count: usize) -> Spending<'static> {
        let mut spending = Spending::open(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Box::leak(Box::new(StopWhenOverBudget)),
        );
        spending.meters = (0..count)
            .map(|_| Meter::Nothing("this run was not weighed"))
            .collect();
        spending.inside = (0..count).map(|_| InsideMeter::Outside(OUTSIDE)).collect();
        spending.inside_opened = vec![Inside::default(); count];
        spending.bound = vec![Target::System; count];
        spending.ceilings = vec![None; count];
        spending.inside_ns = vec![0; count];
        spending.since = vec![Since::default(); count];
        spending.notes = vec![Vec::new(); count];
        spending.served_by = vec![Vec::new(); count];
        spending.window_since = vec![None; count];
        spending.frozen = vec![None; count];
        spending.sheddable = vec![false; count];
        spending.snapshots = vec![std::collections::VecDeque::new(); count];
        spending
    }

    /// A run weighed against this process's own cpu time.
    ///
    /// Numerator and denominator are then the same account -- `getrusage` and
    /// `/proc/self/stat` report the same process -- so the ratio is around one
    /// hundred percent however much or little the machine is doing, and a test
    /// can put a ceiling either side of it without depending on the load. Which
    /// matters here: `cargo test` runs its tests in parallel threads of one
    /// process, and a measurement of "this process" is a measurement of all of
    /// them.
    /// The same, with this thread as the denominator rather than the process.
    ///
    /// For a request attached mid-run the numerator is that one collector's
    /// cpu, read on the thread that drives it, while "this process" is every
    /// test `cargo test` runs in parallel beside it -- twenty-odd threads in a
    /// full run, which put a collector spinning a whole thread under a 50 %
    /// ceiling and made the escalation's shedding test fail in every full run
    /// and pass alone. `/proc/<tid>/task/<tid>/stat` is the thread's own
    /// account; `/proc/<tid>/stat` is its whole process and every child it
    /// reaped, which let the forked children of other tests keep this run
    /// under its ceiling.
    fn weighed_against_this_thread(serving: Vec<ResolvedRequest>) -> Spending<'static> {
        // SAFETY: `gettid` takes no argument and cannot fail.
        let thread = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let count = serving.len();
        Spending::open(
            (0..count).map(|_| Meter::Threads(vec![thread])).collect(),
            (0..count).map(|_| InsideMeter::Outside(OUTSIDE)).collect(),
            vec![Target::Pid(vec![thread]); count],
            serving,
            Box::leak(Box::new(StopWhenOverBudget)),
        )
    }

    fn weighed_against_this_process(serving: Vec<ResolvedRequest>) -> Spending<'static> {
        let own = std::process::id() as i32;
        let count = serving.len();
        Spending::open(
            (0..count).map(|_| Meter::Tasks(vec![own])).collect(),
            (0..count).map(|_| InsideMeter::Outside(OUTSIDE)).collect(),
            vec![Target::Pid(vec![own]); count],
            serving,
            Box::leak(Box::new(StopWhenOverBudget)),
        )
    }
}

/// How one collector is driven: how often it is read, and whether one read is
/// the whole of it.
///
/// One of these per request, not one per run. A session used to take a single
/// interval -- the fastest collector's tick, or, where none declared one, the
/// mode of whichever request happened to be written first -- and apply it to
/// everything. So `poll@1s` beside `poll@10ms` was read every ten
/// milliseconds, `poll@10ms` beside a `count` was read once a second, and a
/// `snapshot` beside either became a series: request order decided another
/// request's sampling rate, which is the one thing a declarative session must
/// not do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pace {
    interval: StdDuration,
    /// A single read, after which this collector is done: there is no series
    /// and no second round. When every collector says so, the run itself is
    /// over after one round.
    snapshot: bool,
}

impl Pace {
    /// The pace of a request whose collector is not attached yet, replaced
    /// by its own the moment it is.
    const UNSET: Pace = Pace {
        interval: StdDuration::ZERO,
        snapshot: false,
    };
}

/// How one request is driven, from the mode it asked for and what its collector
/// needs.
///
/// A streaming collector's tick is not a preference: a ring drained less often
/// than it fills loses events, and nothing a request says makes that
/// acceptable, so it wins. A collector that declares none is read at the
/// cadence the request named -- and a mode that names no cadence at all, which
/// is `count`, `trace` and `sample` on a collector that does not stream, falls
/// back to the default poll interval as it always did.
fn pace_of(collector: &dyn Collector, mode: Mode) -> Pace {
    if mode == Mode::Snapshot {
        return Pace {
            interval: StdDuration::ZERO,
            snapshot: true,
        };
    }
    let interval = collector.tick().unwrap_or(match mode {
        Mode::Poll { interval } => StdDuration::from_nanos(interval.as_nanos()),
        _ => StdDuration::from_nanos(Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
    });
    Pace {
        interval,
        snapshot: false,
    }
}

/// What a run's collectors produced, per request, and why the run ended.
struct Driven {
    events: Vec<Vec<Event>>,
    losses: Vec<Losses>,
    /// The span each request was attached for.
    watched: Vec<Watched>,
    /// Why a request stopped before the run did, when it did: its window
    /// closed, or it was never attached.
    closed_because: Vec<Option<String>>,
    stopped_because: String,
    /// What the escalation said about each request once the run was over.
    escalated: Vec<Vec<String>>,
    drains: Vec<Drains>,
}

/// Why one request stopped being read before the run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Finished {
    /// It was a single read, and that read is taken.
    Snapshot,
    /// Its target went away before the workload did.
    Gone,
    /// Its window closed, or never opened on anything.
    Window,
}

/// Drives a set of collectors over one run, and closes it.
///
/// The order inside a round is the whole of the edge convention: **read
/// first, then decide**. A round reads every collector that is due, and only
/// then asks whether the run is over, so the last round of reads happens at the
/// moment the run ends rather than one interval before it. That is what makes a
/// polled counter's series complete without any collector reading again in
/// `stop`, and `the_runner_closes_the_window` measures how little time is
/// left over for such a read to cover.
///
/// *Due* is per collector, from its own [`Pace`]. A collector whose interval is
/// longer than another's is simply not read on the rounds in between, which is
/// what makes a session of several cadences mean what it says; the loop wakes
/// at the earliest of them, so the run is no less responsive to its stop
/// condition than when one interval governed everything.
///
/// A request with a window is attached and detached here, by its [`Plan`],
/// while the rest keep running. The same convention holds at both of its
/// edges. A window opens at the top of a round, so the collector's first read
/// is the round it was attached in -- a counter's baseline is taken at its
/// attach, not one interval later. And it closes at the bottom: a collector
/// whose window is closing is due at that moment whatever its pace, it is read,
/// and only then stopped and dropped, so its last interval is measured to its
/// detach just as the run's last interval is measured to the run's end.
///
/// A request held for the escalation has no time in its plan. The escalation
/// is asked once per round, after the reads and after the stop condition --
/// a run that is ending attaches nothing -- and what it answers is applied
/// detaches first: the layer it leaves is released before the one it names is
/// loaded, so the run never pays for two detailed layers at once. A collector
/// it attaches is due at once and read on the next round, which follows
/// without a sleep.
///
/// `ended` is consulted once per round and returns why the run should stop.
/// It is a closure rather than a launched child and a deadline so that the
/// convention can be tested against a stop condition a test controls.
///
/// `about_workload` says, per collector, whether its target going away ends the
/// run. A core or an interface beside a program is read for as long as the
/// program runs: one that disappears first stops being read, and its collection
/// says from when, but the program is still the thing being measured. `wake`
/// is a descriptor that becomes readable when the run should look again at
/// once -- the launched program's pidfd -- so that a program exiting between
/// two ticks ends the run at its exit rather than at the next tick.
#[allow(clippy::too_many_arguments)]
fn drive(
    slots: &mut [Slot],
    paces: &mut [Pace],
    plans: &[Plan],
    about_workload: &[bool],
    wake: Option<RawFd>,
    notice_exit: bool,
    spending: &mut Spending<'_>,
    mut escalation: Option<&mut dyn Escalate>,
    mut recording: Option<&mut Recording<'_, '_>>,
    mut ended: impl FnMut() -> Option<String>,
) -> Driven {
    let count = slots.len();
    let opened_ns = spending.opened_ns;
    let mut ledger = Ledger {
        events: vec![Vec::new(); count],
        losses: vec![Losses::default(); count],
        sequences: vec![0u64; count],
        // Everything is due at once on the first round: a series starts with
        // a reading, and a counter's first read is the baseline the rest are
        // against.
        due_ns: vec![monotonic_ns(); count],
        finished: vec![None; count],
        watched: slots
            .iter()
            .map(|slot| Watched {
                run_opened_ns: opened_ns,
                opened_ns,
                closed_ns: opened_ns,
                attached: matches!(slot, Slot::Attached(_)),
                attach_ns: None,
            })
            .collect(),
        closed_because: vec![None; count],
        // Only a run serving several collectors has to share its cpu time
        // out between them, and only then are the two clock reads a round
        // worth taking: with one collector the share is the whole.
        attributing: count > 1,
        drains: vec![Drains::default(); count],
    };
    // A held request the runner could not hold -- a collector given at exec
    // -- is never attached, and has said why already.
    for (index, slot) in slots.iter().enumerate() {
        if matches!(slot, Slot::Missed) {
            ledger.finished[index] = Some(Finished::Window);
            if let Some(reason) = &plans[index].unheld {
                ledger.closed_because[index] = Some(format!("never attached: {reason}"));
            }
        }
    }
    // When a collector's window closes, which makes it due then whatever its
    // pace says.
    let detach_at = |index: usize| plans[index].detach_ns.map(|ns| opened_ns + ns);

    let stopped_because = loop {
        let round_started_ns = monotonic_ns();

        // Windows that open now. The account is opened before the attach, so
        // what attaching costs while the target runs is this request's.
        for index in 0..count {
            if plans[index].held
                || !matches!(slots[index], Slot::Waiting(_))
                || round_started_ns < opened_ns + plans[index].attach_ns
            {
                continue;
            }
            ledger.attach(index, slots, paces, spending, None);
        }

        let mut gone = None;
        // Where each buffer stood before this round, and which requests read
        // in it: what a recording's trigger reads is the round's own events.
        let before: Vec<usize> = if recording.is_some() {
            ledger.events.iter().map(Vec::len).collect()
        } else {
            Vec::new()
        };
        let mut read_now = vec![false; count];
        // When each read of this round ended: a round can outlast its run.
        let mut read_at_ns = vec![0u64; count];
        for (index, slot) in slots.iter_mut().enumerate() {
            let Slot::Attached(collector) = slot else {
                continue;
            };
            let due =
                detach_at(index).map_or(ledger.due_ns[index], |at| ledger.due_ns[index].min(at));
            if ledger.finished[index].is_some() || round_started_ns < due {
                continue;
            }
            read_now[index] = true;
            if let Some(reason) = ledger.read(
                index,
                collector.as_mut(),
                paces[index],
                about_workload[index],
                round_started_ns,
                spending,
            ) {
                gone = Some(reason);
            }
            read_at_ns[index] = monotonic_ns();
        }
        if let Some(reason) = gone {
            break reason;
        }

        // Windows that close now, after the round's reads: the read that was
        // due at the detach has been taken, and what the collector still
        // holds is drained into its own collection before it is dropped.
        for index in 0..count {
            let closing = detach_at(index).is_some_and(|at| round_started_ns >= at);
            if !closing || !matches!(slots[index], Slot::Attached(_)) {
                continue;
            }
            ledger.detach(index, slots, spending, |at| {
                format!("its window closed: {}, as asked", detached_at(at))
            });
        }

        // A held request still waiting keeps nothing going: the escalation
        // decides from the requests that run, and once they are all done
        // there is nothing left for it to decide from.
        let waiting = slots
            .iter()
            .zip(plans)
            .any(|(slot, plan)| !plan.held && matches!(slot, Slot::Waiting(_)));
        let settled = |index: usize| {
            ledger.finished[index].is_some()
                || (plans[index].held && matches!(slots[index], Slot::Waiting(_)))
        };
        if !waiting && (0..count).all(settled) && ledger.finished.iter().any(Option::is_some) {
            let done: Vec<Finished> = ledger.finished.iter().flatten().copied().collect();
            if done.iter().all(|kind| *kind == Finished::Gone) {
                break "every target of the run went away".to_owned();
            }
            if done.contains(&Finished::Snapshot) {
                break "snapshot: a single read".to_owned();
            }
            break "every request's window had closed".to_owned();
        }

        // A recording reads the round before the stop condition is asked, so
        // a trigger that fires on the run's last round still writes its
        // document, and a document due now is cut from this round's reads.
        if let Some(recording) = recording.as_deref_mut() {
            if let Some(reason) = recording.round(&read_now, &before, &mut ledger, slots, spending)
            {
                break reason;
            }
        }

        if let Some(reason) = ended() {
            // The runner closes the window, so the round that meets the stop
            // condition is the last round of reads, and every polled
            // collector takes its closing read in it -- also one whose next
            // read was not due yet. Since the run wakes at its duration, the
            // stop is met before the read that used to follow it: a counted
            // window read at the start and due again a second later ended
            // with its baseline alone, and reported nothing -- as a polled
            // counter beside a launched program always had at its exit. A
            // collector read
            // within the last millisecond has its closing read already, and a
            // second one would be a point of nothing beside it; one read
            // earlier than that, a baseline included, has not.
            let closing_ns = monotonic_ns();
            for (index, slot) in slots.iter_mut().enumerate() {
                let Slot::Attached(collector) = slot else {
                    continue;
                };
                let pace = paces[index];
                let interval_ns = pace.interval.as_nanos() as u64;
                let last_read_ns = ledger.due_ns[index].saturating_sub(interval_ns);
                // Only a collector the runner paces: one that keeps its own
                // tick -- a per-task counter, a ring -- takes its closing read
                // in `stop`, where it can wait for what an exited target's
                // children fold into it, and a read here would take the
                // interval from it and report nothing in its place.
                // Read in this round, and not long ago: a collector read
                // first in a round another collector's read made outlast the
                // run -- a machine-wide `sched.offcpu` loading modules for
                // seconds -- has its baseline alone otherwise, no event and
                // nothing late. Read again, it is late, and counted.
                let read_lately = read_now[index]
                    && closing_ns.saturating_sub(read_at_ns[index]) < CLOSING_READ_SLACK_NS;
                if read_lately
                    || (read_now[index] && interval_ns == 0)
                    || collector.tick().is_some()
                    || pace.snapshot
                    || interval_ns == 0
                    || ledger.finished[index].is_some()
                    || closing_ns.saturating_sub(last_read_ns) < CLOSING_READ_SLACK_NS
                {
                    continue;
                }
                // Its target going away now changes nothing: the run is over.
                let _ = ledger.read(
                    index,
                    collector.as_mut(),
                    pace,
                    about_workload[index],
                    closing_ns,
                    spending,
                );
            }
            break reason;
        }

        // Asked after the stop condition, so a run that is ending attaches
        // nothing; and before the budget, so a layer attached now is weighed
        // from its first round.
        if let Some(escalation) = escalation.as_deref_mut() {
            let said: Vec<Described<'_>> = slots
                .iter()
                .zip(&ledger.drains)
                .map(|(slot, drains)| {
                    let (value_attr, row_stamp) = match slot {
                        Slot::Attached(collector) => {
                            (collector.value_attr(), collector.row_stamp())
                        }
                        Slot::Detached(record) => (record.value_attr, record.row_stamp),
                        Slot::Waiting(_) | Slot::Missed => ("", None),
                    };
                    Described {
                        value_attr,
                        row_stamp,
                        drains: &drains.placed,
                    }
                })
                .collect();
            escalation.described(&said);
            drop(said);
            let standing = standing_of(slots, &ledger.closed_because);
            let switches = escalation.decide(&Progress {
                opened_ns,
                now_ns: monotonic_ns(),
                events: &ledger.events,
                standing: &standing,
                bound: &spending.bound,
            });
            ledger.switch(switches, slots, paces, spending);
        }

        // Asked after the stop condition, so a run that was going to end anyway
        // ends for the reason it was going to end for rather than being
        // condemned on its last tick.
        if let Some(reason) = spending.check(slots, &mut ledger.events, &mut ledger.losses) {
            break reason;
        }
        // A request the escalation attached and nothing cheaper serves is
        // offered back to it before the run is ended for it: detaching it
        // keeps the budget and the cheap answer.
        let mut over = None;
        for (index, reason) in std::mem::take(&mut spending.over) {
            let shed = escalation
                .as_deref_mut()
                .and_then(|escalation| escalation.shed(index, &reason));
            match shed {
                Some(why) => ledger.detach(index, slots, spending, |at| {
                    format!("{} to hold the budget: {why}", detached_at(at))
                }),
                None => {
                    over = Some(reason);
                    break;
                }
            }
        }
        if let Some(reason) = over {
            break reason;
        }

        // Sleep until the next collector falls due, or the next window opens
        // or closes. A collector already behind its own schedule puts that
        // moment in the past, and the run goes straight round again.
        let next_due_ns = (0..count)
            .filter_map(|index| match &slots[index] {
                Slot::Waiting(_) if plans[index].held => None,
                Slot::Waiting(_) => Some(opened_ns + plans[index].attach_ns),
                Slot::Attached(_) if ledger.finished[index].is_none() => Some(
                    detach_at(index)
                        .map_or(ledger.due_ns[index], |at| ledger.due_ns[index].min(at)),
                ),
                Slot::Attached(_) => detach_at(index),
                Slot::Detached(_) | Slot::Missed => None,
            })
            .min();
        // A document due to be cut is a moment to wake for too, and so is
        // the end of the run's duration: the run ends there, not at whichever
        // read would have come after it.
        let next_due_ns = [
            recording.as_deref().and_then(Recording::due_ns),
            spending.deadline_ns,
        ]
        .into_iter()
        .flatten()
        .fold(next_due_ns, |next, at| {
            Some(next.map_or(at, |next| next.min(at)))
        });
        if let Some(next_due_ns) = next_due_ns {
            let remaining_ns = next_due_ns.saturating_sub(monotonic_ns());
            if remaining_ns > 0 {
                if notice_exit && wake.is_none() {
                    sleep_noticing_exit(remaining_ns, &mut ended);
                } else {
                    sleep_or_wake(remaining_ns, wake);
                }
            }
        }
    };

    if let Some(recording) = recording {
        recording.end(&stopped_because, &mut ledger, slots, spending);
    }

    // What the escalation has to say about each request, now that it has
    // seen the whole run.
    let mut escalated = vec![Vec::new(); count];
    if let Some(escalation) = escalation {
        let standing = standing_of(slots, &ledger.closed_because);
        for (index, note) in escalation.close(&Progress {
            opened_ns,
            now_ns: monotonic_ns(),
            events: &ledger.events,
            standing: &standing,
            bound: &spending.bound,
        }) {
            if let Some(notes) = escalated.get_mut(index) {
                notes.push(note);
            }
        }
    }

    Driven {
        events: ledger.events,
        losses: ledger.losses,
        watched: ledger.watched,
        closed_because: ledger.closed_because,
        stopped_because,
        escalated,
        drains: ledger.drains,
    }
}

/// Where each request stands, as the escalation is shown it.
fn standing_of(slots: &[Slot], closed_because: &[Option<String>]) -> Vec<Standing> {
    slots
        .iter()
        .zip(closed_because)
        .map(|(slot, because)| match slot {
            Slot::Waiting(_) => Standing::Waiting,
            Slot::Attached(_) => Standing::Attached,
            Slot::Detached(_) => Standing::Detached,
            Slot::Missed => Standing::Missed(
                because
                    .clone()
                    .unwrap_or_else(|| "it was never attached".to_owned()),
            ),
        })
        .collect()
}

/// Everything a run keeps per request while it drives them: what each has
/// produced and lost, when it is next due, and the span it was watched for.
struct Ledger {
    events: Vec<Vec<Event>>,
    losses: Vec<Losses>,
    sequences: Vec<u64>,
    due_ns: Vec<u64>,
    finished: Vec<Option<Finished>>,
    watched: Vec<Watched>,
    closed_because: Vec<Option<String>>,
    attributing: bool,
    /// Each request's drops placed in time, and the total they sum to.
    drains: Vec<Drains>,
}

impl Ledger {
    fn seconds_in(&self, ns: u64) -> f64 {
        ns.saturating_sub(self.watched.first().map_or(ns, |one| one.run_opened_ns)) as f64 / 1e9
    }

    /// Reads one collector that is due. Returns why the run must end, when
    /// the workload's own target went away.
    fn read(
        &mut self,
        index: usize,
        collector: &mut dyn Collector,
        pace: Pace,
        about_workload: bool,
        round_started_ns: u64,
        spending: &mut Spending<'_>,
    ) -> Option<String> {
        let entered_ns = if self.attributing { monotonic_ns() } else { 0 };
        match collector.sample(self.sequences[index]) {
            Ok(mut sampled) => {
                into_our_ids(
                    spending.serving.get(index),
                    &mut sampled,
                    &mut self.losses[index],
                );
                self.sequences[index] += sampled.len() as u64;
                self.events[index].append(&mut sampled);
                // A drained collector's count is read at each drain, which is
                // what places its drops in time; a polled one's is read once,
                // at its stop, for what a read of its losses would cost.
                if collector.tick().is_some() {
                    let dropped = self.losses[index].dropped + collector.losses().dropped;
                    self.drains[index].grew_to(dropped, monotonic_ns());
                }
            }
            Err(SampleError::TargetGone(reason)) if about_workload => return Some(reason),
            Err(SampleError::TargetGone(reason)) => {
                let at = self.seconds_in(round_started_ns);
                self.losses[index].note(format!(
                    "stopped reading {at:.3} s into the run, before the workload ended: {reason}"
                ));
                self.finished[index] = Some(Finished::Gone);
                return None;
            }
            Err(SampleError::Transient(reason)) => {
                self.losses[index].failed_reads += 1;
                self.losses[index].note(reason);
            }
        }
        if self.attributing {
            spending.inside_ns[index] += monotonic_ns().saturating_sub(entered_ns);
        }

        if pace.snapshot {
            self.finished[index] = Some(Finished::Snapshot);
            return None;
        }
        // When the next reading was due, and whether this one got in the
        // way of it. A round that overran is counted as a late tick -- at
        // that point the series no longer has the spacing it claims -- and
        // the schedule is re-based on now, so that being late once does not
        // leave every reading after it owing time it can never make up.
        let interval_ns = pace.interval.as_nanos() as u64;
        let finished_ns = monotonic_ns();
        let took_ns = finished_ns.saturating_sub(self.due_ns[index]);
        // An interval of zero is "as fast as this can be read", which is
        // not a cadence and cannot be fallen behind. Counting every round
        // of such a run as late is how a snapshot served as a poll came to
        // report 158 losses that had not happened.
        if interval_ns > 0 && took_ns > interval_ns {
            self.losses[index].late_tick(took_ns, interval_ns);
            self.due_ns[index] = finished_ns;
        } else {
            self.due_ns[index] += interval_ns;
        }
        None
    }

    /// Attaches a request that was waiting, while the target runs: by its
    /// window when `escalated` is `None`, by the escalation otherwise, whose
    /// reason is then the first thing its collection says.
    fn attach(
        &mut self,
        index: usize,
        slots: &mut [Slot],
        paces: &mut [Pace],
        spending: &mut Spending<'_>,
        escalated: Option<&str>,
    ) {
        let Slot::Waiting(prepared) = std::mem::replace(&mut slots[index], Slot::Missed) else {
            unreachable!("only a waiting request is attached");
        };
        spending.attaching(index);
        let entered_ns = monotonic_ns();
        let backend = spending.serving[index].backend.id.clone();
        if loads_programs(&backend) {
            crate::bpf::pidns::remember();
        }
        let before = loads_programs(&backend).then(programs_held);
        let account = prepared.inside();
        Target::narrowed_to_one();
        let mut attached = match blind_to_running_threads(prepared.as_ref(), &spending.bound[index])
        {
            Some(reason) => Err(reason),
            None => prepared
                .attach(&spending.bound[index])
                .and_then(|mut collector| collector.start().map(|()| collector)),
        };
        if let Some(named) = Target::narrowed_to_one() {
            attached = Err(one_process_of_several(&spending.serving[index], named).to_string());
        }
        let attached_ns = monotonic_ns();
        if attached.is_ok() {
            // The programs it loaded are its own from their first run: what
            // they have spent is all this collection's, from zero.
            let programs = programs_since(before.as_ref());
            spending.inside[index] =
                InsideMeter::of(&backend, account, programs, &spending.program_stats);
            let opened = spending.inside[index].read().unwrap_or_default();
            spending.since[index].in_target = opened;
            if let Some(since) = spending.window_since[index].as_mut() {
                since.in_target = opened;
            }
        }
        let took_ns = attached_ns.saturating_sub(entered_ns);
        if self.attributing {
            spending.inside_ns[index] += took_ns;
        }
        let source = spending.serving[index].request.source.to_string();
        let at = self.seconds_in(entered_ns);
        match attached {
            Ok(collector) => {
                let mode = spending.serving[index].request.mode;
                paces[index] = pace_of(collector.as_ref(), mode);
                if let Some(note) =
                    unpaced(&spending.serving[index], collector.as_ref(), paces[index])
                {
                    spending.notes[index].push(note);
                }
                spending.notes[index].push(match escalated {
                    None => format!(
                        "watched only from {at:.3} s into the run, as its window asked: \
                         attaching it took {} while the target ran, and its numbers cover its \
                         window alone",
                        span_ns(took_ns),
                    ),
                    Some(why) => format!(
                        "attached {at:.3} s into the run by the escalation, and watched from \
                         then only: {why}; attaching it took {} while the target ran \
                         (CLOCK_MONOTONIC {entered_ns} to {attached_ns} ns)",
                        span_ns(took_ns),
                    ),
                });
                self.due_ns[index] = attached_ns;
                self.watched[index].opened_ns = attached_ns;
                self.watched[index].attached = true;
                self.watched[index].attach_ns = Some(took_ns);
                slots[index] = Slot::Attached(collector);
            }
            Err(reason) => {
                // A window that asked for something and got nothing: a loss,
                // not an empty series that reads as a quiet one.
                self.losses[index].failed_reads += 1;
                self.losses[index].note(format!(
                    "`{source}` could not be attached {at:.3} s into the run, so nothing was \
                     collected over its window: {reason}"
                ));
                self.closed_because[index] = Some(format!("never attached: {reason}"));
                slots[index] = Slot::Missed;
                self.finished[index] = Some(Finished::Window);
            }
        }
    }

    /// Stops and drops an attached collector, draining what it still holds
    /// into its own collection first. `because` is given how far into the run
    /// the detach happened and says why, as the collection's
    /// `stopped_because`.
    fn detach(
        &mut self,
        index: usize,
        slots: &mut [Slot],
        spending: &mut Spending<'_>,
        because: impl FnOnce(f64) -> String,
    ) {
        let Slot::Attached(mut collector) = std::mem::replace(&mut slots[index], Slot::Missed)
        else {
            unreachable!("only an attached request is detached");
        };
        // Its window closes before its last read, as at the end of the run:
        // what the stop drains was all written before it returns.
        let closed_ns = monotonic_ns();
        self.watched[index].closed_ns = closed_ns;
        // Read before the stop as well as after it: a collector whose `stop`
        // drops its programs leaves them nothing to report afterwards.
        let _ = spending.inside[index].read();
        let mut trailing = collector.stop();
        into_our_ids(
            spending.serving.get(index),
            &mut trailing,
            &mut self.losses[index],
        );
        self.events[index].append(&mut trailing);
        self.losses[index].absorb(collector.losses());
        self.drains[index].grew_to(self.losses[index].dropped, closed_ns);
        let record = Detached {
            value_attr: collector.value_attr(),
            stacks: collector.stacks(),
            notes: collector.notes(),
            by_ring: collector.losses_by_ring(),
            facts: collector.facts(),
            row_stamp: collector.row_stamp(),
            failure: collector.failure(),
            fields: collector.fields(),
            provenance: collector.provenance(),
        };
        // The last look at what it spent inside the target, while its
        // programs are still open to be read.
        let _ = spending.inside[index].read();
        drop(collector);
        // What it cost is weighed up to the end of its stop, which it paid.
        let stopped_ns = monotonic_ns();
        spending.detached(index, stopped_ns);
        self.closed_because[index] = Some(because(self.seconds_in(stopped_ns)));
        slots[index] = Slot::Detached(Box::new(record));
        // A collection whose target had already gone keeps saying so.
        self.finished[index].get_or_insert(Finished::Window);
    }

    /// Applies what the escalation answered: detaches, then attaches.
    fn switch(
        &mut self,
        switches: Vec<Switch>,
        slots: &mut [Slot],
        paces: &mut [Pace],
        spending: &mut Spending<'_>,
    ) {
        let (detaches, attaches): (Vec<Switch>, Vec<Switch>) =
            switches.into_iter().partition(|one| !one.attach);
        for Switch { index, why, .. } in detaches {
            if matches!(slots.get(index), Some(Slot::Attached(_))) {
                self.detach(index, slots, spending, |at| {
                    format!("{} by the escalation: {why}", detached_at(at))
                });
            }
        }
        for Switch { index, why, .. } in attaches {
            if matches!(slots.get(index), Some(Slot::Waiting(_))) {
                self.attach(index, slots, paces, spending, Some(&why));
            }
        }
    }
}

/// How recently a collector must have been read for that read to stand as the
/// window's closing read: well under any tick a collector keeps, and well over
/// the gap between two reads of one round.
const CLOSING_READ_SLACK_NS: u64 = 1_000_000;

/// The span a recording's budget is weighed over, at the least: the retention
/// bound when it is longer. Two seconds is twenty budget checks, which is
/// enough for a percentage not to be one unlucky read, and short enough that a
/// burst is weighed against its own seconds rather than the quiet before it.
const RECORDING_BUDGET_FLOOR: StdDuration = StdDuration::from_secs(2);

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
const EVICTION_FRACTION: u64 = 16;

/// A document the trigger opened and `after` has not closed yet.
struct Pending {
    fired: Fired,
    close_at_ns: u64,
}

/// A recording in progress: the runner's half of a [`Recorder`].
///
/// It keeps no events of its own. The ledger's are the buffer; this decides
/// what they may keep, reads the trigger's request after each round, and cuts
/// documents out of them.
struct Recording<'r, 'a> {
    asked: &'r mut Recorder<'a>,
    /// The trigger with `value` read as the collector's own name for it.
    trigger: crate::filter::FilterExpr,
    predicate: String,
    value_attr: String,
    labels: Vec<String>,
    run_opened_ns: u64,
    /// True until the predicate matches, and again after a round in which it
    /// did not: a trigger fires on the round its predicate becomes true, so a
    /// level held for a second writes one document and not ten.
    armed: bool,
    pending: Option<Pending>,
    written: Vec<Fired>,
    /// How many events at the front of each buffer are in a document already,
    /// so that evicting them is not a loss.
    documented: Vec<usize>,
    /// When the last document was cut: a round read at or before it is in
    /// that document or was counted when it was evicted.
    last_cut_ns: u64,
    /// When each round of each request read, for as long as it is retained.
    rounds: Vec<std::collections::VecDeque<u64>>,
    evicted_events: Vec<u64>,
    evicted_rounds: Vec<u64>,
    last_evicted_ns: u64,
    /// Each collection's losses at the last cut, runner's and collector's
    /// together, which a document's are the difference from.
    losses_at_cut: Vec<Losses>,
    /// What was read after the last document and is in none, once the run
    /// is over.
    unrecorded: Vec<Evicted>,
}

impl<'r, 'a> Recording<'r, 'a> {
    fn new(
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
    fn due_ns(&self) -> Option<u64> {
        self.pending.as_ref().map(|pending| pending.close_at_ns)
    }

    /// After a round's reads: note which requests read, read the trigger's
    /// new events, cut what is due, and evict what the retention no longer
    /// holds. Returns why the run ends, once the recording has written all
    /// it may or could not write what it cut.
    fn round(
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
    fn end(&mut self, why: &str, ledger: &mut Ledger, slots: &[Slot], spending: &Spending<'_>) {
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

    fn recorded(&self, stopped_because: &str, closed_ns: u64, budget_interval_ns: u64) -> Recorded {
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
fn read_value_as(
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

/// How long a run without a descriptor to wake on sleeps before it asks
/// again whether the launched program exited.
const EXIT_SLICE_NS: u64 = 10_000_000;

/// Sleeps for `ns`, or until `ended` says the run is over, asking it every
/// [`EXIT_SLICE_NS`].
///
/// For a launched program with no pidfd (Linux before 5.3), whose exit only
/// the next round noticed: the closing round came up to a whole pace after
/// the exit, and every polled rate's last interval was divided by time the
/// program was no longer there for -- `power.wakeups` at `poll@1s` read 75 a
/// second for the last interval of a program that woke 930 times a second to
/// its end (Ubuntu 18.04's 4.15, distroproof). Asking also moves the last time
/// the program was seen alive, which is what bounds its exit's uncertainty.
fn sleep_noticing_exit(ns: u64, ended: &mut impl FnMut() -> Option<String>) {
    let until_ns = monotonic_ns().saturating_add(ns);
    loop {
        let now_ns = monotonic_ns();
        if now_ns >= until_ns {
            return;
        }
        std::thread::sleep(StdDuration::from_nanos(
            (until_ns - now_ns).min(EXIT_SLICE_NS),
        ));
        if ended().is_some() {
            return;
        }
    }
}

/// Sleeps for `ns`, or until `wake` becomes readable if that is sooner.
///
/// A readable `wake` is not consumed: a pidfd stays readable once its process
/// has exited, and the round after this one is what asks why.
fn sleep_or_wake(ns: u64, wake: Option<RawFd>) {
    let Some(fd) = wake else {
        std::thread::sleep(StdDuration::from_nanos(ns));
        return;
    };
    let timeout = libc::timespec {
        tv_sec: (ns / 1_000_000_000) as _,
        tv_nsec: (ns % 1_000_000_000) as libc::c_long,
    };
    let mut watched = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd this frame owns, and a timeout it owns; no signal
    // mask is changed.
    let result = unsafe { libc::ppoll(&mut watched, 1, &timeout, std::ptr::null()) };
    if result < 0 {
        // Interrupted, or refused: the tick is still an honest fallback, and
        // the round after it asks the program directly.
        std::thread::sleep(StdDuration::from_nanos(ns));
    }
}

/// Evaluates the request's filter over what was collected.
///
/// This is the other half of filter placement. The resolver decides where a
/// predicate runs and says so in the resolved file; for a backend that returns
/// whole records, "userspace" means here, after the event has been paid for. A
/// backend that was credited with pushing the predicate down is taken at its
/// word and its events are left alone -- running the filter twice would hide
/// the bug if it had not.
fn apply_filter(resolved: &ResolvedRequest, events: &mut Vec<Event>) -> Option<Filtered> {
    let predicate = resolved.request.filter.as_ref()?;
    if !matches!(resolved.filter, Some(FilterPlacement::Userspace(_))) {
        return None;
    }
    let before = events.len();
    let unseen_fields = predicate
        .fields()
        .into_iter()
        .filter(|field| *field != "kind" && !events.iter().any(|event| event.attr(field).is_some()))
        .map(str::to_owned)
        .collect();
    events.retain(|event| crate::filter::matches(predicate, event));
    Some(Filtered {
        predicate: predicate.to_string(),
        kept: events.len() as u64,
        dropped: (before - events.len()) as u64,
        unseen_fields,
    })
}

/// Collects what every collector needs in the launched program's environment,
/// refusing a disagreement rather than letting one win silently.
fn merge_environments<'a>(
    prepared: impl IntoIterator<Item = &'a dyn Prepared>,
) -> Result<Vec<(String, String)>, CollectError> {
    let mut merged: Vec<(String, String)> = Vec::new();
    for prepared in prepared {
        for (name, value) in prepared.launch_environment() {
            match merged.iter().find(|(existing, _)| *existing == name) {
                Some((_, existing)) if *existing != value => {
                    return Err(CollectError::ConflictingEnvironment {
                        variable: name,
                        first: existing.clone(),
                        second: value,
                    })
                }
                Some(_) => {}
                None => merged.push((name, value)),
            }
        }
    }
    Ok(merged)
}

/// Whether this build can actually collect a source through a backend.
///
/// The resolver asks before choosing. A machine that gains a privilege gains
/// backends, and without this a source that worked five minutes ago stops
/// working because something better became reachable that nothing implements --
/// which is an absurd way for a tool to react to being given more access.
pub fn implements(source_id: &str, backend: &str) -> bool {
    registry()
        .iter()
        .any(|factory| factory.source == source_id && factory.backend == backend)
}

/// The sources a collector exists for, for error messages that say what *is*
/// possible rather than only what is not.
///
/// Derived from the registry, because a hand-written second list had already
/// drifted out of agreement with the first one by the time it was needed.
pub fn implemented() -> Vec<String> {
    coverage()
        .into_iter()
        .map(|(source, backends)| format!("{source} ({})", backends.join(", ")))
        .collect()
}

/// Which backends this build can actually collect, by source.
///
/// The catalog says what the vocabulary is; this says how much of it has been
/// written. The two are deliberately separate -- a source with no collector is
/// still a valid request, and `--no-resolve` will still write it down for a
/// machine and a build that can serve it -- but a reader looking at the catalog
/// deserves to see which is which without running into it.
pub fn coverage() -> BTreeMap<&'static str, Vec<&'static str>> {
    let mut by_source: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for factory in registry() {
        by_source
            .entry(factory.source)
            .or_default()
            .push(factory.backend);
    }
    by_source
}

/// Finds the collector for a resolved backend.
///
/// The lookup is on the backend id, not on the source alone: two backends of the
/// same source read different things and produce differently trustworthy
/// numbers.
fn prepare(resolved: &ResolvedRequest) -> Result<Box<dyn Prepared>, CollectError> {
    let source_id = resolved.request.source.as_str();
    let backend = resolved.backend.id.as_str();
    let factory = registry()
        .into_iter()
        .find(|factory| factory.source == source_id && factory.backend == backend)
        .ok_or_else(|| CollectError::NoCollector {
            source_id: source_id.to_owned(),
            backend: backend.to_owned(),
        })?;
    // A factory refusal is this build's answer to the request as written: it is
    // reached with nothing launched, nothing opened and nothing read, so it
    // cannot be about this machine. `CollectError::start` has the argument.
    (factory.build)(resolved).map_err(|reason| CollectError::start(source_id, reason, Whose::Build))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every way a collection is detached reads back as a detach from its
    /// stored `stopped_because`, and no way a run ends does.
    #[test]
    fn a_stored_reason_says_whether_its_collection_was_detached() {
        for detached in [
            format!("its window closed: {}, as asked", detached_at(1.0)),
            format!("{} to hold the budget: over 2 %", detached_at(0.25)),
            format!("{} by the escalation: quiet", detached_at(12.5)),
        ] {
            assert!(stopped_at_a_detach(&detached), "{detached}");
        }
        for ended in [
            "the duration elapsed",
            "the target exited",
            "never attached: the run ended 3.000 s in, before this request's window `2s..` opened",
            "never attached: the escalation did not name its layer in the 3.000 s the run lasted",
            "",
        ] {
            assert!(!stopped_at_a_detach(ended), "{ended}");
        }
    }

    /// `drive` over collectors that are all attached before the run and stay
    /// attached to its end: every run there was before windows, and what the
    /// pacing and budget tests below are about.
    fn drive_attached(
        collectors: &mut Vec<Box<dyn Collector>>,
        paces: &[Pace],
        about_workload: &[bool],
        wake: Option<RawFd>,
        spending: &mut Spending<'_>,
        ended: impl FnMut() -> Option<String>,
    ) -> (Vec<Vec<Event>>, Vec<Losses>, String) {
        let mut slots: Vec<Slot> = collectors.drain(..).map(Slot::Attached).collect();
        let mut paces = paces.to_vec();
        let plans = vec![Plan::default(); slots.len()];
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            about_workload,
            wake,
            false,
            spending,
            None,
            None,
            ended,
        );
        collectors.extend(slots.into_iter().map(|slot| match slot {
            Slot::Attached(collector) => collector,
            _ => unreachable!("nothing here had a window"),
        }));
        (driven.events, driven.losses, driven.stopped_because)
    }
    use crate::catalog::Catalog;

    /// Every registered collector must name a real catalog entry and a real
    /// backend in its chain. A typo here would surface as "no collector is
    /// written for this" at run time, which reads like an unimplemented source
    /// rather than a mistake.
    /// The two shapes `/proc/self/fdinfo` gives a bpf descriptor, as a 6.x
    /// kernel prints them: a program carries its run time, a link only the id
    /// of the program it holds, and only the first is a program's account.
    #[test]
    fn a_program_s_fdinfo_is_its_account_and_a_link_s_is_not() {
        let program = "pos:\t0\nflags:\t02000002\nmnt_id:\t16\nino:\t2080\nprog_type:\t17\n\
                       prog_jited:\t1\nprog_tag:\t3b185187f1855c4c\nmemlock:\t4096\n\
                       prog_id:\t812\nrun_time_ns:\t123456\nrun_cnt:\t789\n\
                       recursion_misses:\t0\nverified_insns:\t12\n";
        assert_eq!(
            parse_program_fdinfo(program),
            Some((
                812,
                Inside {
                    cpu_ns: 123_456,
                    runs: 789,
                    left_out: 0,
                }
            ))
        );
        let link = "pos:\t0\nflags:\t02000002\nmnt_id:\t16\nino:\t2080\n\
                    link_type:\traw_tracepoint\nlink_id:\t55\nprog_tag:\t3b185187f1855c4c\n\
                    prog_id:\t812\ntp_name:\tsys_enter\n";
        assert_eq!(parse_program_fdinfo(link), None);
    }

    /// What a collector spends inside its target is weighed against what the
    /// target did of its own: a program four times slower for being traced
    /// cost three hundred percent, which is what it measures of itself, not
    /// the seventy-five percent of its cpu the tracing was.
    #[test]
    fn the_cost_inside_the_target_is_weighed_against_what_the_target_did_of_its_own() {
        let overhead = Overhead {
            attributed_cpu_ns: 1_000_000,
            observed_cpu_ns: Some(400_000_000),
            in_target_cpu_ns: Some(300_000_000),
            ..Overhead::default()
        };
        let percent = overhead.percent().expect("a percentage");
        assert!((percent - 301.0).abs() < 1e-9, "{percent}");
        assert!((overhead.own_percent().unwrap() - 0.25).abs() < 1e-9);

        // Unknown is the tool's own share, never a zero added in.
        let unknown = Overhead {
            in_target_cpu_ns: None,
            ..overhead.clone()
        };
        assert_eq!(unknown.percent(), unknown.own_percent());
        assert!(!unknown.in_target_known());

        // More inside than the target spent at all: programs that ran for
        // every task and filtered most out. The whole account is the
        // denominator, which never understates.
        let everywhere = Overhead {
            in_target_cpu_ns: Some(900_000_000),
            ..overhead
        };
        assert!(everywhere.percent().unwrap() > 200.0);
    }

    /// A table loaded once inside the window is weighed apart from the rate:
    /// a 25 ms load against the 15 ms a waiting program burns by the first
    /// check reads 166 % of a collector whose running cost is 6 %. It stays
    /// in what the tool spent.
    #[test]
    fn a_one_time_load_is_weighed_apart_from_the_rate_and_still_spent() {
        let overhead = Overhead {
            collector_cpu_ns: 25_900_000,
            attributed_cpu_ns: 25_900_000,
            one_time_cpu_ns: 25_000_000,
            observed_cpu_ns: Some(15_000_000),
            in_target_cpu_ns: Some(0),
            ..Overhead::default()
        };
        assert!((overhead.percent().unwrap() - 6.0).abs() < 1e-9);
        assert!((overhead.own_percent().unwrap() - 6.0).abs() < 1e-9);
        assert_eq!(overhead.tool_cpu_ns(), 25_900_000);
        let all_of_it = Overhead {
            one_time_cpu_ns: 0,
            ..overhead
        };
        assert!(all_of_it.percent().unwrap() > 170.0);
    }

    /// Which account a collection's in-target cost is read from is decided by
    /// its mechanism, and a mechanism that runs inside the target without an
    /// account is unknown -- never the zero of one that reads from outside.
    #[test]
    fn a_mechanism_inside_the_target_without_an_account_is_unknown_and_one_outside_is_zero() {
        let on = Ok(());
        let off: Result<(), String> = Err("statistics are off".to_owned());
        let mut outside = InsideMeter::of("fs:proc", None, Vec::new(), &on);
        assert_eq!(outside.read(), Ok(Inside::default()));
        for inside in [
            "pmu",
            "pmu:sw",
            "preload",
            "af_packet",
            "vendor:cupti",
            "fs:proc:clear_refs",
        ] {
            let mut meter = InsideMeter::of(inside, None, Vec::new(), &on);
            assert!(meter.read().is_err(), "`{inside}` runs inside the target");
        }
        let mut nvml = InsideMeter::of("vendor:nvml", None, Vec::new(), &on);
        assert_eq!(nvml.read(), Ok(Inside::default()));
        let mut unstated = InsideMeter::of("bpf:raw_tp", None, vec![(3, 7)], &off);
        assert_eq!(unstated.read(), Err("statistics are off".to_owned()));
        let mut unfound = InsideMeter::of("bpf:raw_tp", None, Vec::new(), &on);
        assert!(unfound
            .read()
            .is_err_and(|why| why.contains("not among the descriptors")));
    }

    /// A budget that moves a collection keeps what its first backend spent
    /// inside the target, and an unknown first half leaves the whole unknown.
    #[test]
    fn a_degraded_collection_carries_what_its_first_backend_spent_inside_the_target() {
        struct Fixed(u64);
        impl InsideAccount for Fixed {
            fn read(&self) -> Result<Inside, String> {
                Ok(Inside {
                    cpu_ns: self.0,
                    runs: 1,
                    left_out: 0,
                })
            }
            fn describes(&self) -> String {
                "fixed".to_owned()
            }
        }
        let on = Ok(());
        let mut meter = InsideMeter::of("preload", Some(Box::new(Fixed(10))), Vec::new(), &on);
        meter.moved_onto(InsideMeter::of(
            "preload",
            Some(Box::new(Fixed(5))),
            Vec::new(),
            &on,
        ));
        assert_eq!(meter.read().map(|inside| inside.cpu_ns), Ok(15));

        let mut unknown = InsideMeter::of("pmu", None, Vec::new(), &on);
        unknown.moved_onto(InsideMeter::of("fs:proc", None, Vec::new(), &on));
        assert!(unknown.read().is_err());
    }

    #[test]
    fn every_registered_collector_names_a_real_catalog_backend() {
        let catalog = Catalog::embedded().unwrap();
        for factory in registry() {
            let entry = catalog
                .get(&factory.source.parse().unwrap())
                .unwrap_or_else(|| panic!("`{}` is not in the catalog", factory.source));
            assert!(
                entry.backend(factory.backend).is_some(),
                "`{}` has no backend `{}`",
                factory.source,
                factory.backend
            );
        }
    }

    /// A collector that declares it weighs its samples is one a budget can
    /// move from `trace` to `sample`, so its backend has to serve both modes;
    /// and `mem.faults` on `pmu:sw`, whose weighed series `tests/budget.rs`
    /// holds to the task's own count across the move, declares it.
    #[test]
    fn a_collector_that_weighs_its_samples_serves_both_modes_and_mem_faults_does() {
        use crate::request::ModeKind;
        let catalog = Catalog::embedded().unwrap();
        for factory in registry()
            .iter()
            .filter(|factory| factory.weighs_its_samples)
        {
            let entry = catalog.get(&factory.source.parse().unwrap()).unwrap();
            let modes = entry.backend_modes(entry.backend(factory.backend).unwrap());
            assert!(
                modes.contains(&ModeKind::Trace) && modes.contains(&ModeKind::Sample),
                "`{}` on `{}` declares it weighs its samples, and serves {modes:?}",
                factory.source,
                factory.backend
            );
        }
        assert!(weighs_its_samples("mem.faults", "pmu:sw"));
        assert!(!weighs_its_samples("mem.faults", "no-such-backend"));
        assert!(!weighs_its_samples("mem.rss", "fs:proc:statm"));
    }

    #[test]
    fn no_two_collectors_claim_the_same_source_and_backend() {
        let mut seen = std::collections::BTreeSet::new();
        for factory in registry() {
            assert!(
                seen.insert((factory.source, factory.backend)),
                "two collectors claim `{}` on `{}`",
                factory.source,
                factory.backend
            );
        }
    }

    /// Two collectors that disagree about the launched program's environment
    /// cannot both be satisfied, and picking a winner silently would mean one
    /// of them measuring something other than what it was configured for.
    #[test]
    fn a_disagreement_about_the_child_environment_is_refused_rather_than_resolved() {
        struct Wants(&'static str, &'static str);
        impl Prepared for Wants {
            fn launch_environment(&self) -> Vec<(String, String)> {
                vec![(self.0.to_owned(), self.1.to_owned())]
            }
            fn attach(self: Box<Self>, _: &Target) -> Result<Box<dyn Collector>, String> {
                unreachable!("this test never attaches")
            }
        }

        // Agreeing on the same value is not a disagreement.
        let agreeing: Vec<Box<dyn Prepared>> = vec![
            Box::new(Wants("LD_PRELOAD", "/lib/a.so")),
            Box::new(Wants("LD_PRELOAD", "/lib/a.so")),
        ];
        let merged =
            merge_environments(agreeing.iter().map(Box::as_ref)).expect("identical values agree");
        assert_eq!(merged.len(), 1, "the variable is set once, not twice");

        let conflicting: Vec<Box<dyn Prepared>> = vec![
            Box::new(Wants("LD_PRELOAD", "/lib/a.so")),
            Box::new(Wants("LD_PRELOAD", "/lib/b.so")),
        ];
        match merge_environments(conflicting.iter().map(Box::as_ref)) {
            Err(error @ CollectError::ConflictingEnvironment { .. }) => {
                assert!(error.to_string().contains("LD_PRELOAD"), "{error}");
                // Each collector asked for something real; that no single
                // environment serves both is this build's gap, which is exit 3
                // rather than the request's, which is exit 2.
                assert_eq!(error.whose(), Whose::Build);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// One run observes one workload. Running two in sequence would measure
    /// two different moments and present them as one window, and a join across
    /// that would correlate a program with something that was not happening at
    /// the same time. Machine targets beside a workload are
    /// `tests/runner.rs`'s, measured.
    #[test]
    fn a_session_asking_for_two_workloads_is_refused_before_anything_starts() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let own = std::process::id() as i32;

        let request = |target| crate::request::Request {
            source: "mem.rss".parse().unwrap(),
            target,
            mode: crate::request::Mode::Poll {
                interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
            },
            emit: vec![crate::request::Emit::Summary],
            keys: Vec::new(),
            filter: None,
            duration: Some("50ms".parse().unwrap()),
            budget: None,
            stack: None,
            symbols: None,
            backend: None,
            label: None,
            window: None,
            value: None,
            sum: Vec::new(),
        };
        let resolved: Vec<_> = [Target::Pid(vec![own]), Target::Pid(vec![1])]
            .into_iter()
            .map(|target| {
                crate::resolve::resolve(&catalog, &capabilities, &request(target))
                    .expect("resolves")
            })
            .collect();

        match run_all(&resolved) {
            Err(CollectError::MixedTargets { .. }) => {}
            other => panic!(
                "expected a refusal, got {:?}",
                other.map(|_| "a collection")
            ),
        }
    }

    /// Builds a resolved request for `mem.rss`, optionally pinned to a backend
    /// so the filter placement can be steered: `fs:proc` returns whole records
    /// and gets a userspace placement, an eBPF backend is credited with pushing
    /// the predicate down.
    fn resolved_with_filter(source: &str, backend: Option<&str>, filter: &str) -> ResolvedRequest {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let entry = catalog.get(&source.parse().unwrap()).unwrap();
        let request = crate::request::Request {
            source: source.parse().unwrap(),
            target: Target::Pid(vec![std::process::id() as i32]),
            mode: if entry.modes.contains(&crate::request::ModeKind::Poll) {
                crate::request::Mode::Poll {
                    interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
                }
            } else {
                crate::request::Mode::Trace
            },
            emit: vec![crate::request::Emit::Raw],
            keys: Vec::new(),
            filter: Some(filter.parse().unwrap()),
            duration: None,
            budget: None,
            stack: None,
            symbols: None,
            backend: backend.map(|id| id.parse().unwrap()),
            label: None,
            window: None,
            value: None,
            sum: Vec::new(),
        };
        crate::resolve::resolve(&catalog, &capabilities, &request).expect("resolves")
    }

    fn event_with(attrs: Vec<crate::event::Attr>) -> Event {
        Event {
            ts_ns: 1,
            ts_err_ns: 0,
            kind: "mem.rss".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: CollectorId::new("test"),
            seq: 0,
        }
    }

    /// The filter has to remove what it does not match, and the count has to
    /// survive: a series of three events that was a series of forty before the
    /// predicate ran describes a different run.
    #[test]
    fn a_userspace_filter_removes_what_it_does_not_match_and_counts_it() {
        let resolved = resolved_with_filter("mem.rss", Some("fs:proc:statm"), "comm == 'keep'");
        let mut events = vec![
            event_with(vec![crate::event::Attr::text("comm", "keep")]),
            event_with(vec![crate::event::Attr::text("comm", "drop")]),
            event_with(vec![crate::event::Attr::text("comm", "keep")]),
        ];

        let filtered = apply_filter(&resolved, &mut events).expect("a filtered request reports it");
        assert_eq!(events.len(), 2);
        assert_eq!((filtered.kept, filtered.dropped), (2, 1));
        assert_eq!(filtered.predicate, "comm == 'keep'");
        assert!(filtered.unseen_fields.is_empty(), "{filtered:?}");
    }

    /// A backend the resolver credited with pushing the predicate into the
    /// kernel is taken at its word. Running the filter again here would produce
    /// the same events either way, which is exactly why it must not happen: it
    /// would hide a collector that ignored the predicate it was given.
    #[test]
    fn a_filter_the_backend_pushed_down_is_not_run_a_second_time() {
        // `bpf:tp` has no collector, and a pinned backend that lacks something
        // here is refused for the machine first (the chain's order), so its
        // requirements are cleared: the subject is the placement, not the probe.
        let mut entry = Catalog::embedded()
            .unwrap()
            .get(&"io.latency".parse().unwrap())
            .unwrap()
            .clone();
        for backend in &mut entry.backends {
            backend.requires.clear();
        }
        let catalog = Catalog::from_entries(vec![entry]).unwrap();
        let mut request = resolved_with_filter("mem.rss", None, "comm == 'x'").request;
        request.source = "io.latency".parse().unwrap();
        request.mode = crate::request::Mode::Trace;
        request.filter = Some("bytes > 4096".parse().unwrap());
        request.backend = Some("force:bpf:tp".parse().unwrap());
        let capabilities = crate::capabilities::Capabilities::detect();
        let resolved =
            crate::resolve::resolve(&catalog, &capabilities, &request).expect("resolves");
        assert!(
            matches!(
                resolved.filter,
                Some(crate::resolve::FilterPlacement::Kernel(_))
            ),
            "this test needs a backend credited with pushing the filter down"
        );

        let mut events = vec![event_with(vec![crate::event::Attr::uint("bytes", 1)])];
        assert!(apply_filter(&resolved, &mut events).is_none());
        assert_eq!(events.len(), 1, "userspace must not re-filter kernel work");
    }

    /// A predicate over a field nothing produces is false everywhere, which
    /// empties the series and looks like a workload that did nothing. The field
    /// is named so the reader can tell the two apart.
    #[test]
    fn a_predicate_over_a_field_no_event_carries_says_which_field() {
        let resolved = resolved_with_filter("mem.rss", Some("fs:proc:statm"), "comm == 'anything'");
        let mut events = vec![event_with(vec![crate::event::Attr::uint("bytes", 4096)])];

        let filtered = apply_filter(&resolved, &mut events).expect("a filtered request reports it");
        assert!(events.is_empty());
        assert_eq!(filtered.unseen_fields, vec!["comm".to_owned()]);
    }

    /// `kind` is on every event by construction, so it is never a missing
    /// field even when nothing matches it.
    #[test]
    fn the_kind_field_is_never_reported_as_one_no_event_carried() {
        let resolved =
            resolved_with_filter("mem.rss", Some("fs:proc:statm"), "kind == 'io.latency'");
        let mut events = vec![event_with(Vec::new())];

        let filtered = apply_filter(&resolved, &mut events).expect("a filtered request reports it");
        assert!(events.is_empty());
        assert!(filtered.unseen_fields.is_empty(), "{filtered:?}");
    }

    /// A request with no filter reports nothing rather than reporting that a
    /// filter kept everything: there was no predicate to keep anything.
    #[test]
    fn a_request_without_a_filter_reports_no_filtering() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let request = crate::request::Request {
            source: "mem.rss".parse().unwrap(),
            target: Target::Pid(vec![std::process::id() as i32]),
            mode: crate::request::Mode::Snapshot,
            emit: vec![crate::request::Emit::Raw],
            keys: Vec::new(),
            filter: None,
            duration: None,
            budget: None,
            stack: None,
            symbols: None,
            backend: None,
            label: None,
            window: None,
            value: None,
            sum: Vec::new(),
        };
        let resolved = crate::resolve::resolve(&catalog, &capabilities, &request).unwrap();
        let mut events = vec![event_with(Vec::new())];
        assert!(apply_filter(&resolved, &mut events).is_none());
        assert_eq!(events.len(), 1);
    }

    /// A system target has no program to end the run, so a duration is the only
    /// thing that can. Starting collectors that nothing will ever stop is worse
    /// than refusing.
    #[test]
    fn a_system_run_with_nothing_to_stop_it_is_refused() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let request = crate::request::Request {
            // `/proc/stat`, which every machine this tool runs on serves: the
            // refusal is the run's, and a machine with no GPU -- the CI's --
            // failed here on the resolution before it could say so.
            source: "cpu.util".parse().unwrap(),
            target: Target::System,
            mode: crate::request::Mode::Poll {
                interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
            },
            emit: vec![crate::request::Emit::Raw],
            keys: Vec::new(),
            filter: None,
            duration: None,
            budget: None,
            stack: None,
            symbols: None,
            backend: None,
            label: None,
            window: None,
            value: None,
            sum: Vec::new(),
        };
        let resolved = match crate::resolve::resolve(&catalog, &capabilities, &request) {
            Ok(resolved) => resolved,
            Err(reason) => {
                eprintln!("skipped: `cpu.util --system` cannot serve here: {reason}");
                return;
            }
        };
        match run(&resolved) {
            Err(CollectError::NoStopCondition) => {}
            // A machine with no GPU cannot get that far, and says so first.
            Err(CollectError::Start { reason, .. }) => {
                eprintln!("skipped: {reason}");
            }
            other => panic!(
                "expected a refusal, got {:?}",
                other.map(|_| "a collection")
            ),
        }
    }

    /// A collector that refuses before the target exists is refusing the
    /// request, not the machine.
    ///
    /// The factory is a pure function of the resolved request: nothing has been
    /// launched, no counter has been opened and no file has been read by the
    /// time it answers, so a refusal it produces cannot be about this machine.
    /// That is the whole of the default classification, and it is what moved
    /// thirteen of `mem`'s sixteen combinations, `io.bytes --mode trace`,
    /// `net.hwts --mode trace` and `power.freq --mode trace` off exit 1.
    #[test]
    fn a_refusal_raised_before_the_target_exists_is_this_builds_gap() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let request = crate::request::Request {
            source: "io.bytes".parse().unwrap(),
            target: Target::System,
            mode: crate::request::Mode::Trace,
            emit: vec![crate::request::Emit::TopK { k: 4, by: None }],
            keys: Vec::new(),
            filter: None,
            duration: Some("50ms".parse().unwrap()),
            budget: None,
            stack: None,
            symbols: None,
            backend: Some("force:fs:proc".parse().unwrap()),
            label: None,
            window: None,
            value: None,
            sum: Vec::new(),
        };
        let resolved =
            crate::resolve::resolve(&catalog, &capabilities, &request).expect("resolves");
        match prepare(&resolved) {
            Err(error @ CollectError::Start { .. }) => assert_eq!(
                error.whose(),
                Whose::Build,
                "`/proc/diskstats` cannot trace, and nothing about this machine is why: {error}"
            ),
            other => panic!(
                "expected a refusal from the factory, got {:?}",
                other.map(|_| "a prepared collector")
            ),
        }
    }

    /// A collector can override the classification either way, and the mark it
    /// uses never reaches the reader.
    ///
    /// The bits travel in front of the sentence because `attach` and `start`
    /// answer with a `String` in fifty files. What must never happen is the
    /// control character showing up in a message, so the message is checked
    /// rather than only the verdict.
    #[test]
    fn a_collector_can_say_whose_gap_a_refusal_is_without_showing_its_mark() {
        let unmarked =
            CollectError::start("io.bytes", "the counter would not open", Whose::Machine);
        assert_eq!(unmarked.whose(), Whose::Machine);

        let built = CollectError::start(
            "io.bytes",
            not_built("`trace` needs the tracepoint backend"),
            Whose::Machine,
        );
        assert_eq!(
            built.whose(),
            Whose::Build,
            "a collector that says the gap is this build's is believed over where it was raised"
        );

        let meaningless = CollectError::start(
            "io.inflight",
            cannot_mean_anything("a queue depth counts over no window"),
            Whose::Build,
        );
        assert_eq!(meaningless.whose(), Whose::Request);

        for refusal in [&built, &meaningless] {
            let shown = refusal.to_string();
            assert!(
                !shown.contains(MARK_BUILD) && !shown.contains(MARK_REQUEST),
                "the mark reached the reader: {shown:?}"
            );
            assert!(
                shown.starts_with("cannot start collecting `"),
                "and the sentence is unchanged: {shown}"
            );
        }
        assert!(built.to_string().ends_with("needs the tracepoint backend"));
    }

    /// A session that fails several ways at once reports all of them, and
    /// answers with the code for the work the reader has to do first.
    #[test]
    fn several_refusals_are_reported_together() {
        let refusals = vec![
            CollectError::NoCollector {
                source_id: "sched.offcpu".to_owned(),
                backend: "bpf:tp".to_owned(),
            },
            CollectError::NoCollector {
                source_id: "sched.wakeup".to_owned(),
                backend: "bpf:tp".to_owned(),
            },
        ];
        let several = CollectError::Several { refusals };
        let shown = several.to_string();
        assert!(
            shown.contains("sched.offcpu") && shown.contains("sched.wakeup"),
            "both have to be named, or fixing the session takes one run each: {shown}"
        );
        assert_eq!(several.each().len(), 2);
        assert_eq!(several.whose(), Whose::Build);

        // A request that cannot mean anything has to be rewritten before a
        // missing collector is worth chasing, so it decides the code.
        let mixed = CollectError::Several {
            refusals: vec![
                CollectError::NoCollector {
                    source_id: "sched.offcpu".to_owned(),
                    backend: "bpf:tp".to_owned(),
                },
                CollectError::NoStopCondition,
            ],
        };
        assert_eq!(mixed.whose(), Whose::Request);
    }

    /// A poll asked for faster than it can be served counts every round it
    /// missed, and explains them once.
    ///
    /// The reason is a kind and the durations are magnitudes beside it. They
    /// used to be the same thing: the reason carried the nanoseconds of the
    /// round that overran, so no two were ever equal and a run that was late a
    /// thousand times printed a thousand lines with a count of one each.
    /// A reason noted beside a counter carries the losses it explains, not the
    /// drains that found some: two drains losing 700 and 13 000 print `13700x`,
    /// where they used to print `2x` under a count of 13 700.
    #[test]
    fn a_reason_counts_the_losses_it_explains_not_the_drains() {
        let mut losses = Losses::default();
        for lost in [700, 13_000] {
            losses.dropped += lost;
            losses.note_times("a full ring", lost);
        }
        assert_eq!(losses.reasons.get("a full ring"), Some(&13_700));
        assert_eq!(losses.reasons.values().sum::<u64>(), losses.dropped);
    }

    #[test]
    fn late_ticks_are_one_reason_carrying_the_count_they_explain() {
        let mut losses = Losses::default();
        losses.late_tick(30_000_000, 10_000_000);
        losses.late_tick(11_000_000, 10_000_000);
        losses.late_tick(19_000_000, 10_000_000);
        losses.summarize();

        assert_eq!(losses.late_ticks, 3);
        assert_eq!(
            losses.reasons.len(),
            1,
            "three late ticks, one reason: {:?}",
            losses.reasons
        );
        let (reason, count) = losses.reasons.iter().next().expect("one reason");
        assert_eq!(*count, 3, "the reason carries the count it explains");
        assert!(
            reason.contains("10.0ms tick") && reason.contains("worst 30.0ms"),
            "the magnitudes are reported, not enumerated: {reason}"
        );
        assert_eq!(
            losses.overrun,
            Some(Overrun {
                tick_ns: 10_000_000,
                worst_ns: 30_000_000,
                total_ns: 60_000_000,
                skipped: 2 + 1 + 1,
            })
        );
        assert!(!losses.is_clean());
    }

    /// A round that held a poll for many of its intervals is one late tick,
    /// and the readings that fell due meanwhile are said: a live `omn top` of
    /// the machine read `cpu.util` once in three seconds, and the eleven
    /// readings it never took were counted nowhere.
    #[test]
    fn the_readings_a_late_round_swallowed_are_counted() {
        let mut losses = Losses::default();
        losses.late_tick(3_000_000_000, 250_000_000);
        losses.summarize();
        assert_eq!(losses.late_ticks, 1);
        assert_eq!(losses.overrun.map(|overrun| overrun.skipped), Some(11));
        let (reason, _) = losses.reasons.iter().next().expect("one reason");
        assert!(
            reason.ends_with("11 more readings fell due meanwhile and never ran"),
            "{reason}"
        );
    }

    /// A collector is read at its own pace, and one that has a ring to drain
    /// keeps it whatever the request asked for.
    #[test]
    fn each_request_is_paced_by_its_own_mode_and_its_own_collector() {
        struct Streaming(CollectorId, Option<StdDuration>);
        impl Collector for Streaming {
            fn id(&self) -> &CollectorId {
                &self.0
            }
            fn value_attr(&self) -> &'static str {
                "count"
            }
            fn sample(&mut self, _: u64) -> Result<Vec<Event>, SampleError> {
                Ok(Vec::new())
            }
            fn tick(&self) -> Option<StdDuration> {
                self.1
            }
        }
        let polling = Streaming(CollectorId::new("polling"), None);
        let streaming = Streaming(
            CollectorId::new("streaming"),
            Some(StdDuration::from_millis(10)),
        );

        let poll = |ms: u64| crate::request::Mode::Poll {
            interval: crate::scalar::Duration::from_nanos(ms * 1_000_000),
        };
        assert_eq!(
            pace_of(&polling, poll(200)).interval,
            StdDuration::from_millis(200),
            "a collector with no cadence of its own is read at the one the request named"
        );
        assert!(
            pace_of(&polling, Mode::Snapshot).snapshot,
            "a snapshot is one read, and the session it is written in cannot change that"
        );
        assert_eq!(
            pace_of(&polling, Mode::Count).interval,
            StdDuration::from_nanos(Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
            "a mode that names no cadence falls back to the default poll interval"
        );
        assert_eq!(
            pace_of(&streaming, poll(1_000)).interval,
            StdDuration::from_millis(10),
            "a ring drained less often than it fills loses events, so the collector's own \
             need wins over the request's"
        );
    }

    #[test]
    fn losses_merge_without_losing_their_reasons() {
        let mut first = Losses {
            failed_reads: 1,
            ..Losses::default()
        };
        first.note("a");
        let mut second = Losses {
            dropped: 5,
            ..Losses::default()
        };
        second.throttle("a");
        second.note("b");

        first.absorb(second);
        assert_eq!((first.dropped, first.failed_reads), (5, 1));
        assert_eq!(
            first.throttled, 1,
            "a throttled period must survive being merged, like every other loss"
        );
        assert_eq!(first.reasons.get("a"), Some(&2));
        assert_eq!(first.reasons.get("b"), Some(&1));
        assert!(!first.is_clean());
    }

    /// A throttled period is a loss whose size nobody knows.
    ///
    /// The kernel stops delivering for a while; how many events happened in
    /// that while is not recorded anywhere. Adding one to `dropped` -- which
    /// is what `sched.switches` did -- says "one event was lost", which is a
    /// number, and the honest answer is that there is no number. So it is
    /// counted apart, and a run that hit it is not clean.
    #[test]
    fn a_throttled_period_is_counted_apart_from_the_events_it_did_not_lose() {
        let mut losses = Losses::default();
        assert!(losses.is_clean(), "nothing has gone wrong yet");

        losses.throttle("the kernel throttled this event: see kernel.perf_event_max_sample_rate");

        assert_eq!(losses.throttled, 1, "one period, not one event");
        assert_eq!(
            losses.dropped, 0,
            "a throttle is not a drop: folding it in would claim a count nobody has"
        );
        assert!(
            !losses.is_clean(),
            "a run the kernel throttled is not a complete run, and must not read as one"
        );
        assert!(
            losses
                .reasons
                .keys()
                .any(|reason| reason.contains("perf_event_max_sample_rate")),
            "the reader has to be told what to raise: {:?}",
            losses.reasons
        );
    }

    /// Cpu time this thread has been charged, through the one interface that
    /// answers for a thread rather than a process.
    ///
    /// It is deliberately not `getrusage`: this is the independent witness the
    /// cost accounting is checked against, and a witness that shares the caller
    /// is not one. Wall time will not do either -- a spin of 200 ms of wall
    /// time is 200 ms of cpu only on an idle machine, and `cargo test` runs
    /// seven other threads flat out.
    fn thread_cpu_ns() -> u64 {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `clock_gettime` writes a `timespec` this function owns.
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
        (time.tv_sec as u64) * 1_000_000_000 + (time.tv_nsec as u64)
    }

    /// A collector that burns a known amount of cpu, and says how much.
    ///
    /// The quantity injected is cpu time rather than wall time, so what it
    /// spends does not depend on how contended the machine is; the loop runs
    /// until the thread has been charged what it was asked for.
    struct Burning {
        id: CollectorId,
        spin_ns: u64,
        spun_ns: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl Collector for Burning {
        fn id(&self) -> &CollectorId {
            &self.id
        }

        fn value_attr(&self) -> &'static str {
            "bytes"
        }

        fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
            let started_ns = thread_cpu_ns();
            while thread_cpu_ns().saturating_sub(started_ns) < self.spin_ns {}
            self.spun_ns
                .set(self.spun_ns.get() + thread_cpu_ns().saturating_sub(started_ns));
            Ok(Vec::new())
        }
    }

    fn burning(spin_ns: u64) -> (Vec<Box<dyn Collector>>, std::rc::Rc<std::cell::Cell<u64>>) {
        let spun_ns = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let collectors: Vec<Box<dyn Collector>> = vec![Box::new(Burning {
            id: CollectorId::new("burning"),
            spin_ns,
            spun_ns: spun_ns.clone(),
        })];
        (collectors, spun_ns)
    }

    fn until(deadline_ns: u64) -> impl FnMut() -> Option<String> {
        move || (monotonic_ns() >= deadline_ns).then(|| "the deadline".to_owned())
    }

    /// Ends a run 300 ms after the escalation was first asked to shed, so a
    /// test sees the run go on past the question; and at 10 s whatever
    /// happened, so a run never asked still ends.
    ///
    /// A fixed wall deadline was a guess at how long the held layer takes to
    /// be weighed, and on a machine running the full suite this thread gets a
    /// fraction of the wall clock: the run ended before the question was put.
    fn after_the_shed(
        asked: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
    ) -> impl FnMut() -> Option<String> {
        let cap_ns = monotonic_ns() + 10_000_000_000;
        let mut asked_at: Option<u64> = None;
        move || {
            let now = monotonic_ns();
            if asked_at.is_none() && !asked.borrow().is_empty() {
                asked_at = Some(now);
            }
            let past = asked_at.is_some_and(|at| now >= at + 300_000_000);
            (past || now >= cap_ns).then(|| "the deadline".to_owned())
        }
    }

    /// What a run says it spent is the cpu it actually burned.
    ///
    /// Ground truth is a known quantity of work injected into the collector and
    /// timed by a different clock: 200 ms of spinning, which cannot take less
    /// than 200 ms of cpu and, with nothing else in the process, should not take
    /// much more.
    ///
    /// It runs in a forked child because `getrusage(RUSAGE_SELF)` answers for
    /// the whole process and `cargo test` runs its tests in parallel threads of
    /// one. Measured from this thread, the number would be this test's work plus
    /// whatever seven others happened to be doing, which is the right answer to
    /// a different question. The child is single-threaded, so the two accounts
    /// describe the same work.
    #[test]
    fn what_a_run_reports_spending_is_the_cpu_it_actually_burned() {
        const AGREED: i32 = 0;
        const UNDER_REPORTED: i32 = 1;
        const OVER_REPORTED: i32 = 2;
        const NOTHING_WAS_BURNED: i32 = 3;

        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            let (mut collectors, spun_ns) = burning(20_000_000);
            let mut spending = Spending::unweighed(1);
            // Driven to 200 ms of spun cpu, not 200 ms of wall time: a child
            // on a loaded machine gets a fraction of the wall clock, and a
            // wall deadline then ended the run before the work it is checked
            // against was done (the flaky tally, #11). Ten seconds of wall
            // time is only the bound that keeps a starved child from hanging.
            let spun = spun_ns.clone();
            let cap_ns = monotonic_ns() + 10_000_000_000;
            drive_attached(
                &mut collectors,
                &[Pace {
                    interval: StdDuration::ZERO,
                    snapshot: false,
                }],
                &[true],
                None,
                &mut spending,
                move || {
                    (spun.get() >= 200_000_000 || monotonic_ns() >= cap_ns)
                        .then(|| "the collector spun for 200 ms of cpu".to_owned())
                },
            );
            let reported_ns = spending.close().remove(0).collector_cpu_ns as f64;
            let spun_ns = spun_ns.get() as f64;
            let code = if spun_ns < 150_000_000.0 {
                NOTHING_WAS_BURNED
            } else if reported_ns < spun_ns * 0.8 {
                UNDER_REPORTED
            } else if reported_ns > spun_ns * 1.5 {
                OVER_REPORTED
            } else {
                AGREED
            };
            unsafe { libc::_exit(code) };
        }

        let mut status = 0;
        assert!(
            unsafe { libc::waitpid(child, &mut status, 0) } == child,
            "waiting for the child"
        );
        assert!(libc::WIFEXITED(status), "the child did not exit normally");
        match libc::WEXITSTATUS(status) {
            AGREED => {}
            NOTHING_WAS_BURNED => panic!(
                "the collector was asked to spin for 200 ms and did not, so this measured nothing"
            ),
            UNDER_REPORTED => panic!(
                "the run reported spending less cpu than the collector provably burned: a budget \
                 built on that would let a collector cost what it likes"
            ),
            OVER_REPORTED => panic!(
                "the run reported spending far more cpu than the collector burned, so something \
                 other than the measurement is being charged to it"
            ),
            other => panic!("the child exited with {other}"),
        }
    }

    /// A `mem.rss` request over this process, with a ceiling.
    fn weighed(budget: &str) -> ResolvedRequest {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let request = crate::request::Request {
            source: "mem.rss".parse().unwrap(),
            target: Target::Pid(vec![std::process::id() as i32]),
            mode: crate::request::Mode::Poll {
                interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
            },
            emit: vec![crate::request::Emit::Summary],
            keys: Vec::new(),
            filter: None,
            duration: Some("1s".parse().unwrap()),
            budget: Some(budget.parse().unwrap()),
            stack: None,
            symbols: None,
            backend: None,
            label: None,
            window: None,
            value: None,
            sum: Vec::new(),
        };
        crate::resolve::resolve(&catalog, &capabilities, &request).expect("resolves")
    }

    /// A run that costs more than it was allowed, with nowhere cheaper to go,
    /// ends -- and the reason names the ceiling rather than the clock.
    ///
    /// Both sides are checked, because a test that only shows a run stopping
    /// would pass just as well against a runner that stops every run. The two
    /// differ in the ceiling and in nothing else: the collector burns the same
    /// cpu, over the same window, against the same denominator.
    ///
    /// That denominator is this process's own cpu time, so the ratio is around
    /// one hundred percent whatever else the machine is doing -- which is what
    /// puts 50 % reliably below it and 500 % reliably above.
    #[test]
    fn a_run_over_its_ceiling_with_nowhere_cheaper_to_go_stops_and_says_which() {
        let stopped_because = |ceiling: &str| {
            let (mut collectors, spun_ns) = burning(20_000_000);
            let mut spending = Spending::weighed_against_this_process(vec![weighed(ceiling)]);
            let deadline_ns = monotonic_ns() + 600_000_000;
            let (_, _, stopped_because) = drive_attached(
                &mut collectors,
                &[Pace {
                    interval: StdDuration::ZERO,
                    snapshot: false,
                }],
                &[true],
                None,
                &mut spending,
                until(deadline_ns),
            );
            // Enough to have been weighed at least once: the run under the
            // tighter ceiling ends early by design, so this is the floor both
            // sides share rather than the length of the window.
            // Enough to have been weighed at least once. The run under the
            // tighter ceiling ends about a hundred milliseconds in by design,
            // so this is the floor the two sides share rather than the length
            // of the window.
            assert!(
                spun_ns.get() > 20_000_000,
                "the collector has to cost something before a ceiling can mean anything, and it \
                 only spun for {}ns",
                spun_ns.get()
            );
            stopped_because
        };

        let over = stopped_because("50%");
        assert!(
            over.contains("ceiling of 50") && over.contains("nothing cheaper"),
            "a run the budget ended has to say so, and say what it would have degraded onto: \
             {over}"
        );

        let under = stopped_because("500%");
        assert_eq!(
            under, "the deadline",
            "a run inside its ceiling must end for the reason it was going to end for"
        );
    }

    /// The ceiling is checked often enough to act and rarely enough not to
    /// become the measurement.
    ///
    /// A budget that polled `getrusage` every tick would answer mostly about
    /// itself, which is the trap this whole stream is built against. The run
    /// below lasts 600 ms, so a hundred-millisecond cadence is about six reads;
    /// what those reads cost is reported rather than hidden, and has to stay a
    /// vanishing fraction of the window.
    #[test]
    fn weighing_a_run_costs_a_fraction_of_the_run() {
        let (mut collectors, _) = burning(5_000_000);
        let mut spending = Spending::weighed_against_this_process(vec![weighed("500%")]);
        let deadline_ns = monotonic_ns() + 600_000_000;
        drive_attached(
            &mut collectors,
            &[Pace {
                interval: StdDuration::ZERO,
                snapshot: false,
            }],
            &[true],
            None,
            &mut spending,
            until(deadline_ns),
        );
        let overhead = spending.close().remove(0);

        assert!(
            (4..=12).contains(&overhead.measurements),
            "a 600 ms run at a 100 ms cadence is about six weighings, not {}",
            overhead.measurements
        );
        assert!(
            overhead.measurement_ns * 100 < overhead.wall_ns,
            "weighing the run cost {}ns of a {}ns window, which is enough to change the answer \
             it was asked for",
            overhead.measurement_ns,
            overhead.wall_ns
        );
    }

    /// A run with no ceiling is never interrupted to be weighed.
    ///
    /// The two `getrusage` calls at either end are the whole price of knowing
    /// what a run cost, and that is the reason every run reports it rather than
    /// only the ones that asked.
    #[test]
    fn a_run_with_no_ceiling_is_weighed_twice_and_no_more() {
        let (mut collectors, _) = burning(5_000_000);
        let mut spending = Spending::unweighed(1);
        let deadline_ns = monotonic_ns() + 400_000_000;
        drive_attached(
            &mut collectors,
            &[Pace {
                interval: StdDuration::ZERO,
                snapshot: false,
            }],
            &[true],
            None,
            &mut spending,
            until(deadline_ns),
        );
        let overhead = spending.close().remove(0);
        assert_eq!(
            overhead.measurements, 1,
            "an unbudgeted run reads the clock once at the end; the other read is at `open`, \
             before there is anything to report"
        );
        assert!(
            overhead.wall_ns >= 400_000_000,
            "the window has to be the window: {}",
            overhead.wall_ns
        );
    }

    /// Two collections of one target share one reading of it: this process,
    /// burning between the reads, would otherwise give each its own.
    #[test]
    fn two_meters_of_one_target_are_read_once() {
        let me = std::process::id() as i32;
        let meters = [
            Meter::Tasks(vec![me]),
            Meter::Cpus(None),
            Meter::Tasks(vec![me]),
        ];
        let deadline = monotonic_ns() + 2_000_000;
        while monotonic_ns() < deadline {}
        let read = Meter::read_each(&meters);
        assert!(read[0].is_some(), "this process keeps a cpu account");
        assert_eq!(read[0], read[2], "one target, one reading");
    }

    /// A launched program's meters of one target share their high water: the
    /// second takes the first's reading as its own mark, so a later read of
    /// it alone (after the kill) is not short of what the first one saw.
    #[test]
    fn equal_launched_meters_keep_the_same_high_water() {
        let me = std::process::id() as i32;
        let meters = [
            Meter::Launched(me, HighWater::default()),
            Meter::Launched(me, HighWater::default()),
        ];
        let deadline = monotonic_ns() + 2_000_000;
        while monotonic_ns() < deadline {}
        let read = Meter::read_each(&meters);
        let marks: Vec<u64> = meters
            .iter()
            .map(|meter| match meter {
                Meter::Launched(_, high) => high.0.get(),
                _ => unreachable!(),
            })
            .collect();
        assert!(read[0].is_some_and(|read| read > 0), "{read:?}");
        assert_eq!(read[0], read[1], "one target, one reading");
        assert_eq!(marks, vec![read[0].unwrap(); 2], "and one mark");
    }

    /// A stop is the budget's of the request whose ceiling it names: two
    /// requests of one source under two budgets, and only the one under the
    /// lower ceiling stopped the run.
    #[test]
    fn a_budget_stop_belongs_to_the_request_whose_ceiling_it_names() {
        let reason = format!(
            "`mem.rss` spent 3.70 % of the target's cpu time {OVER_ITS_CEILING} 1 %, and \
             nothing cheaper than `fs:proc:statm` is available"
        );
        assert!(stopped_by_budget(&reason, "mem.rss", 1.0));
        assert!(!stopped_by_budget(&reason, "mem.rss", 50.0));
        assert!(!stopped_by_budget(&reason, "mem.rss", 10.0));
        assert!(!stopped_by_budget(&reason, "cpu.profile", 1.0));
        let fractional = format!("`mem.rss` spent 0.07 % {OVER_ITS_CEILING} 0.05 %, and ...");
        assert!(stopped_by_budget(&fractional, "mem.rss", 0.05));
        assert!(!stopped_by_budget(&fractional, "mem.rss", 0.5));
    }

    /// The cpu time of a target that keeps no cpu account is absent, not zero.
    ///
    /// Zero would make every collector infinitely expensive against an
    /// interface and infinitely cheap against a missing one; absent is the
    /// answer, with what would have been read had there been anything.
    #[test]
    fn a_target_with_no_cpu_account_reports_that_rather_than_zero() {
        let meter = Meter::of(&Target::Iface("lo".to_owned()), None);
        assert!(meter.read().is_none());
        assert!(
            meter.describes().contains("no cpu time"),
            "{}",
            meter.describes()
        );

        // And one that does keep an account answers with it, and the answer
        // follows the work: a meter that returned a constant would satisfy
        // every other check here and make every overhead ratio meaningless.
        let own = std::process::id() as i32;
        let mine = Meter::of(&Target::Pid(vec![own]), None);
        let before_ns = mine.read().expect("this process has a cpu account");
        let until_ns = monotonic_ns() + 80_000_000;
        while monotonic_ns() < until_ns {}
        let after_ns = mine.read().expect("and still has one");
        assert!(
            after_ns >= before_ns + 50_000_000,
            "80 ms of spinning moved the meter from {before_ns}ns to {after_ns}ns, which is less \
             than the work it was given"
        );
    }

    /// `/proc/<pid>/stat` agrees with `getrusage` about this process.
    ///
    /// The two are read through completely different interfaces, and the
    /// overhead ratio divides one by the other: a run's own cost comes from
    /// `getrusage` and its target's from `/proc`. If they disagreed about the
    /// same process, every percentage this stream produces would be wrong by
    /// whatever the disagreement was.
    ///
    /// `/proc` counts `cutime` and `cstime` as well, because a shell that forks
    /// is still the workload that was asked for -- so the account to compare it
    /// against is `RUSAGE_SELF` *plus* `RUSAGE_CHILDREN`. Getting that pairing
    /// wrong is exactly the kind of mistake that shows up as a plausible number
    /// nobody can explain, and this suite forks enough children for the two to
    /// come apart if it were wrong.
    ///
    /// The `/proc` reads sandwich the `getrusage` one because the process does
    /// not stop working in between: `cargo test` has seven other threads
    /// burning cpu, so two readings taken a millisecond apart legitimately
    /// differ. What cannot happen is the middle reading falling outside the two
    /// around it.
    #[test]
    fn the_two_cpu_accounts_agree_about_the_same_process() {
        let own = std::process::id() as i32;
        let before_ns = task_cpu_ns(own).expect("this process has a /proc entry");
        let rusage_ns = rusage_ns(libc::RUSAGE_SELF) + rusage_ns(libc::RUSAGE_CHILDREN);
        let after_ns = task_cpu_ns(own).expect("and still has one");
        let tick_ns = 1_000_000_000 / ticks_per_second();

        // `/proc` truncates each of its four fields down to a whole tick, so it
        // can read up to four ticks short of the same instant however many of
        // them are non-zero -- which is the slack here, not measurement error.
        let slack_ns = 5 * tick_ns;
        assert!(
            rusage_ns + tick_ns >= before_ns && rusage_ns <= after_ns + slack_ns,
            "getrusage says {rusage_ns}ns, which is outside the {before_ns}ns..{after_ns}ns that \
             /proc reported either side of it by more than the {slack_ns}ns four truncated \
             {tick_ns}ns ticks can account for"
        );
    }

    /// A collector that records nothing but the clock at every call.
    struct Clocked {
        id: CollectorId,
        reads_ns: std::rc::Rc<std::cell::RefCell<Vec<u64>>>,
        stopped_ns: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl Collector for Clocked {
        fn id(&self) -> &CollectorId {
            &self.id
        }

        fn value_attr(&self) -> &'static str {
            "bytes"
        }

        fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
            self.reads_ns.borrow_mut().push(monotonic_ns());
            Ok(Vec::new())
        }

        fn stop(&mut self) -> Vec<Event> {
            self.stopped_ns.set(monotonic_ns());
            Vec::new()
        }
    }

    /// Without a pidfd, the exit of a launched program is asked between
    /// slices of the sleep: a run polled at one second whose program exits at
    /// 50 ms closes then, not at the next read a second later.
    #[test]
    fn a_launched_program_without_a_pidfd_is_noticed_between_reads() {
        let reads_ns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut slots = vec![Slot::Attached(Box::new(Clocked {
            id: CollectorId::new("clocked"),
            reads_ns: reads_ns.clone(),
            stopped_ns: std::rc::Rc::new(std::cell::Cell::new(0u64)),
        }) as Box<dyn Collector>)];
        let mut paces = vec![Pace {
            interval: StdDuration::from_secs(1),
            snapshot: false,
        }];
        let started_ns = monotonic_ns();
        let exit_ns = started_ns + 50_000_000;
        let driven = drive(
            &mut slots,
            &mut paces,
            &[Plan::default()],
            &[true],
            None,
            true,
            &mut Spending::unweighed(1),
            None,
            None,
            || (monotonic_ns() >= exit_ns).then(|| "the program exited".to_owned()),
        );
        assert_eq!(driven.stopped_because, "the program exited");
        let closing_ns = *reads_ns.borrow().last().unwrap();
        assert!(
            closing_ns < started_ns + 500_000_000,
            "the closing read came {} ms after the start, where the exit was at 50",
            (closing_ns - started_ns) / 1_000_000
        );
    }

    /// The runner closes the window, so a collector must not close it again.
    ///
    /// This is the measurement behind that decision rather than an assertion
    /// of it. A round of reads happens *before* the run asks whether it is
    /// over, so the last round lands at the end of the window: here the
    /// deadline falls at 90 ms between two 20 ms ticks, and the last read is
    /// the one at 100 ms. What is left for a closing read in `stop` to cover
    /// is the handful of microseconds after it -- and a point covering that,
    /// with a counter delta of zero, is what dragged `io`'s summaries and is
    /// what `net/dev.rs` still produces.
    #[test]
    fn the_runner_closes_the_window() {
        let reads_ns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let stopped_ns = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let mut collectors: Vec<Box<dyn Collector>> = vec![Box::new(Clocked {
            id: CollectorId::new("clocked"),
            reads_ns: reads_ns.clone(),
            stopped_ns: stopped_ns.clone(),
        })];

        let interval = StdDuration::from_millis(20);
        let deadline_ns = monotonic_ns() + 90_000_000;
        let (_, _, stopped_because) = drive_attached(
            &mut collectors,
            &[Pace {
                interval,
                snapshot: false,
            }],
            &[true],
            None,
            &mut Spending::unweighed(1),
            || (monotonic_ns() >= deadline_ns).then(|| "the deadline".to_owned()),
        );
        // The runner calls `stop` right after the loop; this stands in for it.
        for collector in collectors.iter_mut() {
            collector.stop();
        }

        assert_eq!(stopped_because, "the deadline");
        let reads_ns = reads_ns.borrow();
        assert!(
            reads_ns.len() >= 5,
            "a 90 ms window at a 20 ms tick is at least five reads, got {}",
            reads_ns.len()
        );

        let last_read_ns = *reads_ns.last().expect("at least one read");
        assert!(
            last_read_ns >= deadline_ns,
            "the last read must land at the end of the window, not one tick short of it: \
             it came {} ns early",
            deadline_ns - last_read_ns
        );

        let left_over_ns = stopped_ns.get().saturating_sub(last_read_ns);
        assert!(
            left_over_ns * 10 < interval.as_nanos() as u64,
            "a closing read in `stop` would cover {left_over_ns} ns of a {} ns tick, which is \
             not an interval: it is a second point at the same offset",
            interval.as_nanos()
        );
    }

    /// A collector whose target is gone after a known number of reads.
    struct Vanishing {
        id: CollectorId,
        reads_left: u32,
        reads: std::rc::Rc<std::cell::Cell<u32>>,
    }

    impl Collector for Vanishing {
        fn id(&self) -> &CollectorId {
            &self.id
        }

        fn value_attr(&self) -> &'static str {
            "bytes"
        }

        fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
            self.reads.set(self.reads.get() + 1);
            if self.reads_left == 0 {
                return Err(SampleError::TargetGone("the core went offline".to_owned()));
            }
            self.reads_left -= 1;
            Ok(Vec::new())
        }

        fn stop(&mut self) -> Vec<Event> {
            Vec::new()
        }
    }

    /// Only the workload decides when a run ends. A machine target that goes
    /// away first -- a core taken offline, an interface removed -- stops being
    /// read, and its collection says from when; the workload beside it is
    /// still read to the end. The same disappearance on the workload ends the
    /// run, which is what it did before a run could have two targets.
    #[test]
    fn a_machine_target_going_away_does_not_end_the_workload_s_run() {
        let pace = Pace {
            interval: StdDuration::from_millis(10),
            snapshot: false,
        };
        let vanishing = |reads: &std::rc::Rc<std::cell::Cell<u32>>| {
            Box::new(Vanishing {
                id: CollectorId::new("core"),
                reads_left: 2,
                reads: reads.clone(),
            }) as Box<dyn Collector>
        };
        let workload_reads = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let clocked = || {
            Box::new(Clocked {
                id: CollectorId::new("program"),
                reads_ns: workload_reads.clone(),
                stopped_ns: std::rc::Rc::new(std::cell::Cell::new(0)),
            }) as Box<dyn Collector>
        };

        let machine_reads = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut collectors = vec![clocked(), vanishing(&machine_reads)];
        let deadline_ns = monotonic_ns() + 100_000_000;
        let (_, losses, stopped_because) = drive_attached(
            &mut collectors,
            &[pace, pace],
            &[true, false],
            None,
            &mut Spending::unweighed(2),
            || (monotonic_ns() >= deadline_ns).then(|| "the deadline".to_owned()),
        );
        assert_eq!(stopped_because, "the deadline");
        assert_eq!(
            machine_reads.get(),
            3,
            "two good reads and the one that found it gone, and then it is left alone"
        );
        assert!(
            workload_reads.borrow().len() >= 9,
            "the workload is read to the end of a 100 ms window at 10 ms, got {} reads",
            workload_reads.borrow().len()
        );
        assert!(
            losses[1]
                .reasons
                .keys()
                .any(|reason| reason.contains("before the workload ended")
                    && reason.contains("the core went offline")),
            "the collection says it stopped early and why: {:?}",
            losses[1].reasons
        );

        // The same collector as the workload ends the run.
        let workload_gone = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut collectors = vec![vanishing(&workload_gone)];
        let (_, _, stopped_because) = drive_attached(
            &mut collectors,
            &[pace],
            &[true],
            None,
            &mut Spending::unweighed(1),
            || None,
        );
        assert_eq!(stopped_because, "the core went offline");
    }

    // -----------------------------------------------------------------------
    // Hot attach: a request whose window is its own and inside the run's.
    // -----------------------------------------------------------------------

    /// A request resolved against this process, with `window` and `mode`.
    fn windowed(window: Option<&str>, mode: crate::request::Mode) -> ResolvedRequest {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = crate::capabilities::Capabilities::detect();
        let request = crate::request::Request {
            source: "mem.rss".parse().unwrap(),
            target: Target::Pid(vec![std::process::id() as i32]),
            mode,
            emit: vec![crate::request::Emit::Summary],
            keys: Vec::new(),
            filter: None,
            duration: Some("1s".parse().unwrap()),
            budget: None,
            stack: None,
            symbols: None,
            backend: None,
            label: None,
            window: window.map(|text| text.parse().unwrap()),
            value: None,
            sum: Vec::new(),
        };
        crate::resolve::resolve(&catalog, &capabilities, &request).expect("resolves")
    }

    fn poll_ms(ms: u64) -> crate::request::Mode {
        crate::request::Mode::Poll {
            interval: crate::scalar::Duration::from_nanos(ms * 1_000_000),
        }
    }

    /// Something prepared that hands over a collector when it is attached --
    /// after spinning for `spin_ns` of this thread's cpu, which is the attach
    /// cost the test injects -- or refuses to.
    struct Handing {
        collector: Result<Box<dyn Collector>, String>,
        spin_ns: u64,
        at_exec: bool,
    }

    impl Prepared for Handing {
        fn requires_launch(&self) -> Option<&'static str> {
            self.at_exec.then_some("given to the program at exec")
        }

        fn attach(self: Box<Self>, _: &Target) -> Result<Box<dyn Collector>, String> {
            let started_ns = thread_cpu_ns();
            while thread_cpu_ns().saturating_sub(started_ns) < self.spin_ns {}
            self.collector
        }
    }

    fn clocked(
        name: &str,
    ) -> (
        Box<dyn Collector>,
        std::rc::Rc<std::cell::RefCell<Vec<u64>>>,
    ) {
        let reads_ns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let collector = Box::new(Clocked {
            id: CollectorId::new(name),
            reads_ns: reads_ns.clone(),
            stopped_ns: std::rc::Rc::new(std::cell::Cell::new(0)),
        });
        (collector, reads_ns)
    }

    fn pace_ms(ms: u64) -> Pace {
        Pace {
            interval: StdDuration::from_millis(ms),
            snapshot: false,
        }
    }

    /// A window opens and closes while the rest of the run keeps going.
    ///
    /// Ground truth is the collectors' own record of when they were read,
    /// taken on the one clock: every read of the windowed collector must fall
    /// inside the window it asked for, its first read must be the round it was
    /// attached in, and its last must be the one taken at its detach. The
    /// collector beside it is read from the first round to the last, which is
    /// what "without restarting the run" means.
    #[test]
    fn a_window_is_attached_and_detached_while_the_rest_keep_running() {
        const ATTACH_MS: u64 = 60;
        const DETACH_MS: u64 = 140;
        const RUN_MS: u64 = 220;
        let (whole, whole_reads) = clocked("whole");
        let (late, late_reads) = clocked("late");
        let mut slots = vec![
            Slot::Attached(whole),
            Slot::Waiting(Box::new(Handing {
                collector: Ok(late),
                spin_ns: 0,
                at_exec: false,
            })),
        ];
        let mut paces = vec![pace_ms(10), Pace::UNSET];
        let plans = [
            Plan::default(),
            Plan::of(Some(
                format!("{ATTACH_MS}ms..{DETACH_MS}ms").parse().unwrap(),
            )),
        ];
        let mut spending = Spending::weighed_against_this_process(vec![
            windowed(None, poll_ms(10)),
            windowed(Some("60ms..140ms"), poll_ms(10)),
        ]);
        let opened_ns = spending.opened_ns;
        let deadline_ns = opened_ns + RUN_MS * 1_000_000;
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            &[true, true],
            None,
            false,
            &mut spending,
            None,
            None,
            until(deadline_ns),
        );
        assert_eq!(driven.stopped_because, "the deadline");
        assert!(
            matches!(slots[1], Slot::Detached(_)),
            "the window closed on it"
        );
        assert!(
            matches!(slots[0], Slot::Attached(_)),
            "the rest is still running"
        );

        let at = |ms: u64| opened_ns + ms * 1_000_000;
        let late_reads = late_reads.borrow();
        let window = driven.watched[1];
        eprintln!(
            "window opened {} us and closed {} us into the run; {} reads, the first at {} us \
             and the last at {} us",
            (window.opened_ns - opened_ns) / 1_000,
            (window.closed_ns - opened_ns) / 1_000,
            late_reads.len(),
            (late_reads[0] - opened_ns) / 1_000,
            (late_reads.last().unwrap() - opened_ns) / 1_000,
        );
        assert!(window.attached && window.attach_ns.is_some());
        assert!(
            window.opened_ns >= at(ATTACH_MS) && window.closed_ns >= at(DETACH_MS),
            "the window was placed where it was asked, not earlier: {window:?}"
        );
        assert!(
            late_reads.iter().all(|read| window.contains(*read)),
            "every read of a windowed collector is inside its window"
        );
        assert!(
            late_reads[0] - window.opened_ns < 1_000_000,
            "the first read is the round of the attach -- the baseline is taken at the attach"
        );
        assert!(
            *late_reads.last().unwrap() >= at(DETACH_MS),
            "the last read is the one taken at the detach, not one tick short of it"
        );
        assert!(
            window.closed_ns < deadline_ns,
            "a detached collector is stopped at its detach, not kept to the end of the run"
        );
        assert!(
            driven.closed_because[1]
                .as_deref()
                .is_some_and(|why| why.contains("window closed")),
            "{:?}",
            driven.closed_because
        );

        // The collector beside it was read over the whole run, windows or not.
        let whole_reads = whole_reads.borrow();
        assert!(whole_reads[0] < at(ATTACH_MS) && *whole_reads.last().unwrap() >= deadline_ns);
        let during = whole_reads
            .iter()
            .filter(|read| window.contains(**read))
            .count();
        assert!(
            during >= 4,
            "the run went on reading the rest while the window was open: {during} reads"
        );
    }

    /// What attaching costs while the target runs is in the account of the
    /// request that attached.
    ///
    /// Ground truth is a cost injected into the attach: 30 ms of this thread's
    /// cpu, spun by the prepared collector before it hands itself over. That
    /// cannot take less than 30 ms of wall time, and the process cannot have
    /// spent less than 30 ms of cpu over the window -- so both the wall time
    /// the window reports for the attach and the cpu it charges the request
    /// are bounded below by the injection, whatever the rest of the suite is
    /// doing to this process's account.
    #[test]
    fn attaching_mid_run_is_paid_for_by_the_request_that_attached() {
        const SPIN_NS: u64 = 30_000_000;
        let (whole, _) = clocked("whole");
        let (late, _) = clocked("late");
        let mut slots = vec![
            Slot::Attached(whole),
            Slot::Waiting(Box::new(Handing {
                collector: Ok(late),
                spin_ns: SPIN_NS,
                at_exec: false,
            })),
        ];
        let mut paces = vec![pace_ms(10), Pace::UNSET];
        let plans = [Plan::default(), Plan::of(Some("20ms..".parse().unwrap()))];
        let mut spending = Spending::weighed_against_this_process(vec![
            windowed(None, poll_ms(10)),
            windowed(Some("20ms.."), poll_ms(10)),
        ]);
        let deadline_ns = spending.opened_ns + 150_000_000;
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            &[true, true],
            None,
            false,
            &mut spending,
            None,
            None,
            until(deadline_ns),
        );
        let overheads = spending.close();
        let attach_ns = driven.watched[1]
            .attach_ns
            .expect("an attach mid-run is timed");
        eprintln!(
            "attach took {attach_ns} ns of wall time; the late request was charged {} ns of the \
             {} ns this process spent over its window",
            overheads[1].attributed_cpu_ns, overheads[1].collector_cpu_ns
        );
        assert!(
            attach_ns >= SPIN_NS,
            "the attach is timed whole: {attach_ns} ns"
        );
        assert!(
            overheads[1].collector_cpu_ns >= SPIN_NS,
            "the window's account opens before the attach, so the attach is in it"
        );
        // The share is of wall time inside collectors, and the attach is almost
        // all of this request's: most of the process's cpu is charged to it.
        assert!(
            overheads[1].attributed_cpu_ns * 2 >= overheads[1].collector_cpu_ns,
            "the request that attached is charged for attaching: {:?}",
            overheads[1]
        );
        assert!(
            spending.notes[1]
                .iter()
                .any(|note| note.contains("attaching") && note.contains("into the run")),
            "the attach is said where the reader looks: {:?}",
            spending.notes[1]
        );
    }

    /// A collector that could not be attached when its window opened is a
    /// loss of that window, and the run carries on without it.
    #[test]
    fn a_late_attach_that_fails_is_a_loss_and_the_run_goes_on() {
        let (whole, whole_reads) = clocked("whole");
        let mut slots = vec![
            Slot::Attached(whole),
            Slot::Waiting(Box::new(Handing {
                collector: Err("the tracepoint went away".to_owned()),
                spin_ns: 0,
                at_exec: false,
            })),
        ];
        let mut paces = vec![pace_ms(10), Pace::UNSET];
        let plans = [Plan::default(), Plan::of(Some("20ms..".parse().unwrap()))];
        let mut spending = Spending::weighed_against_this_process(vec![
            windowed(None, poll_ms(10)),
            windowed(Some("20ms.."), poll_ms(10)),
        ]);
        let deadline_ns = spending.opened_ns + 100_000_000;
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            &[true, true],
            None,
            false,
            &mut spending,
            None,
            None,
            until(deadline_ns),
        );
        assert_eq!(driven.stopped_because, "the deadline");
        assert!(matches!(slots[1], Slot::Missed));
        assert!(!driven.watched[1].attached);
        let lost = &driven.losses[1];
        assert!(!lost.is_clean(), "an empty window is not a clean one");
        assert!(
            lost.reasons
                .keys()
                .any(|reason| reason.contains("mem.rss") && reason.contains("went away")),
            "the loss names the source and the reason: {lost:?}"
        );
        assert!(*whole_reads.borrow().last().unwrap() >= deadline_ns);
    }

    /// A counter that would follow only threads started after it is not
    /// attached late to a process that already has others: the window's loss
    /// says why, rather than a count of one thread passing for the process.
    ///
    /// Ground truth is this test process, which `cargo test` runs with several
    /// threads -- and one the test starts itself, so that it has more than one
    /// whatever the harness does.
    #[test]
    fn a_counter_blind_to_running_threads_is_not_attached_late_to_a_threaded_target() {
        struct Inheriting;
        impl Prepared for Inheriting {
            fn follows_only_later_threads(&self) -> Option<&'static str> {
                Some("a per-task counter follows threads from its open")
            }
            fn attach(self: Box<Self>, _: &Target) -> Result<Box<dyn Collector>, String> {
                panic!("attached to a target whose threads it cannot see");
            }
        }
        let (stop, parked) = std::sync::mpsc::channel::<()>();
        let helper = std::thread::spawn(move || {
            let _ = parked.recv();
        });
        let own = Target::Pid(vec![std::process::id() as i32]);
        let refusal = blind_to_running_threads(&Inheriting, &own);
        drop(stop);
        helper.join().unwrap();
        let refusal = refusal.expect("this process has threads the counter would miss");
        assert!(
            refusal.contains("threads") && refusal.contains(&std::process::id().to_string()),
            "{refusal}"
        );

        // A collector that does not follow by inheritance is not asked.
        let (collector, _) = clocked("procfs");
        let plain = Handing {
            collector: Ok(collector),
            spin_ns: 0,
            at_exec: false,
        };
        assert_eq!(blind_to_running_threads(&plain, &own), None);
    }

    /// A collector that counts its own losses reports them over its window,
    /// the first tick's included, like every other loss.
    #[test]
    fn a_detached_collector_s_losses_are_its_own() {
        struct Lossy(CollectorId);
        impl Collector for Lossy {
            fn id(&self) -> &CollectorId {
                &self.0
            }
            fn value_attr(&self) -> &'static str {
                "count"
            }
            fn sample(&mut self, _: u64) -> Result<Vec<Event>, SampleError> {
                Ok(Vec::new())
            }
            fn losses(&self) -> Losses {
                Losses {
                    dropped: 7,
                    ..Losses::default()
                }
            }
        }
        let (whole, _) = clocked("whole");
        let mut slots = vec![
            Slot::Attached(whole),
            Slot::Waiting(Box::new(Handing {
                collector: Ok(Box::new(Lossy(CollectorId::new("lossy")))),
                spin_ns: 0,
                at_exec: false,
            })),
        ];
        let mut paces = vec![pace_ms(10), Pace::UNSET];
        let plans = [
            Plan::default(),
            Plan::of(Some("10ms..40ms".parse().unwrap())),
        ];
        let mut spending = Spending::weighed_against_this_process(vec![
            windowed(None, poll_ms(10)),
            windowed(Some("10ms..40ms"), poll_ms(10)),
        ]);
        let deadline_ns = spending.opened_ns + 80_000_000;
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            &[true, true],
            None,
            false,
            &mut spending,
            None,
            None,
            until(deadline_ns),
        );
        assert_eq!(driven.losses[1].dropped, 7);
        assert_eq!(driven.losses[0].dropped, 0, "and nobody else's");
    }

    /// A drained collector's drops are placed at the drains that read them:
    /// one entry per drain whose count grew, by what it grew, in order, and
    /// summing to the collection's drop count. A polled collector's are not
    /// read until its stop. A detach places what its stop read last.
    #[test]
    fn a_drained_collector_s_drops_are_placed_at_the_drains_that_read_them() {
        struct Dropping {
            id: CollectorId,
            dropped: u64,
            reads: u64,
            tick: Option<StdDuration>,
        }
        impl Collector for Dropping {
            fn id(&self) -> &CollectorId {
                &self.id
            }
            fn value_attr(&self) -> &'static str {
                "count"
            }
            fn sample(&mut self, _: u64) -> Result<Vec<Event>, SampleError> {
                // Three drops on every other drain, so some drains add none.
                self.reads += 1;
                if self.reads % 2 == 0 {
                    self.dropped += 3;
                }
                Ok(Vec::new())
            }
            fn stop(&mut self) -> Vec<Event> {
                self.dropped += 5;
                Vec::new()
            }
            fn tick(&self) -> Option<StdDuration> {
                self.tick
            }
            fn losses(&self) -> Losses {
                Losses {
                    dropped: self.dropped,
                    ..Losses::default()
                }
            }
        }
        let dropping = |name: &str, tick| {
            Box::new(Dropping {
                id: CollectorId::new(name),
                dropped: 0,
                reads: 0,
                tick,
            })
        };
        let mut slots = vec![
            Slot::Attached(dropping("drained", Some(StdDuration::from_millis(10)))),
            Slot::Attached(dropping("polled", None)),
        ];
        let mut paces = vec![pace_ms(10), pace_ms(10)];
        let plans = [Plan::of(Some("..60ms".parse().unwrap())), Plan::default()];
        let mut spending = Spending::weighed_against_this_process(vec![
            windowed(Some("..60ms"), poll_ms(10)),
            windowed(None, poll_ms(10)),
        ]);
        let opened_ns = spending.opened_ns;
        let deadline_ns = opened_ns + 100_000_000;
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            &[true, true],
            None,
            false,
            &mut spending,
            None,
            None,
            until(deadline_ns),
        );

        let drained = &driven.drains[0].placed;
        assert!(drained.len() >= 3, "{drained:?}");
        assert_eq!(
            drained.iter().map(|drain| drain.dropped).sum::<u64>(),
            driven.losses[0].dropped,
            "{drained:?}"
        );
        assert!(
            drained
                .windows(2)
                .all(|pair| pair[0].ts_ns <= pair[1].ts_ns),
            "{drained:?}"
        );
        assert!(drained.iter().all(|drain| drain.dropped > 0));
        // The detach's stop dropped five more, placed at the detach.
        let last = drained.last().unwrap();
        assert_eq!(last.dropped, 5, "{drained:?}");
        assert_eq!(last.ts_ns, driven.watched[0].closed_ns);
        assert!(drained
            .iter()
            .all(|drain| drain.ts_ns >= opened_ns && drain.ts_ns <= last.ts_ns));

        // The polled one dropped on its reads too, but nobody read its count:
        // it is placed at its stop, which `drive` leaves to the run's end.
        assert!(driven.drains[1].placed.is_empty());
    }

    /// A run whose every request has a window that closes needs no duration:
    /// it ends when the last of them has closed.
    #[test]
    fn a_run_ends_when_every_window_has_closed() {
        let (first, _) = clocked("first");
        let (second, _) = clocked("second");
        let mut slots = vec![Slot::Attached(first), Slot::Attached(second)];
        let mut paces = vec![pace_ms(10), pace_ms(10)];
        let plans = [
            Plan::of(Some("..30ms".parse().unwrap())),
            Plan::of(Some("..60ms".parse().unwrap())),
        ];
        let mut spending = Spending::weighed_against_this_process(vec![
            windowed(Some("..30ms"), poll_ms(10)),
            windowed(Some("..60ms"), poll_ms(10)),
        ]);
        let opened_ns = spending.opened_ns;
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            &[true, true],
            None,
            false,
            &mut spending,
            None,
            None,
            || None,
        );
        let ended_ms = (monotonic_ns() - opened_ns) / 1_000_000;
        assert_eq!(driven.stopped_because, "every request's window had closed");
        assert!(
            (60..500).contains(&ended_ms),
            "the run ends at the last window's close, {ended_ms} ms in"
        );
        assert!(driven.watched[0].closed_ns < driven.watched[1].closed_ns);
    }

    /// The two windows a collector cannot honour are refused before the run,
    /// by name, with whose gap each one is.
    #[test]
    fn a_window_its_collector_or_mode_cannot_honour_is_refused_by_name() {
        let at_exec = |window: &str| {
            let (collector, _) = clocked("shim");
            let prepared = Handing {
                collector: Ok(collector),
                spin_ns: 0,
                at_exec: true,
            };
            window_honoured(&windowed(Some(window), poll_ms(10)), &prepared)
        };
        for window in ["1s..", "..1s"] {
            match at_exec(window) {
                Err(error @ CollectError::Window { .. }) => {
                    let message = error.to_string();
                    assert_eq!(error.whose(), Whose::Build, "{message}");
                    assert!(
                        message.contains("mem.rss") && message.contains("at exec"),
                        "{message}"
                    );
                }
                other => panic!("expected a refusal of `{window}`, got {other:?}"),
            }
        }

        let (collector, _) = clocked("procfs");
        let prepared = Handing {
            collector: Ok(collector),
            spin_ns: 0,
            at_exec: false,
        };
        let snapshot = windowed(Some("1s..2s"), crate::request::Mode::Snapshot);
        match window_honoured(&snapshot, &prepared) {
            Err(error) => {
                assert_eq!(error.whose(), Whose::Request);
                assert!(error.to_string().contains("single read"), "{error}");
            }
            Ok(_) => panic!("a snapshot with a stop was accepted"),
        }
        assert!(
            window_honoured(
                &windowed(Some("1s.."), crate::request::Mode::Snapshot),
                &prepared
            )
            .is_ok(),
            "a late snapshot is one read taken late, which is fine"
        );

        match window_inside_the_run(&windowed(Some("500ms..2s"), poll_ms(10)), 1_000_000_000) {
            Err(error) => {
                assert_eq!(error.whose(), Whose::Request);
                assert!(error.to_string().contains("2s"), "{error}");
            }
            Ok(()) => panic!("a window past the run's duration was accepted"),
        }
    }

    /// The whole runner, on a real source: a request with a window reports a
    /// span that is its own, and every event it produced falls inside it.
    ///
    /// Ground truth is the clock the test reads around the run: the run lasts
    /// its duration, and the window is where the session put it inside that.
    #[test]
    fn a_windowed_request_s_collection_states_its_own_window() {
        let mut whole = windowed(None, poll_ms(20));
        whole.request.duration = Some("400ms".parse().unwrap());
        let mut late = windowed(Some("100ms..250ms"), poll_ms(20));
        late.request.duration = Some("400ms".parse().unwrap());
        let collections = run_all(&[whole, late]).expect("the run completes");
        let (whole, late) = (&collections[0], &collections[1]);

        eprintln!(
            "whole run {} ms, window {} ms from {} ms in, {} and {} readings",
            whole.observed_ns / 1_000_000,
            late.observed_ns / 1_000_000,
            (late.window.opened_ns - late.window.run_opened_ns) / 1_000_000,
            whole.events.len(),
            late.events.len()
        );
        assert_eq!(whole.window.opened_ns, whole.window.run_opened_ns);
        assert_eq!(late.window.run_opened_ns, whole.window.run_opened_ns);
        assert!(whole.observed_ns >= 400_000_000);
        // Each end is where it was asked or a round after it, so the span is
        // the asked 150 ms give or take the lateness of either end -- and far
        // from the run's 400.
        let into_run = |ns: u64| ns - late.window.run_opened_ns;
        assert!(
            into_run(late.window.opened_ns) >= 100_000_000
                && into_run(late.window.closed_ns) >= 250_000_000,
            "the window is placed where it was asked: {:?}",
            late.window
        );
        assert!(
            (120_000_000..300_000_000).contains(&late.observed_ns),
            "a window of 150 ms observed {} ns",
            late.observed_ns
        );
        assert_eq!(late.observed_ns, late.window.span_ns());
        assert!(!late.events.is_empty());
        assert!(
            late.events
                .iter()
                .all(|event| late.window.contains(event.ts_ns)),
            "every event of a windowed request is inside its window"
        );
        assert!(
            late.stopped_because.contains("window closed"),
            "{}",
            late.stopped_because
        );
        assert!(late.overhead.wall_ns < whole.overhead.wall_ns);
    }

    /// Something prepared that stamps the moment it is attached.
    struct Stamping {
        collector: Box<dyn Collector>,
        attached_ns: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl Prepared for Stamping {
        fn attach(self: Box<Self>, _: &Target) -> Result<Box<dyn Collector>, String> {
            self.attached_ns.set(monotonic_ns());
            Ok(self.collector)
        }
    }

    type Stamps = std::rc::Rc<std::cell::Cell<u64>>;

    /// A held collector, its reads, when it was stopped and when attached.
    fn stamping(
        name: &str,
    ) -> (
        Slot,
        std::rc::Rc<std::cell::RefCell<Vec<u64>>>,
        Stamps,
        Stamps,
    ) {
        let reads_ns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let stopped_ns = std::rc::Rc::new(std::cell::Cell::new(0));
        let attached_ns = std::rc::Rc::new(std::cell::Cell::new(0));
        let slot = Slot::Waiting(Box::new(Stamping {
            collector: Box::new(Clocked {
                id: CollectorId::new(name),
                reads_ns: reads_ns.clone(),
                stopped_ns: stopped_ns.clone(),
            }),
            attached_ns: attached_ns.clone(),
        }));
        (slot, reads_ns, stopped_ns, attached_ns)
    }

    fn held() -> Plan {
        Plan {
            held: true,
            ..Plan::default()
        }
    }

    /// An escalation that answers from a script: at each offset into the run,
    /// these switches, once. It records where each request stood whenever it
    /// was asked, and whether it was asked to shed.
    struct Scripted {
        script: Vec<(u64, Vec<Switch>)>,
        seen: Vec<Vec<Standing>>,
        sheds: bool,
        /// Shared, so a test's stop condition can wait for the question.
        shed_asked: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
    }

    impl Escalate for Scripted {
        fn decide(&mut self, run: &Progress<'_>) -> Vec<Switch> {
            self.seen.push(run.standing.to_vec());
            let into_ms = run.now_ns.saturating_sub(run.opened_ns) / 1_000_000;
            match self.script.first() {
                Some((at_ms, _)) if into_ms >= *at_ms => self.script.remove(0).1,
                _ => Vec::new(),
            }
        }

        fn shed(&mut self, index: usize, over: &str) -> Option<String> {
            self.shed_asked.borrow_mut().push(index);
            self.sheds.then(|| format!("scripted to shed: {over}"))
        }

        fn close(&mut self, _run: &Progress<'_>) -> Vec<(usize, String)> {
            vec![(0, "scripted summary".to_owned())]
        }
    }

    fn switch(index: usize, attach: bool) -> Switch {
        Switch {
            index,
            attach,
            why: format!("scripted {}", if attach { "attach" } else { "detach" }),
        }
    }

    /// The escalation names a layer, then another, and the runner does what
    /// it says -- and nothing it does not.
    ///
    /// Ground truth is each collector's own record of when it was attached,
    /// read and stopped, on the one clock. A held request is not attached
    /// before it is named; the layer the run leaves is stopped *before* the
    /// one it names is attached, even when the answer lists them the other
    /// way round, so two detailed layers are never paid for at once; and the
    /// cheap collector beside them is read from the first round to the last.
    #[test]
    fn an_escalation_attaches_what_it_names_and_lets_go_of_the_layer_it_leaves() {
        const FIRST_MS: u64 = 40;
        const SECOND_MS: u64 = 100;
        const RUN_MS: u64 = 160;
        let (cheap, cheap_reads) = clocked("cheap");
        let (first, first_reads, first_stopped, first_attached) = stamping("first");
        let (second, second_reads, _, second_attached) = stamping("second");
        let mut slots = vec![Slot::Attached(cheap), first, second];
        let mut paces = vec![pace_ms(10), Pace::UNSET, Pace::UNSET];
        let plans = [Plan::default(), held(), held()];
        let mut spending = Spending::weighed_against_this_process(vec![
            windowed(None, poll_ms(10)),
            windowed(None, poll_ms(10)),
            windowed(None, poll_ms(10)),
        ]);
        let mut script = Scripted {
            script: vec![
                (FIRST_MS, vec![switch(1, true)]),
                // Listed attach first: the runner must still detach first.
                (SECOND_MS, vec![switch(2, true), switch(1, false)]),
            ],
            seen: Vec::new(),
            sheds: false,
            shed_asked: Default::default(),
        };
        let opened_ns = spending.opened_ns;
        let deadline_ns = opened_ns + RUN_MS * 1_000_000;
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            &[true, true, true],
            None,
            false,
            &mut spending,
            Some(&mut script),
            None,
            until(deadline_ns),
        );
        let at = |ms: u64| opened_ns + ms * 1_000_000;
        assert_eq!(driven.stopped_because, "the deadline");
        assert!(
            matches!(slots[1], Slot::Detached(_)),
            "the first layer was let go"
        );
        assert!(
            matches!(slots[2], Slot::Attached(_)),
            "the second is attached to the end"
        );

        assert!(
            script.seen[0][1] == Standing::Waiting && script.seen[0][2] == Standing::Waiting,
            "both are held until named: {:?}",
            script.seen[0]
        );
        let first_reads = first_reads.borrow();
        let second_reads = second_reads.borrow();
        assert!(first_attached.get() >= at(FIRST_MS) && second_attached.get() >= at(SECOND_MS));
        assert!(
            first_reads
                .iter()
                .all(|read| *read >= first_attached.get() && *read <= first_stopped.get()),
            "every read of the first layer is inside the span the escalation gave it"
        );
        assert!(
            first_stopped.get() <= second_attached.get(),
            "the layer left is stopped before the layer named is attached: stopped {} us, \
             attached {} us",
            (first_stopped.get() - opened_ns) / 1_000,
            (second_attached.get() - opened_ns) / 1_000
        );
        assert!(second_reads[0] >= second_attached.get() && !second_reads.is_empty());
        let cheap_reads = cheap_reads.borrow();
        assert!(cheap_reads[0] < at(FIRST_MS) && *cheap_reads.last().unwrap() >= deadline_ns);

        assert!(
            driven.closed_because[1]
                .as_deref()
                .is_some_and(|why| why.contains("by the escalation: scripted detach")),
            "{:?}",
            driven.closed_because
        );
        assert!(
            spending.notes[2]
                .iter()
                .any(|note| note.contains("by the escalation") && note.contains("scripted attach")),
            "{:?}",
            spending.notes[2]
        );
        assert_eq!(driven.escalated[0], vec!["scripted summary".to_owned()]);
        assert!(driven.watched[1].attached && driven.watched[1].closed_ns < deadline_ns);
    }

    /// A held request the escalation never names is never attached, keeps
    /// the run open no longer than the rest, and closes it no sooner.
    #[test]
    fn a_held_request_nothing_names_is_never_attached_and_holds_nothing_open() {
        const RUN_MS: u64 = 80;
        let (cheap, _) = clocked("cheap");
        let (never, never_reads, _, never_attached) = stamping("never");
        let mut slots = vec![Slot::Attached(cheap), never];
        let mut paces = vec![pace_ms(10), Pace::UNSET];
        let plans = [Plan::default(), held()];
        let mut spending = Spending::weighed_against_this_process(vec![
            windowed(None, poll_ms(10)),
            windowed(None, poll_ms(10)),
        ]);
        let mut script = Scripted {
            script: Vec::new(),
            seen: Vec::new(),
            sheds: false,
            shed_asked: Default::default(),
        };
        let deadline_ns = spending.opened_ns + RUN_MS * 1_000_000;
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            &[true, true],
            None,
            false,
            &mut spending,
            Some(&mut script),
            None,
            until(deadline_ns),
        );
        assert_eq!(driven.stopped_because, "the deadline");
        assert!(monotonic_ns() >= deadline_ns);
        assert!(matches!(slots[1], Slot::Waiting(_)));
        assert_eq!(never_attached.get(), 0);
        assert!(never_reads.borrow().is_empty());
        assert!(!driven.watched[1].attached);
        assert!(
            script.seen.len() > 3,
            "asked once a round: {}",
            script.seen.len()
        );
    }

    /// A layer the escalation attached that costs more than its ceiling, with
    /// nothing cheaper to move onto, is let go and the run goes on -- where
    /// the same collector not held for an escalation ends the run, as every
    /// run over its budget does.
    ///
    /// The denominator is this thread's own cpu time, the thread the collector
    /// is read on, so a collector that spins is around one hundred percent of
    /// it and a ceiling of 50 % is exceeded whatever else the process and the
    /// machine are doing.
    #[test]
    fn an_escalated_layer_over_its_budget_is_let_go_and_the_run_goes_on() {
        let run = |sheds: bool| {
            let (cheap, _) = clocked("cheap");
            let (mut burning, spun_ns) = burning(20_000_000);
            let mut slots = vec![
                Slot::Attached(cheap),
                Slot::Waiting(Box::new(Handing {
                    collector: Ok(burning.remove(0)),
                    spin_ns: 0,
                    at_exec: false,
                })),
            ];
            let mut paces = vec![pace_ms(10), Pace::UNSET];
            let plans = [Plan::default(), held()];
            let mut spending = Spending::weighed_against_this_thread(vec![
                windowed(None, poll_ms(10)),
                weighed("50%"),
            ]);
            spending.sheddable = vec![false, true];
            let asked: std::rc::Rc<std::cell::RefCell<Vec<usize>>> = Default::default();
            let mut script = Scripted {
                script: vec![(0, vec![switch(1, true)])],
                seen: Vec::new(),
                sheds,
                shed_asked: asked.clone(),
            };
            let driven = drive(
                &mut slots,
                &mut paces,
                &plans,
                &[true, true],
                None,
                false,
                &mut spending,
                Some(&mut script),
                None,
                after_the_shed(asked.clone()),
            );
            assert!(
                spun_ns.get() > 0,
                "the escalated layer ran and cost something"
            );
            assert_eq!(
                *asked.borrow(),
                vec![1],
                "asked about the held one, once; the run stopped because {}",
                driven.stopped_because
            );
            (driven, slots)
        };

        let (shed, slots) = run(true);
        assert_eq!(shed.stopped_because, "the deadline", "the run went on");
        assert!(matches!(slots[1], Slot::Detached(_)));
        let why = shed.closed_because[1].clone().unwrap_or_default();
        assert!(
            why.contains("to hold the budget") && why.contains("ceiling of 50"),
            "the collection says it was let go for the budget, and which: {why}"
        );

        let (kept, _) = run(false);
        assert!(
            kept.stopped_because.contains("ceiling of 50"),
            "an escalation that does not shed leaves the run to end as any run over budget: {}",
            kept.stopped_because
        );
    }

    // -----------------------------------------------------------------------
    // The recorder.
    // -----------------------------------------------------------------------

    /// A level that is high over known spans of its own clock, read once a
    /// round: the workload whose bursts a recording has to find. It counts its
    /// own reads, which is what the recording's arithmetic is checked against.
    struct Level {
        id: CollectorId,
        opened_ns: u64,
        /// Spans, in ms after `opened_ns`, where the level is high.
        high_ms: Vec<(u64, u64)>,
        reads: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl Collector for Level {
        fn id(&self) -> &CollectorId {
            &self.id
        }

        fn value_attr(&self) -> &'static str {
            "open"
        }

        fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
            let now_ns = monotonic_ns();
            let at_ms = now_ns.saturating_sub(self.opened_ns) / 1_000_000;
            let high = self
                .high_ms
                .iter()
                .any(|(from, to)| (*from..*to).contains(&at_ms));
            self.reads.set(self.reads.get() + 1);
            Ok(vec![Event {
                ts_ns: now_ns,
                ts_err_ns: 0,
                kind: "proc.fd".to_owned(),
                keys: Vec::new(),
                attrs: vec![crate::event::Attr::uint("open", if high { 70 } else { 6 })],
                collector_id: self.id.clone(),
                seq,
            }])
        }
    }

    /// A collector whose read takes a known wall time, and emits nothing.
    struct Stalling {
        id: CollectorId,
        stall: StdDuration,
    }

    impl Collector for Stalling {
        fn id(&self) -> &CollectorId {
            &self.id
        }

        fn value_attr(&self) -> &'static str {
            "bytes"
        }

        fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
            std::thread::sleep(self.stall);
            Ok(Vec::new())
        }
    }

    /// A poller read first in a round another collector's read made outlast
    /// the run is read again at the close, late, and counted as late.
    ///
    /// The case is a live `omn top` of the aarch64 machine: `cpu.util` read
    /// its baseline, `sched.offcpu`'s first read took three seconds, the run
    /// ended in the same round, and `cpu.util` reported no event and nothing
    /// lost. Ground truth is the stall, a sleep of a known length.
    #[test]
    fn a_poller_a_stalled_round_outlasted_is_read_at_the_close_and_counted_late() {
        let reads = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let mut collectors: Vec<Box<dyn Collector>> = vec![
            Box::new(Level {
                id: CollectorId::new("poller"),
                opened_ns: monotonic_ns(),
                high_ms: Vec::new(),
                reads: reads.clone(),
            }),
            Box::new(Stalling {
                id: CollectorId::new("stalling"),
                stall: StdDuration::from_millis(300),
            }),
        ];
        let pace = Pace {
            interval: StdDuration::from_millis(50),
            snapshot: false,
        };
        let mut spending = Spending::unweighed(2);
        let (events, losses, _) = drive_attached(
            &mut collectors,
            &[pace, pace],
            &[true, true],
            None,
            &mut spending,
            until(monotonic_ns() + 100_000_000),
        );
        assert_eq!(reads.get(), 2, "the baseline and the closing read");
        assert_eq!(events[0].len(), 2);
        assert!(losses[0].late_ticks >= 1, "{:?}", losses[0]);
    }

    struct RecordedRun {
        documents: Vec<Document>,
        recorded: Recorded,
        reads: u64,
        stopped_because: String,
    }

    /// Drives one `Level` under a recording for `run_ms`, polled every 10 ms.
    fn record_level(
        high_ms: Vec<(u64, u64)>,
        run_ms: u64,
        trigger: &str,
        documents: u32,
    ) -> RecordedRun {
        let resolved = vec![windowed(None, poll_ms(10))];
        let reads = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let opened_ns = monotonic_ns();
        let mut slots = vec![Slot::Attached(Box::new(Level {
            id: CollectorId::new("level"),
            opened_ns,
            high_ms,
            reads: reads.clone(),
        }) as Box<dyn Collector>)];
        let mut paces = vec![Pace {
            interval: StdDuration::from_millis(10),
            snapshot: false,
        }];
        let plans = vec![Plan::default()];
        let mut spending = Spending::unweighed(1);
        spending.serving = resolved.clone();
        let mut written = Vec::new();
        let mut sink = |document: Document| -> Result<(), String> {
            written.push(document);
            Ok(())
        };
        let mut recorder = Recorder {
            on: 0,
            trigger: trigger.parse().unwrap(),
            retain_ns: 100_000_000,
            after_ns: 0,
            documents,
            sink: &mut sink,
        };
        let run_opened_ns = spending.opened_ns;
        let mut recording = Recording::new(&mut recorder, &resolved, &slots, run_opened_ns);
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            &[true],
            None,
            false,
            &mut spending,
            None,
            Some(&mut recording),
            until(opened_ns + run_ms * 1_000_000),
        );
        let recorded = recording.recorded(&driven.stopped_because, monotonic_ns(), 0);
        drop(recording);
        drop(recorder);
        RecordedRun {
            documents: written,
            recorded,
            reads: reads.get(),
            stopped_because: driven.stopped_because,
        }
    }

    /// Two bursts, each held for several rounds, write exactly two documents:
    /// a trigger fires on the round its predicate becomes true, not on every
    /// round it stays true. Each window holds its burst, reaches back no
    /// further than the retention, and every read the collector counted is
    /// either in a document, evicted and counted, or reported unrecorded at
    /// the end -- nothing is lost without a number.
    #[test]
    fn two_held_bursts_write_two_documents_and_every_read_is_accounted_for() {
        let bursts = vec![(250, 330), (550, 630)];
        let run = record_level(bursts.clone(), 800, "value >= 64", 5);
        assert_eq!(
            run.documents.len(),
            2,
            "two bursts of eight rounds each are two firings: {:?}",
            run.recorded.documents
        );
        let mut accounted = run.recorded.unrecorded[0].events;
        for (document, (from_ms, _)) in run.documents.iter().zip(&bursts) {
            let fired = &document.trigger;
            let collection = &document.collections[0];
            let fired_at_ms = fired.fired_ns.saturating_sub(fired.run_opened_ns) / 1_000_000;
            assert!(
                fired_at_ms + 5 >= *from_ms,
                "document {} fired at {fired_at_ms} ms, before its burst at {from_ms} ms",
                fired.number
            );
            assert_eq!(
                fired.value,
                Some(70.0),
                "the value that fired it is the burst's"
            );
            assert!(fired.opened_ns <= fired.event_ts_ns && fired.event_ts_ns <= fired.closed_ns);
            assert!(
                fired.closed_ns - fired.opened_ns <= 100_000_000,
                "the window is the retention, 100 ms, at most: {fired:?}"
            );
            assert!(
                collection
                    .events
                    .iter()
                    .all(|event| fired.opened_ns <= event.ts_ns && event.ts_ns <= fired.closed_ns),
                "a document holds its window and nothing outside it"
            );
            assert_eq!(collection.window.opened_ns, fired.opened_ns);
            assert_eq!(collection.window.closed_ns, fired.closed_ns);
            let lows = collection
                .events
                .iter()
                .filter(|event| {
                    event.attr("open").and_then(crate::event::AttrValue::as_f64) == Some(6.0)
                })
                .count();
            assert!(
                lows > 0,
                "the window reaches back into the idle before the burst"
            );
            assert_eq!(
                collection.losses.evicted, fired.evicted[0].events,
                "what the trigger record says was evicted is what the document's losses count"
            );
            assert!(
                fired.evicted[0].events > 0,
                "the idle before each burst was evicted"
            );
            accounted += fired.evicted[0].events + collection.events.len() as u64;
        }
        assert_eq!(
            accounted, run.reads,
            "every read is in a document, evicted and counted, or unrecorded at the end"
        );
        assert!(
            run.stopped_because.contains("deadline"),
            "fewer firings than the bound: the run ends at its longest ({})",
            run.stopped_because
        );
    }

    /// The bound on documents ends the run, and says so.
    #[test]
    fn the_last_document_the_bound_allows_ends_the_run() {
        let run = record_level(
            vec![(150, 200), (300, 350), (450, 500)],
            800,
            "value >= 64",
            2,
        );
        assert_eq!(run.documents.len(), 2);
        assert!(
            run.stopped_because
                .contains("2 document(s), the most it was allowed"),
            "{}",
            run.stopped_because
        );
    }

    /// A trigger that never fires ends at the longest duration with no
    /// document, and what was read is counted as unrecorded rather than
    /// silently gone.
    #[test]
    fn a_trigger_that_never_fires_writes_nothing_and_counts_what_it_read() {
        let run = record_level(vec![(100, 200)], 400, "value >= 1000", 3);
        assert!(run.documents.is_empty());
        assert!(run.recorded.documents.is_empty());
        assert_eq!(run.recorded.unrecorded[0].events, run.reads);
        assert!(run.reads > 0);
    }

    /// `value` in a trigger is whatever the collector measures in, and every
    /// other field is left as written.
    #[test]
    fn value_in_a_trigger_reads_the_collector_s_own_attribute() {
        let trigger: crate::filter::FilterExpr = "value >= 64 && comm == 'x'".parse().unwrap();
        let read = read_value_as(&trigger, "open");
        let fields: Vec<&str> = read.fields().into_iter().collect();
        assert_eq!(fields, ["comm", "open"]);
    }

    /// A rolling budget weighs an account from the oldest reading inside its
    /// span, not from the run's open: a long quiet stretch does not dilute a
    /// burst after it.
    #[test]
    fn a_rolling_budget_is_weighed_from_inside_its_span() {
        let mut spending = Spending::unweighed(1);
        spending.rolling_ns = Some(1_000);
        spending.since[0] = Since {
            wall_opened_ns: 0,
            ..Since::default()
        };
        let at = |ns: u64| Since {
            self_cpu_ns: ns,
            wall_opened_ns: ns,
            ..Since::default()
        };
        // Inside the first span the run's open is the baseline.
        assert_eq!(spending.baseline(0, at(500)).wall_opened_ns, 0);
        assert_eq!(spending.baseline(0, at(900)).wall_opened_ns, 0);
        // Past it, the latest reading at or before the span's start.
        assert_eq!(spending.baseline(0, at(1_600)).wall_opened_ns, 500);
        assert_eq!(spending.baseline(0, at(5_000)).wall_opened_ns, 1_600);
        // A collection moved onto another backend since is weighed from then.
        spending.since[0] = at(4_500);
        assert_eq!(spending.baseline(0, at(5_800)).wall_opened_ns, 4_500);
        // Without a rolling span, the account's own start, always.
        spending.rolling_ns = None;
        assert_eq!(spending.baseline(0, at(9_000)).wall_opened_ns, 4_500);
    }
}
