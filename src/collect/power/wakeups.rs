//! `power.wakeups` through procfs: how often a process's threads woke up.
//!
//! A cpu that is woken to run a thread leaves its idle state, and a process
//! that wakes a thousand times a second keeps a core out of its deepest one --
//! the question powertop's main view answers per process. The kernel counts,
//! per thread, the times it gave up the cpu voluntarily
//! (`voluntary_ctxt_switches` in `/proc/<pid>/task/<tid>/status`): each is a
//! sleep, and each sleep ends in exactly one wakeup, so the increase over an
//! interval is the wakeups in it (the one sleep still in progress at the read
//! is counted when it ends).
//!
//! What this does not see: a thread that exits between two reads takes its
//! last interval's switches with it. Those threads are counted as dropped and
//! named, never folded into a smaller number silently. A process that is gone
//! at a read (reaped by a parent that is not omn, or a `--pid` the run does
//! not own) takes the interval since the last read with it the same way: each
//! thread it had then is counted as dropped. A launched program is not reaped
//! before the closing round, so its zombie's leader is still read there. A
//! thread born and gone between two reads is in neither listing, so it can be
//! neither counted nor dropped: the caveat says so. And the wakeups are the
//! process's own, not the cpu wakeups it causes in others -- a timer it arms
//! that wakes another task is that task's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

#[derive(Debug)]
pub struct Prepare;

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } => Ok(Self),
            other => Err(format!(
                "wakeups are counted per interval between two reads of procfs: `{}` has no \
                 interval; poll it",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if matches!(target, Target::System) {
            return Ok(Box::new(MachineWakeups::new(PathBuf::from("/proc"))));
        }
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "wakeups through procfs are a process's, and a {} target names none: use --pid \
                 or launch the program after `--`; every wakeup on the machine is \
                 `sched.wakeup`'s",
                target.kind()
            )
        })?;
        Ok(Box::new(Wakeups::new(PathBuf::from("/proc"), pid)))
    }
}

pub struct Wakeups {
    proc_root: PathBuf,
    pid: i32,
    proc_key: Option<KeyValue>,
    /// Each live thread's voluntary switches at the previous read.
    previous: Option<(u64, BTreeMap<i32, u64>)>,
    losses: Losses,
}

impl Wakeups {
    fn new(proc_root: PathBuf, pid: i32) -> Self {
        Self {
            proc_root,
            pid,
            proc_key: None,
            previous: None,
            losses: Losses::default(),
        }
    }

    /// Every thread's voluntary switches, or the process is gone.
    fn read(&self) -> Result<BTreeMap<i32, u64>, SampleError> {
        let tasks = self.proc_root.join(self.pid.to_string()).join("task");
        let entries = std::fs::read_dir(&tasks).map_err(|error| {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                SampleError::TargetGone(format!("{} is not readable: {error}", tasks.display()))
            } else {
                SampleError::TargetGone(format!("process {} exited", self.pid))
            }
        })?;
        let mut threads = BTreeMap::new();
        for entry in entries.flatten() {
            let Some(tid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            else {
                continue;
            };
            // A thread can exit between the listing and the read: it is then
            // simply not among this read's threads.
            if let Some(count) = voluntary_switches(&entry.path().join("status")) {
                threads.insert(tid, count);
            }
        }
        if threads.is_empty() {
            return Err(SampleError::TargetGone(format!(
                "process {} exited",
                self.pid
            )));
        }
        Ok(threads)
    }
}

fn voluntary_switches(status: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(status).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))?
        .trim()
        .parse()
        .ok()
}

/// The wakeups between two reads: each surviving thread's increase, every
/// switch of a thread born since, and how many threads exited in between.
fn wakeups_between(before: &BTreeMap<i32, u64>, after: &BTreeMap<i32, u64>) -> (u64, u64) {
    let mut wakeups = 0u64;
    for (tid, count) in after {
        wakeups += match before.get(tid) {
            Some(earlier) => count.saturating_sub(*earlier),
            None => *count,
        };
    }
    let exited = before.keys().filter(|tid| !after.contains_key(tid)).count() as u64;
    (wakeups, exited)
}

