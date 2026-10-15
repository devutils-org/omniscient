// SPDX-License-Identifier: MIT OR GPL-2.0
//! `io.fsync`: how long a program waited for its data to be durable.
//!
//! **The function the catalog named is not the one `fsync(2)` calls.** The
//! kernel's own comment on `vfs_fsync_range` calls it the place a sync goes
//! through, and on this machine a program that synced twenty-five times put
//! **zero** calls through it and zero through `vfs_fsync`, machine-wide, with
//! no loss anywhere: both are exported, both are in BTF, both attach -- and the
//! compiler inlined both into `do_fsync`, the static function the two syscalls
//! share, while keeping out-of-line copies for other callers. Timing
//! `do_fsync` instead found all twenty-five.
//!
//! So three functions are timed, and which of them a given kernel routes a
//! sync through is the compiler's decision rather than anything this program
//! can know in advance:
//!
//! | Function | Arrives from | The file comes from |
//! |---|---|---|
//! | `do_fsync(fd, datasync)` | `fsync(2)`, `fdatasync(2)` | the caller's descriptor table |
//! | `vfs_fsync(file, datasync)` | the two syscalls, where it is not inlined | the argument |
//! | `vfs_fsync_range(file, start, end, datasync)` | `msync(MS_SYNC)`, nfsd, and the rest | the argument |
//!
//! The stamp records **which function opened it**. The entry half of a nested
//! call finds the thread already stamped and leaves it alone, and the exit half
//! closes only the stamp its own function opened, so one sync is one event on
//! a kernel that inlines and on one that does not.
//!
//! `do_fsync` has no `struct file *` among its arguments, so there the file is
//! found the way the kernel found it a moment before: `current->files->fdt->
//! fd[fd]`, walked with `bpf_probe_read_kernel` because an index into a table
//! is not something the verifier lets a typed load do. The other two are
//! typed loads, which is the point of the attach kind; this one is a walk, and
//! the difference is written down rather than papered over.
//!
//! **One record per call rather than a histogram.** A sync is thousands of
//! times rarer than a read: a database commits a few hundred times a second, a
//! build a few times a minute. At that rate a ring record per call costs
//! nothing worth measuring, and it keeps what a histogram would throw away --
//! which file, which range, and which call, so that a slow commit can be named
//! and joined to the block requests it waited on.

use std::os::fd::AsRawFd;

use crate::bpf::attach::fentry::{self, Function, Link, Narrowing, Side, BPF_H, SKIP};
use crate::bpf::btf::Btf;
use crate::bpf::insn::{
    self, count_drop, helper, read_field, read_narrow, read_pointer, Insn, Program, BPF_DW, BPF_W,
    R10,
};
use crate::bpf::map::{map_failure, Counters, Hash};
use crate::bpf::ring::{ring_bytes, RingBuffer};
use crate::bpf::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// `bpf_get_current_task`, which `insn::helper` does not name yet.
const GET_CURRENT_TASK: i32 = 35;

/// The prototypes these programs read.
const FD_SIGNATURE: [&str; 2] = ["unsigned int", "int"];
const WHOLE_SIGNATURE: [&str; 2] = ["file *", "int"];
const RANGE_SIGNATURE: [&str; 4] = ["file *", "loff_t", "loff_t", "int"];

/// `BPF_NOEXIST`, and what an insert gets when the key is already there.
const BPF_NOEXIST: i32 = 1;
const EEXIST: i32 = 17;

/// Threads that may be inside a sync at once.
const MAX_PENDING: u32 = 16384;

/// Slots of the drop counter.
const DROP_PENDING: u32 = 0;
const DROP_RING: u32 = 1;
/// Syncs whose exit half the kernel skipped; see `fentry::entry_instructions`.
const DROP_ORPHANED: u32 = 2;

/// How young an outer function's stamp must be for an inner entry to count as
/// nested in it. `do_fsync` reaches `vfs_fsync` a descriptor lookup later,
/// which is well under a microsecond; a stamp older than this is a sync whose
/// exit half never ran, and treating it as an outer call would leave the thread
/// untimed from then on.
const NEST_WINDOW_NS: i32 = 50_000;

