//! `mem.cgroup` over the cgroup v2 memory controller, and the cgroup
//! resolution the rest of the family shares.
//!
//! This is the number a container is actually killed by. RSS, PSS and USS are
//! properties of an address space; `memory.current` is a property of the
//! accounting domain the kernel charges pages to, and it is the one the OOM
//! killer and `memory.max` compare against. They disagree for reasons that are
//! not error: page cache read by the process is charged here and is in nobody's
//! RSS, a page shared with a process in another cgroup is charged once to
//! whoever faulted it first, and kernel memory -- stacks, page tables, slab --
//! is charged here and appears in no userspace footprint at all.
//!
//! So the breakdown is carried beside the total rather than left to be guessed
//! at: `memory.stat` says how much of the charge is anonymous, how much is page
//! cache and how much is the kernel's, and the three answer different
//! questions about the same number.
//!
//! **A cgroup path is not a target.** A request may name the cgroup directly,
//! or name a process and expect the tool to find the cgroup it has been placed
//! in -- which is what a person asking "what is this container charged?"
//! actually means. Both resolve here.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::cadence::Cadence;
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

pub(super) const ROOT: &str = "/sys/fs/cgroup";

/// One cgroup, as a directory and as the identity an event carries.
///
/// The identity is the directory's inode, which is what the kernel itself uses
/// as a cgroup id -- `bpf_get_current_cgroup_id` returns the same number -- so
/// an event from this collector and an event from an eBPF one can be joined
/// without either knowing how the other found the cgroup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cgroup {
    pub directory: PathBuf,
    pub id: u64,
}

impl Cgroup {
    /// Resolves a target to the cgroup whose files should be read.
    ///
    /// A `--cgroup` path may be written either way round: rooted at the
    /// hierarchy, the way `/proc/[pid]/cgroup` writes it, or as the mounted
    /// path. Both name the same cgroup, and a tool that accepted only one of
    /// them would refuse the form its own output uses.
    pub fn resolve(target: &Target) -> Result<Self, String> {
        match target {
            Target::Cgroup(path) => Self::at(directory(path)),
            // A launched program is bound to its pid before anything attaches,
            // so this is also how `-- ./prog` arrives here.
            Target::Pid(_) | Target::Tid(_) => {
                let pid = target.first_pid().expect("a pid target names a pid");
                Self::of_pid(pid)
            }
            Target::Container(_) => Err(
                "a container has to be resolved to its cgroup first, which is not built yet; \
                 pass `--cgroup` with the path"
                    .to_owned(),
            ),
            other => Err(format!(
                "cgroup accounting is charged per cgroup, and a {} target names none",
                other.kind()
            )),
        }
    }

    /// The cgroup a process has been placed in.
    ///
    /// A process in the root cgroup is not an error: `0::/` is what
    /// `/proc/[pid]/cgroup` writes for a task nobody moved, and it resolves to
    /// the hierarchy root like any other path. Whether that root can answer the
    /// question asked of it is decided further down, where the answer can name
    /// the file that is missing.
    pub fn of_pid(pid: i32) -> Result<Self, String> {
        let path = format!("/proc/{pid}/cgroup");
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {path}: {error}"))?;
        // Unified hierarchy lines are `0::<path>`. A machine still running
        // cgroup v1 controllers writes other lines beside it, and none of them
        // name a v2 cgroup.
        let relative = text
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(|| {
                format!(
                    "{path} names no unified (`0::`) cgroup, so this process is on a cgroup v1 \
                     hierarchy, which has a different memory controller and a different layout"
                )
            })?;
        Self::at(directory(Path::new(relative.trim())))
    }

    fn at(directory: PathBuf) -> Result<Self, String> {
        let id = std::fs::metadata(&directory)
            .map_err(|error| format!("cannot stat {}: {error}", directory.display()))?
            .ino();
        Ok(Self { directory, id })
    }

    pub fn key(&self) -> KeyValue {
        KeyValue::Cgroup { id: self.id }
    }

    /// The path of one interface file, whether or not it exists.
    pub fn file(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }

    /// Reads an interface file, telling a cgroup that went away from a read
    /// that merely failed.
    ///
    /// A cgroup is removed the moment its last process leaves, which for a
    /// launched program is the normal end of a run rather than a lost sample.
    pub fn read(&self, name: &str) -> Result<String, SampleError> {
        let path = self.file(name);
        std::fs::read_to_string(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SampleError::TargetGone(format!("{} is gone", self.directory.display()))
            } else {
                SampleError::Transient(format!("{}: {error}", path.display()))
            }
        })
    }

    /// Whether this is the hierarchy root, which is accounted differently from
    /// every cgroup below it.
    pub fn is_root(&self) -> bool {
        self.directory == Path::new(ROOT)
    }
}

/// Turns either spelling of a cgroup path into the directory that holds its
/// interface files.
pub(super) fn directory(target: &Path) -> PathBuf {
    if target.starts_with(ROOT) {
        target.to_path_buf()
    } else {
        PathBuf::from(ROOT).join(target.strip_prefix("/").unwrap_or(target))
    }
}

