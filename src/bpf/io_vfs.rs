// SPDX-License-Identifier: MIT OR GPL-2.0
//! `io.syscall` and `io.fs`: two layers of one read, timed by fentry.
//!
//! A `read(2)` passes three layers this family keeps apart, and each is the
//! gap between two moments of the same call:
//!
//! ```text
//!   __x64_sys_read ─┬─ fdget_pos ── vfs_read ─┬─ rw_verify_area ── ->read_iter ── ... block
//!                   │                         │
//!                   └────── io.syscall ───────┴──────────── io.fs ─────────────┘
//! ```
//!
//! **`io.syscall` is timed at the syscall table's own entry points**:
//! `__x64_sys_read`, `_write`, `_pread64` and `_pwrite64`, and the four
//! vectored ones where the kernel has them. The interval is the call the
//! application made, less only the entry stub. What it adds over the layer below
//! is the descriptor: the table lookup and, for a file shared between threads,
//! the file position lock, which serialises two readers of one descriptor in the
//! syscall and never in the filesystem. The row is keyed by the descriptor,
//! which is what the catalog's `fd` filter field has always needed and what
//! nothing below this layer knows.
//!
//! **Why the wrappers, and not the `ksys_*` functions one level in.** The
//! wrappers take a `struct pt_regs *` and read their arguments out of it, which
//! makes this layer x86-64's -- the requirement names `__x64_sys_read`, and on
//! another architecture the probe reports it missing rather than this program
//! reading the wrong registers. The architecture-neutral `ksys_read` family
//! was the first choice and is what this file was written against, and it is
//! **not on the path**: measured here, five hundred `read`s were counted and
//! three hundred `pwrite64`s were not, because the compiler inlined
//! `ksys_pread64` and `ksys_pwrite64` into their syscalls and kept out-of-line
//! copies for the compat callers. The syscall table's entries cannot be inlined
//! into it -- the dispatcher calls them from another compilation unit -- so they
//! are the one layer that is on the path by construction.
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
    Readv = 4,
    Writev = 5,
    Preadv = 6,
    Pwritev = 7,
}

impl Op {
    const ALL: [Op; 8] = [
        Op::Read,
        Op::Write,
        Op::Pread,
        Op::Pwrite,
        Op::Readv,
        Op::Writev,
        Op::Preadv,
        Op::Pwritev,
    ];

