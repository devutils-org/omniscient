//! `proc.tasks` over procfs: the machine's processes, or every thread of one.
//!
//! What `top`, `htop` and `ps` list, read the way they read it: one
//! `/proc/<pid>/stat` per process of the machine (a `system` target), or one
//! `/proc/<pid>/task/<tid>/stat` per thread of a process (a `pid` or launched
//! target). Each poll is one event per task, all stamped at the one instant
//! the poll finished, so a reader can sort one poll's events and have the
//! table.
//!
//! The value is `busy`: the share of one cpu the task ran for over the
//! interval since the previous poll, `top`'s `%CPU` divided by a hundred.
//! A task first seen in a poll has no previous reading, so its share is over
//! its whole life instead -- `ps`'s `%CPU` -- and `since` on the event says
//! which of the two it is. The counters behind both (`utime`, `stime`) are in
//! clock ticks, so a share is good to a tick over its interval: one tick in a
//! second is one per cent.

use std::collections::HashMap;
use std::path::Path;

use super::lifecycle;
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Target;

pub struct Prepare;

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let whose = match target {
            Target::System => Whose::Machine,
            Target::Pid(_) | Target::Tid(_) => {
                Whose::ThreadsOf(target.first_pid().expect("a pid target names a pid"))
            }
            other => {
                return Err(format!(
                    "`proc.tasks` lists the machine's processes or one process's threads, and a \
                     {} target is neither",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(TasksCollector {
            id: CollectorId::new("proc.tasks/fs:proc"),
            whose,
            ticks_per_s: clock_ticks(),
            page_bytes: page_bytes(),
            previous: HashMap::new(),
            previous_ns: 0,
            first_read: true,
            departed: false,
            losses: Losses::default(),
        }))
    }
}

/// What one run lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Whose {
    /// Every process of the machine, one event each.
    Machine,
    /// Every thread of one process, one event each.
    ThreadsOf(i32),
}

pub struct TasksCollector {
    id: CollectorId,
    whose: Whose,
    ticks_per_s: u64,
    page_bytes: u64,
    /// Each task's cpu ticks at the previous poll, by `(tid, starttime)`: a
    /// pid reused by a new process between two polls is another task, and
    /// its ticks are not a difference against the old one's.
    previous: HashMap<(i32, u64), u64>,
    /// When the previous poll finished, `CLOCK_MONOTONIC`.
    previous_ns: u64,
    first_read: bool,
    departed: bool,
    losses: Losses,
}

/// One task's line of `stat`, the fields this source reports.
#[derive(Debug, Clone, PartialEq)]
pub struct Stat {
    pub comm: String,
    pub state: char,
    pub ppid: i32,
    pub minflt: u64,
    pub majflt: u64,
    pub utime: u64,
    pub stime: u64,
    pub priority: i64,
    pub nice: i64,
    pub threads: u64,
    /// Clock ticks after boot.
    pub starttime: u64,
    pub vsize: u64,
    /// Pages.
    pub rss: u64,
    /// The cpu it last ran on.
    pub processor: u32,
}

/// Parses one `stat` line. The name is in parentheses and may itself hold
/// spaces and parentheses, so it ends at the *last* `)`.
pub fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_owned();
    let fields: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    // Field 3 of `proc(5)` is index 0 here.
    let at = |field: usize| fields.get(field - 3).copied();
    let uint = |field: usize| at(field).and_then(|value| value.parse::<u64>().ok());
    let int = |field: usize| at(field).and_then(|value| value.parse::<i64>().ok());
    Some(Stat {
        comm,
        state: at(3)?.chars().next()?,
        ppid: int(4)? as i32,
        minflt: uint(10)?,
        majflt: uint(12)?,
        utime: uint(14)?,
        stime: uint(15)?,
        priority: int(18)?,
        nice: int(19)?,
        threads: uint(20)?,
        starttime: uint(22)?,
        vsize: uint(23)?,
        rss: uint(24)?,
        processor: uint(39).unwrap_or(0) as u32,
    })
}

fn clock_ticks() -> u64 {
    // SAFETY: `sysconf` reads a constant of the running system.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 {
        ticks as u64
    } else {
        100
    }
}

fn page_bytes() -> u64 {
    // SAFETY: as above.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page > 0 {
        page as u64
    } else {
        4096
    }
}