/// The ring: 128 KiB holds about fourteen hundred records between two drains
/// ten milliseconds apart, which is a hundred and forty thousand syncs a
/// second -- two orders of magnitude above a busy database.
pub const RING_BYTES: usize = 128 * 1024;

/// Which of the three functions a sync was timed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// `do_fsync`: the file was found through the descriptor.
    Descriptor = 1,
    /// `vfs_fsync`: the whole file, by argument.
    Whole = 2,
    /// `vfs_fsync_range`: a byte range, by argument.
    Range = 3,
}

impl Via {
    fn from_record(value: u32) -> Option<Self> {
        match value {
            1 => Some(Via::Descriptor),
            2 => Some(Via::Whole),
            3 => Some(Via::Range),
            _ => None,
        }
    }

    pub fn function(self) -> &'static str {
        match self {
            Via::Descriptor => "do_fsync",
            Via::Whole => "vfs_fsync",
            Via::Range => "vfs_fsync_range",
        }
    }
}

/// One sync, as the program writes it and userspace reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fsync {
    /// When the call returned, `CLOCK_MONOTONIC`.
    pub ts_ns: u64,
    pub duration_ns: u64,
    /// Zero when the file could not be found: the descriptor was closed by
    /// another thread while the sync ran, which is the one race the walk has.
    pub ino: u64,
    /// The byte range asked for, inclusive. A whole-file sync is `0` to
    /// `LLONG_MAX`, which is how the kernel writes one.
    pub start: i64,
    pub end: i64,
    /// The superblock's device, as the kernel packs it.
    pub dev: u32,
    /// Non-zero for `fdatasync`: the metadata that does not affect reading the
    /// data back may be left unwritten.
    pub datasync: u32,
    /// What the call returned: zero, or a negative errno.
    pub ret: i32,
    pub tid: u32,
    pub tgid: u32,
    /// The file's type bits, `S_IFMT` of its mode: a directory is synced to
    /// make a rename durable, and that is a different question.
    pub mode: u32,
    pub comm: String,
    /// The descriptor, where the sync arrived by one; `-1` otherwise.
    pub fd: i32,
    pub via: Option<Via>,
}

const RECORD_BYTES: usize = 88;
const EVENT: i16 = -88;
const R_TS: i16 = 0;
const R_DURATION: i16 = 8;
const R_INO: i16 = 16;
const R_START: i16 = 24;
const R_END: i16 = 32;
const R_DEV: i16 = 40;
const R_DATASYNC: i16 = 44;
const R_RET: i16 = 48;
const R_TID: i16 = 52;
const R_TGID: i16 = 56;
const R_MODE: i16 = 60;
const R_COMM: i16 = 64;
const R_FD: i16 = 80;
const R_VIA: i16 = 84;
const PEND_KEY: i16 = -96;
const DROP_KEY: i16 = -100;
/// Eight bytes a probe read lands in on the way to a register.
const SCRATCH: i16 = -112;
/// The calling thread group, as the key of a lookup in the descendants map.
const FOLLOW_KEY: i16 = -104;
/// The pending value: when, and which function.
const PEND_VALUE: i16 = -128;
const PEND_TS: i16 = 0;
const PEND_TAG: i16 = 8;
const PEND_VALUE_BYTES: usize = 16;

/// `S_IFMT`, from the frozen `stat(2)` ABI.
const S_IFMT: i32 = 0o170000;

impl Fsync {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let word = |at: i16| {
            u32::from_ne_bytes(bytes[at as usize..at as usize + 4].try_into().expect("4"))
        };
        let long = |at: i16| {
            u64::from_ne_bytes(bytes[at as usize..at as usize + 8].try_into().expect("8"))
        };
        let comm = &bytes[R_COMM as usize..R_COMM as usize + 16];
        let end = comm.iter().position(|b| *b == 0).unwrap_or(comm.len());
        Some(Self {
            ts_ns: long(R_TS),
            duration_ns: long(R_DURATION),
            ino: long(R_INO),
            start: long(R_START) as i64,
            end: long(R_END) as i64,
            dev: word(R_DEV),
            datasync: word(R_DATASYNC),
            ret: word(R_RET) as i32,
            tid: word(R_TID),
            tgid: word(R_TGID),
            mode: word(R_MODE),
            comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
            fd: word(R_FD) as i32,
            via: Via::from_record(word(R_VIA)),
        })
    }
}

