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
pub mod counter_file;
pub mod kernel;
pub mod poll;
pub mod psi;

use std::any::Any;
use std::collections::BTreeMap;
use std::os::fd::RawFd;
use std::time::Duration as StdDuration;

use serde::{Deserialize, Serialize};

use crate::catalog::BackendKind;
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

mod ask;
mod drive;
mod outcome;
pub(crate) mod overhead;
mod recording;
mod run;
mod spending;

pub use self::ask::*;
use self::drive::*;
pub use self::outcome::*;
pub use self::overhead::*;
pub use self::recording::*;
pub use self::run::*;
use self::spending::*;

/// One source of events.
pub trait Collector {
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

    /// The budget acts once a collection is over its ceiling by the slack,
    /// not before: 7.3 ms over at the first check of a waiting program is not
    /// over yet; 10.9 ms is (two of the five runs that used to stop).
    #[test]
    fn a_collection_over_its_ceiling_by_less_than_the_slack_is_not_over_yet() {
        let at = |spent_ns: u64| Overhead {
            attributed_cpu_ns: spent_ns,
            observed_cpu_ns: Some(20_000_000),
            in_target_cpu_ns: Some(0),
            ..Overhead::default()
        };
        // 10 % of 20 ms is 2 ms: 9.3 ms spent is 7.3 ms over, 12.9 ms is 10.9.
        let under = at(9_300_000).excess_ns(10.0).unwrap();
        let over = at(12_900_000).excess_ns(10.0).unwrap();
        assert!((under - 7_300_000.0).abs() < 1.0, "{under}");
        assert!(under < BUDGET_SLACK_NS as f64 && over >= BUDGET_SLACK_NS as f64);
        assert_eq!(Overhead::default().excess_ns(10.0), None);
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

        match Run::new(&resolved).go().map(|ran| ran.collections) {
            Err(CollectError::MixedTargets { .. }) => {}
            other => panic!(
                "expected a refusal, got {:?}",
                other.map(|_| "a collection")
            ),
        }
    }

