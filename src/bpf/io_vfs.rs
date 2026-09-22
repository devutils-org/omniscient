//! `io.syscall` and `io.fs`: two layers of one read, timed by fentry.
//!
//! A `read(2)` passes three layers this family keeps apart, and each is the
//! gap between two moments of the same call:
//!
//! ```text
//!   ksys_read ─┬─ fdget_pos ── vfs_read ─┬─ rw_verify_area ── ->read_iter ── ... block
//!              │                         │
//!              └──────── io.syscall ─────┴──────────── io.fs ──────────────┘
//! ```
//!
//! **`io.syscall` is timed at `ksys_read`, `ksys_write`, `ksys_pread64` and
//! `ksys_pwrite64`**: the functions the four syscalls are one line around, so
//! the interval is the call the application made, less only the entry stub.
//! What that adds over the layer below is the descriptor: the table lookup and,
//! for a file shared between threads, the file position lock, which serialises
//! two readers of one descriptor in the syscall and never in the filesystem.
//! The row is keyed by the descriptor, which is what the catalog's `fd` filter
//! field has always needed and what nothing below this layer knows.
//!
//! **`io.fs` is timed at `vfs_read` and `vfs_write`, for regular files only.**
//! That is the decision the seventh wave left, and the reasons are three:
//!
//! 1. *It is the one place every filesystem passes.* The alternative the
//!    catalog carried was `tracepoint:ext4:*`, which answers for ext4 on a
//!    machine running btrfs -- this one -- and for nothing at all on xfs. A
//!    function below the VFS but above the filesystem would be `->read_iter`,
//!    which is an indirect call with a different target per filesystem, and
//!    choosing a target is choosing a filesystem again.
//! 2. *The filesystem is named rather than chosen.* Each row carries the
//!    superblock's device, which `/proc/self/mountinfo` maps to a filesystem
//!    type, so a machine with btrfs and xfs mounted gets both answers, each
//!    labelled with its own name.
//! 3. *A pipe is not a filesystem.* `vfs_read` also serves pipes, sockets,
//!    terminals and eventfds, and a reader blocked on an empty pipe for ten
//!    seconds is not ten seconds of filesystem latency. So the entry half reads
//!    `file->f_inode->i_mode` and times regular files only -- and that is the
//!    other half of the separation the scenarios check: a pipe wait is in
//!    `io.syscall` and not in `io.fs`, by construction.
//!
//! Both sources fire on every read and write on the machine, so both **count
//! in the kernel**: a row per `(task, op, object, log2 bucket)` holding how
//! many calls, their exact total and the bytes they returned, which is the
//! shape `src/bpf/runqlat.rs` worked out and `fentry::HistogramProgram` holds
//! once for every source of this shape.

use crate::bpf::attach::fentry::{
    self, HistogramProgram, HistogramSpec, Narrowing, Timed, BPF_H, FILL_DEV, FILL_OBJECT, SKIP,
};
use crate::bpf::btf::Btf;
use crate::bpf::insn::{Insn, Program, BPF_DW, BPF_W, R10};

/// What a row's `op` says the call was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read = 0,
    Write = 1,
    Pread = 2,
    Pwrite = 3,
}

impl Op {
    pub fn from_row(op: u32) -> Option<Self> {
        match op {
            0 => Some(Op::Read),
            1 => Some(Op::Write),
            2 => Some(Op::Pread),
            3 => Some(Op::Pwrite),
            _ => None,
        }
    }

    /// The name a reader filters on, which is the syscall's.
    pub fn name(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
            Op::Pread => "pread64",
            Op::Pwrite => "pwrite64",
        }
    }

    pub fn is_write(self) -> bool {
        matches!(self, Op::Write | Op::Pwrite)
    }
}

/// The two layers this file times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// `io.syscall`: the descriptor-level call.
    Syscall,
    /// `io.fs`: the file-level call, regular files only.
    Fs,
}

