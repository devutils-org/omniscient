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

use super::lifecycle;
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
            proc_key: KeyValue::proc_of(pid),
            first_read: true,
            departed: false,
            losses: Losses::default(),
        }))
    }
}

pub struct StatusCollector {
    id: CollectorId,
    counted: Counted,
    pid: i32,
    /// The process across containers (`KeyValue::proc_of`), read once at
    /// attach: a process does not change pid namespace, and an exec keeps it.
    /// `None` when the namespace link is closed to this user, and the events
    /// then carry `task` alone, which says so by the missing key.
    proc_key: Option<KeyValue>,
    /// Cleared by the first read, which is the one that has to wait for a
    /// launched target to become the program it was asked to be.
    first_read: bool,
    /// Set by the round that first found the target departed.
    ///
    /// That round reports nothing, which is the fix, but it does not end the
    /// run either: the runner asks its own stop condition once the round is
    /// over, and for a launched program that condition has the better reason
    /// -- it knows the exit status. Only a target nothing else is watching,
    /// which is every `--pid` run, reaches the round after and is ended here.
    departed: bool,
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

    /// The process's command name, which the catalog offers as a filter field.
    ///
    /// Read fresh each tick rather than once: a process can rename itself, and a
    /// stale name would attribute the samples after the rename to the wrong
    /// program.
    fn command(&self) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{}/comm", self.pid))
            .ok()
            .map(|name| name.trim().to_owned())
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
        // A launched target is released from its gate and read immediately, and
        // until it reaches its `execve` the pid is right and everything the pid
        // holds is this tool's. Counting that is not a measurement of the
        // program at all, and in `snapshot` mode it is the only reading there
        // is. So the first read waits for the program to exist.
        if self.first_read {
            self.first_read = false;
            if lifecycle::wait_for_exec(self.pid).is_none() {
                self.losses.note(format!(
                    "process {} was still running this tool's own image when the run began, and \
                     did not reach its `execve` in time; this reading may describe the tool \
                     rather than the program",
                    self.pid
                ));
            }
        }

        // A process that has exited and not been reaped keeps a readable
        // `/proc/<pid>`: `fd` is an empty directory and `status` still says one
        // thread. Neither read fails, so without this the run took one more
        // sample and recorded a program that never closed a handle in its life
        // as holding nothing -- zero open descriptors, the whole limit as
        // headroom, and a clean bill of health.
        match lifecycle::departed(self.pid) {
            Some(reason) if self.departed => return Err(SampleError::TargetGone(reason)),
            Some(_) => {
                self.departed = true;
                return Ok(Vec::new());
            }
            None => {}
        }

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

        // The catalog offers `comm` as a filter field for both sources, and a
        // filter field nothing emits is false everywhere: the predicate empties
        // the series and looks exactly like a process that did nothing.
        if let Some(command) = self.command() {
            attrs.push(Attr::text("comm", command));
        }

        // Taken after the read, so it bounds it: the value existed at or before
        // this instant.
        let ts_ns = monotonic_ns();
        Ok(vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: self.counted.source().to_owned(),
            keys: std::iter::once(KeyValue::task(self.pid))
                .chain(self.proc_key.clone())
                .collect(),
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

    /// The pid namespace as the link's own text names it, `pid:[<inode>]`:
    /// a reference read another way than `KeyValue::proc_of` reads it (that
    /// one takes the inode from a `stat`).
    fn own_pidns() -> u64 {
        let link = std::fs::read_link("/proc/self/ns/pid").expect("our own namespace link");
        let text = link.to_string_lossy();
        text.strip_prefix("pid:[")
            .and_then(|rest| rest.strip_suffix(']'))
            .and_then(|inode| inode.parse().ok())
            .unwrap_or_else(|| panic!("an unexpected namespace link: {text}"))
    }

    /// Both counts carry the process across containers after the thread: the
    /// pid namespace this test runs in and its pid there, which in the
    /// namespace `cargo test` runs in is the one `getpid` returns.
    #[test]
    fn a_count_carries_the_process_after_the_thread() {
        let me = std::process::id() as i32;
        for counted in [Counted::Descriptors, Counted::Threads] {
            let events = collector(counted).sample(0).expect("readable");
            let keys = &events[0].keys;
            assert_eq!(keys[0], KeyValue::task(me), "{keys:?}");
            let Some(KeyValue::Proc { pidns, pid }) = keys.get(1) else {
                panic!("{} carries no `proc` key: {keys:?}", counted.source());
            };
            assert_eq!(*pidns, own_pidns());
            let inner: i32 = std::fs::read_to_string("/proc/self/status")
                .expect("our own status")
                .lines()
                .find_map(|line| line.strip_prefix("NSpid:"))
                .and_then(|ids| ids.split_whitespace().last()?.parse().ok())
                .expect("an NSpid line");
            assert_eq!(*pid, inner);
            assert_eq!(keys.len(), 2, "{keys:?}");
        }
    }

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

    /// Every field the catalog offers must be on the events, because the filter
    /// now runs: a predicate over a field nothing emits is false everywhere,
    /// which empties the series and reads as a process that did nothing.
    #[test]
    fn every_filter_field_the_catalog_offers_is_on_the_events() {
        let catalog = crate::catalog::Catalog::embedded().unwrap();
        for counted in [Counted::Descriptors, Counted::Threads] {
            let entry = catalog
                .get(&counted.source().parse().unwrap())
                .expect("the source is in the catalog");
            let events = collector(counted).sample(0).expect("readable");
            for field in &entry.filter_fields {
                assert!(
                    events[0].attr(field).is_some(),
                    "`{}` offers `{field}` as a filter field and does not emit it",
                    counted.source()
                );
            }
        }
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

    /// The first reading of a launched program is of the program.
    ///
    /// The ground truth is the one thing procfs cannot get wrong about a pid:
    /// which image it is running. A gated child is a fork of this test binary
    /// until its `execve`, so a read taken the microsecond after the gate opens
    /// used to report this binary's own command name and its own descriptors
    /// under the launched program's pid. `/bin/sleep` is launched because it is
    /// nothing like the suite: if `comm` says anything but `sleep`, the reading
    /// is not about the program.
    #[test]
    fn the_first_reading_of_a_launched_program_is_of_the_program() {
        let mut child = crate::collect::launch::gated(&["sleep".to_owned(), "5".to_owned()], &[])
            .expect("fork");
        let mut collector = Box::new(Prepare(Counted::Descriptors))
            .attach(&Target::Pid(vec![child.pid()]))
            .expect("a pid target");

        // Exactly what the runner does: release, then read immediately.
        child.release();
        let events = collector.sample(0).expect("the child is alive");

        let comm = events[0]
            .attr("comm")
            .expect("`comm` travels on every event")
            .to_string();
        assert_eq!(
            comm,
            "sleep",
            "the first reading named `{comm}`, which is this tool rather than the program it \
             launched; the collector's own account: {:?}",
            collector.losses()
        );
    }

    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let Err(error) = Box::new(Prepare(Counted::Threads)).attach(&Target::System) else {
            panic!("these counts belong to a process");
        };
        assert!(error.contains("names none"), "{error}");
    }
}
