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
mod io;
mod launch;
pub mod mem;
mod net;
mod power;
mod proc;
mod runtime;
mod sched;
mod task;

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
            CollectError::NoStopCondition | CollectError::MixedTargets { .. } => Whose::Request,
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
}

impl Losses {
    pub fn is_clean(&self) -> bool {
        self.dropped == 0
            && self.failed_reads == 0
            && self.late_ticks == 0
            && self.undecoded == 0
            && self.throttled == 0
    }

    pub fn note(&mut self, reason: impl Into<String>) {
        self.note_times(reason, 1);
    }

    fn note_times(&mut self, reason: impl Into<String>, times: u64) {
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
        let reason = format!(
            "a round of reads outran the {} tick: worst {}, {} on average",
            span_ns(overrun.tick_ns),
            span_ns(overrun.worst_ns),
            span_ns(mean_ns),
        );
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
        self.overrun = match (self.overrun, other.overrun) {
            (Some(mine), Some(theirs)) => Some(Overrun {
                // The two halves of one collection are driven at one cadence,
                // so the ticks agree; the larger is kept rather than averaged
                // so that the reason never understates what it is against.
                tick_ns: mine.tick_ns.max(theirs.tick_ns),
                worst_ns: mine.worst_ns.max(theirs.worst_ns),
                total_ns: mine.total_ns + theirs.total_ns,
            }),
            (mine, theirs) => mine.or(theirs),
        };
        for (reason, count) in other.reasons {
            *self.reasons.entry(reason).or_insert(0) += count;
        }
    }
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
    /// backend. Enforcing a budget is part of the price of having one.
    ///
    /// Reported rather than subtracted. A measurement that hides its own price
    /// is exactly the thing this field exists to stop, and a budget check that
    /// ran often enough to dominate the answer would show up here first.
    pub measurement_ns: u64,
    /// How many times the cost was read during the run.
    pub measurements: u64,
}