/// One `name value` line per field, which is how every cgroup `*.stat` file is
/// written.
pub(super) fn stat_fields(text: &str) -> Vec<(&str, u64)> {
    text.lines()
        .filter_map(|line| line.split_once(' '))
        .filter_map(|(name, value)| Some((name, value.trim().parse().ok()?)))
        .collect()
}

pub(super) fn stat_field(fields: &[(&str, u64)], name: &str) -> Option<u64> {
    fields
        .iter()
        .find(|(field, _)| *field == name)
        .map(|(_, value)| *value)
}

/// A limit file, which holds either a number of bytes or the word `max`.
///
/// `max` is not a number and is not reported as one: a cgroup with no limit and
/// a cgroup limited to `u64::MAX` bytes are the same thing to the kernel, but
/// an event carrying `18446744073709551615` as a ceiling is a number that will
/// be plotted.
fn limit_bytes(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// Holds the mode until the target is known.
pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let cgroup = Cgroup::resolve(target)?;
        if !cgroup.file("memory.current").exists() {
            return Err(if cgroup.is_root() {
                format!(
                    "{} is the hierarchy root, which the kernel does not charge: it publishes \
                     no `memory.current`, so a process that nobody moved into a cgroup has no \
                     cgroup accounting to read. `mem.rss` measures such a process.",
                    cgroup.directory.display()
                )
            } else {
                format!(
                    "{} does not exist: either the cgroup is not there, or the memory \
                     controller is not enabled on it (`cgroup.subtree_control` of its parent)",
                    cgroup.file("memory.current").display()
                )
            });
        }
        let interval = match self.0 {
            Mode::Poll { interval } => std::time::Duration::from_nanos(interval.as_nanos()),
            _ => std::time::Duration::ZERO,
        };
        Ok(Box::new(CgroupCollector::new(cgroup, interval)))
    }
}

/// Reads one cgroup's memory accounting.
pub struct CgroupCollector {
    id: CollectorId,
    cgroup: Cgroup,
    cadence: Cadence,
}

/// The fields of `memory.stat` this collector carries, and the name each one
/// gets on the event.
///
/// Not every field: `memory.stat` has upwards of forty, most of them reclaim
/// bookkeeping that says nothing about what the cgroup is charged. These four
/// are the decomposition of the charge itself -- anonymous memory, page cache,
/// the kernel's own allocations, and the shared memory that belongs to neither
/// of the first two.
const BREAKDOWN: [(&str, &str); 4] = [
    ("anon", "anon_bytes"),
    ("file", "file_bytes"),
    ("kernel", "kernel_bytes"),
    ("shmem", "shmem_bytes"),
];

impl CgroupCollector {
    pub fn new(cgroup: Cgroup, interval: std::time::Duration) -> Self {
        Self {
            id: CollectorId::new("mem.cgroup/fs:cgroup"),
            cgroup,
            cadence: Cadence::new(interval),
        }
    }
}

