//! Collectors: where events come from.
//!
//! A collector turns one resolved backend into a stream of events. It knows its
//! backend and nothing else -- not the emit, not the output format, not the
//! other collectors. Everything above it works on `Event`, which is what makes
//! adding a source an additive change.
//!
//! Loss accounting is part of the interface rather than an afterthought. A
//! collection that loses three percent of its events without saying so produces
//! wrong numbers that look right, and by the time anyone notices, the run is
//! gone. So a `Collection` always carries how many reads failed and why, and the
//! count travels all the way to the user-visible output.
//!
//! Only `mem.rss` is implemented so far. That is the whole point of the current
//! milestone: one source, taken from the request all the way to the rendered
//! number, before any breadth.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration as StdDuration;

use serde::{Deserialize, Serialize};

use crate::clock::monotonic_ns;
use crate::event::{Attr, CollectorId, Event, KeyValue};
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
    /// Events the transport dropped. Always zero for a polling backend, which
    /// has no queue to overflow; meaningful once ring buffers arrive.
    pub dropped: u64,
    /// Reads that returned nothing.
    pub failed_reads: u64,
    /// Ticks that started late enough to shift the series.
    pub late_ticks: u64,
    /// The reasons behind the counts above, with how often each occurred.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reasons: BTreeMap<String, u64>,
}

impl Losses {
    pub fn is_clean(&self) -> bool {
        self.dropped == 0 && self.failed_reads == 0 && self.late_ticks == 0
    }

    fn note(&mut self, reason: impl Into<String>) {
        *self.reasons.entry(reason.into()).or_insert(0) += 1;
    }
}

/// One source of events.
pub trait Collector {
    fn id(&self) -> &CollectorId;

    /// The attribute that holds this collector's measured value.
    fn value_attr(&self) -> &'static str;

    /// Produces the events observable right now.
    ///
    /// An error is a failed read, not the end of the run: a process that
    /// vanished mid-poll ends the collection, a transient read failure is
    /// counted and the run goes on.
    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError>;
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
    if !has_collector(request.source.as_str(), &resolved.backend.id) {
        return Err(CollectError::NoCollector {
            source_id: request.source.to_string(),
            backend: resolved.backend.id.clone(),
        });
    }

    // A launched program is both the target and the clock: the run ends when it
    // does, which is what makes `omn mem.rss -- ./prog` a complete measurement
    // rather than a sample of one.
    let mut child = match &request.target {
        Target::Launch(argv) => Some(launch(argv)?),
        _ => None,
    };
    let pid = match (&request.target, &child) {
        (_, Some(child)) => child.id() as i32,
        (Target::Pid(pids) | Target::Tid(pids), _) => pids[0],
        (target, _) => {
            return Err(CollectError::UnsupportedTarget {
                source_id: request.source.to_string(),
                target: target.kind().to_string(),
            })
        }
    };

    let mut collector = build(resolved, pid)?;

    let interval = match request.mode {
        Mode::Poll { interval } => StdDuration::from_nanos(interval.as_nanos()),
        Mode::Snapshot => StdDuration::ZERO,
        _ => StdDuration::from_nanos(Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
    };
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
            match child.try_wait() {
                Ok(Some(status)) => {
                    stopped_because = format!("the launched program exited with {status}");
                    break;
                }
                Ok(None) => {}
                Err(error) => {
                    stopped_because = format!("cannot wait on the launched program: {error}");
                    break;
                }
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
                    "a read took {spent_ns}ns, longer than the {}ns poll interval",
                    interval.as_nanos()
                ));
            }
        }
    }

    // A launched program that outlived the duration is stopped: the tool does
    // not leave processes behind that it started.
    if let Some(child) = child.as_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    Ok(Collection {
        observed_ns: monotonic_ns().saturating_sub(started_ns),
        value_attr: collector.value_attr().to_owned(),
        events,
        losses,
        stopped_because,
    })
}

fn launch(argv: &[String]) -> Result<Child, CollectError> {
    let (program, arguments) = argv.split_first().ok_or_else(|| CollectError::Launch {
        program: "<empty>".into(),
        error: "no program given".into(),
    })?;
    Command::new(program)
        .args(arguments)
        .spawn()
        .map_err(|error| CollectError::Launch {
            program: program.clone(),
            error: error.to_string(),
        })
}

/// Finds the collector for a resolved backend.
///
/// The lookup is on the backend id, not on the source alone: two backends of the
/// same source read different files and produce differently trustworthy numbers.
fn has_collector(source_id: &str, backend: &str) -> bool {
    matches!(
        (source_id, backend),
        ("mem.rss", "fs:proc:smaps_rollup") | ("mem.rss", "fs:proc:statm")
    )
}

fn build(resolved: &ResolvedRequest, pid: i32) -> Result<Box<dyn Collector>, CollectError> {
    let source_id = resolved.request.source.as_str();
    match (source_id, resolved.backend.id.as_str()) {
        ("mem.rss", "fs:proc:smaps_rollup") => {
            Ok(Box::new(RssCollector::new(pid, RssSource::SmapsRollup)))
        }
        ("mem.rss", "fs:proc:statm") => Ok(Box::new(RssCollector::new(pid, RssSource::Statm))),
        _ => Err(CollectError::NoCollector {
            source_id: source_id.to_owned(),
            backend: resolved.backend.id.clone(),
        }),
    }
}

