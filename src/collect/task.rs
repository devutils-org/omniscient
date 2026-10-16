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

/// How finely a task's cpu time was read.
///
/// A budget weighs the tool's cpu against the target's, and the target's used
/// to be `/proc/<pid>/stat`'s clock ticks: 10 ms each. A target that burns
/// less than a tick between two checks read as having burnt nothing, the
/// percentage stayed "not knowable yet", and a `--budget 5%` over a sleeping
/// loop ended 16 % over its ceiling without ever being moved. The scheduler
/// keeps the same account in nanoseconds (`sum_exec_runtime`), and that is
/// what is read first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuGrain {
    /// The scheduler's own nanoseconds: the process cpu clock for a process,
    /// `/proc/<pid>/task/<tid>/schedstat` for a thread.
    Nanoseconds,
    /// `/proc/<pid>/stat`'s clock ticks, where the nanosecond account could
    /// not be read: a kernel without `CONFIG_SCHED_INFO` publishes no
    /// `schedstat`, and a process in another pid namespace has no clock this
    /// process can name.
    Ticks,
}

impl CpuGrain {
    /// What a cost record says the target's cpu was read from.
    pub fn says(self) -> &'static str {
        match self {
            CpuGrain::Nanoseconds => "the scheduler's own nanoseconds",
            CpuGrain::Ticks => {
                "clock ticks from /proc/<pid>/stat, because the scheduler's nanosecond account \
                 could not be read here: a target that burns less than a tick between two \
                 checks reads as having burnt nothing"
            }
        }
    }
}

/// A task's cpu time, and how finely it was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuRead {
    pub ns: u64,
    pub grain: CpuGrain,
}

/// The kernel's tick, which is what `/proc/<pid>/stat` counts cpu time in.
fn ticks_per_second() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one.
    match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
        ticks if ticks > 0 => ticks as u64,
        _ => 100,
    }
}

/// Fields `range` of `/proc/<path>/stat`, summed and turned into nanoseconds.
///
/// The command name is parenthesised and may contain spaces and parentheses
/// of its own, so the fields are counted from the last `)`: after it, the
/// first field is `state`, which is field 3, and `utime` is field 14.
fn stat_ticks_ns(path: &str, range: std::ops::Range<usize>) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{path}/stat")).ok()?;
    let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    let ticks: u64 = fields
        .get(range)?
        .iter()
        .map(|field| field.parse::<u64>().unwrap_or(0))
        .sum();
    Some(ticks * 1_000_000_000 / ticks_per_second())
}

/// The process cpu clock of another process: every thread's
/// `sum_exec_runtime`, the ones that have exited included, in nanoseconds.
///
/// Linux lets any process read another's process clock (not a thread's), with
/// no check beyond the pid being visible in its namespace.
fn process_clock_ns(pid: i32) -> Option<u64> {
    let mut clock: libc::clockid_t = 0;
    // SAFETY: both take integers and write to a local this function owns.
    if unsafe { libc::clock_getcpuclockid(pid, &mut clock) } != 0 {
        return None;
    }
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(clock, &mut time) } != 0 {
        return None;
    }
    Some(time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64)
}

/// The cpu time one process has consumed, its children reaped included.
///
/// Its own threads, the exited ones included, from the process cpu clock in
/// nanoseconds; the children it has reaped from `cutime` and `cstime`, which
/// are only kept in ticks -- a shell that forks is still the workload that
/// was asked for. Where the clock cannot be read, all four of `stat`'s
/// fields, in ticks, and the grain says so.
pub fn process_cpu(pid: i32) -> Option<CpuRead> {
    let children = stat_ticks_ns(&pid.to_string(), 13..15);
    match (process_clock_ns(pid), children) {
        (Some(own), Some(children)) => Some(CpuRead {
            ns: own + children,
            grain: CpuGrain::Nanoseconds,
        }),
        _ => stat_ticks_ns(&pid.to_string(), 11..15).map(|ns| CpuRead {
            ns,
            grain: CpuGrain::Ticks,
        }),
    }
}