impl Collector for Wakeups {
    fn value_attr(&self) -> &'static str {
        "wakeups"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let threads =
            match self.read() {
                Ok(threads) => threads,
                Err(gone) => {
                    // The interval since the last read is lost with the process:
                    // every thread it then had is a series that ends unread.
                    if let Some((_, before)) = self.previous.take() {
                        self.losses.dropped += before.len() as u64;
                        self.losses.note_times(
                        "the process was gone at a read, and its wakeups since the previous one \
                         are not counted", before.len() as u64);
                    }
                    return Err(gone);
                }
            };
        let ts_ns = monotonic_ns();
        if self.proc_key.is_none() {
            self.proc_key = KeyValue::proc_of(self.pid);
        }
        // The first read is the baseline: the switches since the process
        // started are not this window's.
        let Some((before_ns, before)) = self.previous.replace((ts_ns, threads.clone())) else {
            return Ok(Vec::new());
        };
        let (wakeups, exited) = wakeups_between(&before, &threads);
        if exited > 0 {
            self.losses.dropped += exited;
            self.losses.note_times(
                "a thread exited between two reads, and its wakeups since the earlier one are \
                 not counted",
                exited,
            );
        }
        let interval_ns = ts_ns.saturating_sub(before_ns).max(1);
        let comm = std::fs::read_to_string(self.proc_root.join(self.pid.to_string()).join("comm"))
            .map(|name| name.trim_end().to_owned())
            .unwrap_or_default();
        Ok(vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "power.wakeups".to_owned(),
            keys: self
                .proc_key
                .clone()
                .into_iter()
                .chain([KeyValue::task(self.pid), KeyValue::comm(comm.clone())])
                .collect(),
            attrs: vec![
                Attr::uint("wakeups", wakeups),
                Attr::float("per_second", wakeups as f64 * 1e9 / interval_ns as f64),
                Attr::uint("pid", self.pid as u64),
                Attr::text("comm", comm),
                Attr::uint("threads", threads.len() as u64),
                Attr::uint("threads_exited", exited),
                Attr::uint("interval_ns", interval_ns),
            ],
            collector_id: CollectorId::unstamped(),
            seq,
        }])
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

/// `--system`: every process on the machine, one event per process that
/// woke in the interval -- powertop's list, sorted by the reader.
///
/// Each read walks `/proc` and every thread of every process; a process
/// present at one read and gone at the next took its last interval with it,
/// counted as dropped like a thread is under a process target, and a process
/// born since the previous read is a baseline, as the first read is.
/// Every process's name and its threads' voluntary switches, by pid.
type Processes = BTreeMap<i32, (String, BTreeMap<i32, u64>)>;

pub struct MachineWakeups {
    proc_root: PathBuf,
    /// Each process's threads' voluntary switches at the previous read, and
    /// its name.
    previous: Option<(u64, Processes)>,
    losses: Losses,
}

impl MachineWakeups {
    fn new(proc_root: PathBuf) -> Self {
        Self {
            proc_root,
            previous: None,
            losses: Losses::default(),
        }
    }

    fn read(&self) -> Result<Processes, SampleError> {
        let entries = std::fs::read_dir(&self.proc_root).map_err(|error| {
            SampleError::Transient(format!("{}: {error}", self.proc_root.display()))
        })?;
        let mut processes = BTreeMap::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            let one = Wakeups::new(self.proc_root.clone(), pid);
            // Gone between the listing and the read: not in this read.
            let Ok(threads) = one.read() else {
                continue;
            };
            let comm = std::fs::read_to_string(entry.path().join("comm"))
                .map(|name| name.trim_end().to_owned())
                .unwrap_or_default();
            processes.insert(pid, (comm, threads));
        }
        Ok(processes)
    }
}

impl Collector for MachineWakeups {
    fn value_attr(&self) -> &'static str {
        "wakeups"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let processes = self.read()?;
        let ts_ns = monotonic_ns();
        let Some((before_ns, before)) = self.previous.replace((ts_ns, processes.clone())) else {
            return Ok(Vec::new());
        };
        let interval_ns = ts_ns.saturating_sub(before_ns).max(1);
        let mut exited_threads = 0u64;
        let mut gone = 0u64;
        for (pid, (_, threads)) in &before {
            if !processes.contains_key(pid) {
                gone += 1;
                exited_threads += threads.len() as u64;
            }
        }
        let mut events = Vec::new();
        for (pid, (comm, threads)) in &processes {
            let Some((_, earlier)) = before.get(pid) else {
                continue;
            };
            let (wakeups, exited) = wakeups_between(earlier, threads);
            exited_threads += exited;
            if wakeups == 0 {
                continue;
            }
            events.push(Event {
                ts_ns,
                ts_err_ns: 0,
                kind: "power.wakeups".to_owned(),
                // `proc` where the namespace link is readable (this user's
                // processes), and always the pid and the name: another user's
                // process has no `proc` key for an unprivileged reader.
                keys: KeyValue::proc_of(*pid)
                    .into_iter()
                    .chain([KeyValue::task(*pid), KeyValue::comm(comm.clone())])
                    .collect(),
                attrs: vec![
                    Attr::uint("wakeups", wakeups),
                    Attr::float("per_second", wakeups as f64 * 1e9 / interval_ns as f64),
                    Attr::uint("pid", *pid as u64),
                    Attr::text("comm", comm.clone()),
                    Attr::uint("threads", threads.len() as u64),
                    Attr::uint("threads_exited", exited),
                    Attr::uint("interval_ns", interval_ns),
                ],
                collector_id: CollectorId::unstamped(),
                seq: seq + events.len() as u64,
            });
        }
        if exited_threads > 0 {
            self.losses.dropped += exited_threads;
            self.losses.note_times(
                format!(
                "threads that exited between two reads took their wakeups since the earlier one \
                 with them ({gone} whole processes among them, by the last interval)"
            ),
                exited_threads,
            );
        }
        Ok(events)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::power::sysfs::scratch;