/// Where the fields of a file's identity, and of the walk to one, sit on this
/// kernel.
struct Offsets {
    f_inode: i32,
    i_mode: i32,
    i_ino: i32,
    i_sb: i32,
    s_dev: i32,
    task_files: i32,
    files_fdt: i32,
    fdt_max_fds: i32,
    fdt_fd: i32,
}

impl Offsets {
    fn read(btf: &Btf) -> Result<Self, String> {
        Ok(Self {
            f_inode: btf.member_offset("file", "f_inode")?,
            i_mode: btf.member_offset("inode", "i_mode")?,
            i_ino: btf.member_offset("inode", "i_ino")?,
            i_sb: btf.member_offset("inode", "i_sb")?,
            s_dev: btf.member_offset("super_block", "s_dev")?,
            task_files: btf.member_offset("task_struct", "files")?,
            files_fdt: btf.member_offset("files_struct", "fdt")?,
            fdt_max_fds: btf.member_offset("fdtable", "max_fds")?,
            fdt_fd: btf.member_offset("fdtable", "fd")?,
        })
    }
}

/// One of the three functions, and how its arguments are read.
struct Shape<'a> {
    function: &'a Function,
    via: Via,
}

/// The entry half: stamp the thread, unless an outer sync already has.
///
/// A process target is compared as a constant, and with `descendants` every
/// other thread group is looked up in the map of the processes it started:
/// a launched shell's `sync`-ing child is the run's.
fn entry_instructions(
    shape: &Shape<'_>,
    narrowing: Narrowing,
    descendants: Option<i32>,
    pending_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::mov64_reg(7, 0));
    if let Some(tgid) = narrowing.tgid {
        program.push(Insn::mov64_reg(1, 7));
        program.push(Insn::rsh64_imm(1, 32));
        match descendants {
            None => {
                program.jump_to(SKIP, Insn::jne_imm(1, tgid as i32, 0));
            }
            Some(map) => {
                program.push(Insn::stx(BPF_W, R10, FOLLOW_KEY, 1));
                program.jump_to("followed", Insn::jeq_imm(1, tgid as i32, 0));
                program.extend(Insn::load_map_fd(1, map));
                program.push(Insn::mov64_reg(2, R10));
                program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
                program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
                program.jump_to(SKIP, Insn::jeq_imm(0, 0, 0));
                program.label("followed");
            }
        }
    }
    program.push(Insn::st_imm(BPF_DW, R10, PEND_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, PEND_KEY, 7));
    if let Some(tid) = narrowing.tid {
        program.push(Insn::ldx(BPF_W, 1, R10, PEND_KEY));
        program.jump_to(SKIP, Insn::jne_imm(1, tid as i32, 0));
    }
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + PEND_TS, 0));
    program.push(Insn::st_imm(
        BPF_DW,
        R10,
        PEND_VALUE + PEND_TAG,
        shape.via as i32,
    ));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, PEND_VALUE as i32));
    // Only if absent. A thread already stamped is either inside an outer
    // function of the three -- and that call is the one being timed -- or it
    // is a sync whose exit half the kernel skipped, and the two are told apart
    // below.
    program.push(Insn::mov64_imm(4, BPF_NOEXIST));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to(SKIP, Insn::jeq_imm(0, 0, 0));
    program.jump_to("full", Insn::jne_imm(0, -EEXIST, 0));

    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to(SKIP, Insn::jeq_imm(0, 0, 0));
    // A function at this depth or deeper cannot be the outer call of this one:
    // the stamp is an orphan.
    program.push(Insn::ldx(BPF_DW, 1, 0, PEND_TAG));
    program.jump_to("orphan", Insn::jsge_imm(1, shape.via as i32, 0));
    // A shallower function, stamped a moment ago, is the call this one is
    // nested in. Stamped long ago, it is an orphan all the same.
    program.push(Insn::ldx(BPF_DW, 2, 0, PEND_TS));
    program.push(Insn::ldx(BPF_DW, 1, R10, PEND_VALUE + PEND_TS));
    program.push(Insn::sub64_reg(1, 2));
    program.jump_to("orphan", Insn::jsge_imm(1, NEST_WINDOW_NS, 0));
    program.jump_to(SKIP, Insn::ja());

    program.label("orphan");
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_ORPHANED, "restamp");
    program.label("restamp");
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, PEND_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to(SKIP, Insn::jsge_imm(0, 0, 0));
    program.label("full");
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_PENDING, SKIP);
    program.label(SKIP);
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    insn::audit(&instructions)?;
    Ok(instructions)
}

