//! `mem.wss` over `/proc/[pid]/clear_refs` and `Referenced:`.
//!
//! Resident size says how much memory a process *has*. The working set says how
//! much of it the process actually uses, and the gap between the two is what
//! decides whether adding memory to a machine will change anything: a service
//! holding four gigabytes and touching two hundred megabytes of them every
//! second is not short of memory, however large its RSS.
//!
//! There is no counter for this. The kernel tracks a referenced bit per page
//! for its own reclaim decisions, and the only way to read a working set out of
//! it is to **clear the bit, wait, and see which pages got it back**. That is
//! what this collector does, once per published point: read `Referenced:` out
//! of `smaps_rollup` -- the bytes referenced since the last clear -- and then
//! clear again to open the next window.
//!
//! **This measurement changes what it measures, in two ways that have to be
//! said rather than discovered.** Clearing the referenced bit is process-wide
//! and visible to anything else looking: a second reader of the same process
//! sees windows it did not open. And the bit is the kernel's own input to
//! reclaim, so clearing it makes every page of the process look cold for as
//! long as it takes the process to touch them again; under memory pressure
//! that is a measurement that causes eviction. Both are in the backend's
//! declared cost and in the source's caveats.
//!
//! A snapshot is refused. A working set is a quantity per unit time -- "what is
//! touched in a second" -- and a single read observes no window at all, so
//! there is nothing it could honestly report.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::cadence::Cadence;
use crate::clock::monotonic_ns;
use crate::collect::task::Comm;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

/// Holds the mode until the target exists.
pub struct Prepare {
    interval: std::time::Duration,
}

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { interval } => Ok(Self {
                interval: std::time::Duration::from_nanos(interval.as_nanos()),
            }),
            other => Err(format!(
                "a working set is what gets touched over a window, and `{}` observes none: this \
                 backend clears the kernel's referenced bits and reads back which pages got them \
                 again, so it needs two instants. Poll it.",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "the referenced bits are cleared and read per process, and a {} target names \
                 none: a working set for a cgroup or a machine is what DAMON measures, which is \
                 a different backend",
                target.kind()
            )
        })?;
        let collector = WorkingSet::new(pid, self.interval);
        // Asked before the program is released rather than at the first tick,
        // so that a process this tool may not write to is refused with the
        // reason instead of producing an empty series.
        if let Err(error) = std::fs::OpenOptions::new()
            .write(true)
            .open(collector.clear_refs())
        {
            return Err(format!(
                "cannot write {}: {error}. The referenced bits of another user's process cannot \
                 be cleared, and without clearing them there is no window to measure.",
                collector.clear_refs().display()
            ));
        }
        Ok(Box::new(collector))
    }
}

/// Measures one process's working set by clearing and re-reading.
pub struct WorkingSet {
    id: CollectorId,
    pid: i32,
    rollup: PathBuf,
    clear_refs: PathBuf,
    comm: Comm,
    cadence: Cadence,
    /// When the bits were last cleared, which is when the window now being
    /// measured opened. `None` until the first clear.
    cleared_at_ns: Option<u64>,
    losses: Losses,
}

impl WorkingSet {
    pub fn new(pid: i32, interval: std::time::Duration) -> Self {
        Self {
            id: CollectorId::new("mem.wss/fs:proc:clear_refs"),
            pid,
            rollup: PathBuf::from(format!("/proc/{pid}/smaps_rollup")),
            clear_refs: PathBuf::from(format!("/proc/{pid}/clear_refs")),
            comm: Comm::of(pid),
            cadence: Cadence::new(interval),
            cleared_at_ns: None,
            losses: Losses::default(),
        }
    }

    fn clear_refs(&self) -> &Path {
        &self.clear_refs
    }

