//! Counting a cgroup rather than a task or a cpu.
//!
//! `perf_event_open` has a third shape beside a task and a cpu: an event on a
//! cpu that counts only while a task of one cgroup v2 directory -- or of a
//! cgroup below it -- runs there (`PERF_FLAG_PID_CGROUP`). So a cgroup is one
//! event per cpu, like a machine, and what it counts is one group of tasks,
//! like a process.
//!
//! It is reported like a process: **one event, the sum over the cpus**. A
//! machine's cpus are what `--system` asks about and are kept apart; a
//! cgroup's cpus are where its tasks happened to run, and a count split across
//! them describes the scheduler rather than the cgroup. The times add too,
//! the way a process's threads do: each cpu's enabled time runs only while the
//! cgroup is on that cpu.
//!
//! The privilege is a cpu's. The kernel asks `perf_allow_cpu` of a cgroup
//! event, because the tasks in a cgroup may belong to anybody; the refusal
//! names the sysctl and the capability, and nothing falls back to counting
//! this process instead.
//!
//! **A container is served as its cgroup.** `omn` binds `--container NAME` to
//! the cgroup its runtime names before resolution (`request::find_container`),
//! so what reaches this collector is `Target::Cgroup`. A container target
//! falls back to `cgroup` in the catalog's model, and the entries this
//! collector serves declare nothing for it.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use super::counters::{measured, measured_tick, multiplexing_notes, Spec, Ticks};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::perf::PerCpuGroups;

/// Where the cgroup v2 hierarchy is mounted.
pub(in crate::collect) const ROOT: &str = "/sys/fs/cgroup";

/// See `percpu.rs`: nothing is produced until the window closes, and the tick
/// only keeps the observed window from being rounded up.
const TICK: StdDuration = StdDuration::from_millis(10);

/// A cgroup target, resolved to its directory and its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::collect) struct Cgroup {
    pub directory: PathBuf,
    /// The directory's inode number, which is the id the kernel itself uses
    /// for the cgroup (`bpf_get_current_cgroup_id` returns the same number).
    pub id: u64,
}

impl Cgroup {
    /// Resolves either spelling of a cgroup -- rooted at the hierarchy, the
    /// way `/proc/self/cgroup` writes it, or as the mounted path -- and checks
    /// that it is a directory of the v2 hierarchy, before anything is opened.
    pub fn resolve(target: &Path) -> Result<Self, String> {
        let directory = directory(target);
        if !directory.join("cgroup.procs").exists() {
            return Err(format!(
                "{} is not a cgroup v2 directory (it has no cgroup.procs): a cgroup target \
                 names a directory of the hierarchy mounted at {ROOT}, written either way \
                 /proc/<pid>/cgroup writes it or as the mounted path",
                directory.display()
            ));
        }
        let id = std::fs::metadata(&directory)
            .map(|metadata| metadata.ino())
            .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
        Ok(Self { directory, id })
    }

    pub fn key(&self) -> KeyValue {
        KeyValue::Cgroup { id: self.id }
    }
}

/// Turns either spelling of a cgroup path into the directory that holds its
/// interface files.
pub(in crate::collect) fn directory(target: &Path) -> PathBuf {
    if target.starts_with(ROOT) {
        target.to_path_buf()
    } else {
        PathBuf::from(ROOT).join(target.strip_prefix("/").unwrap_or(target))
    }
}

/// One source's counters over one cgroup, on every online cpu, read at the
/// end.
pub struct CgroupCounters {
    id: CollectorId,
    spec: &'static Spec,
    cgroup: Cgroup,
    cpus: Vec<u32>,
    groups: Option<PerCpuGroups>,
    losses: Losses,
    notes: Vec<String>,
    /// A poll's tick and the previous sum; `None` counts the window once.
    poll: Option<StdDuration>,
    ticks: Ticks,
    opened_ns: u64,
    seq: u64,
}

impl CgroupCounters {
    /// Reads the cgroup's sum every `poll` while the counters run, one event
    /// per tick, instead of once for the window.
    pub fn polled(mut self, poll: Option<StdDuration>) -> Self {
        self.poll = poll;
        self
    }

