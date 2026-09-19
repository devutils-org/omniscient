//! Properties of the observed task that several families report and none owns.
//!
//! `comm` and `pid` are declared as filter fields by more than half the
//! catalog, across families whose mechanisms have nothing else in common: a
//! `perf_event_open` counter, a procfs read and an eBPF program all observe a
//! task, and all three can say what it is called. Before this, each family that
//! wanted them read `/proc` for itself or -- far more often -- did not, and a
//! predicate over `comm` emptied the series and looked exactly like a program
//! that did nothing.
//!
//! What a filter field must not be is optional. `filter.rs` reports a field no
//! event carried, so the failure is visible rather than silent, but a visible
//! failure is still a promise the catalog made and the build did not keep.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a name is trusted before it is read again.
///
/// A task can rename itself with `prctl(PR_SET_NAME)`, and a thread pool that
/// labels its workers does it routinely, so the name is not read once and kept.
/// It is not read per event either: a streaming collector emits thousands per
/// second and each would cost an open, a read and a close of the same file for
/// a string that changes at most a few times in a run.
const FRESH_FOR: Duration = Duration::from_millis(100);

/// The command name of a task, re-read at a bounded rate.
pub struct Comm {
    tid: i32,
    name: Option<String>,
    read_at: Option<Instant>,
}

impl Comm {
    pub fn of(tid: i32) -> Self {
        Self {
            tid,
            name: None,
            read_at: None,
        }
    }

    /// The last name this task was seen to have, or `None` if it was never
    /// readable at all.
    ///
    /// A failed read keeps the previous answer rather than replacing it with
    /// nothing. That is not a cache being lenient: a `count` collector reads its
    /// counter when the window closes, and for a launched program the window
    /// closes *because the program exited* -- so at the one moment the event is
    /// built, `/proc/<pid>/comm` is already gone. The name a task had is the
    /// right label for the measurement of that task, and dropping it would make
    /// `comm` work for streaming sources and silently not for counting ones.
    ///
    /// `None` is still not an empty string: a collector that attached the
    /// attribute anyway would report every unreadable task as being called
    /// nothing, and a predicate written against `''` would match them all.
    pub fn get(&mut self) -> Option<&str> {
        let stale = self
            .read_at
            .is_none_or(|read_at| read_at.elapsed() >= FRESH_FOR);
        if stale {
            if let Some(name) = read(self.tid) {
                self.name = Some(name);
            }
            self.read_at = Some(Instant::now());
        }
        self.name.as_deref()
    }
}

/// Thread groups for many threads at once.
///
/// Unlike a name, a thread's group never changes, so a hit is kept for the
/// whole run. A miss is not cached: a thread seen before it appeared in procfs
/// would otherwise be "no process" for the rest of the run.
#[derive(Default)]
pub struct Tgids {
    by_tid: HashMap<i32, i32>,
}

impl Tgids {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&mut self, tid: i32) -> Option<i32> {
        if let Some(tgid) = self.by_tid.get(&tid) {
            return Some(*tgid);
        }
        let tgid = tgid_of(tid)?;
        self.by_tid.insert(tid, tgid);
        Some(tgid)
    }
}

/// One read of `/proc/<tid>/comm`, trimmed of the newline the kernel appends.
fn read(tid: i32) -> Option<String> {
    // `0` means "the calling task" to every syscall that takes a pid, and
    // several collectors carry it that way. Procfs has no such spelling, so it
    // is resolved here rather than in each of them.
    let name = if tid == 0 {
        "thread-self".to_owned()
    } else {
        tid.to_string()
    };
    let text = std::fs::read_to_string(format!("/proc/{name}/comm")).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Names for many tasks at once, for the collectors whose events are not all
/// about the same one.
///
/// A sampling collector opened over a process receives samples from whichever
/// of its threads faulted, so the name belongs to the sample's tid and not to
/// the collector's pid. Each tid keeps its own freshness window, and the map is
/// bounded by how many threads the observed process actually has.
#[derive(Default)]
pub struct Comms {
    by_tid: HashMap<i32, Comm>,
}

impl Comms {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&mut self, tid: i32) -> Option<&str> {
        self.by_tid
            .entry(tid)
            .or_insert_with(|| Comm::of(tid))
            .get()
    }
}