    pub fn from_row(op: u32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| *candidate as u32 == op)
    }

    /// The name a reader filters on, which is the syscall's.
    pub fn name(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
            Op::Pread => "pread64",
            Op::Pwrite => "pwrite64",
            Op::Readv => "readv",
            Op::Writev => "writev",
            Op::Preadv => "preadv",
            Op::Pwritev => "pwritev",
        }
    }

    pub fn is_write(self) -> bool {
        matches!(self, Op::Write | Op::Pwrite | Op::Writev | Op::Pwritev)
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

/// The prototypes each layer reads. A syscall wrapper takes the saved
/// registers and nothing else; the VFS layer takes the file first. The return
/// value is in the slot after the last argument of either.
const WRAPPER: [&str; 1] = ["pt_regs *"];
const VFS_RW: [&str; 4] = ["file *", "char *", "size_t", "loff_t *"];

/// The syscall wrappers `io.syscall` times, and whether each is required. The
/// four plain ones are the catalog's requirement; the vectored ones are timed
/// where they exist.
const WRAPPERS: [(&str, Op, bool); 8] = [
    ("__x64_sys_read", Op::Read, true),
    ("__x64_sys_write", Op::Write, true),
    ("__x64_sys_pread64", Op::Pread, true),
    ("__x64_sys_pwrite64", Op::Pwrite, true),
    ("__x64_sys_readv", Op::Readv, false),
    ("__x64_sys_writev", Op::Writev, false),
    ("__x64_sys_preadv", Op::Preadv, false),
    ("__x64_sys_pwritev", Op::Pwritev, false),
];

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
            Layer::Syscall => {
                if !cfg!(target_arch = "x86_64") {
                    return Err(format!(
                        "`io.syscall` is timed at the syscall table's entry points, which take \
                         the saved registers, and this build reads x86-64's; on {} the \
                         registers are laid out differently and reading them as x86-64's \
                         would report another argument as the descriptor",
                        std::env::consts::ARCH
                    ));
                }
                let mut timed = Vec::new();
                for (name, op, required) in WRAPPERS {
                    match self::timed(name, &WRAPPER, op) {
                        Ok(found) => timed.push(found),
                        Err(reason) if required => return Err(reason),
                        Err(_) => {}
                    }
                }
                timed
            }
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
                    // The descriptor is the first argument, so it is in `di`
                    // of the saved registers: a typed load through the
                    // wrapper's one argument. It is an `unsigned int` in a
                    // 64-bit register, and the word store keeps the low half,
                    // which is all of it.
                    program.push(Insn::ldx(BPF_DW, 1, 6, 0));
                    program.push(Insn::ldx(BPF_DW, 1, 1, offsets.di as i16));
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

    pub fn drain(&self, last: bool) -> Result<fentry::Drained, String> {
        self.inner.drain(last)
    }

    pub fn still_pending(&self) -> (usize, usize) {
        self.inner.still_pending()
    }

    pub fn dropped(&self) -> (u64, u64, u64) {
        self.inner.dropped()
    }

    pub fn missed(&self) -> u64 {
        self.inner.missed()
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
    /// The first argument's register, in the syscall wrappers' `pt_regs`.
    di: i32,
    f_inode: i32,
    i_mode: i32,
    i_ino: i32,
    i_sb: i32,
    s_dev: i32,
}

impl FileOffsets {
    fn read(btf: &Btf) -> Result<Self, String> {
        Ok(Self {
            di: btf.member_offset("pt_regs", "di")?,
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

// ---------------------------------------------------------------------------
// `io.vfs`: every call into the VFS, counted by function.

/// The VFS entry points `io.vfs` counts, each named by its function: what
/// `vfsstat` counts (read, write, fsync, open, create) and the directory
/// operations beside them. Every one is counted where it exists; a function
/// this kernel does not declare is named in the run's notes, not guessed at.
///
/// `do_fsync` beside `vfs_fsync_range`: on this machine the compiler inlined
/// `vfs_fsync` and `vfs_fsync_range` into `do_fsync`, so an `fsync(2)` reaches
/// neither out-of-line copy (`catalog/io.yaml`, `io.fsync`) -- `msync` and the
/// in-kernel callers reach the range function, the two syscalls `do_fsync`.
/// Where the compiler did not inline them, an `fsync(2)` passes `do_fsync`,
/// then `vfs_fsync`, then `vfs_fsync_range`, and counting both functions would
/// count it twice. So `do_fsync` marks its thread on entry and clears the mark
/// on exit, and a `vfs_fsync_range` on a marked thread is the same sync and is
/// not counted again ([`Guard`]): one sync is one count on a kernel that
/// inlines and on one that does not.
pub const VFS_CALLS: [&str; 12] = [
    "vfs_read",
    "vfs_write",
    "vfs_readv",
    "vfs_writev",
    "vfs_open",
    "vfs_create",
    "vfs_fsync_range",
    "do_fsync",
    "vfs_unlink",
    "vfs_mkdir",
    "vfs_rmdir",
    "vfs_rename",
];

/// The stack slots of a counting program.
const CALLS_IDS_SCRATCH: i16 = -8;
const CALLS_KEY: i16 = -16;
const CALLS_TID: i16 = -24;
const CALLS_MARK: i16 = -32;

/// The slot after the functions' own: `do_fsync` calls whose thread could not
/// be marked because the map of marks was full, each a sync that a
/// non-inlining kernel may count twice.
pub const UNGUARDED: u32 = VFS_CALLS.len() as u32;

/// Threads inside `do_fsync` at once, at most, before a mark is refused.
const MAX_INSIDE: u32 = 4096;

/// What a counting program does beside counting: the guard that keeps one
/// `fsync(2)` from being counted by both `do_fsync` and `vfs_fsync_range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// Count, and nothing else.
    Plain,
    /// `do_fsync`: count, then mark the thread in the map of marks.
    Marks(i32),
    /// `vfs_fsync_range`: count only on a thread no `do_fsync` has marked.
    Unless(i32),
}

/// One program per function on its entry: the target's filter, then one
/// atomic add to the function's slot of a per-cpu array. Any file type: a
/// pipe's read is a `vfs_read` as a file's is, which is what `vfsstat`
/// counts and what `io.fs`, regular files only, does not.
pub fn calls_instructions(
    followed: &crate::bpf::sched_common::Followed,
    counts_fd: i32,
    slot: u32,
    guard: Guard,
) -> Vec<Insn> {
    use crate::bpf::insn::helper;
    let mut program = Program::new();
    crate::bpf::sched_common::push_current_followed_filter(
        &mut program,
        followed,
        6,
        CALLS_IDS_SCRATCH,
        SKIP,
        "calls_kept",
    );
    // The thread id, the low half, as the key of the map of marks.
    program.push(Insn::stx(BPF_W, R10, CALLS_TID, 6));
    if let Guard::Unless(inside_fd) = guard {
        program.extend(Insn::load_map_fd(1, inside_fd));
        program.push(Insn::mov64_reg(2, R10));
        program.push(Insn::add64_imm(2, CALLS_TID as i32));
        program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
        program.jump_to(SKIP, Insn::jne_imm(0, 0, 0));
    }
    count_slot(&mut program, counts_fd, slot, "counted");
    program.label("counted");
    if let Guard::Marks(inside_fd) = guard {
        program.push(Insn::st_imm(BPF_DW, R10, CALLS_MARK, 0));
        program.extend(Insn::load_map_fd(1, inside_fd));
        program.push(Insn::mov64_reg(2, R10));
        program.push(Insn::add64_imm(2, CALLS_TID as i32));
        program.push(Insn::mov64_reg(3, R10));
        program.push(Insn::add64_imm(3, CALLS_MARK as i32));
        program.push(Insn::mov64_imm(4, 0));
        program.push(Insn::call(helper::MAP_UPDATE_ELEM));
        program.jump_to(SKIP, Insn::jeq_imm(0, 0, 0));
        count_slot(&mut program, counts_fd, UNGUARDED, SKIP);
    }
    program.label(SKIP);
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    program.build()
}

/// One atomic add to `slot` of the counters, then on to `next`.
fn count_slot(program: &mut Program, counts_fd: i32, slot: u32, next: &'static str) {
    program.push(Insn::st_imm(BPF_W, R10, CALLS_KEY, slot as i32));
    program.extend(Insn::load_map_fd(1, counts_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, CALLS_KEY as i32));
    program.push(Insn::call(crate::bpf::insn::helper::MAP_LOOKUP_ELEM));
    program.jump_to(next, Insn::jeq_imm(0, 0, 0));
    // Atomic although the array is per-cpu: a tracing program runs with
    // migration disabled, not preemption, so two calls on one cpu can
    // interleave a load and a store.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, 0, 1));
}

/// `do_fsync`'s exit half: the thread's mark, cleared. Unfiltered, since
/// clearing a mark nobody set is a failed delete and nothing more.
pub fn unmark_instructions(inside_fd: i32) -> Vec<Insn> {
    use crate::bpf::insn::helper;
    let mut program = Program::new();
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, CALLS_TID, 0));
    program.extend(Insn::load_map_fd(1, inside_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, CALLS_TID as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    program.build()
}

/// The counting programs, attached, and the array they count into.
pub struct VfsCalls {
    counts: crate::bpf::map::Counters,
    /// `(slot, function)` of every function attached.
    links: Vec<(u32, fentry::Link)>,
    /// `do_fsync`'s exit half, which clears the marks, and the map of them.
    unmark: Option<fentry::Link>,
    _inside: Option<crate::bpf::map::Hash>,
    /// The functions this kernel does not declare, or would not attach, and
    /// why.
    pub absent: Vec<String>,
    descendants: Option<crate::bpf::sched_common::Descendants>,
}

impl VfsCalls {
    pub fn load(whose: crate::bpf::Whose) -> Result<Self, String> {
        use crate::bpf::sched_common::{Descendants, Followed};
        let index = fentry::index()?;
        let descendants = match whose {
            crate::bpf::Whose::Process(root) => Some(Descendants::follow(root)?),
            _ => None,
        };
        let followed = descendants
            .as_ref()
            .map_or(Followed::only(whose), |descendants| {
                descendants.followed(whose)
            });
        let counts = crate::bpf::map::Counters::create(VFS_CALLS.len() as u32 + 1, "omn_vfs_calls")
            .map_err(|error| crate::bpf::map::map_failure("VFS call counters", error))?;
        let mut links = Vec::new();
        let mut absent = Vec::new();
        // The guard, where `do_fsync` exists and its exit half attaches. The
        // exit half goes first, so that no mark is ever set with nothing to
        // clear it.
        let mut inside = None;
        let mut unmark = None;
        if let Ok(function) = index.function("do_fsync") {
            let map = crate::bpf::map::Hash::create(4, 8, MAX_INSIDE, "omn_vfs_inside")
                .map_err(|error| crate::bpf::map::map_failure("VFS sync marks", error))?;
            let instructions = unmark_instructions(map.as_raw_fd());
            crate::bpf::insn::audit(&instructions)?;
            match fentry::attach(
                &function,
                fentry::Side::Exit,
                &instructions,
                "omn_vfs_unmark",
            ) {
                Ok(link) => {
                    unmark = Some(link);
                    inside = Some(map);
                }
                Err(reason) => absent.push(format!(
                    "`do_fsync`'s exit half: {reason}; without it a kernel that does not inline \
                     `vfs_fsync_range` into `do_fsync` counts an `fsync(2)` under both"
                )),
            }
        }
        let inside_fd = inside.as_ref().map(crate::bpf::map::Hash::as_raw_fd);
        for (slot, name) in VFS_CALLS.iter().enumerate() {
            let function = match index.function(name) {
                Ok(function) => function,
                Err(reason) => {
                    absent.push(format!("`{name}`: {reason}"));
                    continue;
                }
            };
            let guard = match (*name, inside_fd) {
                ("do_fsync", Some(fd)) => Guard::Marks(fd),
                ("vfs_fsync_range", Some(fd)) => Guard::Unless(fd),
                _ => Guard::Plain,
            };
            let instructions =
                calls_instructions(&followed, counts.as_raw_fd(), slot as u32, guard);
            crate::bpf::insn::audit(&instructions)?;
            match fentry::attach(
                &function,
                fentry::Side::Entry,
                &instructions,
                "omn_vfs_call",
            ) {
                Ok(link) => links.push((slot as u32, link)),
                // The first function refused is the machine's answer for all
                // of them (a permission, a kernel with no trampolines); a later
                // one alone is a function ftrace will not patch, named.
                Err(reason) if links.is_empty() => return Err(reason),
                Err(reason) => absent.push(format!("`{name}`: {reason}")),
            }
        }
        if links.is_empty() {
            return Err(format!(
                "none of the VFS functions `io.vfs` counts is in this kernel: {}",
                absent.join("; ")
            ));
        }
        Ok(Self {
            counts,
            links,
            unmark,
            _inside: inside,
            absent,
            descendants,
        })
    }

    /// Every attached function's count so far, cumulative.
    pub fn read(&self) -> Vec<(&'static str, u64)> {
        self.links
            .iter()
            .map(|(slot, _)| (VFS_CALLS[*slot as usize], self.counts.read(*slot)))
            .collect()
    }

    /// Runs the kernel skipped: calls never counted. A skipped run of
    /// `do_fsync`'s exit half is one too, since the mark it left keeps the
    /// thread's next `vfs_fsync_range` out of the count.
    pub fn missed(&self) -> u64 {
        self.links
            .iter()
            .map(|(_, link)| link.misses())
            .sum::<u64>()
            + self.unmark.as_ref().map_or(0, fentry::Link::misses)
            + self
                .descendants
                .as_ref()
                .map_or(0, crate::bpf::sched_common::Descendants::skipped)
    }

    /// `do_fsync` calls whose thread could not be marked, each a sync a
    /// kernel that does not inline `vfs_fsync_range` counts twice.
    pub fn unguarded(&self) -> u64 {
        self.counts.read(UNGUARDED)
    }

    /// Processes the target started that the map had no room for.
    pub fn unfollowed(&self) -> u64 {
        self.descendants
            .as_ref()
            .map_or(0, crate::bpf::sched_common::Descendants::missed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoding against glibc's `makedev`, which is what `stat` uses, for
    /// a device with a minor above 255 -- the case the two encodings differ in.
    #[test]
    fn a_kernel_device_number_is_reported_the_way_stat_reports_it() {
        let kernel = (259 << 20) | 300;
        let expected = libc::makedev(259, 300);
        assert_eq!(stat_dev(kernel), expected);
        assert_eq!(major_minor(kernel), (259, 300));
        // A btrfs subvolume's anonymous device: major zero.
        assert_eq!(stat_dev(41), libc::makedev(0, 41));
    }

    #[test]
    fn every_op_has_the_name_of_the_syscall_it_came_from() {
        for op in Op::ALL {
            assert_eq!(Op::from_row(op as u32), Some(op));
            assert!(WRAPPERS
                .iter()
                .any(|(name, of, _)| *of == op && name.ends_with(op.name())));
        }
        assert_eq!(Op::from_row(9), None);
        assert!(Op::Pwritev.is_write() && !Op::Preadv.is_write());
    }
}