/// Writes the file's identity from a typed `struct file *` in slot zero.
fn typed_identity(program: &mut Program, function: &Function, at: &Offsets) {
    program.push(Insn::ldx(BPF_DW, 1, 6, function.arg(0)));
    program.push(Insn::ldx(BPF_DW, 1, 1, at.f_inode as i16));
    program.push(Insn::ldx(BPF_DW, 2, 1, at.i_ino as i16));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_INO, 2));
    program.push(Insn::ldx(BPF_H, 2, 1, at.i_mode as i16));
    program.push(Insn::and64_imm(2, S_IFMT));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_MODE, 2));
    program.push(Insn::ldx(BPF_DW, 1, 1, at.i_sb as i16));
    program.push(Insn::ldx(BPF_W, 1, 1, at.s_dev as i16));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_DEV, 1));
}

/// Writes the file's identity by walking the calling task's descriptor table.
///
/// Each step is a probe read, and each can find nothing: the table is replaced
/// when it grows, and another thread can close the descriptor while the sync
/// runs. A step that finds a null pointer leaves the identity at zero, which the
/// collector reports as a sync it could not name rather than as file zero.
fn descriptor_identity(program: &mut Program, function: &Function, at: &Offsets) {
    // The descriptor, zero-extended: a word store and a word load.
    program.push(Insn::ldx(BPF_DW, 1, 6, function.arg(0)));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_FD, 1));
    program.push(Insn::call(GET_CURRENT_TASK));
    program.push(Insn::mov64_reg(7, 0));
    read_pointer(program, SCRATCH, 7, 7, at.task_files);
    program.jump_to("unnamed", Insn::jeq_imm(7, 0, 0));
    read_pointer(program, SCRATCH, 7, 7, at.files_fdt);
    program.jump_to("unnamed", Insn::jeq_imm(7, 0, 0));
    // Bounded by the table's own size before it is indexed: the descriptor is
    // the caller's number, and a probe read past the end reads somebody else's.
    read_narrow(program, SCRATCH, 4, 7, at.fdt_max_fds);
    program.push(Insn::ldx(BPF_W, 1, R10, SCRATCH));
    program.push(Insn::ldx(BPF_W, 2, R10, EVENT + R_FD));
    program.jump_to("unnamed", Insn::jge_reg(2, 1, 0));
    read_pointer(program, SCRATCH, 7, 7, at.fdt_fd);
    program.jump_to("unnamed", Insn::jeq_imm(7, 0, 0));
    program.push(Insn::ldx(BPF_W, 2, R10, EVENT + R_FD));
    program.push(Insn::lsh64_imm(2, 3));
    program.push(Insn::add64_reg(7, 2));
    read_pointer(program, SCRATCH, 7, 7, 0);
    program.jump_to("unnamed", Insn::jeq_imm(7, 0, 0));
    read_pointer(program, SCRATCH, 7, 7, at.f_inode);
    program.jump_to("unnamed", Insn::jeq_imm(7, 0, 0));
    read_field(program, EVENT + R_INO, 8, 7, at.i_ino);
    read_narrow(program, SCRATCH, 2, 7, at.i_mode);
    program.push(Insn::ldx(BPF_W, 1, R10, SCRATCH));
    program.push(Insn::and64_imm(1, S_IFMT));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_MODE, 1));
    read_pointer(program, SCRATCH, 7, 7, at.i_sb);
    program.jump_to("unnamed", Insn::jeq_imm(7, 0, 0));
    read_field(program, EVENT + R_DEV, 4, 7, at.s_dev);
    program.label("unnamed");
}