/// Which file the resident size is read from.
///
/// They do not agree to the byte: `smaps_rollup` sums the VMAs the kernel walks
/// at that instant, `statm` reports a counter maintained on fault. The file is
/// recorded on every event so a series is never silently a mix of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RssSource {
    SmapsRollup,
    Statm,
}

impl RssSource {
    fn path(self, pid: i32) -> PathBuf {
        match self {
            RssSource::SmapsRollup => PathBuf::from(format!("/proc/{pid}/smaps_rollup")),
            RssSource::Statm => PathBuf::from(format!("/proc/{pid}/statm")),
        }
    }

    fn label(self) -> &'static str {
        match self {
            RssSource::SmapsRollup => "smaps_rollup",
            RssSource::Statm => "statm",
        }
    }
}

/// Reads the resident set size of one process.
struct RssCollector {
    id: CollectorId,
    pid: i32,
    source: RssSource,
    path: PathBuf,
    page_size: u64,
}

impl RssCollector {
    fn new(pid: i32, source: RssSource) -> Self {
        Self {
            id: CollectorId::new(format!("mem.rss/fs:proc:{}", source.label())),
            pid,
            source,
            path: source.path(pid),
            page_size: page_size(),
        }
    }

    fn read_bytes(&self) -> Result<u64, SampleError> {
        let text = std::fs::read_to_string(&self.path).map_err(|error| {
            // A process that dies between two reads is the end of the run, not
            // a lost sample. procfs reports it as ENOENT when the directory is
            // already gone and as ESRCH when it is going: counting either as a
            // failed read would make every clean run look lossy.
            let gone = error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH);
            if gone {
                SampleError::TargetGone(format!("process {} exited", self.pid))
            } else if error.kind() == std::io::ErrorKind::PermissionDenied {
                SampleError::TargetGone(format!(
                    "{} is no longer readable: {error}",
                    self.path.display()
                ))
            } else {
                SampleError::Transient(format!("{}: {error}", self.path.display()))
            }
        })?;

        match self.source {
            // `Rss:   12345 kB`
            RssSource::SmapsRollup => text
                .lines()
                .find_map(|line| line.strip_prefix("Rss:"))
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|kilobytes| kilobytes * 1024)
                .ok_or_else(|| {
                    SampleError::Transient(format!("no Rss line in {}", self.path.display()))
                }),
            // `size resident shared text lib data dt`, in pages.
            RssSource::Statm => text
                .split_whitespace()
                .nth(1)
                .and_then(|value| value.parse::<u64>().ok())
                .map(|pages| pages * self.page_size)
                .ok_or_else(|| {
                    SampleError::Transient(format!("cannot parse {}", self.path.display()))
                }),
        }
    }
}

impl Collector for RssCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        // The timestamp is taken after the read, so it bounds the read rather
        // than preceding it: the value existed at or before this instant.
        let bytes = self.read_bytes()?;
        let ts_ns = monotonic_ns();
        Ok(vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.rss".to_owned(),
            keys: vec![KeyValue::task(self.pid)],
            attrs: vec![
                Attr::uint("bytes", bytes),
                Attr::text("read_from", self.source.label()),
            ],
            collector_id: self.id.clone(),
            seq,
        }])
    }
}

fn page_size() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one; no memory is touched.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as u64
    } else {
        4096
    }
}

/// The sources a collector exists for, for error messages that say what *is*
/// possible rather than only what is not.
pub fn implemented() -> Vec<&'static str> {
    vec!["mem.rss (fs:proc:smaps_rollup, fs:proc:statm)"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_backends_read_a_plausible_and_similar_resident_size() {
        let pid = std::process::id() as i32;
        let mut rollup = RssCollector::new(pid, RssSource::SmapsRollup);
        let mut statm = RssCollector::new(pid, RssSource::Statm);

        let from_statm = statm
            .sample(0)
            .expect("statm is always readable for our own process");
        let statm_bytes = from_statm[0].attr("bytes").unwrap().as_f64().unwrap();
        assert!(
            statm_bytes > 512.0 * 1024.0,
            "a running process holds more than 512 KiB"
        );

        // `smaps_rollup` may be restricted; when it reads, it must broadly agree.
        if let Ok(from_rollup) = rollup.sample(0) {
            let rollup_bytes = from_rollup[0].attr("bytes").unwrap().as_f64().unwrap();
            let ratio = rollup_bytes / statm_bytes;
            assert!(
                (0.5..2.0).contains(&ratio),
                "smaps_rollup said {rollup_bytes} and statm said {statm_bytes}"
            );
        }
    }

    #[test]
    fn every_event_records_which_file_it_came_from() {
        let mut collector = RssCollector::new(std::process::id() as i32, RssSource::Statm);
        let events = collector.sample(3).unwrap();
        assert_eq!(events[0].attr("read_from").unwrap().to_string(), "statm");
        assert_eq!(events[0].seq, 3);
        assert!(events[0].ts_ns > 0);
    }

    #[test]
    fn a_process_that_does_not_exist_ends_the_run_rather_than_failing_a_read() {
        // pid 0 never names a readable process directory.
        let mut collector = RssCollector::new(0, RssSource::Statm);
        match collector.sample(0) {
            Err(SampleError::TargetGone(_)) => {}
            other => panic!("expected the run to end, got {other:?}"),
        }
    }
}