impl Collector for CgroupCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if !self.cadence.due(monotonic_ns()) {
            return Ok(Vec::new());
        }
        let current = self.cgroup.read("memory.current")?;
        let ts_ns = monotonic_ns();
        let bytes = current.trim().parse::<u64>().map_err(|error| {
            SampleError::Transient(format!(
                "{}: {error}",
                self.cgroup.file("memory.current").display()
            ))
        })?;

        let mut attrs = vec![
            Attr::uint("bytes", bytes),
            Attr::text("read_from", "memory.current"),
        ];
        // The peak is the kernel's own high-water mark over the cgroup's whole
        // life, not over this run. It is carried because it is the number that
        // decides whether a limit was ever approached, and named so that it
        // cannot be read as something this run measured.
        if let Some(peak) = self
            .cgroup
            .read("memory.peak")
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
        {
            attrs.push(Attr::uint("peak_bytes_since_boot", peak));
        }
        // A limit that is not set is absent rather than infinite.
        if let Some(max) = self
            .cgroup
            .read("memory.max")
            .ok()
            .and_then(|text| limit_bytes(&text))
        {
            attrs.push(Attr::uint("max_bytes", max));
        }
        if let Ok(stat) = self.cgroup.read("memory.stat") {
            let fields = stat_fields(&stat);
            for (field, name) in BREAKDOWN {
                if let Some(value) = stat_field(&fields, field) {
                    attrs.push(Attr::uint(name, value));
                }
            }
        }

        Ok(vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.cgroup".to_owned(),
            keys: vec![self.cgroup.key()],
            attrs,
            collector_id: self.id.clone(),
            seq,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::mem::testing::Scratch;

    #[test]
    fn either_spelling_of_a_cgroup_path_names_the_same_directory() {
        let mounted = directory(Path::new("/sys/fs/cgroup/user.slice"));
        let relative = directory(Path::new("/user.slice"));
        assert_eq!(mounted, relative);
        assert_eq!(mounted, Path::new("/sys/fs/cgroup/user.slice"));
    }

    /// Ground truth against the kernel: the cgroup this process reports for
    /// itself is the one `/proc/self/cgroup` names, and its id is the inode the
    /// kernel would hand an eBPF program.
    #[test]
    fn a_pid_resolves_to_the_cgroup_the_kernel_put_it_in() {
        let pid = std::process::id() as i32;
        let resolved = match Cgroup::of_pid(pid) {
            Ok(cgroup) => cgroup,
            // A machine on cgroup v1 has no unified hierarchy to resolve into.
            Err(reason) => {
                assert!(reason.contains("cgroup v1"), "{reason}");
                return;
            }
        };
        let text = std::fs::read_to_string("/proc/self/cgroup").expect("our own cgroup file");
        let relative = text
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .expect("a unified line");
        assert_eq!(
            resolved.directory,
            PathBuf::from(ROOT).join(relative.trim().trim_start_matches('/'))
        );
        assert_eq!(
            resolved.id,
            std::fs::metadata(&resolved.directory)
                .expect("the directory we just resolved")
                .ino()
        );
    }

    /// The root cgroup is not a parse failure and not a silent zero: it is a
    /// cgroup the kernel does not charge, and the refusal has to say that
    /// rather than report that the file is missing.
    #[test]
    fn the_hierarchy_root_is_refused_by_name_rather_than_answered() {
        // In a cgroup namespace `/` is the namespace's root, an ordinary
        // cgroup that the kernel charges, and answering it is right.
        if !crate::bpf::pidns::in_initial_cgroup_namespace() {
            eprintln!("skipped: `omn` runs in a cgroup namespace, whose root is no hierarchy root");
            return;
        }
        let refused = Box::new(Prepare(Mode::Snapshot))
            .attach(&Target::Cgroup(PathBuf::from("/")))
            .err()
            .expect("the root publishes no memory.current");
        assert!(
            refused.contains("hierarchy root") && refused.contains("memory.current"),
            "{refused}"
        );
    }

    #[test]
    fn a_target_that_names_no_cgroup_is_refused_rather_than_answered_machine_wide() {
        let refused = Box::new(Prepare(Mode::Snapshot))
            .attach(&Target::System)
            .err()
            .expect("the machine is not a cgroup");
        assert!(refused.contains("names none"), "{refused}");
    }

    /// A limit file holds a number or the word `max`, and `max` is not a
    /// number: reporting it as one puts 16 exabytes on a chart.
    #[test]
    fn an_unset_limit_is_absent_rather_than_a_very_large_number() {
        assert_eq!(limit_bytes("max\n"), None);
        assert_eq!(limit_bytes("134217728\n"), Some(134_217_728));
    }

    /// Ground truth for the reader itself: the file is written `name value`
    /// per line, and a field that is not there is absent rather than zero.
    #[test]
    fn the_stat_reader_finds_what_is_there_and_invents_nothing() {
        let fields = stat_fields("anon 4096\nfile 8192\nkernel 128\n");
        assert_eq!(stat_field(&fields, "anon"), Some(4096));
        assert_eq!(stat_field(&fields, "shmem"), None);
    }

    /// Ground truth against the kernel's own accounting: a cgroup this test
    /// makes, with a process in it that touches a known number of bytes, has to
    /// be charged for them.
    ///
    /// The charge is compared against `memory.current` read independently a
    /// moment later rather than against the allocation alone, because the
    /// cgroup also holds the shell that put itself there and whatever page
    /// cache the read faulted in. What the allocation bounds is the *rise*.
    #[test]
    fn a_known_allocation_inside_a_cgroup_is_charged_to_it() {
        const TOUCHED: u64 = 64 * 1024 * 1024;
        let Some(scratch) = Scratch::cgroup("charge") else {
            eprintln!("skipped: this user cannot create a cgroup under its own slice");
            return;
        };

        let mut collector = CgroupCollector::new(
            Cgroup::at(scratch.path().to_path_buf()).expect("the cgroup we just made"),
            std::time::Duration::ZERO,
        );
        let before = collector.sample(0).expect("an empty cgroup still reads");
        let before_bytes = before[0].attr("bytes").unwrap().as_f64().unwrap();

        let mut child = scratch.spawn_toucher(TOUCHED);
        let after = collector.sample(1).expect("the cgroup is still there");
        let after_bytes = after[0].attr("bytes").unwrap().as_f64().unwrap();
        child.stop();

        let charged = after_bytes - before_bytes;
        assert!(
            charged >= 0.9 * TOUCHED as f64,
            "a process that touched {TOUCHED} bytes inside the cgroup moved memory.current by \
             {charged}"
        );
        assert_eq!(after[0].keys, vec![KeyValue::Cgroup { id: scratch.id() }]);
        assert!(after[0].attr("anon_bytes").is_some(), "{:?}", after[0]);
    }
}