/// The thread group a thread belongs to: what a user means by "the process".
///
/// Declared as the `pid` filter field by ten `sched` sources, which key their
/// events on a *thread*. Without it there is no way to ask for one process's
/// scheduling behaviour when its threads are what the kernel reports.
pub fn tgid_of(tid: i32) -> Option<i32> {
    let name = if tid == 0 {
        "thread-self".to_owned()
    } else {
        tid.to_string()
    };
    let status = std::fs::read_to_string(format!("/proc/{name}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:"))?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ground truth from the other side: the kernel is asked what this thread
    /// is called, and the answer has to be the name the test binary set.
    #[test]
    fn a_threads_name_is_the_one_the_kernel_reports() {
        let mut comm = Comm::of(0);
        let name = comm.get().expect("this thread can read its own name");
        assert!(!name.is_empty());
        assert!(
            !name.ends_with('\n'),
            "the newline the kernel appends must not reach a filter: {name:?}"
        );
        // Same answer through the explicit tid, which is the path every
        // collector but a self-observing one takes.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let mut explicit = Comm::of(tid);
        assert_eq!(explicit.get(), Some(name));
    }

    /// A renamed task reports its new name, which is the whole reason the value
    /// is not read once and kept.
    #[test]
    fn a_rename_is_visible_after_the_freshness_window() {
        let original = read(0).expect("a name to restore");
        let mut comm = Comm::of(0);
        assert_eq!(comm.get(), Some(original.as_str()));

        let renamed = c"omn-renamed";
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_NAME, renamed.as_ptr()) },
            0,
            "a task may always rename itself"
        );
        std::thread::sleep(FRESH_FOR + Duration::from_millis(10));
        let seen = comm.get().map(str::to_owned);

        // Restored before the assertion: the rest of the suite runs in the
        // other threads of this process and a thread's name is its own, but a
        // failure here should not leave a confusing name behind either.
        let restore = std::ffi::CString::new(original.clone()).expect("a name without a nul");
        unsafe { libc::prctl(libc::PR_SET_NAME, restore.as_ptr()) };

        assert_eq!(seen.as_deref(), Some("omn-renamed"));
    }

    /// A task that does not exist has no name, and no name is not a name that
    /// is empty: the difference is what stops an exited task from matching a
    /// predicate written against `''`.
    #[test]
    fn a_task_that_is_not_there_has_no_name_rather_than_an_empty_one() {
        // A pid above the maximum cannot be live, whatever else is running.
        let impossible = i32::MAX;
        assert_eq!(Comm::of(impossible).get(), None);
        assert_eq!(tgid_of(impossible), None);
    }

    /// The case that made `comm` look implemented and not be: a counting
    /// collector builds its one event when the window closes, and for a
    /// launched program the window closes because the program exited.
    #[test]
    fn a_name_read_while_the_task_lived_outlives_it() {
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            // Long enough to be primed, short enough not to hold the suite up.
            std::thread::sleep(Duration::from_millis(50));
            unsafe { libc::_exit(0) };
        }

        let mut comm = Comm::of(child);
        let alive = comm.get().map(str::to_owned);
        assert!(alive.is_some(), "the child was readable while it lived");

        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
        // Reaped: /proc/<child> is gone, and a fresh reader now sees nothing.
        assert_eq!(read(child), None, "the test reaped the wrong thing");

        std::thread::sleep(FRESH_FOR + Duration::from_millis(10));
        assert_eq!(
            comm.get().map(str::to_owned),
            alive,
            "the name the task had is the label for the measurement of that task"
        );
    }

    /// The thread group of this thread is this process, which `std` reports
    /// independently.
    #[test]
    fn the_thread_group_of_this_thread_is_this_process() {
        assert_eq!(tgid_of(0), Some(std::process::id() as i32));
    }

    /// The keyed caches answer for a tid what the single reader answers for
    /// itself, and a miss is not remembered as an answer.
    #[test]
    fn the_keyed_caches_agree_with_a_direct_read() {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let mut comms = Comms::new();
        let mut tgids = Tgids::new();
        assert_eq!(comms.get(tid).map(str::to_owned), read(tid));
        assert_eq!(tgids.get(tid), Some(std::process::id() as i32));

        // A tid that cannot exist stays unanswered rather than being cached as
        // an answer, so a thread first seen before procfs caught up is not
        // "no process" for the rest of the run.
        assert_eq!(comms.get(i32::MAX), None);
        assert_eq!(tgids.get(i32::MAX), None);
        assert!(!tgids.by_tid.contains_key(&i32::MAX));
    }
}