    /// A collector does not name its events: whatever it wrote, the runner
    /// names each after the registry's pair for the request it serves, so the
    /// provenance in a results document is the pair `omn list` shows.
    #[test]
    fn every_event_handed_over_is_named_after_the_pair_it_was_built_for() {
        let serving = resolved_with_filter("mem.rss", None, "comm == 'omn'");
        let mut events = vec![Event {
            ts_ns: 1,
            ts_err_ns: 0,
            kind: "mem.rss".to_owned(),
            keys: Vec::new(),
            attrs: Vec::new(),
            collector_id: CollectorId::new("a name the collector made up"),
            seq: 0,
        }];
        let mut losses = Losses::default();

        into_our_ids(Some(&serving), &mut events, &mut losses);

        assert_eq!(
            events[0].collector_id.as_str(),
            format!("mem.rss/{}", serving.backend.id)
        );
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
        match Run::new(std::slice::from_ref(&resolved))
            .go()
            .map(|mut ran| ran.collections.remove(0))
        {
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
        struct Streaming(Option<StdDuration>);
        impl Collector for Streaming {
            fn value_attr(&self) -> &'static str {
                "count"
            }
            fn sample(&mut self, _: u64) -> Result<Vec<Event>, SampleError> {
                Ok(Vec::new())
            }
            fn tick(&self) -> Option<StdDuration> {
                self.0
            }
        }
        let polling = Streaming(None);
        let streaming = Streaming(Some(StdDuration::from_millis(10)));

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
        spin_ns: u64,
        spun_ns: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl Collector for Burning {
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
        reads_ns: std::rc::Rc<std::cell::RefCell<Vec<u64>>>,
        stopped_ns: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl Collector for Clocked {
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
        reads_left: u32,
        reads: std::rc::Rc<std::cell::Cell<u32>>,
    }

    impl Collector for Vanishing {
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
                reads_left: 2,
                reads: reads.clone(),
            }) as Box<dyn Collector>
        };
        let workload_reads = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let clocked = || {
            Box::new(Clocked {
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
        _name: &str,
    ) -> (
        Box<dyn Collector>,
        std::rc::Rc<std::cell::RefCell<Vec<u64>>>,
    ) {
        let reads_ns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let collector = Box::new(Clocked {
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

    /// A sole failed request must wait for the workload, rather than closing
    /// the run immediately and causing the owner to kill its launched child.
    #[test]
    fn a_sole_failed_late_attach_waits_for_the_workload() {
        let mut slots = vec![Slot::Waiting(Box::new(Handing {
            collector: Err("mmap: Operation not permitted".to_owned()),
            spin_ns: 0,
            at_exec: false,
        }))];
        let mut paces = vec![Pace::UNSET];
        let plans = [Plan::of(Some("10ms..".parse().unwrap()))];
        let mut spending =
            Spending::weighed_against_this_process(vec![windowed(Some("10ms.."), poll_ms(10))]);
        let deadline = spending.opened_ns + 60_000_000;
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            &[true],
            None,
            true,
            &mut spending,
            None,
            None,
            until(deadline),
        );
        assert_eq!(driven.stopped_because, "the deadline");
        assert!(monotonic_ns() >= deadline);
        assert!(!driven.watched[0].attached);
        assert!(driven.closed_because[0]
            .as_ref()
            .unwrap()
            .contains("attach failed: mmap"));
        assert_eq!(driven.losses[0].failed_reads, 1);
        assert!(driven.failures[0]
            .as_ref()
            .unwrap()
            .contains("could not attach"));
    }

    #[test]
    fn a_failed_late_attach_preserves_the_launched_programs_natural_exit() {
        let path =
            std::env::temp_dir().join(format!("omn-late-failed-workload-{}", std::process::id()));
        let argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "sleep .12; printf complete > \"$1\"; exit 7".to_owned(),
            "sh".to_owned(),
            path.display().to_string(),
        ];
        let mut child = launch::gated(&argv, &[]).unwrap();
        let mut slots = vec![Slot::Waiting(Box::new(Handing {
            collector: Err("mmap: Operation not permitted".to_owned()),
            spin_ns: 0,
            at_exec: false,
        }))];
        let mut paces = vec![Pace::UNSET];
        let plans = [Plan::of(Some("10ms..".parse().unwrap()))];
        let mut spending =
            Spending::weighed_against_this_process(vec![windowed(Some("10ms.."), poll_ms(10))]);
        let wake = child.exit_fd();
        child.release();
        let safety = monotonic_ns() + 5_000_000_000;
        let driven = drive(
            &mut slots,
            &mut paces,
            &plans,
            &[true],
            wake,
            wake.is_none(),
            &mut spending,
            None,
            None,
            || {
                child
                    .finished()
                    .or_else(|| (monotonic_ns() >= safety).then(|| "test timed out".to_owned()))
            },
        );
        assert_eq!(driven.stopped_because, "exit status 7");
        assert!(!child.lifetime().unwrap().terminated);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "complete");
        assert!(driven.failures[0]
            .as_ref()
            .unwrap()
            .contains("could not attach"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_run_with_no_attached_request_fails_after_the_program_exits() {
        let mut resolved = windowed(Some("10s.."), poll_ms(10));
        resolved.request.duration = None;
        resolved.request.target = Target::Launch(vec!["/bin/true".to_owned()]);
        let ran = Run::new(&[resolved]).go().unwrap();
        let collection = &ran.collections[0];
        assert!(!collection.window.attached);
        assert!(collection
            .failure
            .as_ref()
            .unwrap()
            .contains("no request attached"));
        assert_eq!(collection.lifetime.unwrap().exit_code, Some(0));
        assert!(!collection.lifetime.unwrap().terminated);
        assert!(!crate::command::failures(&ran.collections).is_empty());
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
        struct Lossy;
        impl Collector for Lossy {
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
                collector: Ok(Box::new(Lossy)),
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
            dropped: u64,
            reads: u64,
            tick: Option<StdDuration>,
        }
        impl Collector for Dropping {
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
        let dropping = |_name: &str, tick| {
            Box::new(Dropping {
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
        let collections = Run::new(&[whole, late])
            .go()
            .map(|ran| ran.collections)
            .expect("the run completes");
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
        _name: &str,
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
        opened_ns: u64,
        /// Spans, in ms after `opened_ns`, where the level is high.
        high_ms: Vec<(u64, u64)>,
        reads: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl Collector for Level {
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
                collector_id: CollectorId::unstamped(),
                seq,
            }])
        }
    }

    /// A collector whose read takes a known wall time, and emits nothing.
    struct Stalling {
        stall: StdDuration,
    }

    impl Collector for Stalling {
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
                opened_ns: monotonic_ns(),
                high_ms: Vec::new(),
                reads: reads.clone(),
            }),
            Box::new(Stalling {
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