    /// Clears the referenced bit of every page of the process, opening a new
    /// window.
    ///
    /// `1` is the referenced bit. `4` would clear the soft-dirty bit, which is
    /// a different measurement and would leave this one reading a bit nothing
    /// cleared.
    fn clear(&mut self) -> Result<u64, SampleError> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.clear_refs)
            .map_err(|error| self.gone_or_transient(&self.clear_refs, error))?;
        file.write_all(b"1\n")
            .map_err(|error| self.gone_or_transient(&self.clear_refs, error))?;
        Ok(monotonic_ns())
    }

    /// Bytes of this process referenced since the last clear, and the resident
    /// set they are a part of.
    fn read(&self) -> Result<(u64, Option<u64>), SampleError> {
        let text = std::fs::read_to_string(&self.rollup)
            .map_err(|error| self.gone_or_transient(&self.rollup, error))?;
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|kilobytes| kilobytes * 1024)
        };
        let referenced = field("Referenced:").ok_or_else(|| {
            SampleError::Transient(format!(
                "{} holds no `Referenced:` line, so this kernel does not publish the bit this \
                 backend clears",
                self.rollup.display()
            ))
        })?;
        Ok((referenced, field("Rss:")))
    }

    fn gone_or_transient(&self, path: &Path, error: std::io::Error) -> SampleError {
        let gone = error.kind() == std::io::ErrorKind::NotFound
            || error.raw_os_error() == Some(libc::ESRCH);
        if gone {
            SampleError::TargetGone(format!("process {} exited", self.pid))
        } else {
            SampleError::Transient(format!("{}: {error}", path.display()))
        }
    }
}

