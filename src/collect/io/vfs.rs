//! `io.syscall` and `io.fs`, over fentry, bucketed in the kernel.
//!
//! Two layers of one read. `src/bpf/io_vfs.rs` says where each is timed and
//! why; this file is what an event carries, what is refused, and what is noted.
//!
//! **The series is different on purpose.** `io.syscall` is keyed by task first,
//! because it answers "what does this program wait for" and a descriptor number
//! means nothing outside the process that holds it. `io.fs` is keyed by file
//! first -- `file:<dev>+<ino>`, the superblock's device and the inode -- because it
//! answers "which file is slow", and the same file read by two processes is one
//! series there and two here.

use std::collections::BTreeMap;

use super::histogram::{HistogramCollector, Rows, Source};
use crate::bpf::attach::fentry::{self, Narrowing, Row};
use crate::bpf::attach::io_vfs::{self, Layer, Op, VfsProgram};
use crate::collect::{not_built, Collector, Prepared};
use crate::event::{Attr, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

pub struct Prepare {
    pub layer: Layer,
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Layer {
    fn source(self) -> &'static str {
        match self {
            Layer::Syscall => "io.syscall",
            Layer::Fs => "io.fs",
        }
    }

    fn what(self) -> &'static str {
        match self {
            Layer::Syscall => "the time a read or a write took as the application made it",
            Layer::Fs => "the time a read or a write of a regular file took inside the VFS",
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let source = self.layer.source();
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot measure {}: it is the gap between the two halves of one call, \
                 and a sample that catches one without the other has no duration",
                self.mode,
                self.layer.what()
            ));
        }
        let narrowing = narrowing(source, target)?;
        let layer = self.layer;
        let rows = Box::new(move || -> Result<Box<dyn Rows>, String> {
            Ok(Box::new(VfsProgram::load(layer, narrowing)?))
        });
        Ok(Box::new(HistogramCollector::new(
            Source {
                kind: source,
                collector: match layer {
                    Layer::Syscall => "io.syscall/bpf:fentry",
                    Layer::Fs => "io.fs/bpf:fentry",
                },
                call: "a read or a write",
                load: rows,
                describe: describe(layer),
                notes: vec![coverage(layer)],
            },
            self.mode,
            self.filter,
        )))
    }
}

/// Whose calls the programs keep, or the refusal for a target they cannot
/// narrow to.
///
/// Shared with `net.socket`, which narrows the same way for the same reason.
pub(in crate::collect) fn narrowing(source: &str, target: &Target) -> Result<Narrowing, String> {
    match target {
        Target::System => Ok(Narrowing::default()),
        Target::Pid(pids) => Ok(Narrowing {
            tgid: Some(first(source, pids, "pid")? as u32),
            tid: None,
        }),
        Target::Tid(tids) => Ok(Narrowing {
            tgid: None,
            tid: Some(first(source, tids, "tid")? as u32),
        }),
        Target::Cgroup(_) | Target::Container(_) => Err(not_built(format!(
            "`{source}` can be scoped to a cgroup -- the catalog says so -- but this build \
             narrows by thread group and thread id only: a cgroup filter needs \
             `bpf_get_current_cgroup_id` in the program, and the request is written down in \
             `docs/open-findings.md`"
        ))),
        other => Err(format!(
            "a {} target names no task, and `{source}` times calls a task makes; use --system to \
             watch the whole machine",
            other.kind()
        )),
    }
}

fn first(source: &str, ids: &[i32], kind: &str) -> Result<i32, String> {
    ids.first().copied().ok_or_else(|| {
        format!("a {kind} target with no {kind} in it names nothing for `{source}` to time")
    })
}

/// What the functions timed do not see, which is part of what the number is.
fn coverage(layer: Layer) -> String {
    match layer {
        Layer::Syscall => "timed at ksys_read, ksys_write, ksys_pread64 and ksys_pwrite64: \
                           readv/writev, sendfile, splice, io_uring and page faults on a \
                           mapping are I/O this does not see"
            .to_owned(),
        Layer::Fs => "timed at vfs_read and vfs_write, regular files only: a pipe, a socket or \
                      a terminal is not a filesystem, and readv/writev, io_uring and mmap \
                      faults do not pass through these two functions"
            .to_owned(),
    }
}

/// Which filesystem a superblock's device belongs to, by `major:minor`, from
/// `/proc/self/mountinfo`.
///
/// Read once per run and refreshed when a device turns up that is not in it: a
/// filesystem mounted mid-run is the one case the first read misses.
pub(super) struct Mounts {
    by_dev: BTreeMap<(u32, u32), (String, String)>,
    names: BTreeMap<(u32, u32), String>,
}