/// The exit half: close this function's stamp, fill one record, publish it.
fn exit_instructions(
    shape: &Shape<'_>,
    at: &Offsets,
    pending_fd: i32,
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let function = shape.function;
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::mov64_reg(7, 0));
    program.push(Insn::st_imm(BPF_DW, R10, PEND_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, PEND_KEY, 7));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    // Another function's stamp: this is the inner call of a nested pair, and
    // the outer one closes it.
    program.push(Insn::ldx(BPF_DW, 1, 0, PEND_TAG));
    program.jump_to("done", Insn::jne_imm(1, shape.via as i32, 0));
    program.push(Insn::ldx(BPF_DW, 9, 0, PEND_TS));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));
    program.push(Insn::sub64_reg(0, 9));
    program.push(Insn::mov64_reg(9, 0));

    // Every byte of the record, before any of it is filled.
    for slot in 0..(RECORD_BYTES as i16 / 8) {
        program.push(Insn::st_imm(BPF_DW, R10, EVENT + slot * 8, 0));
    }
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_TS, 8));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_DURATION, 9));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_TID, 7));
    program.push(Insn::mov64_reg(1, 7));
    program.push(Insn::rsh64_imm(1, 32));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_TGID, 1));
    program.push(Insn::st_imm(BPF_W, R10, EVENT + R_VIA, shape.via as i32));
    program.push(Insn::st_imm(BPF_W, R10, EVENT + R_FD, -1));

    let datasync_arg = match shape.via {
        Via::Descriptor | Via::Whole => 1,
        Via::Range => 3,
    };
    match shape.via {
        Via::Range => {
            program.push(Insn::ldx(BPF_DW, 1, 6, function.arg(1)));
            program.push(Insn::stx(BPF_DW, R10, EVENT + R_START, 1));
            program.push(Insn::ldx(BPF_DW, 1, 6, function.arg(2)));
            program.push(Insn::stx(BPF_DW, R10, EVENT + R_END, 1));
        }
        Via::Descriptor | Via::Whole => {
            // A whole-file sync is `vfs_fsync_range(file, 0, LLONG_MAX, ...)`
            // and the record says so in the same numbers. `LLONG_MAX` does not
            // fit an immediate, so it is built: all ones, shifted right once.
            program.push(Insn::mov64_imm(1, -1));
            program.push(Insn::rsh64_imm(1, 1));
            program.push(Insn::stx(BPF_DW, R10, EVENT + R_END, 1));
        }
    }
    program.push(Insn::ldx(BPF_DW, 1, 6, function.arg(datasync_arg)));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_DATASYNC, 1));
    program.push(Insn::ldx(BPF_DW, 1, 6, function.ret()));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_RET, 1));

    match shape.via {
        Via::Descriptor => descriptor_identity(&mut program, function, at),
        Via::Whole | Via::Range => typed_identity(&mut program, function, at),
    }

    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (EVENT + R_COMM) as i32));
    program.push(Insn::mov64_imm(2, 16));
    program.push(Insn::call(helper::GET_CURRENT_COMM));

    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EVENT as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    // The ring was full: the sync happened and this run will never see it.
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_RING, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    insn::audit(&instructions)?;
    Ok(instructions)
}

/// Both halves of the three functions, attached, and the ring between them and
/// userspace.
pub struct FsyncProgram {
    ring: RingBuffer,
    drops: Counters,
    pending: Hash,
    links: Vec<Link>,
    /// Whether `do_fsync` was found and timed.
    pub by_descriptor: bool,
    /// The processes a process target started, followed from the fork.
    descendants: Option<crate::bpf::sched_common::Descendants>,
}

