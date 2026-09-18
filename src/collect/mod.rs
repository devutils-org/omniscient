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

pub mod heap;
mod launch;
mod page_faults;
mod proc_rss;

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use serde::{Deserialize, Serialize};

use crate::clock::monotonic_ns;
use crate::event::{CollectorId, Event};
use crate::request::{Mode, Target};
use crate::resolve::ResolvedRequest;

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
    /// The reasons behind the counts above, with how often each occurred.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reasons: BTreeMap<String, u64>,
}

impl Losses {
    pub fn is_clean(&self) -> bool {
        self.dropped == 0 && self.failed_reads == 0 && self.late_ticks == 0 && self.undecoded == 0
    }

    pub fn note(&mut self, reason: impl Into<String>) {
        *self.reasons.entry(reason.into()).or_insert(0) += 1;
    }

    fn absorb(&mut self, other: Losses) {
        self.dropped += other.dropped;
        self.failed_reads += other.failed_reads;
        self.late_ticks += other.late_ticks;
        self.undecoded += other.undecoded;
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
    fn attach(self: Box<Self>, pid: i32) -> Result<Box<dyn Collector>, String>;
}

/// What a single sample can go wrong with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleError {
    /// The thing being observed no longer exists. The run ends, cleanly.
    TargetGone(String),
    /// This read failed; another one may not.
    Transient(String),
}

/// Runs a resolved request to completion.
pub fn run(resolved: &ResolvedRequest) -> Result<Collection, CollectError> {
    let request = &resolved.request;

    // Asked first, because "no collector exists for this backend" is a
    // different answer from "this target is not wired up yet", and reporting
    // the second when the first is true sends the reader to the wrong place.
    let prepared = prepare(resolved)?;

    // A launched program is both the target and the clock: the run ends when it
    // does, which is what makes `omn <source> -- ./prog` a complete measurement
    // rather than a sample of one. It is held at a gate so that collectors are
    // attached before its first instruction; otherwise the interesting part of
    // a short program is over before anything is watching.
    let mut child = match &request.target {
        Target::Launch(argv) => Some(launch::gated(argv, &prepared.launch_environment()).map_err(
            |error| CollectError::Launch {
                program: argv.first().cloned().unwrap_or_default(),
                error: error.to_string(),
            },
        )?),
        _ => None,
    };

    if child.is_none() {
        if let Some(reason) = prepared.requires_launch() {
            return Err(CollectError::UnsupportedTarget {
                source_id: request.source.to_string(),
                target: format!("{} ({reason})", request.target.kind()),
            });
        }
    }

    let pid = match (&request.target, &child) {
        (_, Some(child)) => child.pid(),
        (Target::Pid(pids) | Target::Tid(pids), _) => pids[0],
        (target, _) => {
            return Err(CollectError::UnsupportedTarget {
                source_id: request.source.to_string(),
                target: target.kind().to_string(),
            })
        }
    };

    let mut collector = prepared.attach(pid).map_err(|reason| {
        if let Some(child) = child.as_mut() {
            child.terminate();
        }
        CollectError::Start {
            source_id: request.source.to_string(),
            reason,
        }
    })?;
    collector.start().map_err(|reason| {
        if let Some(child) = child.as_mut() {
            child.terminate();
        }
        CollectError::Start {
            source_id: request.source.to_string(),
            reason,
        }
    })?;

    // Everything is attached; let the program run.
    if let Some(child) = child.as_mut() {
        child.release();
    }

    let interval = collector.tick().unwrap_or_else(|| match request.mode {
        Mode::Poll { interval } => StdDuration::from_nanos(interval.as_nanos()),
        Mode::Snapshot => StdDuration::ZERO,
        _ => StdDuration::from_nanos(Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
    });
    let deadline_ns = request
        .duration
        .map(|duration| monotonic_ns() + duration.as_nanos());
    if deadline_ns.is_none() && child.is_none() && request.mode != Mode::Snapshot {
        return Err(CollectError::NoStopCondition);
    }

    let started_ns = monotonic_ns();
    let mut events = Vec::new();
    let mut losses = Losses::default();
    let mut seq = 0u64;
    let stopped_because;

    loop {
        let tick_started_ns = monotonic_ns();
        match collector.sample(seq) {
            Ok(mut sampled) => {
                seq += sampled.len() as u64;
                events.append(&mut sampled);
            }
            Err(SampleError::TargetGone(reason)) => {
                stopped_because = reason;
                break;
            }
            Err(SampleError::Transient(reason)) => {
                losses.failed_reads += 1;
                losses.note(reason);
            }
        }

        if request.mode == Mode::Snapshot {
            stopped_because = "snapshot: a single read".to_owned();
            break;
        }

        if let Some(child) = child.as_mut() {
            if let Some(status) = child.finished() {
                stopped_because = format!("the launched program exited with {status}");
                break;
            }
        }

        if let Some(deadline_ns) = deadline_ns {
            if monotonic_ns() >= deadline_ns {
                stopped_because = "the requested duration elapsed".to_owned();
                break;
            }
        }

        // Sleep for what is left of the interval, and count the tick as late
        // when the read itself took longer than the interval: at that point the
        // series no longer has the spacing it claims.
        let spent_ns = monotonic_ns().saturating_sub(tick_started_ns);
        match interval.checked_sub(StdDuration::from_nanos(spent_ns)) {
            Some(remaining) => std::thread::sleep(remaining),
            None => {
                losses.late_ticks += 1;
                losses.note(format!(
                    "a read took {spent_ns}ns, longer than the {}ns tick",
                    interval.as_nanos()
                ));
            }
        }
    }

    // Whatever the kernel published between the last tick and the stop still
    // belongs to this run.
    let mut trailing = collector.stop();
    events.append(&mut trailing);
    losses.absorb(collector.losses());

    // A launched program that outlived the duration is stopped: the tool does
    // not leave behind processes it started.
    if let Some(child) = child.as_mut() {
        if child.finished().is_none() {
            child.terminate();
        }
    }

    events.sort_by_key(|event| event.ts_ns);

    Ok(Collection {
        observed_ns: monotonic_ns().saturating_sub(started_ns),
        value_attr: collector.value_attr().to_owned(),
        events,
        losses,
        stopped_because,
    })
}

pub use heap::find_shim;

/// The sources a collector exists for, for error messages that say what *is*
/// possible rather than only what is not.
pub fn implemented() -> Vec<&'static str> {
    vec![
        "mem.rss (fs:proc:smaps_rollup, fs:proc:statm)",
        "mem.faults (pmu:sw)",
        "mem.heap.alloc (preload)",
    ]
}