/// The cpu time one thread has been charged, its own and no other's.
///
/// `/proc/<tid>/task/<tid>/schedstat`'s first field, the thread's
/// `sum_exec_runtime` in nanoseconds; its `stat` in ticks where the kernel
/// keeps no schedstat. Not `/proc/<tid>/stat`: a thread id looked up at the
/// top of `/proc` names its whole thread group.
pub fn thread_cpu(tid: i32) -> Option<CpuRead> {
    let schedstat = std::fs::read_to_string(format!("/proc/{tid}/task/{tid}/schedstat"))
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<u64>().ok());
    match schedstat {
        Some(ns) => Some(CpuRead {
            ns,
            grain: CpuGrain::Nanoseconds,
        }),
        None => stat_ticks_ns(&format!("{tid}/task/{tid}"), 11..13).map(|ns| CpuRead {
            ns,
            grain: CpuGrain::Ticks,
        }),
    }
}

/// The children of one process, every thread's: a child belongs to the thread
/// that forked it, so a process whose worker threads spawn has children under
/// each of them. Zombies are included, since their cpu is in no parent's
/// account until they are reaped.
///
/// `/proc/<pid>/task/<tid>/children` (`CONFIG_PROC_CHILDREN`); `None` where
/// the kernel does not publish it, so the caller can tell "no children" from
/// "cannot be asked this way".
fn children_of(pid: i32) -> Option<Vec<i32>> {
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let mut children = Vec::new();
    let mut published = false;
    for task in tasks.flatten() {
        let Ok(text) = std::fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        published = true;
        children.extend(
            text.split_whitespace()
                .filter_map(|pid| pid.parse::<i32>().ok()),
        );
    }
    published.then_some(children)
}

/// The parent of every process, from one pass over `/proc`: the fallback for
/// a kernel without `CONFIG_PROC_CHILDREN`, and as costly as the machine has
/// processes, which is why it is not the first way asked.
fn parents() -> Vec<(i32, i32)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // After the parenthesised name: the state, then the parent.
            let parent = stat[stat.rfind(')')? + 1..]
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            Some((pid, parent))
        })
        .collect()
}

/// Every live (or not yet reaped) descendant of `pid`, each parent before its
/// children.
///
/// A program is often not one process: a compiler driver waits in `wait4`
/// while `cc1` does the work, `make` while its jobs do. What such a program
/// spends, and what has to end when it is stopped, is the tree it started.
/// A descendant that has been orphaned -- its parent exited first, and it was
/// reparented to a subreaper or to init -- is no longer in that tree, and is
/// not found.
pub fn descendants(pid: i32) -> Vec<i32> {
    let mut found: Vec<i32> = Vec::new();
    if children_of(pid).is_some() {
        let mut next = 0;
        let mut frontier = vec![pid];
        while let Some(&parent) = frontier.get(next) {
            next += 1;
            for child in children_of(parent).unwrap_or_default() {
                // A pid seen twice is a cycle only a reused pid can make, and
                // walking it again would never end.
                if child != pid && !found.contains(&child) {
                    found.push(child);
                    frontier.push(child);
                }
            }
        }
        return found;
    }
    let parents = parents();
    let mut next = 0;
    let mut frontier = vec![pid];
    while let Some(&parent) = frontier.get(next) {
        next += 1;
        for (child, _) in parents.iter().filter(|(_, of)| *of == parent) {
            if *child != pid && !found.contains(child) {
                found.push(*child);
                frontier.push(*child);
            }
        }
    }
    found
}

/// The cpu a launched program's whole tree has spent: its own
/// ([`process_cpu`], its reaped children included), and every live or
/// unreaped descendant's ([`descendants`]), each with the children it reaped
/// in turn.
///
/// Each nanosecond is in exactly one of those accounts at any one instant: a
/// descendant is counted on its own clock until its parent reaps it, and from
/// then on in that parent's `cutime`. Between two reads of one walk it can be
/// in neither -- the parent read before the reap, the child gone by its turn
/// -- so a walk can come out short by one process, never long, and the caller
/// keeps the highest it has read. The grain is the coarsest of the reads.
pub fn tree_cpu(pid: i32) -> Option<CpuRead> {
    let mut total = process_cpu(pid)?;
    for descendant in descendants(pid) {
        if let Some(read) = process_cpu(descendant) {
            total.ns += read.ns;
            if read.grain == CpuGrain::Ticks {
                total.grain = CpuGrain::Ticks;
            }
        }
    }
    Some(total)
}