impl FsyncProgram {
    pub fn load(narrowing: Narrowing) -> Result<Self, String> {
        let btf = crate::bpf::kernel_btf()?;
        // `do_fsync` is static: a kernel that inlines it into the two
        // syscalls has no `FUNC` for it, and on that kernel `vfs_fsync` is the
        // one the syscalls call out of line. So it is timed where it exists
        // and its absence is not a refusal.
        let by_descriptor = fentry::expect("do_fsync", &[&FD_SIGNATURE]).ok();
        let whole = fentry::expect("vfs_fsync", &[&WHOLE_SIGNATURE])?;
        let range = fentry::expect("vfs_fsync_range", &[&RANGE_SIGNATURE])?;
        let mut shapes = vec![
            Shape {
                function: &whole,
                via: Via::Whole,
            },
            Shape {
                function: &range,
                via: Via::Range,
            },
        ];
        if let Some(function) = by_descriptor.as_ref() {
            shapes.insert(
                0,
                Shape {
                    function,
                    via: Via::Descriptor,
                },
            );
        }
        let offsets = Offsets::read(btf)?;
        let descendants = match narrowing.tgid {
            Some(root) => Some(crate::bpf::sched_common::Descendants::follow(root)?),
            None => None,
        };
        let followed = descendants
            .as_ref()
            .map(crate::bpf::sched_common::Descendants::map_fd);

        let bytes = ring_bytes(RING_BYTES);
        let ring_map =
            syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_fsync_ring")
                .map_err(|error| map_failure("fsync ring buffer", error))?;
        let drops = Counters::create(3, "omn_fsync_drop")
            .map_err(|error| map_failure("fsync drop counter", error))?;
        let pending = Hash::create(4, PEND_VALUE_BYTES, MAX_PENDING, "omn_fsync_pend")
            .map_err(|error| map_failure("fsync pending map", error))?;

        let mut built = Vec::new();
        for shape in &shapes {
            let entry = entry_instructions(
                shape,
                narrowing,
                followed,
                pending.as_raw_fd(),
                drops.as_raw_fd(),
            )?;
            let exit = exit_instructions(
                shape,
                &offsets,
                pending.as_raw_fd(),
                ring_map.as_raw_fd(),
                drops.as_raw_fd(),
            )?;
            built.push((shape, entry, exit));
        }
        // Every exit before any entry, for the reason `HistogramProgram::load`
        // gives.
        let mut links = Vec::new();
        for (shape, _, exit) in &built {
            links.push(fentry::attach(
                shape.function,
                Side::Exit,
                exit,
                "omn_fsync_x",
            )?);
        }
        for (shape, entry, _) in &built {
            links.push(fentry::attach(
                shape.function,
                Side::Entry,
                entry,
                "omn_fsync_e",
            )?);
        }
        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the fsync ring buffer: {error}"))?;
        Ok(Self {
            ring,
            drops,
            pending,
            links,
            by_descriptor: by_descriptor.is_some(),
            descendants,
        })
    }

    pub fn drain(&mut self, mut handle: impl FnMut(Fsync), undecoded: &mut u64) {
        self.ring.drain(|bytes| match Fsync::from_bytes(bytes) {
            Some(record) => handle(record),
            None => *undecoded += 1,
        });
    }

    /// `(pending, ring, orphaned)`: syncs never timed, syncs timed and never
    /// seen, and syncs whose exit half the kernel skipped.
    pub fn dropped(&self) -> (u64, u64, u64) {
        (
            self.drops.read(DROP_PENDING),
            self.drops.read(DROP_RING),
            self.drops.read(DROP_ORPHANED),
        )
    }

    /// Runs the kernel skipped; see `fentry::Link::misses`.
    pub fn missed(&self) -> u64 {
        fentry::misses(&self.links)
            + self
                .descendants
                .as_ref()
                .map_or(0, crate::bpf::sched_common::Descendants::skipped)
    }

    /// Processes the target started that the map had no room for.
    pub fn unfollowed(&self) -> u64 {
        self.descendants
            .as_ref()
            .map_or(0, crate::bpf::sched_common::Descendants::missed)
    }