impl Mounts {
    pub(super) fn read() -> Self {
        Self {
            by_dev: parse_mountinfo(
                &std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(),
            ),
            names: super::device_names(),
        }
    }

    pub(super) fn lookup(&mut self, dev: (u32, u32)) -> (String, String, String) {
        if !self.by_dev.contains_key(&dev) {
            *self = Self::read();
        }
        let (fs, mount) = self
            .by_dev
            .get(&dev)
            .cloned()
            .unwrap_or_else(|| ("unknown".to_owned(), String::new()));
        let name = self
            .names
            .get(&dev)
            .cloned()
            .unwrap_or_else(|| format!("{}:{}", dev.0, dev.1));
        (fs, mount, name)
    }
}

/// `major:minor` to `(fstype, mount point)`, first mount of each device wins.
fn parse_mountinfo(text: &str) -> BTreeMap<(u32, u32), (String, String)> {
    let mut mounts = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        let (Some(dev), Some(mount), Some(fstype)) =
            (fields.get(2), fields.get(4), fields.get(separator + 1))
        else {
            continue;
        };
        let Some((major, minor)) = dev.split_once(':') else {
            continue;
        };
        let (Ok(major), Ok(minor)) = (major.parse(), minor.parse()) else {
            continue;
        };
        mounts
            .entry((major, minor))
            .or_insert_with(|| ((*fstype).to_owned(), (*mount).to_owned()));
    }
    mounts
}

fn describe(layer: Layer) -> super::histogram::Describe {
    let mut mounts = (layer == Layer::Fs).then(Mounts::read);
    Box::new(move |row: &Row| {
        let op = Op::from_row(row.op);
        let name = op.map(Op::name).unwrap_or("unknown");
        let direction = match op {
            Some(op) if op.is_write() => "write",
            Some(_) => "read",
            None => "unknown",
        };
        let mut attrs = vec![Attr::text("op", direction), Attr::text("syscall", name)];
        match layer {
            Layer::Syscall => {
                attrs.push(Attr::uint("fd", row.object));
                (vec![KeyValue::task(row.tid as i32)], attrs)
            }
            Layer::Fs => {
                let dev = io_vfs::major_minor(row.dev);
                let (fs, _, device) = mounts.as_mut().expect("read for io.fs").lookup(dev);
                attrs.push(Attr::uint("ino", row.object));
                attrs.push(Attr::text("dev", device));
                attrs.push(Attr::uint("sb_dev", io_vfs::stat_dev(row.dev)));
                attrs.push(Attr::text("fs", fs));
                (
                    vec![
                        KeyValue::File {
                            dev: io_vfs::stat_dev(row.dev),
                            ino: row.object,
                        },
                        KeyValue::task(row.tid as i32),
                    ],
                    attrs,
                )
            }
        }
    })
}

impl Rows for VfsProgram {
    fn drain(&self) -> Result<(Vec<Row>, fentry::DrainLosses), String> {
        VfsProgram::drain(self)
    }

    fn still_pending(&self) -> usize {
        VfsProgram::still_pending(self)
    }

    fn dropped(&self) -> (u64, u64) {
        VfsProgram::dropped(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The superblock device of a btrfs subvolume is anonymous and major zero,
    /// and it is on the mountinfo line all the same: that is where the
    /// filesystem's name has to come from.
    #[test]
    fn a_mountinfo_line_names_the_filesystem_on_a_device() {
        let text = "36 25 0:41 / /home rw,relatime shared:1 - btrfs /dev/nvme0n1p6 rw\n\
                    37 25 259:3 / /boot rw - vfat /dev/nvme0n1p1 rw\n\
                    38 25 0:41 /other /mnt rw - btrfs /dev/nvme0n1p6 rw\n";
        let mounts = parse_mountinfo(text);
        assert_eq!(
            mounts.get(&(0, 41)),
            Some(&("btrfs".to_owned(), "/home".to_owned()))
        );
        assert_eq!(mounts.get(&(259, 3)).map(|m| m.0.as_str()), Some("vfat"));
    }

    /// Optional fields before the separator vary in number, which is why the
    /// separator is searched for rather than counted to.
    #[test]
    fn optional_fields_do_not_move_the_filesystem_type() {
        let text = "36 25 8:1 / /data rw shared:1 master:2 propagate_from:3 - xfs /dev/sda1 rw\n";
        assert_eq!(parse_mountinfo(text)[&(8, 1)].0, "xfs");
    }
}
