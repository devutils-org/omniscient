//! `proc.fd` and `proc.threads` over procfs.
//!
//! Two counts a process keeps about itself, and both are only meaningful
//! against something else. A thread count is compared against what the program
//! was designed to run; an open-descriptor count means nothing at all without
//! the limit it is approaching, which is why this collector reads both and
//! reports the headroom alongside.
//!
//! Unlike most of this tool, these two are exactly checkable: open ten files
//! and the count rises by ten. That makes them the cheapest honest ground truth
//! in the catalog, and the tests take it.

use std::path::{Path, PathBuf};

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Target;

/// Which of the two counts is being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counted {
    Descriptors,
    Threads,
}

impl Counted {
    fn source(self) -> &'static str {
        match self {
            Counted::Descriptors => "proc.fd",
            Counted::Threads => "proc.threads",
        }
    }

    fn value_attr(self) -> &'static str {
        match self {
            Counted::Descriptors => "open",
            Counted::Threads => "threads",
        }
    }
}

pub struct Prepare(pub Counted);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "these counts belong to a process, and a {} target names none",
                target.kind()
            )
        })?;
        Ok(Box::new(StatusCollector {
            id: CollectorId::new(format!("{}/fs:proc", self.0.source())),
            counted: self.0,
            pid,
            losses: Losses::default(),
        }))
    }
}

pub struct StatusCollector {
    id: CollectorId,
    counted: Counted,
    pid: i32,
    losses: Losses,
}

/// Turns a procfs read error into the right kind of outcome.
///
/// A process that exits mid-run ends the collection; it has not failed. procfs
/// says so with ENOENT once the directory is gone and ESRCH while it is going,
/// and counting either as a lost read would make every clean run look lossy.
fn classify(path: &Path, pid: i32, error: &std::io::Error) -> SampleError {
    if error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) {
        SampleError::TargetGone(format!("process {pid} exited"))
    } else if error.kind() == std::io::ErrorKind::PermissionDenied {
        SampleError::TargetGone(format!("{} is not readable: {error}", path.display()))
    } else {
        SampleError::Transient(format!("{}: {error}", path.display()))
    }
}

impl StatusCollector {
    /// Counts the entries of `/proc/<pid>/fd`.
    ///
    /// There is no cheaper way: `FDSize` in `status` is the size of the
    /// allocated table rounded up to a power of two, not the number in use, and
    /// reporting it as the count would overstate by up to a factor of two. So
    /// the directory is walked, which costs one entry's work per open
    /// descriptor -- worth stating for a process holding tens of thousands.
    fn descriptors(&self) -> Result<u64, SampleError> {
        let path = PathBuf::from(format!("/proc/{}/fd", self.pid));
        let entries =
            std::fs::read_dir(&path).map_err(|error| classify(&path, self.pid, &error))?;
        let counted = entries.filter_map(Result::ok).count() as u64;

        // The handle opened to walk the directory is itself a descriptor, but
        // only of *this* process. Reading another process's table, it is not in
        // the listing and subtracting it would report one fewer than the
        // process holds; reading our own, it is, and not subtracting it would
        // report one more. Which applies depends on who is being measured.
        if self.pid == std::process::id() as i32 {
            Ok(counted.saturating_sub(1))
        } else {
            Ok(counted)
        }
    }

    /// The soft limit on open descriptors, from `/proc/<pid>/limits`.
    fn descriptor_limit(&self) -> Option<u64> {
        let path = format!("/proc/{}/limits", self.pid);
        let text = std::fs::read_to_string(path).ok()?;
        let line = text
            .lines()
            .find(|line| line.starts_with("Max open files"))?;
        // `Max open files   1024   524288   files`
        line.split_whitespace()
            .rev()
            .nth(2)
            .and_then(|value| value.parse().ok())
    }

