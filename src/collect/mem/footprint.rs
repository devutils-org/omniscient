//! `mem.rss`, `mem.pss` and `mem.uss` over procfs.
//!
//! Three numbers, one file, and the difference between them is the whole point
//! of the family's cardinal rule. Resident size counts a shared page in full in
//! every process that maps it, proportional size divides it by the number of
//! mappers, and private size counts only what dies with the process. Summing
//! the resident size of every process on a machine therefore double-counts
//! libc; summing the proportional size does not. A tool that answered "memory"
//! with one of the three would be right for one question and wrong for the
//! other two, so the metric is named in the request and carried on every event.
//!
//! These are files, so this collector polls: there is no event to wait for,
//! only a counter to look at. Which file it read is recorded on every event,
//! because `smaps_rollup`, `smaps` and `statm` do not agree to the byte -- the
//! first two sum the VMAs the kernel walks at that instant, the last reports a
//! counter maintained on fault -- and a series that silently mixed them would
//! look like noise.

use std::path::PathBuf;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Prepared, SampleError};
use crate::request::Target;
use crate::event::{Attr, CollectorId, Event, KeyValue};

/// Which footprint is being measured.
///
/// Never inferred from anything: a request names its accounting metric, and the
/// validator refuses one that does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Resident: every page in RAM, shared ones counted in full.
    Rss,
    /// Proportional: each shared page divided by the number of processes
    /// mapping it.
    Pss,
    /// Private: what would be freed if the process exited now.
    Uss,
}

impl Metric {
    fn source_id(self) -> &'static str {
        match self {
            Metric::Rss => "mem.rss",
            Metric::Pss => "mem.pss",
            Metric::Uss => "mem.uss",
        }
    }

    /// The `smaps` fields that add up to this metric.
    ///
    /// Unique shared size is the kernel's two private counters added together;
    /// there is no `Uss:` line, which is exactly why the tool that reports it
    /// has to say where it came from.
    fn fields(self) -> &'static [&'static str] {
        match self {
            Metric::Rss => &["Rss:"],
            Metric::Pss => &["Pss:"],
            Metric::Uss => &["Private_Clean:", "Private_Dirty:"],
        }
    }
}

/// Which file the number is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// One pre-summed block, computed by the kernel over every mapping.
    SmapsRollup,
    /// Every mapping, summed here. The same numbers at a higher price.
    Smaps,
    /// Seven counters, resident size among them, and nothing else.
    Statm,
}

impl Source {
    fn path(self, pid: i32) -> PathBuf {
        match self {
            Source::SmapsRollup => PathBuf::from(format!("/proc/{pid}/smaps_rollup")),
            Source::Smaps => PathBuf::from(format!("/proc/{pid}/smaps")),
            Source::Statm => PathBuf::from(format!("/proc/{pid}/statm")),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Source::SmapsRollup => "smaps_rollup",
            Source::Smaps => "smaps",
            Source::Statm => "statm",
        }
    }
}

/// Holds the choice of metric and file until the target exists.
pub struct Prepare(pub Metric, pub Source);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "a footprint is read per process, and a {} target names none",
                target.kind()
            )
        })?;
        Ok(Box::new(FootprintCollector::new(pid, self.0, self.1)))
    }
}

/// Reads one footprint of one process.
pub struct FootprintCollector {
    id: CollectorId,
    pid: i32,
    metric: Metric,
    source: Source,
    path: PathBuf,
    page_size: u64,
}

impl FootprintCollector {
    pub fn new(pid: i32, metric: Metric, source: Source) -> Self {
        Self {
            id: CollectorId::new(format!("{}/fs:proc:{}", metric.source_id(), source.label())),
            pid,
            metric,
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
            // `size resident shared text lib data dt`, in pages. Resident size
            // and nothing else, which is why it is the fallback and not the
            // first choice.
            Source::Statm => text
                .split_whitespace()
                .nth(1)
                .and_then(|value| value.parse::<u64>().ok())
                .map(|pages| pages * self.page_size)
                .ok_or_else(|| {
                    SampleError::Transient(format!("cannot parse {}", self.path.display()))
                }),
            // `Rss:   12345 kB`, once in the rollup and once per mapping in
            // `smaps`. Summing both the same way is what makes the two backends
            // comparable.
            Source::SmapsRollup | Source::Smaps => sum_fields(&text, self.metric.fields())
                .ok_or_else(|| {
                    SampleError::Transient(format!(
                        "no {} line in {}",
                        self.metric.fields().join(" or "),
                        self.path.display()
                    ))
                }),
        }
    }
}

impl Collector for FootprintCollector {
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
            kind: self.metric.source_id().to_owned(),
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

/// Adds up every occurrence of the named `smaps` fields, in bytes.
///
/// Returns `None` when not one of them appears, which means the file is not
/// what this collector thinks it is -- a kernel without `smaps_rollup`
/// support answers with an empty block rather than an error.
fn sum_fields(text: &str, fields: &[&str]) -> Option<u64> {
    let mut total = 0u64;
    let mut seen = false;
    for line in text.lines() {
        for field in fields {
            let Some(rest) = line.strip_prefix(field) else {
                continue;
            };
            // `Pss:` must not match `Pss_Dirty:`; the prefix ends at the colon,
            // so what follows is whitespace and then the number.
            let Some(kilobytes) = rest
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok())
            else {
                continue;
            };
            total += kilobytes * 1024;
            seen = true;
        }
    }
    seen.then_some(total)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn read(metric: Metric, source: Source) -> Option<f64> {
        let mut collector = FootprintCollector::new(std::process::id() as i32, metric, source);
        let events = collector.sample(0).ok()?;
        events[0].attr("bytes").unwrap().as_f64()
    }

