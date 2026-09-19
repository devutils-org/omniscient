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

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use serde::{Deserialize, Serialize};

use crate::clock::monotonic_ns;
use crate::event::{CollectorId, Event};
use crate::request::{Mode, Target};
use crate::resolve::{FilterPlacement, ResolvedRequest};

pub use mem::heap::find_shim;

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
    Start { source_id: String, reason: String },
    #[error(
        "one run observes one target, but this session asks for `{first}` and `{second}`; \
         use one session per target"
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
        *self.reasons.entry(reason.into()).or_insert(0) += 1;
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
        for (reason, count) in other.reasons {
            *self.reasons.entry(reason).or_insert(0) += count;
        }
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

    /// Set when this collector can only observe a program the tool launches,
    /// with the reason, so the refusal explains itself.
    fn requires_launch(&self) -> Option<&'static str> {
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
pub fn run_all(resolved: &[ResolvedRequest]) -> Result<Vec<Collection>, CollectError> {
    assert!(!resolved.is_empty(), "a run needs at least one request");
    let target = &resolved[0].request.target;
    if let Some(other) = resolved
        .iter()
        .find(|candidate| &candidate.request.target != target)
    {
        return Err(CollectError::MixedTargets {
            first: target.to_string(),
            second: other.request.target.to_string(),
        });
    }

    // Asked first, because "no collector exists for this backend" is a
    // different answer from "this target is not wired up yet", and reporting
    // the second when the first is true sends the reader to the wrong place.
    let prepared: Vec<Box<dyn Prepared>> =
        resolved.iter().map(prepare).collect::<Result<_, _>>()?;

    let environment = merge_environments(&prepared)?;

    // A collector that can only be given to a program at exec is asked about
    // next, and before the stop condition, because the two refusals send the
    // reader to different places: "pass --duration" is advice for a request
    // that can work, and this one cannot. Answering the shallower question
    // first sent a caller off to add a flag that would change nothing.
    if !matches!(target, Target::Launch(_)) {
        for (index, prepared) in prepared.iter().enumerate() {
            if let Some(reason) = prepared.requires_launch() {
                return Err(CollectError::UnsupportedTarget {
                    source_id: resolved[index].request.source.to_string(),
                    target: format!("{} ({reason})", target.kind()),
                });
            }
        }
    }

    // A run that nothing will ever stop is refused before the target is
    // launched or anything is attached to it -- but after the collectors have
    // been asked for, because "no collector is written for this" is the deeper
    // answer and the one worth giving first. A duration, a program whose exit
    // ends the run, or a single read: with none of the three there is no
    // window, and observing a target that nothing will stop watching is worse
    // than refusing.
    let duration_ns = resolved
        .iter()
        .filter_map(|resolved| resolved.request.duration)
        .map(|duration| duration.as_nanos())
        .max();
    let snapshot = resolved
        .iter()
        .all(|resolved| resolved.request.mode == Mode::Snapshot);
    if duration_ns.is_none() && !matches!(target, Target::Launch(_)) && !snapshot {
        return Err(CollectError::NoStopCondition);
    }

    // A launched program is both the target and the clock: the run ends when it
    // does, which is what makes `omn <source> -- ./prog` a complete measurement
    // rather than a sample of one. It is held at a gate so that collectors are
    // attached before its first instruction; otherwise the interesting part of
    // a short program is over before anything is watching.
    let mut child = match target {
        Target::Launch(argv) => {
            Some(
                launch::gated(argv, &environment).map_err(|error| CollectError::Launch {
                    program: argv.first().cloned().unwrap_or_default(),
                    error: error.to_string(),
                })?,
            )
        }
        _ => None,
    };

    // A launched program replaces the target's own identity: the collectors
    // observe the process that was just created, not a pid the request named.
    // A target that names no process -- an interface, the machine -- is passed
    // through, and a collector that needs a pid refuses it by name.
    let bound = match &child {
        Some(child) => Target::Pid(vec![child.pid()]),
        None => target.clone(),
    };

    let mut collectors = Vec::with_capacity(prepared.len());
    for (index, prepared) in prepared.into_iter().enumerate() {
        let source_id = resolved[index].request.source.to_string();
        let mut collector = prepared
            .attach(&bound)
            .map_err(|reason| CollectError::Start {
                source_id: source_id.clone(),
                reason,
            })?;
        if let Err(reason) = collector.start() {
            if let Some(child) = child.as_mut() {
                child.terminate();
            }
            return Err(CollectError::Start { source_id, reason });
        }
        collectors.push(collector);
    }

    // Everything is attached; let the program run.
    if let Some(child) = child.as_mut() {
        child.release();
    }

    // The fastest collector sets the pace: draining a ring every second loses
    // events, and the cheap pollers do not mind being asked more often.
    let interval = collectors
        .iter()
        .filter_map(|collector| collector.tick())
        .min()
        .unwrap_or_else(|| match resolved[0].request.mode {
            Mode::Poll { interval } => StdDuration::from_nanos(interval.as_nanos()),
            Mode::Snapshot => StdDuration::ZERO,
            _ => StdDuration::from_nanos(Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
        });

    let deadline_ns = duration_ns.map(|longest| monotonic_ns() + longest);

    let started_ns = monotonic_ns();
    let (mut events, mut losses, stopped_because) =
        drive(&mut collectors, Window { interval, snapshot }, || {
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
        });

    // Whatever the kernel or the shim published between the last tick and the
    // stop still belongs to this run.
    for (index, collector) in collectors.iter_mut().enumerate() {
        let mut trailing = collector.stop();
        events[index].append(&mut trailing);
        let collector_losses = collector.losses();
        losses[index].absorb(collector_losses);
    }

    // A launched program that outlived the duration is stopped: the tool does
    // not leave behind processes it started.
    if let Some(child) = child.as_mut() {
        if child.finished().is_none() {
            child.terminate();
        }
    }

    let observed_ns = monotonic_ns().saturating_sub(started_ns);
    Ok(collectors
        .iter()
        .enumerate()
        .map(|(index, collector)| {
            let mut collected = std::mem::take(&mut events[index]);
            collected.sort_by_key(|event| event.ts_ns);
            let filtered = apply_filter(&resolved[index], &mut collected);
            Collection {
                observed_ns,
                value_attr: collector.value_attr().to_owned(),
                events: collected,
                losses: std::mem::take(&mut losses[index]),
                stopped_because: stopped_because.clone(),
                notes: collector.notes(),
                filtered,
            }
        })
        .collect())
}

/// The pace of a run and what a single read means, as the runner sees them.
struct Window {
    interval: StdDuration,
    /// A single read, after which the run is over: there is no series and no
    /// second round.
    snapshot: bool,
}

/// Drives a set of collectors over one window, and closes it.
///
/// The order inside a round is the whole of the edge convention: **read
/// first, then decide**. A round reads every collector, and only then asks
/// whether the run is over, so the last round of reads happens at the moment
/// the run ends rather than one interval before it. That is what makes a
/// polled counter's series complete without any collector reading again in
/// `stop`, and `the_runner_closes_the_window` measures how little time is
/// left over for such a read to cover.
///
/// `ended` is consulted once per round and returns why the run should stop.
/// It is a closure rather than a launched child and a deadline so that the
/// convention can be tested against a stop condition a test controls.
fn drive(
    collectors: &mut [Box<dyn Collector>],
    window: Window,
    mut ended: impl FnMut() -> Option<String>,
) -> (Vec<Vec<Event>>, Vec<Losses>, String) {
    let mut events: Vec<Vec<Event>> = vec![Vec::new(); collectors.len()];
    let mut losses: Vec<Losses> = vec![Losses::default(); collectors.len()];
    let mut sequences = vec![0u64; collectors.len()];

    let stopped_because = loop {
        let tick_started_ns = monotonic_ns();
        let mut gone = None;
        for (index, collector) in collectors.iter_mut().enumerate() {
            match collector.sample(sequences[index]) {
                Ok(mut sampled) => {
                    sequences[index] += sampled.len() as u64;
                    events[index].append(&mut sampled);
                }
                Err(SampleError::TargetGone(reason)) => gone = Some(reason),
                Err(SampleError::Transient(reason)) => {
                    losses[index].failed_reads += 1;
                    losses[index].note(reason);
                }
            }
        }
        if let Some(reason) = gone {
            break reason;
        }

        if window.snapshot {
            break "snapshot: a single read".to_owned();
        }

        if let Some(reason) = ended() {
            break reason;
        }

        // Sleep for what is left of the interval, and count the tick as late
        // when the reads themselves took longer than the interval: at that
        // point the series no longer has the spacing it claims.
        let spent_ns = monotonic_ns().saturating_sub(tick_started_ns);
        match window
            .interval
            .checked_sub(StdDuration::from_nanos(spent_ns))
        {
            Some(remaining) => std::thread::sleep(remaining),
            None => {
                for losses in losses.iter_mut() {
                    losses.late_ticks += 1;
                    losses.note(format!(
                        "a round of reads took {spent_ns}ns, longer than the {}ns tick",
                        window.interval.as_nanos()
                    ));
                }
            }
        }
    };

    (events, losses, stopped_because)
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
fn merge_environments(
    prepared: &[Box<dyn Prepared>],
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
    (factory.build)(resolved).map_err(|reason| CollectError::Start {
        source_id: source_id.to_owned(),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let merged = merge_environments(&agreeing).expect("identical values agree");
        assert_eq!(merged.len(), 1, "the variable is set once, not twice");

        let conflicting: Vec<Box<dyn Prepared>> = vec![
            Box::new(Wants("LD_PRELOAD", "/lib/a.so")),
            Box::new(Wants("LD_PRELOAD", "/lib/b.so")),
        ];
        match merge_environments(&conflicting) {
            Err(CollectError::ConflictingEnvironment { variable, .. }) => {
                assert_eq!(variable, "LD_PRELOAD");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// One run observes one target. Running two targets in sequence would
    /// measure two different moments and present them as one window, and a join
    /// across that would correlate a program with something that was not
    /// happening at the same time.
    #[test]
    fn a_session_asking_for_two_targets_is_refused_before_anything_starts() {
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
        let (_, _, stopped_because) = drive(
            &mut collectors,
            Window {
                interval,
                snapshot: false,
            },
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
}