/// `CLOCK_BOOTTIME` in nanoseconds: the clock `starttime` counts on. Used for
/// a duration only (a task's age), never as a timestamp.
fn boottime_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid, writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// The numeric entries of a procfs directory: the pids of `/proc`, or the
/// tids of `/proc/<pid>/task`.
fn numeric_entries(dir: &Path) -> std::io::Result<Vec<i32>> {
    let mut ids: Vec<i32> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

/// The owner of a task's procfs directory: its effective uid, the one `ps -o
/// user` shows (root for a task that is not dumpable, as procfs presents it).
fn owner(dir: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(dir).ok().map(|meta| meta.uid())
}

impl TasksCollector {
    /// The tasks this poll lists, as `(pid, tid, the directory of its stat)`.
    fn listed(&self) -> Result<Vec<(i32, i32, std::path::PathBuf)>, SampleError> {
        match self.whose {
            Whose::Machine => {
                let pids = numeric_entries(Path::new("/proc"))
                    .map_err(|error| SampleError::Transient(format!("/proc: {error}")))?;
                Ok(pids
                    .into_iter()
                    .map(|pid| (pid, pid, format!("/proc/{pid}").into()))
                    .collect())
            }
            Whose::ThreadsOf(pid) => {
                let dir = format!("/proc/{pid}/task");
                let tids = numeric_entries(Path::new(&dir)).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(libc::ESRCH)
                    {
                        SampleError::TargetGone(format!("process {pid} exited"))
                    } else {
                        SampleError::Transient(format!("{dir}: {error}"))
                    }
                })?;
                Ok(tids
                    .into_iter()
                    .map(|tid| (pid, tid, format!("/proc/{pid}/task/{tid}").into()))
                    .collect())
            }
        }
    }
}