    fn threads(&self) -> Result<u64, SampleError> {
        let path = PathBuf::from(format!("/proc/{}/status", self.pid));
        let text =
            std::fs::read_to_string(&path).map_err(|error| classify(&path, self.pid, &error))?;
        text.lines()
            .find_map(|line| line.strip_prefix("Threads:"))
            .and_then(|value| value.trim().parse().ok())
            .ok_or_else(|| SampleError::Transient(format!("no Threads line in {}", path.display())))
    }
}

impl Collector for StatusCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        self.counted.value_attr()
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut attrs = Vec::new();
        match self.counted {
            Counted::Descriptors => {
                let open = self.descriptors()?;
                attrs.push(Attr::uint("open", open));
                // A count of open descriptors is not a measurement on its own:
                // 900 is fine against a limit of 524288 and an outage against a
                // limit of 1024. The limit travels with the count.
                match self.descriptor_limit() {
                    Some(limit) => {
                        attrs.push(Attr::uint("limit", limit));
                        attrs.push(Attr::uint("headroom", limit.saturating_sub(open)));
                    }
                    None => {
                        self.losses.note(format!(
                            "the descriptor limit of process {} could not be read, so the count \
                             has nothing to be measured against",
                            self.pid
                        ));
                    }
                }
            }
            Counted::Threads => attrs.push(Attr::uint("threads", self.threads()?)),
        }

        // Taken after the read, so it bounds it: the value existed at or before
        // this instant.
        let ts_ns = monotonic_ns();
        Ok(vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: self.counted.source().to_owned(),
            keys: vec![KeyValue::task(self.pid)],
            attrs,
            collector_id: self.id.clone(),
            seq,
        }])
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collector(counted: Counted) -> Box<dyn Collector> {
        Box::new(Prepare(counted))
            .attach(&Target::Pid(vec![std::process::id() as i32]))
            .expect("our own process")
    }

    fn value(collector: &mut Box<dyn Collector>, name: &str) -> u64 {
        collector.sample(0).expect("readable")[0]
            .attr(name)
            .and_then(|value| value.as_f64())
            .expect("the attribute is present") as u64
    }

    // The exact ground truth for both counts lives in `tests/family_proc.rs`,
    // against a launched process. It cannot live here: `cargo test` runs tests
    // in parallel threads of one process, so other tests open files and start
    // threads while these read the counts, and an exact delta is not available
    // to a test that shares its process with the rest of the suite.

    #[test]
    fn the_limit_travels_with_the_count() {
        let mut collector = collector(Counted::Descriptors);
        let events = collector.sample(0).unwrap();
        let open = events[0].attr("open").unwrap().as_f64().unwrap();
        let limit = events[0]
            .attr("limit")
            .and_then(|value| value.as_f64())
            .expect("a count of descriptors without its limit means nothing");
        assert!(limit > open, "the process is already at its limit?");
        assert_eq!(
            events[0].attr("headroom").unwrap().as_f64().unwrap(),
            limit - open
        );
    }

    #[test]
    fn the_thread_count_is_at_least_the_threads_this_test_can_see() {
        let mut collector = collector(Counted::Threads);
        // A weak claim, deliberately: the exact one belongs to a process this
        // test does not share. What it does catch is a parser returning zero,
        // or the wrong line of `status`.
        assert!(
            value(&mut collector, "threads") >= 1,
            "a running process has at least one thread"
        );
    }

    #[test]
    fn a_process_that_does_not_exist_ends_the_run() {
        let mut collector = Box::new(Prepare(Counted::Threads))
            .attach(&Target::Pid(vec![0]))
            .expect("attaching does not read");
        match collector.sample(0) {
            Err(SampleError::TargetGone(_)) => {}
            other => panic!("expected the run to end, got {other:?}"),
        }
    }

    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let Err(error) = Box::new(Prepare(Counted::Threads)).attach(&Target::System) else {
            panic!("these counts belong to a process");
        };
        assert!(error.contains("names none"), "{error}");
    }
}