    /// Stamps left, as `(in flight, orphaned)`.
    pub fn still_pending(&self) -> (usize, usize) {
        fentry::split_stamps(&self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field of the record is written by the program, and none twice;
    /// and the stack regions do not overlap.
    #[test]
    fn the_record_layout_covers_every_byte() {
        let fields = [
            (R_TS, 8),
            (R_DURATION, 8),
            (R_INO, 8),
            (R_START, 8),
            (R_END, 8),
            (R_DEV, 4),
            (R_DATASYNC, 4),
            (R_RET, 4),
            (R_TID, 4),
            (R_TGID, 4),
            (R_MODE, 4),
            (R_COMM, 16),
            (R_FD, 4),
            (R_VIA, 4),
        ];
        let mut covered = [false; RECORD_BYTES];
        for (at, size) in fields {
            for byte in at..at + size {
                assert!(!covered[byte as usize], "byte {byte} twice");
                covered[byte as usize] = true;
            }
        }
        assert!(covered.iter().all(|byte| *byte));
        let regions = [
            (EVENT, RECORD_BYTES as i16),
            (PEND_KEY, 8),
            (DROP_KEY, 4),
            (SCRATCH, 8),
            (PEND_VALUE, PEND_VALUE_BYTES as i16),
        ];
        for (index, (at, size)) in regions.iter().enumerate() {
            assert!(*at >= -512 && at + size <= 0);
            for (other_at, other_size) in &regions[index + 1..] {
                assert!(at + size <= *other_at || other_at + other_size <= *at);
            }
        }
    }

    /// A negative return is an errno and must survive as one.
    #[test]
    fn a_failed_sync_keeps_its_errno() {
        let mut bytes = [0u8; RECORD_BYTES];
        bytes[R_RET as usize..R_RET as usize + 4].copy_from_slice(&(-5i32).to_ne_bytes());
        bytes[R_END as usize..R_END as usize + 8].copy_from_slice(&i64::MAX.to_ne_bytes());
        bytes[R_VIA as usize..R_VIA as usize + 4].copy_from_slice(&1u32.to_ne_bytes());
        let record = Fsync::from_bytes(&bytes).expect("a whole record");
        assert_eq!((record.ret, record.end), (-5, i64::MAX));
        assert_eq!(record.via, Some(Via::Descriptor));
        assert!(Fsync::from_bytes(&bytes[..40]).is_none());
    }

    /// The three functions, built against this kernel. Each exit half closes
    /// only the stamp its own entry half opened, and each entry half stamps
    /// only a thread nothing has stamped: that is what keeps one sync one event
    /// whether or not the compiler nested them.
    #[test]
    fn each_exit_closes_only_its_own_functions_stamp() {
        let Ok(btf) = crate::bpf::kernel_btf() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let offsets = Offsets::read(btf).expect("the offsets");
        for (name, signature, via) in [
            ("do_fsync", &FD_SIGNATURE[..], Via::Descriptor),
            ("vfs_fsync", &WHOLE_SIGNATURE[..], Via::Whole),
            ("vfs_fsync_range", &RANGE_SIGNATURE[..], Via::Range),
        ] {
            // A function this kernel does not have -- `do_fsync` inlined into
            // the syscall, as a clang build does -- is one it cannot serve.
            if let Err(reason) = fentry::index().and_then(|index| index.function(name)) {
                eprintln!("skipped `{name}`: {reason}");
                continue;
            }
            let function = fentry::expect(name, &[signature]).expect("the prototype");
            let shape = Shape {
                function: &function,
                via,
            };
            let exit = exit_instructions(&shape, &offsets, 3, 4, 5).expect("a program");
            assert!(
                exit.iter()
                    .any(|insn| insn.code == 0x55 && insn.imm == via as i32),
                "the {name} exit must compare the stamp's tag with its own"
            );
            let entry =
                entry_instructions(&shape, Narrowing::default(), None, 3, 5).expect("a program");
            assert!(
                entry
                    .iter()
                    .any(|insn| insn.imm == BPF_NOEXIST && insn.code == 0xb7),
                "the {name} entry must stamp only a thread nothing has stamped"
            );
        }
    }

    /// The walk bounds the descriptor by the table's size before it indexes.
    #[test]
    fn the_descriptor_is_bounded_before_the_table_is_indexed() {
        let Ok(btf) = crate::bpf::kernel_btf() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let offsets = Offsets::read(btf).expect("the offsets");
        if let Err(reason) = fentry::index().and_then(|index| index.function("do_fsync")) {
            eprintln!("skipped: {reason}");
            return;
        }
        let function = fentry::expect("do_fsync", &[&FD_SIGNATURE]).expect("the prototype");
        let exit = exit_instructions(
            &Shape {
                function: &function,
                via: Via::Descriptor,
            },
            &offsets,
            3,
            4,
            5,
        )
        .expect("a program");
        let bound = exit
            .iter()
            .position(|insn| insn.code == 0x3d)
            .expect("an unsigned bound check");
        let index = exit
            .iter()
            .position(|insn| insn.code == 0x67)
            .expect("the descriptor scaled to a pointer's width");
        assert!(bound < index);
    }
}