    fn status(root: &Path, pid: i32, tid: i32, switches: u64) {
        scratch::write(
            root,
            &format!("{pid}/task/{tid}/status"),
            &format!(
                "Name:\tx\nvoluntary_ctxt_switches:\t{switches}\nnonvoluntary_ctxt_switches:\t9"
            ),
        );
    }

    /// A thread that survives counts its increase, one born since counts all
    /// of its switches, and one that exited is dropped by name, not as zero.
    #[test]
    fn the_wakeups_of_an_interval_are_every_threads_increase() {
        let root = scratch::tree("wakeups");
        status(&root, 10, 10, 100);
        status(&root, 10, 11, 50);
        let mut collector = Wakeups::new(root.clone(), 10);
        assert!(collector.sample(0).expect("the baseline").is_empty());

        status(&root, 10, 10, 130);
        std::fs::remove_dir_all(root.join("10/task/11")).unwrap();
        status(&root, 10, 12, 7);
        let events = collector.sample(1).expect("a read");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.attr("wakeups").unwrap().as_f64(), Some(37.0));
        assert_eq!(event.attr("threads").unwrap().as_f64(), Some(2.0));
        assert_eq!(event.attr("threads_exited").unwrap().as_f64(), Some(1.0));
        let losses = collector.losses();
        assert_eq!(losses.dropped, 1);
        assert!(!losses.is_clean());
        std::fs::remove_dir_all(&root).ok();
    }

    /// A process that vanishes after the baseline loses the interval since
    /// it, and says so: one dropped per thread it had, never a clean run.
    #[test]
    fn a_process_gone_after_a_read_counts_its_last_interval_as_lost() {
        let root = scratch::tree("wakeups-lost");
        status(&root, 20, 20, 5);
        status(&root, 20, 21, 5);
        let mut collector = Wakeups::new(root.clone(), 20);
        assert!(collector.sample(0).expect("the baseline").is_empty());
        std::fs::remove_dir_all(root.join("20")).unwrap();
        assert!(matches!(
            collector.sample(1),
            Err(SampleError::TargetGone(_))
        ));
        let losses = collector.losses();
        assert_eq!(losses.dropped, 2);
        assert!(losses.reasons.keys().any(|r| r.contains("gone at a read")));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_process_that_is_gone_ends_the_run() {
        let root = scratch::tree("wakeups-gone");
        let mut collector = Wakeups::new(root.clone(), 4242);
        assert!(matches!(
            collector.sample(0),
            Err(SampleError::TargetGone(_))
        ));
        assert!(
            collector.losses().is_clean(),
            "gone before any read: nothing was measured, so nothing was lost"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_machine_target_lists_every_process_that_woke() {
        let root = scratch::tree("wakeups-machine");
        status(&root, 10, 10, 100);
        status(&root, 10, 11, 5);
        status(&root, 20, 20, 7);
        status(&root, 30, 30, 1);
        scratch::write(&root, "10/comm", "busy\n");
        let mut collector = MachineWakeups::new(root.clone());
        assert!(collector.sample(0).unwrap().is_empty(), "a baseline");
        status(&root, 10, 10, 150);
        status(&root, 10, 11, 15);
        // 20 did not wake; 30 is gone; 40 is new.
        std::fs::remove_dir_all(root.join("30")).unwrap();
        status(&root, 40, 40, 3);
        let events = collector.sample(1).unwrap();
        assert_eq!(events.len(), 1, "only the process that woke: {events:?}");
        assert_eq!(events[0].attr("wakeups").unwrap().as_f64(), Some(60.0));
        assert_eq!(
            events[0].attr("comm").map(|value| value.to_string()),
            Some("busy".to_owned())
        );
        assert_eq!(collector.losses().dropped, 1, "the gone process's thread");

        let prepared = Box::new(
            Prepare::new(Mode::Poll {
                interval: Mode::DEFAULT_POLL_INTERVAL,
            })
            .unwrap(),
        );
        assert!(prepared.attach(&Target::System).is_ok());
        assert!(Prepare::new(Mode::Count).is_err());
    }

    /// This process, against a count it chose: 200 sleeps of a millisecond on
    /// one thread are at least 200 wakeups of that thread.
    #[test]
    fn two_hundred_sleeps_are_at_least_two_hundred_wakeups() {
        let pid = std::process::id() as i32;
        let mut collector = Wakeups::new(PathBuf::from("/proc"), pid);
        collector.sample(0).expect("the baseline");
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let events = collector.sample(1).expect("a read");
        let wakeups = events[0].attr("wakeups").unwrap().as_f64().unwrap();
        assert!(wakeups >= 200.0, "200 sleeps, {wakeups} wakeups");
    }
}