/// The cpu time of every live (or not yet reaped) descendant of `me`, each
/// on its own account as [`tree_cpu`] reads them, in one walk, leaving out
/// `except` and their descendants; zero when there are none.
///
/// For the `omn` binary, the subreaper of what it launches
/// ([`super::launch::adopt_orphans`]): its descendants are the launched
/// program's tree, the orphans that left it, and the helpers `omn` runs itself
/// ([`super::launch::helper_output`]), which are `except`. This with this
/// process's own children account, less the helpers reaped, is the program's
/// whole spend, orphans included.
pub fn descendants_cpu(me: i32, except: &[i32]) -> u64 {
    let except = with_descendants(except);
    descendants(me)
        .into_iter()
        .filter(|pid| !except.contains(pid))
        .filter_map(process_cpu)
        .map(|read| read.ns)
        .sum()
}

/// `pids` and every descendant of each.
pub fn with_descendants(pids: &[i32]) -> Vec<i32> {
    let mut all = pids.to_vec();
    for pid in pids {
        all.extend(descendants(*pid));
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell whose child burns the cpu while the shell itself waits: the
    /// tree's account holds the child's work while it runs, which the shell's
    /// own account cannot until it reaps it.
    #[test]
    fn a_waiting_parent_s_tree_holds_the_cpu_its_running_child_spends() {
        const BURN: &str = "python3 -c 'import time\nt = time.process_time()\n\
             while time.process_time() - t < 0.3: pass\ntime.sleep(30)'; :";
        let mut shell = std::process::Command::new("/bin/sh")
            .args(["-c", BURN])
            .spawn()
            .expect("a shell");
        let pid = shell.id() as i32;
        // Wait for the child to appear and burn its 300 ms, however loaded the
        // machine is.
        let mut child_cpu = 0;
        for _ in 0..500 {
            std::thread::sleep(Duration::from_millis(20));
            child_cpu = descendants(pid)
                .iter()
                .filter_map(|child| process_cpu(*child))
                .map(|read| read.ns)
                .sum();
            if child_cpu >= 300_000_000 {
                break;
            }
        }
        let own = process_cpu(pid).map(|read| read.ns);
        let tree = tree_cpu(pid).map(|read| read.ns);
        let found = descendants(pid);
        for child in &found {
            // SAFETY: signalling a process this test's shell started.
            unsafe { libc::kill(*child, libc::SIGKILL) };
        }
        let _ = shell.kill();
        let _ = shell.wait();
        if child_cpu < 300_000_000 {
            eprintln!("skipped: no python3 child burnt its 300 ms ({found:?})");
            return;
        }
        let (own, tree) = (own.unwrap(), tree.unwrap());
        assert!(
            own < 100_000_000,
            "the shell itself only waited, so its own account is small: {own} ns"
        );
        assert!(
            tree >= own + 300_000_000,
            "the tree holds the child's 300 ms beside the shell's {own} ns: {tree} ns \
             ({found:?})"
        );
    }

    /// A process left out of the walk is left out with what it started: the
    /// helpers `omn` runs itself, and their children, are not the program's.
    #[test]
    fn a_process_left_out_of_the_descendants_cpu_takes_its_own_children_with_it() {
        const BURN: &str = "python3 -c 'import time\nt = time.process_time()\n\
             while time.process_time() - t < 0.3: pass\ntime.sleep(30)'; :";
        let mut root = std::process::Command::new("/bin/sh")
            .args(["-c", "sh -c \"$BURN\"; :"])
            .env("BURN", BURN)
            .spawn()
            .expect("a shell");
        let pid = root.id() as i32;
        let mut all = 0;
        for _ in 0..500 {
            std::thread::sleep(Duration::from_millis(20));
            all = descendants_cpu(pid, &[]);
            if all >= 300_000_000 {
                break;
            }
        }
        let found = descendants(pid);
        let left_out = found.first().map(|inner| descendants_cpu(pid, &[*inner]));
        for child in &found {
            // SAFETY: signalling a process this test's shell started.
            unsafe { libc::kill(*child, libc::SIGKILL) };
        }
        let _ = root.kill();
        let _ = root.wait();
        if all < 300_000_000 {
            eprintln!("skipped: no python3 grandchild burnt its 300 ms ({found:?})");
            return;
        }
        assert_eq!(
            left_out,
            Some(0),
            "the inner shell left out takes its burning child with it ({found:?}, {all} ns \
             with them)"
        );
    }

    /// A child this process started is among its descendants, and a pid that
    /// cannot exist has none and no cpu.
    #[test]
    fn a_child_is_a_descendant_and_a_missing_process_has_none() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep");
        let mine = descendants(std::process::id() as i32);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            mine.contains(&(child.id() as i32)),
            "a child this process started is among its descendants: {mine:?}"
        );
        assert!(descendants(i32::MAX).is_empty());
        assert_eq!(tree_cpu(i32::MAX), None);
    }

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

    /// A process that burns a few milliseconds -- well under the tens of
    /// milliseconds `stat` counts in -- and then waits is read to the
    /// nanosecond, against the figures it reports of itself: its process cpu
    /// clock and its main thread's, through Python's `time` module. Its
    /// `stat` ticks are a whole number of ticks, which is what the budget used
    /// to read and why it could not see such a target.
    #[test]
    fn a_target_that_burns_under_a_tick_is_read_in_nanoseconds_against_its_own_clock() {
        use std::io::{BufRead, Write};
        const BURN: &str = "import sys, time\n\
             t = time.process_time_ns()\n\
             while time.process_time_ns() - t < 3_000_000: pass\n\
             print(time.process_time_ns(), time.thread_time_ns(), flush=True)\n\
             sys.stdin.readline()\n\
             print(time.process_time_ns(), time.thread_time_ns(), flush=True)\n";
        let Ok(mut child) = std::process::Command::new("python3")
            .args(["-c", BURN])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
        else {
            eprintln!("skipped: no python3 to be the target");
            return;
        };
        let pid = child.id() as i32;
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let figures = |line: &str| -> (u64, u64) {
            let mut parts = line.split_whitespace().map(|n| n.parse::<u64>().unwrap());
            (parts.next().unwrap(), parts.next().unwrap())
        };
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let (process_before, thread_before) = figures(&line);
        let process = process_cpu(pid).expect("a live child's cpu");
        let thread = thread_cpu(pid).expect("its main thread's cpu");
        let ticks = stat_ticks_ns(&pid.to_string(), 11..13).expect("its stat");
        child.stdin.take().unwrap().write_all(b"\n").unwrap();
        line.clear();
        stdout.read_line(&mut line).unwrap();
        let (process_after, thread_after) = figures(&line);
        let _ = child.wait();

        let account = format!(
            "read {} ns ({:?}) of the process and {} ns ({:?}) of its thread; the child said \
             {process_before}..{process_after} and {thread_before}..{thread_after}; stat said \
             {ticks} ns",
            process.ns, process.grain, thread.ns, thread.grain
        );
        assert_eq!(process.grain, CpuGrain::Nanoseconds, "{account}");
        assert!(
            (process_before..=process_after).contains(&process.ns),
            "{account}"
        );
        if thread.grain == CpuGrain::Nanoseconds {
            assert!(
                (thread_before..=thread_after).contains(&thread.ns),
                "{account}"
            );
        } else {
            eprintln!("skipped the thread half: this kernel keeps no schedstat ({account})");
        }
        let tick = 1_000_000_000 / ticks_per_second();
        assert_eq!(ticks % tick, 0, "{account}");
        assert!(process.ns >= 3_000_000, "{account}");
    }
}