    #[test]
    fn both_backends_read_a_plausible_and_similar_resident_size() {
        let statm = read(Metric::Rss, Source::Statm).expect("statm is always readable for us");
        assert!(
            statm > 512.0 * 1024.0,
            "a running process holds more than 512 KiB"
        );

        // `smaps_rollup` may be restricted; when it reads, it must broadly agree.
        if let Some(rollup) = read(Metric::Rss, Source::SmapsRollup) {
            let ratio = rollup / statm;
            assert!(
                (0.5..2.0).contains(&ratio),
                "smaps_rollup said {rollup} and statm said {statm}"
            );
        }
    }

    /// The rollup is the kernel's own sum over the same mappings this walks, so
    /// the two must agree -- but they cannot be read at the same instant, and
    /// reading them faults in pages of their own. So the walk is bracketed
    /// between a rollup taken before it and one taken after: it has to land
    /// inside what the kernel itself reported on either side.
    ///
    /// The first measurement of this disagreed by 20 %, and the cause was the
    /// reads: the first read of each file grows this process by a few hundred
    /// kilobytes. Ordering, not arithmetic.
    #[test]
    fn the_rollup_agrees_with_the_per_mapping_walk() {
        // Reading each file once first, so that the pages the reads themselves
        // need are already resident when the comparison starts.
        for metric in [Metric::Rss, Metric::Pss, Metric::Uss] {
            for source in [Source::SmapsRollup, Source::Smaps] {
                let _ = read(metric, source);
            }
        }

        // Other threads of this test binary keep allocating while this runs,
        // and shared pages come and go in other processes, which moves PSS.
        const SLACK: f64 = 512.0 * 1024.0;

        for metric in [Metric::Rss, Metric::Pss, Metric::Uss] {
            let (Some(before), Some(walked), Some(after)) = (
                read(metric, Source::SmapsRollup),
                read(metric, Source::Smaps),
                read(metric, Source::SmapsRollup),
            ) else {
                continue; // `smaps_rollup` is restricted here; nothing to check.
            };
            let low = before.min(after) - SLACK;
            let high = before.max(after) + SLACK;
            assert!(
                (low..=high).contains(&walked),
                "{metric:?}: the walk said {walked}, outside the {before}..{after} the rollup \
                 reported around it"
            );
        }
    }

    /// Private memory is a subset of proportional memory, which is a subset of
    /// resident memory. The inequality holds by construction in the kernel, so
    /// a violation here means this code picked up the wrong lines.
    ///
    /// The three numbers come out of one read of the file. Reading it three
    /// times would compare three different instants of a process that grows
    /// while it reads, and the ordering would break for a reason that has
    /// nothing to do with the metrics.
    #[test]
    fn the_three_metrics_stay_in_their_order() {
        let path = Source::SmapsRollup.path(std::process::id() as i32);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return; // `smaps_rollup` is restricted here; nothing to check.
        };
        let read = |metric: Metric| sum_fields(&text, metric.fields()).expect("a rollup block");
        let (rss, pss, uss) = (read(Metric::Rss), read(Metric::Pss), read(Metric::Uss));
        assert!(uss <= pss, "uss {uss} exceeds pss {pss}");
        assert!(pss <= rss, "pss {pss} exceeds rss {rss}");
    }

    #[test]
    fn a_prefix_does_not_match_a_longer_field_name() {
        let text = "Pss:  100 kB\nPss_Dirty:  40 kB\nPss_Anon:  40 kB\n";
        assert_eq!(sum_fields(text, &["Pss:"]), Some(100 * 1024));
        assert_eq!(sum_fields(text, &["Rss:"]), None);
    }

    #[test]
    fn unique_size_is_the_two_private_counters_added_up() {
        let text = "Private_Clean:  48 kB\nPrivate_Dirty:  108 kB\n";
        assert_eq!(sum_fields(text, Metric::Uss.fields()), Some(156 * 1024));
    }

    #[test]
    fn every_event_records_which_file_it_came_from() {
        let mut collector =
            FootprintCollector::new(std::process::id() as i32, Metric::Rss, Source::Statm);
        let events = collector.sample(3).unwrap();
        assert_eq!(events[0].attr("read_from").unwrap().to_string(), "statm");
        assert_eq!(events[0].kind, "mem.rss");
        assert_eq!(events[0].seq, 3);
        assert!(events[0].ts_ns > 0);
    }

    #[test]
    fn a_process_that_does_not_exist_ends_the_run_rather_than_failing_a_read() {
        // pid 0 never names a readable process directory.
        let mut collector = FootprintCollector::new(0, Metric::Rss, Source::Statm);
        match collector.sample(0) {
            Err(SampleError::TargetGone(_)) => {}
            other => panic!("expected the run to end, got {other:?}"),
        }
    }
}
