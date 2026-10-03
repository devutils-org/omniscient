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
    ///
    /// Read twice when the first answer is ESRCH from a process that is still
    /// running. `smaps_rollup` is bound at its open to the address space the
    /// process had then, and an `execve` between the open and the read leaves
    /// it bound to one that no longer exists: the read answers ESRCH although
    /// the process is alive and has a new address space to measure. A
    /// launched program is in `execve` right when the run's first reads are
    /// taken -- the gate releases it as the window opens -- and a read that
    /// straddled it ended the run as *"process exited"* with nothing
    /// measured. A fresh open is bound to the new address space.
    fn read(&self) -> Result<(u64, Option<u64>), SampleError> {
        let text = self.rollup_text(|| std::fs::read_to_string(&self.rollup))?;
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

    /// The rollup's text, from `open_and_read` (a fresh open each call):
    /// once, and once more when the first answer is ESRCH from a process
    /// that is still running. A second failure is the first's verdict:
    /// transient from a running process, the end from a gone one.
    fn rollup_text(
        &self,
        open_and_read: impl Fn() -> std::io::Result<String>,
    ) -> Result<String, SampleError> {
        match open_and_read() {
            Ok(text) => Ok(text),
            Err(error) if replaced_under_the_read(&error, self.pid) => {
                open_and_read().map_err(|error| self.gone_or_transient(&self.rollup, error))
            }
            Err(error) => Err(self.gone_or_transient(&self.rollup, error)),
        }
    }

    /// The end of the run only when the process is gone; an ESRCH from one
    /// that is still running is a read that failed, counted as one.
    fn gone_or_transient(&self, path: &Path, error: std::io::Error) -> SampleError {
        let gone = error.kind() == std::io::ErrorKind::NotFound
            || (error.raw_os_error() == Some(libc::ESRCH) && !still_running(self.pid));
        if gone {
            SampleError::TargetGone(format!("process {} exited", self.pid))
        } else {
            SampleError::Transient(format!("{}: {error}", path.display()))
        }
    }
}

/// Whether `error` is the ESRCH of a file opened on an address space the
/// process has since replaced, rather than of a process that has gone.
fn replaced_under_the_read(error: &std::io::Error, pid: i32) -> bool {
    error.raw_os_error() == Some(libc::ESRCH) && still_running(pid)
}