/// Finds the collector for a resolved backend.
///
/// The lookup is on the backend id, not on the source alone: two backends of the
/// same source read different things and produce differently trustworthy
/// numbers.
fn prepare(resolved: &ResolvedRequest) -> Result<Box<dyn Prepared>, CollectError> {
    let source_id = resolved.request.source.as_str();
    let start_error = |reason: String| CollectError::Start {
        source_id: source_id.to_owned(),
        reason,
    };
    match (source_id, resolved.backend.id.as_str()) {
        ("mem.rss", "fs:proc:smaps_rollup") => Ok(Box::new(proc_rss::Prepare(
            proc_rss::RssSource::SmapsRollup,
        ))),
        ("mem.rss", "fs:proc:statm") => Ok(Box::new(proc_rss::Prepare(proc_rss::RssSource::Statm))),
        ("mem.faults", "pmu:sw") => Ok(Box::new(page_faults::Prepare(resolved.request.mode))),
        ("mem.heap.alloc", "preload") => {
            Ok(Box::new(heap::HeapPrepared::new().map_err(start_error)?))
        }
        _ => Err(CollectError::NoCollector {
            source_id: source_id.to_owned(),
            backend: resolved.backend.id.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::Capabilities;
    use crate::catalog::Catalog;

    /// `implemented` is written by hand for the error message; this keeps it
    /// from drifting away from what `prepare` actually knows how to build.
    #[test]
    fn every_source_listed_as_implemented_can_be_prepared() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        for listed in implemented() {
            let (source, backends) = listed.split_once(" (").expect("`source (backends)`");
            let entry = catalog
                .get(&source.parse().unwrap())
                .unwrap_or_else(|| panic!("{source} is not in the catalog"));
            for backend in backends.trim_end_matches(')').split(", ") {
                assert!(
                    entry.backend(backend).is_some(),
                    "{source} has no backend `{backend}`"
                );
                let mut request = crate::request::Request {
                    source: source.parse().unwrap(),
                    target: crate::request::Target::Launch(vec!["/bin/true".to_owned()]),
                    mode: crate::request::Mode::Trace,
                    emit: vec![crate::request::Emit::Raw],
                    keys: Vec::new(),
                    filter: None,
                    duration: None,
                    budget: None,
                    stack: None,
                    backend: Some(backend.parse().unwrap()),
                    label: None,
                };
                if !entry.modes.contains(&crate::request::ModeKind::Trace) {
                    request.mode = crate::request::Mode::Poll {
                        interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
                    };
                }
                let resolved = crate::resolve::resolve(&catalog, &capabilities, &request)
                    .unwrap_or_else(|error| panic!("{source}/{backend}: {error}"));
                match prepare(&resolved) {
                    Ok(_) => {}
                    Err(error) => {
                        panic!("{source}/{backend} is listed but cannot be prepared: {error}")
                    }
                }
            }
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
        second.note("a");
        second.note("b");

        first.absorb(second);
        assert_eq!((first.dropped, first.failed_reads), (5, 1));
        assert_eq!(first.reasons.get("a"), Some(&2));
        assert_eq!(first.reasons.get("b"), Some(&1));
        assert!(!first.is_clean());
    }
}