impl Overhead {
    /// The overhead as a percentage of the observed workload, when there is a
    /// workload to be a percentage of.
    pub fn percent(&self) -> Option<f64> {
        match self.observed_cpu_ns {
            Some(observed) if observed > 0 => {
                Some(self.attributed_cpu_ns as f64 * 100.0 / observed as f64)
            }
            _ => None,
        }
    }
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

/// Cpu time one task has consumed, from its own `/proc` entry.
///
/// `cutime` and `cstime` are included: a shell that forks is still the workload
/// that was asked for, and leaving its children out would make every collector
/// look expensive against it.
fn task_cpu_ns(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesised and may contain spaces and parentheses
    // of its own, so the fields are counted from the last `)` rather than split
    // from the start. After it, the first field is `state`, which is field 3.
    let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    let ticks: u64 = fields
        .get(11..15)?
        .iter()
        .map(|field| field.parse::<u64>().unwrap_or(0))
        .sum();
    Some(ticks * 1_000_000_000 / ticks_per_second())
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

/// Reads the cpu time of whatever is being observed, so that the tool's own
/// cost can be a fraction of something real.
enum Meter {
    /// A program this tool launched. While it runs, its own `/proc` entry says
    /// what it has spent; once it has been reaped its entry is gone and the
    /// kernel has rolled its usage into this process's children account, which
    /// is the same number `time` prints and is read from there instead.
    Launched(i32),
    /// Processes or threads the request named.
    Tasks(Vec<i32>),
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
                Some(pid) => Meter::Launched(pid),
                None => Meter::Nothing("the program was never launched"),
            },
            Target::Pid(pids) | Target::Tid(pids) => Meter::Tasks(pids.clone()),
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

    /// The cumulative cpu time of the target, to be differenced against a
    /// baseline taken when the window opened.
    fn read(&self) -> Option<u64> {
        match self {
            Meter::Launched(pid) => {
                Some(task_cpu_ns(*pid).unwrap_or_else(|| rusage_ns(libc::RUSAGE_CHILDREN)))
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
            Meter::Cpus(set) => cpus_busy_ns(set.as_deref()),
            Meter::Cgroup(directory) => cgroup_cpu_ns(directory),
            Meter::Nothing(_) => None,
        }
    }

    /// What to record beside the number, so a reader knows which account it is.
    fn describes(&self) -> String {
        match self {
            Meter::Launched(pid) => format!(
                "the launched program's own cpu time (/proc/{pid}/stat while it ran, \
                 getrusage(RUSAGE_CHILDREN) once it was reaped)"
            ),
            Meter::Tasks(pids) => format!(
                "the cpu time of {} (/proc/<pid>/stat, children included)",
                pids.iter()
                    .map(|pid| pid.to_string())
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
        }
    }
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
    assert!(!resolved.is_empty(), "a run needs at least one request");
    let workload = workload_of(resolved)?;
    // Whether each request is about the workload, which decides what it is
    // bound to, what its overhead is a percentage of, and whether its target
    // going away ends the run. With no workload in the session every target
    // is the run's own, as it was when a run had exactly one.
    let about_workload: Vec<bool> = resolved
        .iter()
        .map(|resolved| workload.is_none_or(|workload| &resolved.request.target == workload))
        .collect();
    let launched_argv = match workload {
        Some(Target::Launch(argv)) => Some(argv),
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
    let mut prepared: Vec<Box<dyn Prepared>> = all_or_nothing(resolved.iter().map(prepare))?;

    // A window this collector or this mode cannot honour is refused while
    // nothing is running, and before the environment is merged: a collector
    // given to the program at exec is exactly the one a window cannot move.
    all_or_nothing(
        resolved
            .iter()
            .zip(&prepared)
            .map(|(resolved, prepared)| window_honoured(resolved, prepared.as_ref())),
    )?;

    // Only the collectors of the launch act on it. A machine target beside it
    // is not given to the program, so what its collector would want of the
    // program's environment -- nothing, for every collector written -- is not
    // the program's business.
    let launching: Vec<bool> = resolved
        .iter()
        .map(|resolved| matches!(resolved.request.target, Target::Launch(_)))
        .collect();
    share_launches(&mut prepared, &launching);
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
        if matches!(target, Target::Launch(_)) {
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
    if duration_ns.is_none() && launched_argv.is_none() && !snapshot && !every_window_closes {
        return Err(CollectError::NoStopCondition);
    }
    if let Some(duration_ns) = duration_ns {
        all_or_nothing(
            resolved
                .iter()
                .map(|resolved| window_inside_the_run(resolved, duration_ns)),
        )?;
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
        if resolved[index]
            .request
            .window
            .is_some_and(|window| window.attaches_late())
        {
            slots.push(Slot::Waiting(prepared));
            continue;
        }
        let source_id = resolved[index].request.source.to_string();
        // These two happen against a real target -- a counter that would not
        // open, a file that is not readable, a process that has gone -- so an
        // unmarked refusal here is a failure of this run. A collector refusing
        // a shape it has not implemented says so with `not_built`.
        let mut collector = prepared
            .attach(&bound[index])
            .map_err(|reason| CollectError::start(&source_id, reason, Whose::Machine))?;
        if let Err(reason) = collector.start() {
            if let Some(child) = child.as_mut() {
                child.terminate();
            }
            return Err(CollectError::start(source_id, reason, Whose::Machine));
        }
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
    let plans: Vec<Plan> = resolved
        .iter()
        .map(|resolved| Plan::of(resolved.request.window))
        .collect();
    let mut spending = Spending::open(meters, bound, resolved.to_vec(), cheaper);
    let started_ns = spending.opened_ns;
    let wake = child.as_ref().and_then(|child| child.exit_fd());
    let Driven {
        mut events,
        mut losses,
        mut watched,
        mut closed_because,
        stopped_because,
    } = drive(
        &mut slots,
        &mut paces,
        &plans,
        &about_workload,
        wake,
        &mut spending,
        || {
            if let Some(child) = child.as_mut() {
                if let Some(status) = child.finished() {
                    return Some(format!("the launched program exited with {status}"));
                }
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
    let ended_s = monotonic_ns().saturating_sub(started_ns) as f64 / 1e9;
    for (index, slot) in slots.iter_mut().enumerate() {
        match slot {
            Slot::Attached(collector) => {
                let mut trailing = collector.stop();
                events[index].append(&mut trailing);
                let collector_losses = collector.losses();
                losses[index].absorb(collector_losses);
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
            Slot::Waiting(_) => {
                let window = resolved[index].request.window.unwrap_or(Window {
                    start: None,
                    stop: None,
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
    // not leave behind processes it started.
    if let Some(child) = child.as_mut() {
        if child.finished().is_none() {
            child.terminate();
        }
    }

    let closed_ns = monotonic_ns();
    for (index, slot) in slots.iter().enumerate() {
        if matches!(slot, Slot::Attached(_)) {
            watched[index].closed_ns = closed_ns;
        } else if !watched[index].attached {
            // Never attached: a span of nothing, at the end.
            watched[index].opened_ns = closed_ns;
            watched[index].closed_ns = closed_ns;
        }
    }
    let lifetime = child.as_ref().and_then(|child| child.lifetime());
    // Read once the child has been reaped: `getrusage(RUSAGE_CHILDREN)` is what
    // `time` prints, and it only prints it after the wait.
    let overheads = spending.close();
    Ok(slots
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let (value_attr, stacks, collector_notes) = match slot {
                Slot::Attached(collector) => (
                    collector.value_attr(),
                    collector.stacks(),
                    collector.notes(),
                ),
                Slot::Detached(record) => (
                    record.value_attr,
                    record.stacks.clone(),
                    record.notes.clone(),
                ),
                Slot::Waiting(_) | Slot::Missed => ("", BTreeMap::new(), Vec::new()),
            };
            let mut collected = std::mem::take(&mut events[index]);
            collected.sort_by_key(|event| event.ts_ns);
            // The filter is the request's, and a degradation does not change
            // the request -- only how it was served.
            let filtered = apply_filter(&resolved[index], &mut collected);
            let mut notes = std::mem::take(&mut degradations[index]);
            notes.extend(std::mem::take(&mut spending.notes[index]));
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
            }
        })
        .collect())
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
) -> Result<(), CollectError> {
    let Some(window) = resolved.request.window else {
        return Ok(());
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
    Ok(())
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
            "{reason}, and process {pid} was already running {threads} threads, so a count              from here would be of one of them"
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
    Detached(Detached),
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
}

/// When one request is attached and detached, in nanoseconds after the run
/// opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Plan {
    attach_ns: u64,
    detach_ns: Option<u64>,
}

impl Plan {
    fn of(window: Option<Window>) -> Plan {
        window.map_or(Plan::default(), |window| Plan {
            attach_ns: window.start_ns(),
            detach_ns: window.stop.map(crate::scalar::Duration::as_nanos),
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
}

/// The cost half of a run: what it is spending, what it was allowed to spend,
/// and where it turns when the two disagree.
struct Spending<'a> {
    /// What each collection's overhead is a percentage of: its own target's
    /// cpu account, since a core beside a program is weighed against the core.
    meters: Vec<Meter>,
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
}

impl<'a> Spending<'a> {
    /// Opens the accounting, with the collectors attached and the program
    /// running: what was spent resolving, probing and attaching is this tool's
    /// startup, not the price of the measurement.
    fn open(
        meters: Vec<Meter>,
        bound: Vec<Target>,
        serving: Vec<ResolvedRequest>,
        cheaper: &'a mut dyn Cheaper,
    ) -> Self {
        let count = serving.len();
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
        let observed_opened: Vec<u64> = meters
            .iter()
            .map(|meter| meter.read().unwrap_or(0))
            .collect();
        Self {
            meters,
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
                })
                .collect(),
            opened_ns,
            self_cpu_opened_ns,
            observed_opened,
            last_check_ns: opened_ns,
            measurement_ns: 0,
            measurements: 0,
            notes: vec![Vec::new(); count],
            served_by: serving_ids,
            window_since: vec![None; count],
            frozen: vec![None; count],
        }
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
        Overhead {
            collector_cpu_ns,
            attributed_cpu_ns,
            observed_cpu_ns: observed_cpu_ns
                .map(|observed| observed.saturating_sub(since.observed_cpu_ns)),
            observed_from: self.meters[index].describes(),
            wall_ns: now_ns.saturating_sub(since.wall_opened_ns),
            measurement_ns: self.measurement_ns,
            measurements: self.measurements,
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
            let observed_cpu_ns = self.meters[index].read();
            let spent = self.spent(
                index,
                self.since[index],
                self_cpu_ns,
                observed_cpu_ns,
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
            // A collection a budget has already moved once is weighed on the
            // window since it moved, so the reason has to say which window it
            // is talking about or it reads as disagreeing with the run's own
            // total.
            let window = if self.served_by[index].len() > 1 {
                format!(" since it was moved onto `{was}`")
            } else {
                String::new()
            };
            match self.cheaper.next(index, &spent, ceiling) {
                Some(next) => match self.degrade(index, next, slots, events, losses) {
                    Ok(onto) => self.notes[index].push(format!(
                        "over budget at {:.2} ms into the run: `{source}` through `{was}` spent \
                         {percent:.2} % of the target's cpu time against a ceiling of {ceiling} %, \
                         so it was moved onto `{onto}` for the rest of it",
                        entered_ns.saturating_sub(self.opened_ns) as f64 / 1e6,
                    )),
                    Err(why) => {
                        stop = Some(format!(
                            "`{source}` spent {percent:.2} % of the target's cpu time{window} \
                             against a ceiling of {ceiling} %, and the cheaper backend could \
                             not be started: {why}"
                        ));
                        break;
                    }
                },
                None => {
                    stop = Some(format!(
                        "`{source}` spent {percent:.2} % of the target's cpu time{window} against \
                         a ceiling of {ceiling} %, and nothing cheaper than `{was}` is available"
                    ));
                    break;
                }
            }
        }

        self.measurement_ns += monotonic_ns().saturating_sub(entered_ns);
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
    ) -> Result<String, String> {
        let onto = next.backend.id.clone();
        let Slot::Attached(current) = &mut slots[index] else {
            return Err("it is not attached".to_owned());
        };
        let prepared = prepare(&next).map_err(|error| error.to_string())?;
        let mut replacement = prepared.attach(&self.bound[index])?;
        replacement.start()?;

        let mut trailing = current.stop();
        events[index].append(&mut trailing);
        losses[index].absorb(current.losses());
        losses[index].note(format!(
            "the budget moved this collection from `{}` to `{onto}` mid-run; the series is two \
             backends end to end",
            self.serving[index].backend.id
        ));
        *current = replacement;
        self.serving[index] = next;
        self.served_by[index].push(onto.clone());
        // The replacement is weighed on its own account.
        self.since[index] = Since {
            wall_opened_ns: self.opened_ns,
            ..self.now(index)
        };
        Ok(onto)
    }

    /// Where one collection's account stands at this instant.
    fn now(&self, index: usize) -> Since {
        Since {
            self_cpu_ns: rusage_ns(libc::RUSAGE_SELF),
            observed_cpu_ns: self.meters[index].read().unwrap_or(0),
            inside_ns: self.inside_ns[index],
            total_inside_ns: self.inside_ns.iter().sum(),
            wall_opened_ns: monotonic_ns(),
        }
    }

    /// Opens the account of a collection attached mid-run: its budget, and
    /// the overhead its results report, are over its own window.
    ///
    /// Taken *before* the attach, so that what attaching cost -- loading a
    /// program into the kernel while the target runs -- is in its account
    /// rather than in nobody's.
    fn attaching(&mut self, index: usize) {
        let entered_ns = monotonic_ns();
        let since = self.now(index);
        self.since[index] = since;
        self.window_since[index] = Some(since);
        self.measurements += 1;
        self.measurement_ns += monotonic_ns().saturating_sub(entered_ns);
    }

    /// Closes the account of a collection detached mid-run.
    fn detached(&mut self, index: usize, closed_ns: u64) {
        let entered_ns = monotonic_ns();
        let since = self.window_since[index].unwrap_or(self.whole_run(index));
        let self_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
        let observed_cpu_ns = self.meters[index].read();
        self.measurements += 1;
        self.measurement_ns += monotonic_ns().saturating_sub(entered_ns);
        self.frozen[index] =
            Some(self.spent(index, since, self_cpu_ns, observed_cpu_ns, closed_ns));
    }

    /// The account of a collection that ran for the whole of the run.
    fn whole_run(&self, index: usize) -> Since {
        Since {
            self_cpu_ns: self.self_cpu_opened_ns,
            observed_cpu_ns: self.observed_opened.get(index).copied().unwrap_or(0),
            inside_ns: 0,
            total_inside_ns: 0,
            wall_opened_ns: self.opened_ns,
        }
    }

    /// Closes the accounting and shares the run's cost out between the
    /// collections. This is the whole window, not the part since a degradation:
    /// what the run cost is what it cost.
    fn close(&mut self) -> Vec<Overhead> {
        let entered_ns = monotonic_ns();
        let self_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
        let observed_cpu_ns: Vec<Option<u64>> = self.meters.iter().map(Meter::read).collect();
        self.measurements += 1;
        let closed_ns = monotonic_ns();
        self.measurement_ns += closed_ns.saturating_sub(entered_ns);

        (0..self.ceilings.len())
            .map(|index| {
                if let Some(frozen) = &self.frozen[index] {
                    return frozen.clone();
                }
                let since = self.window_since[index].unwrap_or(self.whole_run(index));
                self.spent(index, since, self_cpu_ns, observed_cpu_ns[index], closed_ns)
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
            Box::leak(Box::new(StopWhenOverBudget)),
        );
        spending.meters = (0..count)
            .map(|_| Meter::Nothing("this run was not weighed"))
            .collect();
        spending.bound = vec![Target::System; count];
        spending.ceilings = vec![None; count];
        spending.inside_ns = vec![0; count];
        spending.since = vec![Since::default(); count];
        spending.notes = vec![Vec::new(); count];
        spending.served_by = vec![Vec::new(); count];
        spending.window_since = vec![None; count];
        spending.frozen = vec![None; count];
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
    fn weighed_against_this_process(serving: Vec<ResolvedRequest>) -> Spending<'static> {
        let own = std::process::id() as i32;
        let count = serving.len();
        Spending::open(
            (0..count).map(|_| Meter::Tasks(vec![own])).collect(),
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
fn drive(
    slots: &mut [Slot],
    paces: &mut [Pace],
    plans: &[Plan],
    about_workload: &[bool],
    wake: Option<RawFd>,
    spending: &mut Spending<'_>,
    mut ended: impl FnMut() -> Option<String>,
) -> Driven {
    let count = slots.len();
    let mut events: Vec<Vec<Event>> = vec![Vec::new(); count];
    let mut losses: Vec<Losses> = vec![Losses::default(); count];
    let mut sequences = vec![0u64; count];
    // Only a run serving several collectors has to share its cpu time out
    // between them, and only then are the two clock reads a round worth taking:
    // with one collector the share is the whole.
    let attributing = count > 1;
    let opened_ns = spending.opened_ns;
    // Everything is due at once on the first round: a series starts with a
    // reading, and a counter's first read is the baseline the rest are against.
    let mut due_ns = vec![monotonic_ns(); count];
    let mut finished: Vec<Option<Finished>> = vec![None; count];
    let mut watched: Vec<Watched> = slots
        .iter()
        .map(|slot| Watched {
            run_opened_ns: opened_ns,
            opened_ns,
            closed_ns: opened_ns,
            attached: matches!(slot, Slot::Attached(_)),
            attach_ns: None,
        })
        .collect();
    let mut closed_because: Vec<Option<String>> = vec![None; count];
    let into_run = |ns: u64| ns.saturating_sub(opened_ns) as f64 / 1e9;
    // When a collector's window closes, which makes it due then whatever its
    // pace says.
    let detach_at = |index: usize| plans[index].detach_ns.map(|ns| opened_ns + ns);

    let stopped_because = loop {
        let round_started_ns = monotonic_ns();

        // Windows that open now. The account is opened before the attach, so
        // what attaching costs while the target runs is this request's.
        for index in 0..count {
            if !matches!(slots[index], Slot::Waiting(_))
                || round_started_ns < opened_ns + plans[index].attach_ns
            {
                continue;
            }
            let Slot::Waiting(prepared) = std::mem::replace(&mut slots[index], Slot::Missed) else {
                unreachable!("checked above");
            };
            spending.attaching(index);
            let entered_ns = monotonic_ns();
            let attached = match blind_to_running_threads(prepared.as_ref(), &spending.bound[index])
            {
                Some(reason) => Err(reason),
                None => prepared
                    .attach(&spending.bound[index])
                    .and_then(|mut collector| collector.start().map(|()| collector)),
            };
            let attached_ns = monotonic_ns();
            let took_ns = attached_ns.saturating_sub(entered_ns);
            if attributing {
                spending.inside_ns[index] += took_ns;
            }
            let source = spending.serving[index].request.source.to_string();
            match attached {
                Ok(collector) => {
                    let mode = spending.serving[index].request.mode;
                    paces[index] = pace_of(collector.as_ref(), mode);
                    if let Some(note) =
                        unpaced(&spending.serving[index], collector.as_ref(), paces[index])
                    {
                        spending.notes[index].push(note);
                    }
                    spending.notes[index].push(format!(
                        "watched only from {:.3} s into the run, as its window asked: attaching \
                         it took {} while the target ran, and its numbers cover its window alone",
                        into_run(entered_ns),
                        span_ns(took_ns),
                    ));
                    due_ns[index] = attached_ns;
                    watched[index].opened_ns = attached_ns;
                    watched[index].attached = true;
                    watched[index].attach_ns = Some(took_ns);
                    slots[index] = Slot::Attached(collector);
                }
                Err(reason) => {
                    // A window that asked for something and got nothing: a
                    // loss, not an empty series that reads as a quiet one.
                    losses[index].failed_reads += 1;
                    losses[index].note(format!(
                        "`{source}` could not be attached {:.3} s into the run, so nothing was \
                         collected over its window: {reason}",
                        into_run(entered_ns),
                    ));
                    closed_because[index] = Some(format!("never attached: {reason}"));
                    slots[index] = Slot::Missed;
                    finished[index] = Some(Finished::Window);
                }
            }
        }

        let mut gone = None;
        for (index, slot) in slots.iter_mut().enumerate() {
            let Slot::Attached(collector) = slot else {
                continue;
            };
            let due = detach_at(index).map_or(due_ns[index], |at| due_ns[index].min(at));
            if finished[index].is_some() || round_started_ns < due {
                continue;
            }
            let entered_ns = if attributing { monotonic_ns() } else { 0 };
            match collector.sample(sequences[index]) {
                Ok(mut sampled) => {
                    sequences[index] += sampled.len() as u64;
                    events[index].append(&mut sampled);
                }
                Err(SampleError::TargetGone(reason)) if about_workload[index] => {
                    gone = Some(reason)
                }
                Err(SampleError::TargetGone(reason)) => {
                    losses[index].note(format!(
                        "stopped reading {:.3} s into the run, before the workload ended: {reason}",
                        into_run(round_started_ns)
                    ));
                    finished[index] = Some(Finished::Gone);
                    continue;
                }
                Err(SampleError::Transient(reason)) => {
                    losses[index].failed_reads += 1;
                    losses[index].note(reason);
                }
            }
            if attributing {
                spending.inside_ns[index] += monotonic_ns().saturating_sub(entered_ns);
            }

            if paces[index].snapshot {
                finished[index] = Some(Finished::Snapshot);
                continue;
            }
            // When the next reading was due, and whether this one got in the
            // way of it. A round that overran is counted as a late tick -- at
            // that point the series no longer has the spacing it claims -- and
            // the schedule is re-based on now, so that being late once does not
            // leave every reading after it owing time it can never make up.
            let interval_ns = paces[index].interval.as_nanos() as u64;
            let finished_ns = monotonic_ns();
            let took_ns = finished_ns.saturating_sub(due_ns[index]);
            // An interval of zero is "as fast as this can be read", which is
            // not a cadence and cannot be fallen behind. Counting every round
            // of such a run as late is how a snapshot served as a poll came to
            // report 158 losses that had not happened.
            if interval_ns > 0 && took_ns > interval_ns {
                losses[index].late_tick(took_ns, interval_ns);
                due_ns[index] = finished_ns;
            } else {
                due_ns[index] += interval_ns;
            }
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
            let Slot::Attached(mut collector) = std::mem::replace(&mut slots[index], Slot::Missed)
            else {
                unreachable!("checked above");
            };
            let mut trailing = collector.stop();
            events[index].append(&mut trailing);
            losses[index].absorb(collector.losses());
            let record = Detached {
                value_attr: collector.value_attr(),
                stacks: collector.stacks(),
                notes: collector.notes(),
            };
            drop(collector);
            let closed_ns = monotonic_ns();
            watched[index].closed_ns = closed_ns;
            spending.detached(index, closed_ns);
            closed_because[index] = Some(format!(
                "its window closed: detached {:.3} s into the run, as asked",
                into_run(closed_ns)
            ));
            slots[index] = Slot::Detached(record);
            // A collection whose target had already gone keeps saying so.
            finished[index].get_or_insert(Finished::Window);
        }

        let waiting = slots.iter().any(|slot| matches!(slot, Slot::Waiting(_)));
        if !waiting && finished.iter().all(Option::is_some) {
            let every = |kind: Finished| finished.iter().all(|done| *done == Some(kind));
            if every(Finished::Gone) {
                break "every target of the run went away".to_owned();
            }
            if finished.contains(&Some(Finished::Snapshot)) {
                break "snapshot: a single read".to_owned();
            }
            break "every request's window had closed".to_owned();
        }

        if let Some(reason) = ended() {
            break reason;
        }

        // Asked after the stop condition, so a run that was going to end anyway
        // ends for the reason it was going to end for rather than being
        // condemned on its last tick.
        if let Some(reason) = spending.check(slots, &mut events, &mut losses) {
            break reason;
        }

        // Sleep until the next collector falls due, or the next window opens
        // or closes. A collector already behind its own schedule puts that
        // moment in the past, and the run goes straight round again.
        let next_due_ns = (0..count)
            .filter_map(|index| match &slots[index] {
                Slot::Waiting(_) => Some(opened_ns + plans[index].attach_ns),
                Slot::Attached(_) if finished[index].is_none() => {
                    Some(detach_at(index).map_or(due_ns[index], |at| due_ns[index].min(at)))
                }
                Slot::Attached(_) => detach_at(index),
                Slot::Detached(_) | Slot::Missed => None,
            })
            .min();
        if let Some(next_due_ns) = next_due_ns {
            let remaining_ns = next_due_ns.saturating_sub(monotonic_ns());
            if remaining_ns > 0 {
                sleep_or_wake(remaining_ns, wake);
            }
        }
    };

    Driven {
        events,
        losses,
        watched,
        closed_because,
        stopped_because,
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
        tv_sec: (ns / 1_000_000_000) as libc::time_t,
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
            spending,
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
            backend: None,
            label: None,
            window: None,
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
            backend: backend.map(|id| id.parse().unwrap()),
            label: None,
            window: None,
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
        let resolved = resolved_with_filter("io.latency", Some("bpf:tp"), "bytes > 4096");
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
            backend: None,
            label: None,
            window: None,
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
            source: "gpu.temp".parse().unwrap(),
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
            backend: None,
            label: None,
            window: None,
        };
        let resolved = crate::resolve::resolve(&catalog, &capabilities, &request).unwrap();
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
            emit: vec![crate::request::Emit::TopK(4)],
            keys: Vec::new(),
            filter: None,
            duration: Some("50ms".parse().unwrap()),
            budget: None,
            stack: None,
            backend: Some("force:fs:proc".parse().unwrap()),
            label: None,
            window: None,
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
            })
        );
        assert!(!losses.is_clean());
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
            let deadline_ns = monotonic_ns() + 200_000_000;
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
            backend: None,
            label: None,
            window: None,
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
            backend: None,
            label: None,
            window: window.map(|text| text.parse().unwrap()),
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
            &mut spending,
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
            &mut spending,
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
            &mut spending,
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
            &mut spending,
            until(deadline_ns),
        );
        assert_eq!(driven.losses[1].dropped, 7);
        assert_eq!(driven.losses[0].dropped, 0, "and nobody else's");
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
            &mut spending,
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
            Ok(()) => panic!("a snapshot with a stop was accepted"),
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
}