/// Whether `pid` is a process with an address space: present in procfs and
/// neither a zombie nor dead. A zombie's `smaps_rollup` answers ESRCH too,
/// and that one is the end of the run.
fn still_running(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state is the first field after the command, which is in
    // parentheses and may itself hold spaces and parentheses.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|state| !matches!(state, "Z" | "X" | "x"))
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
    /// The child maps 1 GiB and writes every page of it, so its resident set
    /// is 1 GiB and the kernel's referenced bits are set across all of it.
    /// It then spends the rest of its life touching only the first 128 MiB.
    /// The collector clears the bits, waits, and reads back: what comes back
    /// has to be the 128 MiB that is being used, while the resident set it
    /// reports beside it is still the 1 GiB that is merely held.
    ///
    /// Nothing here is the collector's own arithmetic. The two numbers are
    /// chosen by the test, maintained by the kernel in two different places,
    /// and differ by a factor of eight.
    ///
    /// The working set is larger than any TLB can map, on purpose. A clear
    /// does not flush the TLB, and a page whose translation the TLB still
    /// holds is touched without a page-table walk, so its bit stays clear
    /// (the source's first caveat). With 16 MiB of small pages -- 4 096 of
    /// them -- the CI's runner read 8 351 744 bytes, half (run 37155975223,
    /// `defaults`): its cpu kept about 2 000 of the translations across the
    /// clear. Here, a Ryzen 9 3900X read 64 small pages back as 2 %, 1 024
    /// as 37 to 99 %, and 4 096 and more in full, the same walk each time.
    /// 32 768 pages are eight times a 4 096-entry second-level TLB, and more
    /// than twice one that coalesces four pages per entry.
    #[test]
    fn a_process_that_holds_much_and_touches_little_reports_the_little() {
        const RESIDENT: u64 = 1024 * 1024 * 1024;
        const WORKING: u64 = 128 * 1024 * 1024;

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
            "the child touches an eighth of what it holds, and the working set came back as \
             {working} of {resident} resident"
        );
    }

    /// The kernel fact the second read rests on, shown rather than assumed: a
    /// `smaps_rollup` opened before an `execve` and read after it answers
    /// ESRCH from a process that is still running, and a fresh open reads the
    /// new address space. Which way the collector takes that ESRCH is the
    /// next tests'. The straddling read is then handed to the collector as
    /// its first answer, so the second read is the collector's own.
    #[test]
    fn a_rollup_opened_across_an_exec_answers_esrch_from_a_running_process() {
        use std::io::Read;

        let program = std::ffi::CString::new("/bin/sleep").unwrap();
        let seconds = std::ffi::CString::new("5").unwrap();
        let argv = [program.as_ptr(), seconds.as_ptr(), std::ptr::null()];
        let mut gate = [0i32; 2];
        // SAFETY: two descriptors into an array this test owns.
        assert_eq!(unsafe { libc::pipe(gate.as_mut_ptr()) }, 0);
        // SAFETY: the child reads, execs and exits, all async-signal-safe.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork must succeed");
        if pid == 0 {
            // SAFETY: the child's own descriptors and the argv built above.
            unsafe {
                let mut go = 0u8;
                libc::read(gate[0], (&mut go as *mut u8).cast(), 1);
                libc::execv(program.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }
        let rollup = format!("/proc/{pid}/smaps_rollup");
        let mut early = std::fs::File::open(&rollup).expect("the child's rollup opens");
        // SAFETY: one byte to the gate this test made.
        unsafe { libc::write(gate[1], b"x".as_ptr().cast(), 1) };
        let exe = std::path::PathBuf::from("/bin/sleep")
            .canonicalize()
            .unwrap_or_else(|_| "/bin/sleep".into());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::fs::read_link(format!("/proc/{pid}/exe")).ok().as_ref() != Some(&exe) {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never became {}",
                exe.display()
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        let mut text = String::new();
        let straddled = early.read_to_string(&mut text);
        let straddled_errno = straddled.as_ref().err().and_then(|e| e.raw_os_error());
        let collector = WorkingSet::new(pid, Duration::ZERO);
        let calls = std::cell::Cell::new(0);
        let first = std::cell::RefCell::new(Some(straddled.map(|_| text.clone())));
        let outcome = collector
            .rollup_text(|| {
                calls.set(calls.get() + 1);
                match first.borrow_mut().take() {
                    Some(answer) => answer,
                    None => std::fs::read_to_string(&collector.rollup),
                }
            })
            .and_then(|_| collector.read());
        // SAFETY: ending and reaping a child of this test.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
            libc::close(gate[0]);
            libc::close(gate[1]);
        }

        assert_eq!(
            straddled_errno,
            Some(libc::ESRCH),
            "a rollup opened before the exec and read after it must answer ESRCH"
        );
        let (_, resident) = outcome.expect("the collector's second, fresh open reads");
        assert_eq!(calls.get(), 2, "the straddling ESRCH is read once more");
        assert!(resident.is_some_and(|bytes| bytes > 0), "{resident:?}");
    }

    /// ESRCH ends the run only from a process that is gone: from a running
    /// one it is a failed read, and from a zombie the end.
    #[test]
    fn an_esrch_ends_the_run_only_when_the_process_is_gone() {
        let esrch = || std::io::Error::from_raw_os_error(libc::ESRCH);
        let alive = WorkingSet::new(std::process::id() as i32, Duration::ZERO);
        assert!(
            matches!(
                alive.gone_or_transient(Path::new("x"), esrch()),
                SampleError::Transient(_)
            ),
            "this process is running"
        );

        // SAFETY: the child only exits.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork must succeed");
        if pid == 0 {
            // SAFETY: the only call the child makes.
            unsafe { libc::_exit(0) };
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while still_running(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never exited"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let zombie = WorkingSet::new(pid, Duration::ZERO);
        let verdict = zombie.gone_or_transient(Path::new("x"), esrch());
        // SAFETY: reaping a child of this test.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        assert!(
            matches!(verdict, SampleError::TargetGone(_)),
            "a zombie has no address space left: {verdict:?}"
        );
    }

    /// The second read, driven: an ESRCH from a running process is read once
    /// more and the second answer stands; an ESRCH that persists is a failed
    /// read, not the end, and is not read a third time; from a gone process
    /// it is the end at once, with no second read.
    #[test]
    fn an_esrch_from_a_running_process_is_read_once_more_and_never_ends_the_run() {
        let esrch = || std::io::Error::from_raw_os_error(libc::ESRCH);
        let alive = WorkingSet::new(std::process::id() as i32, Duration::ZERO);

        let calls = std::cell::Cell::new(0);
        let text = alive.rollup_text(|| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Err(esrch())
            } else {
                Ok("Rss: 4 kB\nReferenced: 4 kB\n".to_string())
            }
        });
        assert_eq!(
            calls.get(),
            2,
            "an ESRCH from a running process is read again"
        );
        assert!(text.is_ok_and(|t| t.contains("Referenced")));

        calls.set(0);
        let verdict = alive.rollup_text(|| {
            calls.set(calls.get() + 1);
            Err(esrch())
        });
        assert_eq!(calls.get(), 2, "read twice, not more");
        assert!(
            matches!(verdict, Err(SampleError::Transient(_))),
            "a persisting ESRCH from a running process is a failed read: {verdict:?}"
        );

        calls.set(0);
        let other = alive.rollup_text(|| {
            calls.set(calls.get() + 1);
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        });
        assert_eq!(calls.get(), 1, "only ESRCH is read again");
        assert!(matches!(other, Err(SampleError::Transient(_))), "{other:?}");

        // SAFETY: the child only exits.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork must succeed");
        if pid == 0 {
            // SAFETY: the only call the child makes.
            unsafe { libc::_exit(0) };
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while still_running(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never exited"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let zombie = WorkingSet::new(pid, Duration::ZERO);
        calls.set(0);
        let verdict = zombie.rollup_text(|| {
            calls.set(calls.get() + 1);
            Err(esrch())
        });
        // SAFETY: reaping a child of this test.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        assert_eq!(calls.get(), 1, "a zombie is not read again");
        assert!(
            matches!(verdict, Err(SampleError::TargetGone(_))),
            "{verdict:?}"
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