/// The prototypes each layer reads. The descriptor is the first argument of
/// the syscall layer, the file the first of the VFS layer, and the return value
/// is in the slot after the last argument of either.
const KSYS_RW: [&str; 3] = ["unsigned int", "char *", "size_t"];
const KSYS_PRW: [&str; 4] = ["unsigned int", "char *", "size_t", "loff_t"];
const VFS_RW: [&str; 4] = ["file *", "char *", "size_t", "loff_t *"];

/// `S_IFMT` and `S_IFREG`, from the frozen `stat(2)` ABI.
const S_IFMT: i32 = 0o170000;
const S_IFREG: i32 = 0o100000;

/// Threads that may be inside a timed call at once. Every thread on the
/// machine can be, and on a machine doing I/O most of them are.
const MAX_PENDING: u32 = 16384;

/// Distinct rows one drain interval may hold. A thread touches a handful of
/// descriptors or files and each lands in a handful of buckets, so this is far
/// above an ordinary interval -- and a workload that exceeds it is counted as
/// losing calls, which is the loss path the scenarios drive.
pub const MAX_ROWS: u32 = 32768;

/// The loaded programs of one layer.
pub struct VfsProgram {
    pub layer: Layer,
    inner: HistogramProgram,
}

impl VfsProgram {
    pub fn load(layer: Layer, narrowing: Narrowing) -> Result<Self, String> {
        let btf = crate::bpf::kernel_btf()?;
        let timed = match layer {
            Layer::Syscall => vec![
                timed("ksys_read", &KSYS_RW, Op::Read)?,
                timed("ksys_write", &KSYS_RW, Op::Write)?,
                timed("ksys_pread64", &KSYS_PRW, Op::Pread)?,
                timed("ksys_pwrite64", &KSYS_PRW, Op::Pwrite)?,
            ],
            Layer::Fs => vec![
                timed("vfs_read", &VFS_RW, Op::Read)?,
                timed("vfs_write", &VFS_RW, Op::Write)?,
            ],
        };
        let offsets = FileOffsets::read(btf)?;
        let gate = |program: &mut Program, _: &Timed| {
            if layer == Layer::Fs {
                regular_files_only(program, &offsets);
            }
        };
        let fill = |program: &mut Program, _: &Timed| {
            match layer {
                Layer::Syscall => {
                    // The descriptor, a 32-bit argument in a 64-bit slot: the
                    // word store keeps the low half, which is all of it, and
                    // leaves the high half the zero the key started as.
                    program.push(Insn::ldx(BPF_DW, 1, 6, 0));
                    program.push(Insn::stx(BPF_W, R10, FILL_OBJECT, 1));
                }
                Layer::Fs => file_identity(program, &offsets),
            }
            Ok(())
        };
        let inner = HistogramProgram::load(HistogramSpec {
            name: match layer {
                Layer::Syscall => "omn_iosys",
                Layer::Fs => "omn_iofs",
            },
            timed,
            narrowing,
            gate: &gate,
            fill: &fill,
            max_pending: MAX_PENDING,
            max_rows: MAX_ROWS,
        })?;
        Ok(Self { layer, inner })
    }

    pub fn drain(&self) -> Result<(Vec<fentry::Row>, fentry::DrainLosses), String> {
        self.inner.drain()
    }

    pub fn still_pending(&self) -> usize {
        self.inner.still_pending()
    }

    pub fn dropped(&self) -> (u64, u64) {
        self.inner.dropped()
    }
}

fn timed(name: &str, shape: &[&str], op: Op) -> Result<Timed, String> {
    Ok(Timed {
        function: fentry::expect(name, &[shape])?,
        op: op as u32,
    })
}

/// Where the fields a file's identity is made of sit on this kernel.
struct FileOffsets {
    f_inode: i32,
    i_mode: i32,
    i_ino: i32,
    i_sb: i32,
    s_dev: i32,
}

impl FileOffsets {
    fn read(btf: &Btf) -> Result<Self, String> {
        Ok(Self {
            f_inode: btf.member_offset("file", "f_inode")?,
            i_mode: btf.member_offset("inode", "i_mode")?,
            i_ino: btf.member_offset("inode", "i_ino")?,
            i_sb: btf.member_offset("inode", "i_sb")?,
            s_dev: btf.member_offset("super_block", "s_dev")?,
        })
    }
}