impl Collector for WorkingSet {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    /// The first window opens before the observed program runs, so that the
    /// first published point covers the program's own first interval rather
    /// than whatever the process did between exec and the first tick.
    fn start(&mut self) -> Result<(), String> {
        match self.clear() {
            Ok(at) => {
                self.cleared_at_ns = Some(at);
                Ok(())
            }
            Err(error) => Err(format!("{error:?}")),
        }
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if !self.cadence.due(monotonic_ns()) {
            return Ok(Vec::new());
        }
        let Some(opened_at_ns) = self.cleared_at_ns else {
            // `start` did not run, which is only the case in a test that drives
            // the collector directly. Open the window now and publish nothing:
            // a window of zero length has no working set.
            self.cleared_at_ns = Some(self.clear()?);
            return Ok(Vec::new());
        };
        let (referenced, resident) = self.read()?;
        let ts_ns = monotonic_ns();
        let interval_ns = ts_ns.saturating_sub(opened_at_ns);
        self.cleared_at_ns = Some(self.clear()?);

        if interval_ns == 0 {
            self.losses.undecoded += 1;
            self.losses.note(
                "the referenced bits were cleared and read back in the same nanosecond, so there \
                 is no window this working set belongs to"
                    .to_owned(),
            );
            return Ok(Vec::new());
        }

        let mut attrs = vec![
            Attr::uint("bytes", referenced),
            Attr::uint("interval_ns", interval_ns),
            Attr::text("read_from", "smaps_rollup:Referenced"),
            Attr::text(
                "basis",
                "pages that got their referenced bit back since this collector cleared it",
            ),
        ];
        // The resident set is carried for scale and is not part of the number:
        // the working set is the fraction of it that is live, and a reader
        // needs both to know whether that fraction is small.
        if let Some(resident) = resident {
            attrs.push(Attr::uint("resident_bytes", resident));
        }
        if let Some(comm) = self.comm.get() {
            attrs.push(Attr::text("comm", comm));
        }

        Ok(vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.wss".to_owned(),
            keys: vec![KeyValue::task(self.pid)],
            attrs,
            collector_id: self.id.clone(),
            seq,
        }])
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        vec![format!(
            "the referenced bits of process {} were cleared once per published point: another \
             reader of this process saw windows it did not open, and the kernel's reclaim saw \
             every page of it look cold",
            self.pid
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::mem::testing::Toucher;
    use std::time::Duration;

    fn poll(interval: Duration) -> Mode {
        Mode::Poll {
            interval: crate::scalar::Duration::from_nanos(interval.as_nanos() as u64),
        }
    }

    /// A single read observes no window, and a working set over no window is
    /// not a smaller number -- it is not a number.
    #[test]
    fn a_snapshot_is_refused_because_it_observes_no_window() {
        let refused = Prepare::new(Mode::Snapshot)
            .err()
            .expect("a snapshot cannot measure a working set");
        assert!(refused.contains("observes none"), "{refused}");
    }

    #[test]
    fn a_target_that_names_no_process_is_refused_by_name() {
        let refused = Box::new(Prepare::new(poll(Duration::from_millis(10))).expect("poll works"))
            .attach(&Target::System)
            .err()
            .expect("clear_refs is per process");
        assert!(refused.contains("DAMON"), "{refused}");
    }

    /// The first point covers a window, not the instant the run started.
    #[test]
    fn nothing_is_published_before_a_window_has_been_opened() {
        let mut collector = WorkingSet::new(std::process::id() as i32, Duration::ZERO);
        assert!(
            collector
                .sample(0)
                .expect("our own clear_refs is writable")
                .is_empty(),
            "the first tick opens the window and measures nothing"
        );
        assert!(collector.cleared_at_ns.is_some());
    }

    /// Ground truth, and the whole reason the source exists: a process that
    /// *holds* a lot of memory and *touches* very little of it must report a
    /// working set that is the small number, not the large one.
    ///
    /// The child maps 256 MiB and writes every page of it, so its resident set
    /// is 256 MiB and the kernel's referenced bits are set across all of it.
    /// It then spends the rest of its life touching only the first 16 MiB. The
    /// collector clears the bits, waits, and reads back: what comes back has to
    /// be the 16 MiB that is being used, while the resident set it reports
    /// beside it is still the 256 MiB that is merely held.
    ///
    /// Nothing here is the collector's own arithmetic. The two numbers are
    /// chosen by the test, maintained by the kernel in two different places,
    /// and differ by a factor of sixteen.
    #[test]
    fn a_process_that_holds_much_and_touches_little_reports_the_little() {
        const RESIDENT: u64 = 256 * 1024 * 1024;
        const WORKING: u64 = 16 * 1024 * 1024;

        let mut child = Toucher::walker(RESIDENT, WORKING);
        let mut collector = WorkingSet::new(child.pid(), Duration::ZERO);
        if collector.start().is_err() {
            child.stop();
            eprintln!(
                "skipped: this kernel does not let a process clear another's referenced \
                       bits"
            );
            return;
        }

        // Two whole walks of the working set after the bits were cleared --
        // the child counts them -- so every page of it has been touched in
        // this window however the child was scheduled (the flaky tally's #23,
        // which slept 300 ms instead). The rest it never touches again.
        child.wait_for_walks(2);
        let events = collector.sample(1).expect("the child is still running");
        // The kernel's own account of what the child holds, read beside the
        // sample: memory pressure from a loaded machine may have taken some of
        // it, and the resident set is compared against that, not a constant.
        let held = std::fs::read_to_string(format!("/proc/{}/status", child.pid()))
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("VmRSS:"))
                    .and_then(|rest| rest.split_whitespace().next()?.parse::<f64>().ok())
            })
            .map(|kilobytes| kilobytes * 1024.0)
            .expect("the child's VmRSS");
        child.stop();

        let event = events.first().expect("a window was open");
        let working = event.attr("bytes").unwrap().as_f64().unwrap();
        let resident = event
            .attr("resident_bytes")
            .and_then(|value| value.as_f64())
            .expect("the resident set is carried for scale");

        assert!(
            (resident - held).abs() <= 0.1 * held,
            "the kernel says the child holds {held} bytes and its resident set reads {resident}"
        );
        assert!(
            held >= 0.5 * RESIDENT as f64,
            "the child was reclaimed down to {held} of its {RESIDENT} bytes, so it no longer holds \
             much and touches little; nothing here measures that"
        );
        assert!(
            working >= 0.5 * WORKING as f64,
            "the child touches {WORKING} bytes every few milliseconds and the working set reads \
             {working}"
        );
        assert!(
            working < 0.25 * resident,
            "the child touches a sixteenth of what it holds, and the working set came back as \
             {working} of {resident} resident"
        );
    }

    /// The perturbation is not a footnote: a run says it happened.
    #[test]
    fn the_run_says_that_the_referenced_bits_were_cleared() {
        let collector = WorkingSet::new(std::process::id() as i32, Duration::ZERO);
        let notes = collector.notes().join(" ");
        assert!(notes.contains("cleared"), "{notes}");
    }
}