    /// The sum over the cpus as an event, noting each cpu whose read failed.
    fn event(
        &mut self,
        (total, failed): (
            Option<Vec<crate::perf::Reading>>,
            Vec<(u32, std::io::Error)>,
        ),
    ) -> Option<Event> {
        for (cpu, error) in failed {
            // A cpu taken offline mid-run: the others counted a real window,
            // and the count is short by whatever ran there, which is said.
            self.losses.failed_reads += 1;
            self.losses.note(format!(
                "reading `{}` on cpu {cpu} failed ({error}), so what the cgroup ran there is \
                 not in the count",
                self.spec.source
            ));
        }
        let readings = total?;
        let ts_ns = monotonic_ns();
        let mut attrs = if self.poll.is_some() {
            let (delta, interval_ns) = self.ticks.next(self.opened_ns, ts_ns, readings);
            measured_tick(self.spec, &delta, interval_ns, &mut self.losses)?
        } else {
            let whose = format!("the cgroup {}", self.cgroup.directory.display());
            self.notes.extend(multiplexing_notes(&readings, &whose));
            measured(self.spec, &readings, &mut self.losses)?
        };
        attrs.push(Attr::uint("cpus", self.cpus.len() as u64));
        let seq = self.seq;
        self.seq += 1;
        Some(Event {
            ts_ns,
            ts_err_ns: 0,
            kind: self.spec.source.to_owned(),
            keys: vec![self.cgroup.key()],
            attrs,
            collector_id: self.id.clone(),
            seq,
        })
    }
    pub fn new(cgroup: Cgroup, cpus: Vec<u32>, spec: &'static Spec) -> Self {
        Self {
            id: CollectorId::new(format!("{}/{}", spec.source, spec.backend)),
            spec,
            cgroup,
            cpus,
            groups: None,
            losses: Losses::default(),
            notes: Vec::new(),
            poll: None,
            ticks: Ticks::default(),
            opened_ns: 0,
            seq: 0,
        }
    }
}

impl Collector for CgroupCounters {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        self.spec.value_attr()
    }

    fn start(&mut self) -> Result<(), String> {
        let groups =
            PerCpuGroups::open_cgroup(self.spec.counters, &self.cgroup.directory, &self.cpus)
                .map_err(|reason| {
                    format!(
                        "`{}` over the cgroup {}: {reason}",
                        self.spec.source,
                        self.cgroup.directory.display()
                    )
                })?;
        if groups.kernel_excluded() {
            self.notes.push(format!(
                "this machine would only open `{}` for user mode, so kernel-mode events of \
                 the cgroup's tasks are not in this count",
                self.spec.source
            ));
        }
        groups.enable()?;
        self.opened_ns = monotonic_ns();
        self.groups = Some(groups);
        Ok(())
    }

    fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
        if self.poll.is_none() {
            return Ok(Vec::new());
        }
        let Some(sum) = self.groups.as_ref().map(PerCpuGroups::read_sum) else {
            return Ok(Vec::new());
        };
        Ok(self.event(sum).into_iter().collect())
    }

    fn stop(&mut self) -> Vec<Event> {
        let Some(groups) = self.groups.take() else {
            return Vec::new();
        };
        let total = groups.read_total();
        self.event(total).into_iter().collect()
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(self.poll.unwrap_or(TICK))
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        self.notes.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_spellings_of_a_cgroup_name_the_same_directory() {
        assert_eq!(
            directory(Path::new("/user.slice")),
            PathBuf::from("/sys/fs/cgroup/user.slice")
        );
        assert_eq!(
            directory(Path::new("/sys/fs/cgroup/user.slice")),
            PathBuf::from("/sys/fs/cgroup/user.slice")
        );
        assert_eq!(directory(Path::new("/")), PathBuf::from("/sys/fs/cgroup"));
    }

    /// A path that is not a cgroup is refused as that, before any event is
    /// opened -- not as a permission problem, which is what `perf_event_open`
    /// would have said about a descriptor that is not a cgroup's.
    #[test]
    fn a_directory_that_is_not_a_cgroup_is_refused_by_name() {
        let error = Cgroup::resolve(Path::new("/sys/fs/cgroup/omn-no-such-cgroup"))
            .expect_err("a cgroup that does not exist");
        assert!(error.contains("not a cgroup v2 directory"), "{error}");
        assert!(error.contains("omn-no-such-cgroup"), "{error}");
    }

    /// The id is the directory's inode, which the eBPF sources report as the
    /// cgroup's id: two sources joined on `cgroup` have to agree on it.
    #[test]
    fn the_id_is_the_directorys_inode() {
        let Ok(cgroup) = Cgroup::resolve(Path::new("/")) else {
            eprintln!("skipped: no cgroup v2 hierarchy is mounted at {ROOT}");
            return;
        };
        let inode = std::fs::metadata(ROOT).unwrap().ino();
        assert_eq!(cgroup.id, inode);
        assert_eq!(cgroup.key(), KeyValue::Cgroup { id: inode });
    }
}
