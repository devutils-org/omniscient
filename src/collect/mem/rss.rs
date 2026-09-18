//! `mem.rss` over procfs.
//!
//! Resident size is read from a file, so this collector polls: there is no
//! event to wait for, only a counter to look at. Which file it reads is
//! recorded on every event, because `smaps_rollup` and `statm` do not agree to
//! the byte -- one sums the VMAs the kernel walks at that instant, the other
//! reports a counter maintained on fault -- and a series that silently mixed
//! them would look like noise.

use std::path::PathBuf;

use crate::collect::{Collector, Prepared, SampleError};
use crate::clock::monotonic_ns;
use crate::event::{Attr, CollectorId, Event, KeyValue};

/// Which file the resident size is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RssSource {
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

/// Holds the choice of file until the target exists.
pub struct Prepare(pub RssSource);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, pid: i32) -> Result<Box<dyn Collector>, String> {
        Ok(Box::new(RssCollector::new(pid, self.0)))
    }
}

/// Reads the resident set size of one process.
pub struct RssCollector {
    id: CollectorId,
    pid: i32,
    source: RssSource,
    path: PathBuf,
    page_size: u64,
}

impl RssCollector {
    pub fn new(pid: i32, source: RssSource) -> Self {
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