impl Collector for TasksCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "busy"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if let Whose::ThreadsOf(pid) = self.whose {
            // The two traps `proc.threads` has, for the reasons
            // `lifecycle` gives: a launched target read before its exec is
            // this tool, and one read after its exit is a zombie.
            if self.first_read && lifecycle::wait_for_exec(pid).is_none() {
                self.losses.note(format!(
                    "process {pid} was still running this tool's own image when the run began, \
                     and did not reach its `execve` in time; this reading may describe the tool \
                     rather than the program"
                ));
            }
            match lifecycle::departed(pid) {
                Some(reason) if self.departed => return Err(SampleError::TargetGone(reason)),
                Some(_) => {
                    self.departed = true;
                    return Ok(Vec::new());
                }
                None => {}
            }
        }
        self.first_read = false;

        let listed = self.listed()?;
        let mut read = Vec::with_capacity(listed.len());
        for (pid, tid, dir) in listed {
            // A task that left between the listing and its read is not a
            // loss: it is not on the machine any more, and the next poll would
            // not list it either.
            let Ok(text) = std::fs::read_to_string(dir.join("stat")) else {
                continue;
            };
            let Some(mut stat) = parse_stat(&text) else {
                self.losses.undecoded += 1;
                self.losses
                    .note("a `stat` line did not parse, and its task is in no poll's table");
                continue;
            };
            // The resident set from `statm`, which `top` and `ps` read: the
            // kernel sums its per-cpu counters for it, where `stat`'s field is
            // the approximate reading (642 pages against 568 for one small
            // process here). `stat`'s stands where `statm` cannot be read.
            if let Some(resident) = std::fs::read_to_string(dir.join("statm"))
                .ok()
                .and_then(|statm| statm.split_whitespace().nth(1)?.parse().ok())
            {
                stat.rss = resident;
            }
            read.push((pid, tid, owner(&dir), stat));
        }
        let ts_ns = monotonic_ns();
        let booted_ns = boottime_ns();
        let interval_ns = ts_ns.saturating_sub(self.previous_ns);
        let tick_ns = 1_000_000_000 / self.ticks_per_s.max(1);

        let mut current = HashMap::with_capacity(read.len());
        let mut events = Vec::with_capacity(read.len());
        for (index, (pid, tid, uid, stat)) in read.into_iter().enumerate() {
            let ticks = stat.utime + stat.stime;
            let started_ns = stat.starttime.saturating_mul(tick_ns);
            let (since, ran_ticks, over_ns) = match self.previous.get(&(tid, stat.starttime)) {
                Some(before) if self.previous_ns != 0 => {
                    ("poll", ticks.saturating_sub(*before), interval_ns)
                }
                _ => ("start", ticks, booted_ns.saturating_sub(started_ns)),
            };
            current.insert((tid, stat.starttime), ticks);
            let busy = if over_ns == 0 {
                0.0
            } else {
                (ran_ticks * tick_ns) as f64 / over_ns as f64
            };
            let mut attrs = vec![
                Attr::float("busy", busy),
                Attr::text("since", since),
                Attr::uint("over_ns", over_ns),
                Attr::uint("pid", pid as u64),
                Attr::uint("tid", tid as u64),
                Attr::uint("ppid", stat.ppid.max(0) as u64),
                Attr::text("comm", stat.comm.clone()),
                Attr::text("state", stat.state.to_string()),
                Attr::uint("cpu_ns", ticks * tick_ns),
                Attr::uint("utime_ns", stat.utime * tick_ns),
                Attr::uint("stime_ns", stat.stime * tick_ns),
                Attr::uint("rss_bytes", stat.rss * self.page_bytes),
                Attr::uint("vsz_bytes", stat.vsize),
                Attr::uint("threads", stat.threads),
                Attr::int("priority", stat.priority),
                Attr::int("nice", stat.nice),
                Attr::uint("minflt", stat.minflt),
                Attr::uint("majflt", stat.majflt),
                Attr::uint("cpu", u64::from(stat.processor)),
                Attr::uint("age_ns", booted_ns.saturating_sub(started_ns)),
            ];
            if let Some(uid) = uid {
                attrs.push(Attr::uint("uid", u64::from(uid)));
            }
            events.push(Event {
                ts_ns,
                ts_err_ns: 0,
                kind: "proc.tasks".to_owned(),
                keys: vec![KeyValue::task(tid), KeyValue::comm(stat.comm)],
                attrs,
                collector_id: self.id.clone(),
                seq: seq + index as u64,
            });
        }
        self.previous = current;
        self.previous_ns = ts_ns;
        Ok(events)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_with_spaces_and_parentheses_parses() {
        let line = "1234 (a (weird) name) S 1 1234 1234 0 -1 4194560 120 0 3 0 \
                    250 75 0 0 20 0 4 0 9876 123456789 512 18446744073709551615 \
                    1 1 0 0 0 0 0 0 0 0 0 0 17 5 0 0 0 0 0";
        let stat = parse_stat(line).expect("a stat line");
        assert_eq!(stat.comm, "a (weird) name");
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.ppid, 1);
        assert_eq!(stat.minflt, 120);
        assert_eq!(stat.majflt, 3);
        assert_eq!(stat.utime, 250);
        assert_eq!(stat.stime, 75);
        assert_eq!(stat.priority, 20);
        assert_eq!(stat.nice, 0);
        assert_eq!(stat.threads, 4);
        assert_eq!(stat.starttime, 9876);
        assert_eq!(stat.vsize, 123456789);
        assert_eq!(stat.rss, 512);
        assert_eq!(stat.processor, 5);
    }

    #[test]
    fn our_own_stat_parses_against_what_status_says() {
        let stat = parse_stat(&std::fs::read_to_string("/proc/self/stat").unwrap()).unwrap();
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .map(|value| value.trim().to_owned())
                .unwrap()
        };
        assert_eq!(stat.comm, field("Name:"));
        assert_eq!(stat.ppid.to_string(), field("PPid:"));
    }

    #[test]
    fn the_machine_lists_this_process_once_with_its_pid() {
        let mut collector = Box::new(Prepare).attach(&Target::System).unwrap();
        let events = collector.sample(0).unwrap();
        let me = std::process::id() as u64;
        let mine: Vec<_> = events
            .iter()
            .filter(|event| event.attr("pid").and_then(|v| v.as_f64()) == Some(me as f64))
            .collect();
        assert_eq!(mine.len(), 1, "one event per process");
        assert!(events.len() > 1, "a machine runs more than this test");
        // Every event of a poll is stamped at the one instant.
        assert!(events.iter().all(|event| event.ts_ns == events[0].ts_ns));
        assert_eq!(
            mine[0].attr("since").map(|v| v.to_string()),
            Some("start".to_owned())
        );
        let again = collector.sample(1).unwrap();
        let mine = again
            .iter()
            .find(|event| event.attr("pid").and_then(|v| v.as_f64()) == Some(me as f64))
            .unwrap();
        assert_eq!(
            mine.attr("since").map(|v| v.to_string()),
            Some("poll".to_owned())
        );
    }

    #[test]
    fn a_process_lists_each_of_its_threads() {
        let me = std::process::id() as i32;
        let mut collector = Box::new(Prepare).attach(&Target::Pid(vec![me])).unwrap();
        let events = collector.sample(0).unwrap();
        let tids = numeric_entries(Path::new("/proc/self/task")).unwrap();
        // The test harness may start or end a thread between the two reads,
        // so the claim is about our own thread and the shape.
        let own = unsafe { libc::gettid() } as u64;
        assert!(events
            .iter()
            .any(|event| event.attr("tid").and_then(|v| v.as_f64()) == Some(own as f64)));
        assert!(!tids.is_empty());
        assert!(events
            .iter()
            .all(|event| event.attr("pid").and_then(|v| v.as_f64()) == Some(me as f64)));
    }

    #[test]
    fn every_filter_field_the_catalog_offers_is_on_the_events() {
        let catalog = crate::catalog::Catalog::embedded().unwrap();
        let entry = catalog.get(&"proc.tasks".parse().unwrap()).unwrap();
        let mut collector = Box::new(Prepare).attach(&Target::System).unwrap();
        let events = collector.sample(0).unwrap();
        let mine = events
            .iter()
            .find(|event| {
                event.attr("pid").and_then(|v| v.as_f64()) == Some(std::process::id() as f64)
            })
            .unwrap();
        for field in &entry.filter_fields {
            assert!(mine.attr(field).is_some(), "`{field}` is not on the events");
        }
    }

    #[test]
    fn a_target_that_is_neither_is_refused_by_name() {
        let Err(error) = Box::new(Prepare).attach(&Target::Cgroup("/x".into())) else {
            panic!("a cgroup is not a process");
        };
        assert!(error.contains("neither"), "{error}");
    }
}