/// Skips a call on anything that is not a regular file.
///
/// Plain loads, not `bpf_probe_read_kernel`: the file is a typed argument, so
/// the verifier follows `file->f_inode` itself and checks each load against the
/// member it lands on. `i_mode` is a `umode_t` and is read as exactly two
/// bytes -- a four-byte load there is refused at load time, which is the
/// property the whole attach kind is for.
fn regular_files_only(program: &mut Program, at: &FileOffsets) {
    program.push(Insn::ldx(BPF_DW, 1, 6, 0));
    program.push(Insn::ldx(BPF_DW, 1, 1, at.f_inode as i16));
    program.push(Insn::ldx(BPF_H, 1, 1, at.i_mode as i16));
    // `and` sign-extends its immediate, and `S_IFMT` is positive, so the mask
    // is exact on the zero-extended half-word.
    program.push(Insn::and64_imm(1, S_IFMT));
    program.jump_to(SKIP, Insn::jne_imm(1, S_IFREG, 0));
}

/// Writes the inode number and the superblock's device into the row key.
fn file_identity(program: &mut Program, at: &FileOffsets) {
    program.push(Insn::ldx(BPF_DW, 1, 6, 0));
    program.push(Insn::ldx(BPF_DW, 1, 1, at.f_inode as i16));
    program.push(Insn::ldx(BPF_DW, 2, 1, at.i_ino as i16));
    program.push(Insn::stx(BPF_DW, R10, FILL_OBJECT, 2));
    program.push(Insn::ldx(BPF_DW, 1, 1, at.i_sb as i16));
    program.push(Insn::ldx(BPF_W, 1, 1, at.s_dev as i16));
    program.push(Insn::stx(BPF_W, R10, FILL_DEV, 1));
}

/// A kernel `dev_t` -- twelve bits of major above twenty of minor -- in the
/// encoding userspace uses for one, which is what `stat(2)` puts in `st_dev`.
///
/// **The number is the superblock's device, and on btrfs that is not the
/// `st_dev` of the file.** btrfs gives every subvolume its own anonymous device
/// and reports *that* from `stat`, while `inode->i_sb->s_dev` -- the only
/// device a VFS-level program can read without knowing the filesystem -- is one
/// device for the whole filesystem, and it is the one `/proc/self/mountinfo`
/// prints. Measured here: `stat` says `0:45` for a file under `/home`, and the
/// superblock and `mountinfo` both say `0:28`. So the file key names the
/// filesystem and the inode, which on every other filesystem is exactly what
/// `stat` names, and on btrfs is ambiguous across subvolumes: each subvolume
/// numbers its inodes from 256. `docs/families/io.md` has the measurement.
pub fn stat_dev(kernel_dev: u32) -> u64 {
    let major = u64::from(kernel_dev >> 20);
    let minor = u64::from(kernel_dev & 0xf_ffff);
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

/// The major and minor of a kernel `dev_t`.
pub fn major_minor(kernel_dev: u32) -> (u32, u32) {
    (kernel_dev >> 20, kernel_dev & 0xf_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoding against glibc's `makedev`, which is what `stat` uses, for
    /// a device with a minor above 255 -- the case the two encodings differ in.
    #[test]
    fn a_kernel_device_number_is_reported_the_way_stat_reports_it() {
        let kernel = (259 << 20) | 300;
        let expected = unsafe { libc::makedev(259, 300) };
        assert_eq!(stat_dev(kernel), expected);
        assert_eq!(major_minor(kernel), (259, 300));
        // A btrfs subvolume's anonymous device: major zero.
        assert_eq!(stat_dev(41), unsafe { libc::makedev(0, 41) });
    }

    #[test]
    fn every_op_has_the_name_of_the_syscall_it_came_from() {
        for op in [Op::Read, Op::Write, Op::Pread, Op::Pwrite] {
            assert_eq!(Op::from_row(op as u32), Some(op));
        }
        assert_eq!(Op::from_row(9), None);
        assert!(Op::Pwrite.is_write() && !Op::Pread.is_write());
    }
}
