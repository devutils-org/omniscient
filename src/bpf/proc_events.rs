// SPDX-License-Identifier: MIT OR GPL-2.0
//! The three process-event programs: exec, exit and signal.
//!
//! All three are the streaming shape `wakeup.rs` is the worked example for: one
//! ring record per event, because each event matters on its own and there is
//! nothing to add up. An exec is a path and a moment, an exit is a status, and
//! a signal is a number sent from one task to another; none of them is a
//! distribution, and a machine that execs a few hundred times a second is a
//! machine doing something unusual rather than a machine doing its job.
//!
//! **What an exec event unlocks is larger than what it says.** Until now a
//! launched target's lifecycle was inferred by polling `/proc`, and three
//! defects in `docs/scenarios/proc.md` are races that creates: before the
//! child's `execve` its `/proc` entry still describes this tool's own forked
//! image, and after its exit it describes a zombie. `sched_process_exec` is the
//! instant the first stops being true, published by the kernel rather than
//! guessed at from the outside.
//!
//! Each program checks the tracepoint's prototype against the shape it was
//! written for before it builds an instruction. A raw tracepoint hands its
//! program an array of `u64` and nothing in that array says what any of them
//! is: a program that reads a `task_struct *` out of a slot holding an integer
//! does not fail, it produces a plausible number.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;

use super::btf::Btf;
use super::context::Record;
use super::insn::{
    helper, push_current_filter, push_task_filter, read_field, read_narrow, read_pointer, Insn,
    Program, Subject, TaskFields, Whose, BPF_DW, BPF_W, R10,
};
use super::map::{map_failure, Counters, Hash};
use super::syscall;
use super::transport::{Output, Transport};
use crate::bpf::program::LICENSE;

/// The prototypes these programs are written against, as the kernel publishes
/// them in its own BTF, with the tracepoint's leading data pointer dropped.
pub(super) const EXEC_SIGNATURE: [&str; 3] = ["task_struct *", "pid_t", "linux_binprm *"];
pub(super) const EXIT_SIGNATURE: [&str; 2] = ["task_struct *", "bool"];
/// `sched_process_exit` before it was handed whether the thread was its
/// process's last (6.6 and the kernels before it). The program reads the same
/// fact out of `task->signal->live` there; see [`exit_instructions`].
pub(super) const EXIT_SIGNATURE_WITHOUT_GROUP: [&str; 1] = ["task_struct *"];
pub(super) const SIGNAL_SIGNATURE: [&str; 5] =
    ["int", "kernel_siginfo *", "task_struct *", "int", "int"];

/// How much of a path a record carries.
///
/// A path is up to `PATH_MAX`, which is four kilobytes, and a ring record of
/// four kilobytes is a ring that holds sixty events. Sixty-four bytes is what
/// an executable's path almost always fits in, and what it does not fit in is
/// reported truncated and said to be truncated rather than reported whole and
/// wrong.
const PATH_BYTES: usize = 64;
const COMM_BYTES: usize = 16;

/// One `execve` that succeeded, as the program writes it and userspace reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exec {
    /// `bpf_ktime_get_ns`, which is `CLOCK_MONOTONIC`.
    pub ts_ns: u64,
    pub tid: u32,
    pub tgid: u32,
    /// The thread id the task had before this exec, which the kernel passes as
    /// the tracepoint's second argument. For the usual fork-then-exec it is the
    /// same as `tid`; it differs when a thread other than the group leader
    /// execs, and the whole thread group is then replaced by it.
    pub old_pid: i32,
    pub cpu: u32,
    pub(crate) comm: [u8; COMM_BYTES],
    pub(crate) filename: [u8; PATH_BYTES],
    /// The parent's thread group, `task_struct.real_parent->tgid` at the
    /// exec: what lets a launched run follow its program's descendants
    /// (`execsnoop`'s `PPID`, the twenty-second wave's bpftrace gap 3).
    pub ppid: u32,
    pub(crate) pad: u32,
}

const EXEC_BYTES: usize = 112;

/// One task leaving, as the program writes it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    pub ts_ns: u64,
    pub tid: u32,
    pub tgid: u32,
    /// `task_struct.exit_code`, undecoded. The kernel packs two different
    /// things in it and this carries the word it read; `status` and `signal`
    /// below take it apart, in userspace, where getting it wrong is visible.
    pub exit_code: i32,
    /// Whether this was the last thread of its process. A process that exits is
    /// one event with this set among as many events as it had threads, and a
    /// reader counting processes wants only those.
    pub group_dead: u32,
    pub(crate) comm: [u8; COMM_BYTES],
    pub cpu: u32,
    /// What the program could not read, by bit: [`Exit::CODE_UNKNOWN`] and
    /// [`Exit::GROUP_UNKNOWN`]. Zero from the raw program, which reads both
    /// out of the task; the classic one has neither in its record.
    pub unknown: u32,
}

const EXIT_BYTES: usize = 48;

/// One signal the kernel generated for a task.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal {
    pub ts_ns: u64,
    /// The task the signal is for, which is what the event is keyed on.
    pub target_tid: u32,
    pub target_tgid: u32,
    /// The task that was running when the signal was generated. For a signal
    /// raised by the kernel itself -- a timer, a fault, a dying child -- that is
    /// whatever was interrupted, and it is evidence rather than proof, for the
    /// same reason `sched.wakeup`'s waker is.
    pub sender_tid: u32,
    pub sender_tgid: u32,
    pub(crate) comm: [u8; COMM_BYTES],
    pub sig: i32,
    /// The tracepoint's own `result`: zero when the signal was queued for
    /// delivery, non-zero when it was not -- already pending, or ignored. A
    /// count of generated signals is not a count of delivered ones, and this is
    /// the field that says which this row is.
    pub result: i32,
    /// The name of the task that was running when the signal was generated:
    /// the sender `killsnoop` names (the twenty-second wave's bpftrace gap
    /// 6), evidence in the same sense `sender_tid` is.
    pub(crate) sender_comm: [u8; COMM_BYTES],
}

const SIGNAL_BYTES: usize = 64;

/// `bpf_get_current_comm(buf, size)`, which `insn::helper` does not name.
const GET_CURRENT_COMM: i32 = 16;

/// A name the kernel holds as a fixed-width, NUL-padded array.
fn text(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn long(bytes: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

impl Exec {
    /// The name the task carries after the exec, which the kernel derives from
    /// the last component of the path and truncates to fifteen characters.
    pub fn comm(&self) -> String {
        text(&self.comm)
    }

    /// The path `execve` was given, as far as the record carries it.
    ///
    /// Empty when the kernel could not be read at that address at all, which
    /// the collector counts rather than reporting as a program with no name.
    pub fn filename(&self) -> String {
        text(&self.filename)
    }

    /// Whether the path filled the record, which is the only way a reader can
    /// tell a long path from a complete one.
    pub fn filename_truncated(&self) -> bool {
        self.filename[PATH_BYTES - 1] != 0
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < EXEC_BYTES {
            return None;
        }
        let mut comm = [0u8; COMM_BYTES];
        comm.copy_from_slice(&bytes[24..40]);
        let mut filename = [0u8; PATH_BYTES];
        filename.copy_from_slice(&bytes[40..104]);
        Some(Self {
            ts_ns: long(bytes, 0),
            tid: word(bytes, 8),
            tgid: word(bytes, 12),
            old_pid: word(bytes, 16) as i32,
            cpu: word(bytes, 20),
            comm,
            filename,
            ppid: word(bytes, 104),
            pad: 0,
        })
    }
}

impl Exit {
    /// `exit_code` is not known: the task left without a call to `exit` or
    /// `exit_group` and without a fatal signal this run saw delivered.
    pub const CODE_UNKNOWN: u32 = 1;
    /// `group_dead` is not in the record, and was decided at the drain.
    pub const GROUP_UNKNOWN: u32 = 2;
    /// Whether a core was dumped is not known: a code from the classic
    /// programs is the signal or the status, without the kernel's core bit.
    pub const CORE_UNKNOWN: u32 = 4;

    /// Whether `exit_code` is the task's.
    pub fn code_known(&self) -> bool {
        self.unknown & Self::CODE_UNKNOWN == 0
    }

    pub fn comm(&self) -> String {
        text(&self.comm)
    }

    /// The status the program passed to `exit`, when it left of its own accord.
    ///
    /// `task_struct.exit_code` holds two different things in one word, and
    /// which one it holds is decided by the low seven bits: a task killed by a
    /// signal carries the signal number there, and a task that returned carries
    /// its status in the second byte. Reporting the raw word as a status is how
    /// a program killed by `SIGKILL` comes to be reported as having exited 0.
    pub fn status(&self) -> Option<i32> {
        (self.exit_code & 0x7f == 0).then_some((self.exit_code >> 8) & 0xff)
    }

    /// The signal that killed it, when one did.
    pub fn signal(&self) -> Option<i32> {
        let signal = self.exit_code & 0x7f;
        (signal != 0).then_some(signal)
    }

    /// Whether the kernel dumped core for it, which is the eighth bit.
    pub fn core_dumped(&self) -> bool {
        self.exit_code & 0x80 != 0
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < EXIT_BYTES {
            return None;
        }
        let mut comm = [0u8; COMM_BYTES];
        comm.copy_from_slice(&bytes[24..40]);
        Some(Self {
            ts_ns: long(bytes, 0),
            tid: word(bytes, 8),
            tgid: word(bytes, 12),
            exit_code: word(bytes, 16) as i32,
            group_dead: word(bytes, 20),
            comm,
            cpu: word(bytes, 40),
            unknown: word(bytes, 44),
        })
    }
}

impl Signal {
    /// The name of the task the signal is for.
    pub fn comm(&self) -> String {
        text(&self.comm)
    }

    /// The name of the task that raised it.
    pub fn sender_comm(&self) -> String {
        text(&self.sender_comm)
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < SIGNAL_BYTES {
            return None;
        }
        let mut comm = [0u8; COMM_BYTES];
        comm.copy_from_slice(&bytes[24..40]);
        let mut sender_comm = [0u8; COMM_BYTES];
        sender_comm.copy_from_slice(&bytes[48..64]);
        Some(Self {
            ts_ns: long(bytes, 0),
            target_tid: word(bytes, 8),
            target_tgid: word(bytes, 12),
            sender_tid: word(bytes, 16),
            sender_tgid: word(bytes, 20),
            comm,
            sig: word(bytes, 40) as i32,
            result: word(bytes, 44) as i32,
            sender_comm,
        })
    }
}

// ---- stack layout ----
//
// Stated once per record, as offsets from the frame pointer, so that the
// program's stores and the struct's fields come from the same place. A test
// below crosses the two, because nothing else connects them and a field written
// where the struct does not read it is a plausible number rather than an error.

const EXEC_EVENT: i16 = -120;
const EXEC_TID: i16 = EXEC_EVENT + 8;
const EXEC_TGID: i16 = EXEC_EVENT + 12;
const EXEC_OLD_PID: i16 = EXEC_EVENT + 16;
const EXEC_CPU: i16 = EXEC_EVENT + 20;
const EXEC_COMM: i16 = EXEC_EVENT + 24;
const EXEC_FILENAME: i16 = EXEC_EVENT + 40;
const EXEC_PPID: i16 = EXEC_EVENT + 104;
const EXEC_PAD: i16 = EXEC_EVENT + 108;
/// Scratch, below the record: the `char *` read out of the `linux_binprm`
/// before the string behind it is read.
const EXEC_PATH_PTR: i16 = EXEC_EVENT - 8;
const EXEC_DROP_KEY: i16 = EXEC_EVENT - 16;
/// Sixteen bytes for the target comparison, `insn::push_task_filter`.
const EXEC_FILTER: i16 = EXEC_EVENT - 32;

const EXIT_EVENT: i16 = -48;
const EXIT_TID: i16 = EXIT_EVENT + 8;
const EXIT_TGID: i16 = EXIT_EVENT + 12;
const EXIT_CODE: i16 = EXIT_EVENT + 16;
const EXIT_GROUP_DEAD: i16 = EXIT_EVENT + 20;
const EXIT_COMM: i16 = EXIT_EVENT + 24;
const EXIT_CPU: i16 = EXIT_EVENT + 40;
const EXIT_UNKNOWN: i16 = EXIT_EVENT + 44;
const EXIT_DROP_KEY: i16 = EXIT_EVENT - 8;
const EXIT_FILTER: i16 = EXIT_EVENT - 24;
/// Eight bytes for the walk to `signal->live` on a kernel whose tracepoint
/// does not pass `group_dead`.
const EXIT_SCRATCH: i16 = EXIT_EVENT - 32;

const SIG_EVENT: i16 = -64;
const SIG_TARGET_TID: i16 = SIG_EVENT + 8;
const SIG_TARGET_TGID: i16 = SIG_EVENT + 12;
const SIG_SENDER_TID: i16 = SIG_EVENT + 16;
const SIG_SENDER_TGID: i16 = SIG_EVENT + 20;
const SIG_COMM: i16 = SIG_EVENT + 24;
const SIG_NUMBER: i16 = SIG_EVENT + 40;
const SIG_RESULT: i16 = SIG_EVENT + 44;
const SIG_SENDER_COMM: i16 = SIG_EVENT + 48;
const SIG_DROP_KEY: i16 = SIG_EVENT - 8;
const SIG_FILTER: i16 = SIG_EVENT - 24;

/// Publishes the record on the stack into either transport, the context in
/// `ctx_reg`, and counts it when the ring is full.
///
/// A full ring returns an error, the event is gone, and nothing anywhere else
/// records that it existed -- the BPF ring buffer does not report its own
/// overflows. So the program counts them itself, which is the only reason a
/// stream missing a third of its events does not look like a quiet machine.
pub(super) fn publish_to(
    program: &mut Program,
    output: Output,
    ctx_reg: u8,
    drops_fd: i32,
    event: i16,
    bytes: usize,
    drop_key: i16,
) {
    output.push(program, ctx_reg, event, bytes as i32);

    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    program.push(Insn::st_imm(BPF_W, R10, drop_key, 0));
    program.extend(Insn::load_map_fd(1, drops_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, drop_key as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    // Per-cpu, so this slot belongs to this cpu alone and needs no atomic.
    program.push(Insn::ldx(BPF_DW, 1, 0, 0));
    program.push(Insn::add64_imm(1, 1));
    program.push(Insn::stx(BPF_DW, 0, 0, 1));
}

/// The program on `sched_process_exec`: a task has just become another program.
fn exec_instructions(
    btf: &Btf,
    whose: Whose,
    output: Output,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    crate::bpf::program::prototype(
        btf,
        "sched_process_exec",
        &[&EXEC_SIGNATURE],
        "its arguments",
    )?;
    let fields = TaskFields::locate(btf, whose)?;
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;
    let filename_offset = btf.member_offset("linux_binprm", "filename")?;
    let parent_offset = btf.member_offset("task_struct", "real_parent")?;

    let mut program = Program::new();
    // r6 keeps the context, r7 the task and r8 the `linux_binprm`. All three
    // survive helper calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    program.push(Insn::ldx(BPF_DW, 8, 6, 16));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, EXEC_EVENT, 0));
    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, EXEC_CPU, 0));

    // The second argument is the thread id the task carried before the exec.
    program.push(Insn::ldx(BPF_DW, 1, 6, 8));
    program.push(Insn::stx(BPF_W, R10, EXEC_OLD_PID, 1));

    read_field(&mut program, EXEC_TID, 4, 7, pid_offset);
    read_field(&mut program, EXEC_TGID, 4, 7, tgid_offset);
    // Narrowed to a cgroup before the name and the path are read: the task is
    // the one that exec'd, and the comparison is cheaper than both reads.
    push_task_filter(
        &mut program,
        &fields,
        whose,
        Subject::with_ids(7, EXEC_TID, EXEC_TGID),
        EXEC_FILTER,
        "done",
    );
    read_field(&mut program, EXEC_COMM, COMM_BYTES as i32, 7, comm_offset);

    // The parent's thread group, two reads: the pointer, then its `tgid`. The
    // pointer goes through the path's slot, which is written again below. A
    // read that fails leaves the zero stored first.
    program.push(Insn::st_imm(BPF_W, R10, EXEC_PPID, 0));
    program.push(Insn::st_imm(BPF_W, R10, EXEC_PAD, 0));
    read_field(&mut program, EXEC_PATH_PTR, 8, 7, parent_offset);
    program.push(Insn::ldx(BPF_DW, 9, R10, EXEC_PATH_PTR));
    read_field(&mut program, EXEC_PPID, 4, 9, tgid_offset);

    // The path is two reads: the `char *` out of the `linux_binprm`, and then
    // the string it points at. The second is a `_str` read rather than a fixed
    // one so that it stops at the terminator instead of walking past the end of
    // whatever allocation holds the name.
    read_field(&mut program, EXEC_PATH_PTR, 8, 8, filename_offset);
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, EXEC_FILENAME as i32));
    program.push(Insn::mov64_imm(2, PATH_BYTES as i32));
    program.push(Insn::ldx(BPF_DW, 3, R10, EXEC_PATH_PTR));
    program.push(Insn::call(helper::PROBE_READ_KERNEL_STR));

    publish_to(
        &mut program,
        output,
        6,
        drops_fd,
        EXEC_EVENT,
        EXEC_BYTES,
        EXEC_DROP_KEY,
    );

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `sched_process_exit`: a task is leaving, and says how.
///
/// Two shapes of the tracepoint are read. Since it was handed `group_dead`,
/// the second slot says whether this was the last thread of its process. On
/// 6.6 and before there is no second slot, and the program reads the same fact
/// where the kernel decided it: `do_exit` has already decremented
/// `signal->live` when the tracepoint fires, so a count of zero is the last
/// thread out. Two threads of one process leaving at the same instant can both
/// read zero there, which the collector says.
fn exit_instructions(
    btf: &Btf,
    whose: Whose,
    output: Output,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let passes_group_dead = crate::bpf::program::prototype(
        btf,
        "sched_process_exit",
        &[&EXIT_SIGNATURE, &EXIT_SIGNATURE_WITHOUT_GROUP],
        "its arguments",
    )? == 0;
    let fields = TaskFields::locate(btf, whose)?;
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;
    let exit_code_offset = btf.member_offset("task_struct", "exit_code")?;
    let live = if passes_group_dead {
        None
    } else {
        Some((
            btf.member_offset("task_struct", "signal")?,
            btf.member_offset("signal_struct", "live")?,
        ))
    };

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, EXIT_EVENT, 0));
    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, EXIT_CPU, 0));
    // Every byte of the record is written before it is published; a padding
    // word left uninitialised is whatever was on the stack before.
    program.push(Insn::st_imm(BPF_W, R10, EXIT_UNKNOWN, 0));

    match live {
        // The second argument says whether this was the last thread of its
        // process.
        None => {
            program.push(Insn::ldx(BPF_DW, 1, 6, 8));
            program.push(Insn::stx(BPF_W, R10, EXIT_GROUP_DEAD, 1));
        }
        // `task->signal->live.counter == 0`. A null `signal` is a task this
        // cannot judge, and is recorded as not the last rather than as the last.
        Some((signal, live)) => {
            program.push(Insn::st_imm(BPF_W, R10, EXIT_GROUP_DEAD, 0));
            read_pointer(&mut program, EXIT_SCRATCH, 4, 7, signal);
            program.jump_to("group_known", Insn::jeq_imm(4, 0, 0));
            read_narrow(&mut program, EXIT_SCRATCH, 4, 4, live);
            program.push(Insn::ldx(BPF_W, 1, R10, EXIT_SCRATCH));
            program.jump_to("group_known", Insn::jne_imm(1, 0, 0));
            program.push(Insn::st_imm(BPF_W, R10, EXIT_GROUP_DEAD, 1));
            program.label("group_known");
        }
    }

    read_field(&mut program, EXIT_TID, 4, 7, pid_offset);
    read_field(&mut program, EXIT_TGID, 4, 7, tgid_offset);
    push_task_filter(
        &mut program,
        &fields,
        whose,
        Subject::with_ids(7, EXIT_TID, EXIT_TGID),
        EXIT_FILTER,
        "done",
    );
    read_field(&mut program, EXIT_CODE, 4, 7, exit_code_offset);
    read_field(&mut program, EXIT_COMM, COMM_BYTES as i32, 7, comm_offset);

    publish_to(
        &mut program,
        output,
        6,
        drops_fd,
        EXIT_EVENT,
        EXIT_BYTES,
        EXIT_DROP_KEY,
    );

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `signal_generate`: a signal has been raised for a task.
///
/// `target_tgid` filters the stream to one process, at either end: the signals
/// it receives and the signals it sends are both things a reader watching that
/// process asked about, and a process signalling itself -- which is what a
/// timer, an assertion and a `raise` all look like -- is both.
fn signal_instructions(
    btf: &Btf,
    whose: Whose,
    output: Output,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    crate::bpf::program::prototype(
        btf,
        "signal_generate",
        &[&SIGNAL_SIGNATURE],
        "its arguments",
    )?;
    let fields = TaskFields::locate(btf, whose)?;
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    // The third argument is the task the signal is for.
    program.push(Insn::ldx(BPF_DW, 7, 6, 16));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, SIG_EVENT, 0));

    // This tracepoint runs in the context of whoever raised the signal, so the
    // current task is the sender. That is the same argument `wakeup.rs` makes
    // for attaching to `sched_waking` rather than to `sched_wakeup`.
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, SIG_SENDER_TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, SIG_SENDER_TGID, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, SIG_SENDER_COMM as i32));
    program.push(Insn::mov64_imm(2, COMM_BYTES as i32));
    program.push(Insn::call(GET_CURRENT_COMM));

    // The first argument is the signal number, the fifth the kernel's own
    // verdict on what happened to it.
    program.push(Insn::ldx(BPF_DW, 1, 6, 0));
    program.push(Insn::stx(BPF_W, R10, SIG_NUMBER, 1));
    program.push(Insn::ldx(BPF_DW, 1, 6, 32));
    program.push(Insn::stx(BPF_W, R10, SIG_RESULT, 1));

    read_field(&mut program, SIG_TARGET_TID, 4, 7, pid_offset);
    read_field(&mut program, SIG_TARGET_TGID, 4, 7, tgid_offset);
    read_field(&mut program, SIG_COMM, COMM_BYTES as i32, 7, comm_offset);

    // Kept when either end is the target's: the task the signal is for, or
    // the current task, which raised it.
    if whose != Whose::Any {
        push_task_filter(
            &mut program,
            &fields,
            whose,
            Subject::with_ids(7, SIG_TARGET_TID, SIG_TARGET_TGID),
            SIG_FILTER,
            "not_for_the_target",
        );
        program.jump_to("emit", Insn::ja());
        program.label("not_for_the_target");
        match whose {
            Whose::Cgroup(_) => push_current_filter(&mut program, whose, SIG_FILTER, "done"),
            _ => push_task_filter(
                &mut program,
                &fields,
                whose,
                Subject::with_ids(7, SIG_SENDER_TID, SIG_SENDER_TGID),
                SIG_FILTER,
                "done",
            ),
        }
    }

    program.label("emit");
    publish_to(
        &mut program,
        output,
        6,
        drops_fd,
        SIG_EVENT,
        SIGNAL_BYTES,
        SIG_DROP_KEY,
    );

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// A loaded, attached streaming program with its ring and its drop counter.
///
/// Dropping it detaches: the link, the program and the maps are file
/// descriptors, and the kernel unloads the program when the last one closes.
pub struct EventStream {
    ring: Transport,
    drops: Counters,
    attached: Vec<Attached>,
    /// Maps the classic programs share among themselves, held for as long as
    /// they run.
    _maps: Vec<Hash>,
}

/// One program and what holds it attached.
enum Attached {
    Raw { program: OwnedFd, _link: OwnedFd },
    Classic(super::attach::PerfAttachment),
}

impl Attached {
    fn program(&self) -> &OwnedFd {
        match self {
            Attached::Raw { program, .. } => program,
            Attached::Classic(attachment) => attachment.program(),
        }
    }
}

/// A classic program to attach: its tracepoint, its instructions, its name.
pub(super) struct ClassicProgram {
    pub category: &'static str,
    pub tracepoint: &'static str,
    pub instructions: Vec<Insn>,
    pub name: &'static str,
}

impl EventStream {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::program::skipped(self.attached.iter().map(Attached::program))
    }

    /// Creates the two maps, builds the program with their descriptors, loads
    /// it and attaches it to `tracepoint`.
    pub(super) fn load(
        tracepoint: &str,
        name: &str,
        requested_ring_bytes: usize,
        build: impl FnOnce(&Btf, Output, i32) -> Result<Vec<Insn>, String>,
    ) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        // Every record here is stamped in its first eight bytes.
        let ring = Transport::create(requested_ring_bytes, Some(0), name)?;
        let drops = Counters::create(1, name)
            .map_err(|error| map_failure(&format!("`{tracepoint}` drop counter"), error))?;

        let instructions = build(btf, ring.output(), drops.as_raw_fd())?;
        let program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &instructions,
            name,
            LICENSE,
        )
        .map_err(|error| {
            if error.is_permission() {
                super::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the `{tracepoint}` program: {error}")
            }
        })?;

        let link = crate::bpf::program::attach_raw(tracepoint, &program)?;

        Ok(Self {
            ring,
            drops,
            attached: vec![Attached::Raw {
                program,
                _link: link,
            }],
            _maps: Vec::new(),
        })
    }

    /// The classic tracepoints' form (`bpf:tp`): `build` is handed the
    /// transport and the drop counter and returns the programs, in the order
    /// they are attached, with the maps they share. The one that publishes
    /// last: a helper program attached after it would miss what fired first.
    pub(super) fn load_classic(
        name: &str,
        requested_ring_bytes: usize,
        build: impl FnOnce(Output, i32) -> Result<(Vec<ClassicProgram>, Vec<Hash>), String>,
    ) -> Result<Self, String> {
        let ring = Transport::create(requested_ring_bytes, Some(0), name)?;
        let drops = Counters::create(1, name)
            .map_err(|error| map_failure(&format!("`{name}` drop counter"), error))?;
        let (programs, maps) = build(ring.output(), drops.as_raw_fd())?;
        let mut attached = Vec::with_capacity(programs.len());
        for program in programs {
            attached.push(Attached::Classic(super::attach::tracepoint(
                program.category,
                program.tracepoint,
                &program.instructions,
                program.name,
                super::attach::Scope::System,
            )?));
        }
        Ok(Self {
            ring,
            drops,
            attached,
            _maps: maps,
        })
    }

    /// Hands every record this reader recognises to `handle`, and counts the
    /// ones it does not rather than reinterpreting them.
    pub(super) fn drain<T>(
        &mut self,
        decode: impl Fn(&[u8]) -> Option<T>,
        mut handle: impl FnMut(T),
        undecoded: &mut u64,
    ) {
        self.ring.drain(|bytes| match decode(bytes) {
            Some(record) => handle(record),
            None => *undecoded += 1,
        });
    }

    /// Events the program could not publish because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.drops.read(0)
    }
}

/// `BPF_MAP_TYPE_LRU_HASH`: a map the kernel evicts the oldest entry of when
/// it is full, for what the classic programs remember about the machine's
/// tasks without a free to forget them by.
const BPF_MAP_TYPE_LRU_HASH: u32 = 9;
/// How many tasks the classic programs remember at once.
const CLASSIC_TASKS: u32 = 16384;
/// `CLONE_THREAD`, the flag that makes a new task a thread of its parent's
/// group rather than a process.
const CLONE_THREAD: i32 = 0x0001_0000;
/// Scratch the classic helper programs use for a key and a value.
const HELPER_KEY: i16 = -8;
const HELPER_VALUE: i16 = -16;

fn lru(name: &str, what: &str) -> Result<Hash, String> {
    Hash::create_typed(BPF_MAP_TYPE_LRU_HASH, 4, 4, CLASSIC_TASKS, name)
        .map_err(|error| map_failure(what, error))
}

fn finish(program: Program) -> Result<Vec<Insn>, String> {
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// `task:task_newtask`, in the parent's context: a new process's parent, by
/// its thread group, into `parents` -- the `ppid` a classic exec record does
/// not carry. A new thread is not a process and is left out.
fn newtask_parents(record: &Record, parents_fd: i32) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    record.load(&mut program, 7, 6, "clone_flags", HELPER_VALUE)?;
    program.jump_to("done", Insn::jset_imm(7, CLONE_THREAD, 0));
    record.load(&mut program, 1, 6, "pid", HELPER_VALUE)?;
    program.push(Insn::stx(BPF_W, R10, HELPER_KEY, 1));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, HELPER_VALUE, 0));
    program.extend(Insn::load_map_fd(1, parents_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, HELPER_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, HELPER_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    finish(program)
}

/// The classic program on `sched:sched_process_exec`, which runs in the task
/// that exec'd: its ids, name and cpu from the helpers, `old_pid` and the
/// path from the record, the parent from `parents`.
fn classic_exec_instructions(
    record: &Record,
    whose: Whose,
    parents_fd: i32,
    output: Output,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    if let Whose::Cgroup(_) = whose {
        push_current_filter(&mut program, whose, EXEC_FILTER, "done");
    }
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, EXEC_EVENT, 0));
    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, EXEC_CPU, 0));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, EXEC_TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, EXEC_TGID, 0));
    record.load(&mut program, 1, 6, "old_pid", EXEC_PATH_PTR)?;
    program.push(Insn::stx(BPF_W, R10, EXEC_OLD_PID, 1));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, EXEC_COMM as i32));
    program.push(Insn::mov64_imm(2, COMM_BYTES as i32));
    program.push(Insn::call(GET_CURRENT_COMM));

    program.push(Insn::st_imm(BPF_W, R10, EXEC_PPID, 0));
    program.push(Insn::st_imm(BPF_W, R10, EXEC_PAD, 0));
    program.extend(Insn::load_map_fd(1, parents_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EXEC_TGID as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("parent_known", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_W, 1, 0, 0));
    program.push(Insn::stx(BPF_W, R10, EXEC_PPID, 1));
    program.label("parent_known");

    record.copy_string(&mut program, 6, "filename", EXEC_FILENAME, PATH_BYTES)?;
    publish_to(
        &mut program,
        output,
        6,
        drops_fd,
        EXEC_EVENT,
        EXEC_BYTES,
        EXEC_DROP_KEY,
    );
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    finish(program)
}

/// Writes `(value_reg) ` under the current task's thread group (`by_group`)
/// or thread id into `map`, `flags` as `bpf_map_update_elem` takes them.
fn store_current(program: &mut Program, map: i32, by_group: bool, value_reg: u8, flags: i32) {
    program.push(Insn::stx(BPF_W, R10, HELPER_VALUE, value_reg));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    if by_group {
        program.push(Insn::rsh64_imm(0, 32));
    }
    program.push(Insn::stx(BPF_W, R10, HELPER_KEY, 0));
    program.extend(Insn::load_map_fd(1, map));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, HELPER_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, HELPER_VALUE as i32));
    program.push(Insn::mov64_imm(4, flags));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
}

/// `BPF_NOEXIST`: the first code a dying group records is its code, as the
/// kernel keeps the first `group_exit_code`.
const BPF_NOEXIST: i32 = 1;

/// `syscalls:sys_enter_exit_group` and `sys_enter_exit`: the status a task
/// asked to leave with, `(status & 0xff) << 8` as the kernel stores it, under
/// its group for `exit_group` and under itself for `exit`.
fn exit_call_instructions(record: &Record, map: i32, by_group: bool) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    record.load(&mut program, 7, 6, "error_code", HELPER_VALUE)?;
    program.push(Insn::and64_imm(7, 0xff));
    program.push(Insn::lsh64_imm(7, 8));
    store_current(
        &mut program,
        map,
        by_group,
        7,
        if by_group { BPF_NOEXIST } else { 0 },
    );
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    finish(program)
}

/// Signals whose default action is to ignore, stop or continue a task.
const SURVIVED: [i32; 8] = [17, 18, 19, 20, 21, 22, 23, 28];

/// `signal:signal_generate`: the last signal sent to each task, by the
/// receiver's id. What a `SIGKILL` delivery is read against: a fatal signal
/// to a task with no handler is turned into a `SIGKILL` of its whole group as
/// it is sent (`complete_signal`), and the group leaves with the signal sent,
/// not with the `SIGKILL` each thread is then delivered.
fn generated_instructions(record: &Record, generated: i32) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    record.load(&mut program, 1, 6, "pid", HELPER_VALUE)?;
    program.push(Insn::stx(BPF_W, R10, HELPER_KEY, 1));
    record.load(&mut program, 1, 6, "sig", HELPER_VALUE)?;
    program.push(Insn::stx(BPF_W, R10, HELPER_VALUE, 1));
    program.extend(Insn::load_map_fd(1, generated));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, HELPER_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, HELPER_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    finish(program)
}

/// `signal:signal_deliver`: a signal whose default action ends the task,
/// delivered with no handler, is the code the task and its group leave with
/// -- the group's first such code, the task's own last. `SIGKILL` cannot be
/// caught, so its handler is not looked at, and a `SIGKILL` is the fatal
/// signal last sent to the task or its group when there is one: the kernel
/// kills a group with `SIGKILL` for the signal that was sent.
fn fatal_signal_instructions(
    record: &Record,
    group_codes: i32,
    thread_codes: i32,
    generated: i32,
) -> Result<Vec<Insn>, String> {
    const SIG_DFL: i32 = 0;
    const SENT: [(&str, &str); 2] = [
        ("sent_by_thread", "thread_sent"),
        ("sent_by_group", "group_sent"),
    ];
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    record.load(&mut program, 7, 6, "sig", HELPER_VALUE)?;
    program.jump_to("killed", Insn::jeq_imm(7, 9, 0));
    record.load(&mut program, 1, 6, "sa_handler", HELPER_VALUE)?;
    program.jump_to("done", Insn::jne_imm(1, SIG_DFL, 0));
    for signal in SURVIVED {
        program.jump_to("done", Insn::jeq_imm(7, signal, 0));
    }
    program.jump_to("fatal", Insn::ja());

    // A `SIGKILL`: the fatal signal sent to this thread, else to its group.
    program.label("killed");
    for (by_group, (lookup, found)) in [false, true].into_iter().zip(SENT) {
        program.label(lookup);
        program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
        if by_group {
            program.push(Insn::rsh64_imm(0, 32));
        }
        program.push(Insn::stx(BPF_W, R10, HELPER_KEY, 0));
        program.extend(Insn::load_map_fd(1, generated));
        program.push(Insn::mov64_reg(2, R10));
        program.push(Insn::add64_imm(2, HELPER_KEY as i32));
        program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
        program.jump_to(found, Insn::jne_imm(0, 0, 0));
    }
    program.jump_to("fatal", Insn::ja());
    for (_, found) in SENT {
        program.label(found);
        program.push(Insn::ldx(BPF_W, 8, 0, 0));
        program.jump_to("fatal", Insn::jeq_imm(8, 0, 0));
        for signal in SURVIVED {
            program.jump_to("fatal", Insn::jeq_imm(8, signal, 0));
        }
        program.push(Insn::mov64_reg(7, 8));
        program.jump_to("fatal", Insn::ja());
    }

    program.label("fatal");
    store_current(&mut program, group_codes, true, 7, BPF_NOEXIST);
    store_current(&mut program, thread_codes, false, 7, 0);
    // A delivered signal is no longer the last one sent.
    program.label("done");
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, HELPER_KEY, 0));
    program.extend(Insn::load_map_fd(1, generated));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, HELPER_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    finish(program)
}

/// Forgets a code under the key the record's field or the current task
/// names: an exec starts a group over, and a freed task's id is free.
fn forget_instructions(
    record: Option<(&Record, &str)>,
    map: i32,
    by_group: bool,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    match record {
        Some((record, field)) => {
            record.load(&mut program, 1, 6, field, HELPER_VALUE)?;
            program.push(Insn::stx(BPF_W, R10, HELPER_KEY, 1));
        }
        None => {
            program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
            if by_group {
                program.push(Insn::rsh64_imm(0, 32));
            }
            program.push(Insn::stx(BPF_W, R10, HELPER_KEY, 0));
        }
    }
    program.extend(Insn::load_map_fd(1, map));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, HELPER_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    finish(program)
}

/// The classic program on `sched:sched_process_exit`, in the exiting task:
/// the group's code first, the thread's own then, and unknown when neither
/// was seen; `group_dead` from the record where the kernel publishes it.
fn classic_exit_instructions(
    record: &Record,
    whose: Whose,
    group_codes: i32,
    thread_codes: i32,
    output: Output,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    if let Whose::Cgroup(_) = whose {
        push_current_filter(&mut program, whose, EXIT_FILTER, "done");
    }
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, EXIT_EVENT, 0));
    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, EXIT_CPU, 0));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, EXIT_TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, EXIT_TGID, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, EXIT_COMM as i32));
    program.push(Insn::mov64_imm(2, COMM_BYTES as i32));
    program.push(Insn::call(GET_CURRENT_COMM));

    let mut unknown = Exit::CORE_UNKNOWN as i32;
    program.push(Insn::st_imm(BPF_W, R10, EXIT_GROUP_DEAD, 0));
    if record.has("group_dead") {
        record.load(&mut program, 1, 6, "group_dead", EXIT_SCRATCH)?;
        program.push(Insn::stx(BPF_W, R10, EXIT_GROUP_DEAD, 1));
    } else {
        unknown |= Exit::GROUP_UNKNOWN as i32;
    }

    program.push(Insn::st_imm(BPF_W, R10, EXIT_CODE, 0));
    program.push(Insn::st_imm(BPF_W, R10, EXIT_UNKNOWN, unknown));
    for (map, key, found) in [
        (group_codes, EXIT_TGID, "group_code"),
        (thread_codes, EXIT_TID, "thread_code"),
    ] {
        program.extend(Insn::load_map_fd(1, map));
        program.push(Insn::mov64_reg(2, R10));
        program.push(Insn::add64_imm(2, key as i32));
        program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
        program.jump_to(found, Insn::jne_imm(0, 0, 0));
    }
    program.push(Insn::st_imm(
        BPF_W,
        R10,
        EXIT_UNKNOWN,
        unknown | Exit::CODE_UNKNOWN as i32,
    ));
    program.jump_to("code_read", Insn::ja());
    program.label("group_code");
    program.push(Insn::ldx(BPF_W, 1, 0, 0));
    program.push(Insn::stx(BPF_W, R10, EXIT_CODE, 1));
    program.jump_to("code_read", Insn::ja());
    program.label("thread_code");
    program.push(Insn::ldx(BPF_W, 1, 0, 0));
    program.push(Insn::stx(BPF_W, R10, EXIT_CODE, 1));
    program.label("code_read");
    // The thread's own code is this exit's alone.
    program.extend(Insn::load_map_fd(1, thread_codes));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EXIT_TID as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    publish_to(
        &mut program,
        output,
        6,
        drops_fd,
        EXIT_EVENT,
        EXIT_BYTES,
        EXIT_DROP_KEY,
    );
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    finish(program)
}

/// The classic program on `signal:signal_generate`, in the sender's context:
/// the receiver is the record's `pid`, its thread id; its thread group comes
/// from `threads` where the target's threads are kept, and is found in
/// `/proc` at the drain otherwise.
fn classic_signal_instructions(
    record: &Record,
    whose: Whose,
    threads: Option<i32>,
    output: Output,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, SIG_EVENT, 0));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, SIG_SENDER_TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, SIG_SENDER_TGID, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, SIG_SENDER_COMM as i32));
    program.push(Insn::mov64_imm(2, COMM_BYTES as i32));
    program.push(Insn::call(GET_CURRENT_COMM));
    record.load(&mut program, 1, 6, "sig", SIG_FILTER)?;
    program.push(Insn::stx(BPF_W, R10, SIG_NUMBER, 1));
    record.load(&mut program, 1, 6, "result", SIG_FILTER)?;
    program.push(Insn::stx(BPF_W, R10, SIG_RESULT, 1));
    record.load(&mut program, 1, 6, "pid", SIG_FILTER)?;
    program.push(Insn::stx(BPF_W, R10, SIG_TARGET_TID, 1));
    program.push(Insn::st_imm(BPF_W, R10, SIG_TARGET_TGID, 0));
    record.copy(&mut program, 6, "comm", SIG_COMM, COMM_BYTES)?;
    if let Some(threads) = threads {
        program.extend(Insn::load_map_fd(1, threads));
        program.push(Insn::mov64_reg(2, R10));
        program.push(Insn::add64_imm(2, SIG_TARGET_TID as i32));
        program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
        program.jump_to("group_read", Insn::jeq_imm(0, 0, 0));
        program.push(Insn::ldx(BPF_W, 1, 0, 0));
        program.push(Insn::stx(BPF_W, R10, SIG_TARGET_TGID, 1));
        program.label("group_read");
    }

    // Kept when either end is the target's, as the raw program keeps it.
    match whose {
        Whose::Any => {}
        Whose::Process(root) => {
            program.push(Insn::ldx(BPF_W, 1, R10, SIG_TARGET_TGID));
            program.jump_to("emit", Insn::jeq_imm(1, root as i32, 0));
            program.push(Insn::ldx(BPF_W, 1, R10, SIG_SENDER_TGID));
            program.jump_to("done", Insn::jne_imm(1, root as i32, 0));
        }
        Whose::Thread(tid) => {
            program.push(Insn::ldx(BPF_W, 1, R10, SIG_TARGET_TID));
            program.jump_to("emit", Insn::jeq_imm(1, tid as i32, 0));
            program.push(Insn::ldx(BPF_W, 1, R10, SIG_SENDER_TID));
            program.jump_to("done", Insn::jne_imm(1, tid as i32, 0));
        }
        // The receiver's cgroup is not in the record: what the cgroup's
        // tasks sent is kept, and the collector says so.
        Whose::Cgroup(_) => push_current_filter(&mut program, whose, SIG_FILTER, "done"),
    }
    program.label("emit");
    publish_to(
        &mut program,
        output,
        6,
        drops_fd,
        SIG_EVENT,
        SIGNAL_BYTES,
        SIG_DROP_KEY,
    );
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    finish(program)
}

/// The thread group a live thread belongs to, from `/proc`: what a classic
/// record that names a task by its id leaves to the drain.
fn group_of(tid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:"))?
        .trim()
        .parse()
        .ok()
}

/// The parent of a live process, from `/proc/<pid>/stat`.
fn parent_of(pid: u32) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = text.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Whether any thread of `tgid` is still running: what decides, at the
/// drain, that a classic exit record without `group_dead` was its process's
/// last. A zombie or a dead entry is not running.
fn has_live_thread(tgid: u32) -> bool {
    let Ok(threads) = std::fs::read_dir(format!("/proc/{tgid}/task")) else {
        return false;
    };
    threads.flatten().any(|thread| {
        std::fs::read_to_string(thread.path().join("stat"))
            .ok()
            .and_then(|stat| {
                let (_, rest) = stat.rsplit_once(')')?;
                rest.split_whitespace().next().map(str::to_owned)
            })
            .is_some_and(|state| state != "Z" && state != "X")
    })
}

/// The `sched_process_exec` stream.
pub struct ExecProgram {
    stream: EventStream,
    /// The classic record carries no parent: a process older than the run
    /// has its parent read from `/proc` at the drain.
    classic: bool,
}

impl ExecProgram {
    /// See [`EventStream::skipped`].
    pub fn skipped(&self) -> u64 {
        self.stream.skipped()
    }

    pub fn load(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        let stream = EventStream::load(
            "sched_process_exec",
            "omn_exec",
            requested_ring_bytes,
            move |btf, output, drops| exec_instructions(btf, whose, output, drops),
        )?;
        Ok(Self {
            stream,
            classic: false,
        })
    }

    /// The classic tracepoints (`bpf:tp`), for a kernel without BTF.
    pub fn load_classic(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        let stream =
            EventStream::load_classic("omn_exec", requested_ring_bytes, |output, drops| {
                let newtask = Record::of("task", "task_newtask")?;
                let exec = Record::of("sched", "sched_process_exec")?;
                let parents = lru("omn_exec_par", "exec parents map")?;
                let programs = vec![
                    ClassicProgram {
                        category: "task",
                        tracepoint: "task_newtask",
                        instructions: newtask_parents(&newtask, parents.as_raw_fd())?,
                        name: "omn_exec_fork",
                    },
                    ClassicProgram {
                        category: "sched",
                        tracepoint: "sched_process_exec",
                        instructions: classic_exec_instructions(
                            &exec,
                            whose,
                            parents.as_raw_fd(),
                            output,
                            drops,
                        )?,
                        name: "omn_exec",
                    },
                ];
                Ok((programs, vec![parents]))
            })?;
        Ok(Self {
            stream,
            classic: true,
        })
    }

    pub fn drain(&mut self, mut handle: impl FnMut(Exec), undecoded: &mut u64) {
        let classic = self.classic;
        self.stream.drain(
            Exec::from_bytes,
            |mut exec: Exec| {
                if classic && exec.ppid == 0 {
                    exec.ppid = parent_of(exec.tgid).unwrap_or(0);
                }
                handle(exec)
            },
            undecoded,
        );
    }

    pub fn dropped(&self) -> u64 {
        self.stream.dropped()
    }
}

/// The `sched_process_exit` stream.
pub struct ExitProgram {
    stream: EventStream,
    classic: bool,
}

impl ExitProgram {
    /// See [`EventStream::skipped`].
    pub fn skipped(&self) -> u64 {
        self.stream.skipped()
    }

    pub fn load(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        let stream = EventStream::load(
            "sched_process_exit",
            "omn_exit",
            requested_ring_bytes,
            move |btf, output, drops| exit_instructions(btf, whose, output, drops),
        )?;
        Ok(Self {
            stream,
            classic: false,
        })
    }

    /// The classic tracepoints (`bpf:tp`): the exit's code is the status
    /// `exit` or `exit_group` was called with, or the fatal signal delivered
    /// with no handler, as this run saw them.
    pub fn load_classic(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        let stream =
            EventStream::load_classic("omn_exit", requested_ring_bytes, |output, drops| {
                let exit = Record::of("sched", "sched_process_exit")?;
                let exit_group = Record::of("syscalls", "sys_enter_exit_group")?;
                let exit_call = Record::of("syscalls", "sys_enter_exit")?;
                let deliver = Record::of("signal", "signal_deliver")?;
                let generate = Record::of("signal", "signal_generate")?;
                let free = Record::of("sched", "sched_process_free")?;
                let group = lru("omn_exit_grp", "exit codes map")?;
                let thread = lru("omn_exit_thr", "thread exit codes map")?;
                let sent = lru("omn_exit_sent", "signals sent map")?;
                let (group_fd, thread_fd, sent_fd) =
                    (group.as_raw_fd(), thread.as_raw_fd(), sent.as_raw_fd());
                let program = |category, tracepoint, instructions, name| ClassicProgram {
                    category,
                    tracepoint,
                    instructions,
                    name,
                };
                let programs = vec![
                    program(
                        "syscalls",
                        "sys_enter_exit_group",
                        exit_call_instructions(&exit_group, group_fd, true)?,
                        "omn_exit_xgrp",
                    ),
                    program(
                        "syscalls",
                        "sys_enter_exit",
                        exit_call_instructions(&exit_call, thread_fd, false)?,
                        "omn_exit_xthr",
                    ),
                    program(
                        "signal",
                        "signal_generate",
                        generated_instructions(&generate, sent_fd)?,
                        "omn_exit_sent",
                    ),
                    program(
                        "signal",
                        "signal_deliver",
                        fatal_signal_instructions(&deliver, group_fd, thread_fd, sent_fd)?,
                        "omn_exit_sig",
                    ),
                    program(
                        "sched",
                        "sched_process_exec",
                        forget_instructions(None, group_fd, true)?,
                        "omn_exit_exec",
                    ),
                    program(
                        "sched",
                        "sched_process_free",
                        forget_instructions(Some((&free, "pid")), group_fd, true)?,
                        "omn_exit_free",
                    ),
                    program(
                        "sched",
                        "sched_process_exit",
                        classic_exit_instructions(
                            &exit, whose, group_fd, thread_fd, output, drops,
                        )?,
                        "omn_exit",
                    ),
                ];
                Ok((programs, vec![group, thread, sent]))
            })?;
        Ok(Self {
            stream,
            classic: true,
        })
    }

    /// Whether this kernel's tracepoint passes `group_dead`, rather than the
    /// program reading it out of `signal->live`.
    pub fn group_dead_is_passed() -> bool {
        super::kernel_btf()
            .and_then(|btf| btf.tracepoint_signature("sched_process_exit"))
            .is_ok_and(|signature| signature == EXIT_SIGNATURE)
    }

    /// Hands every exit to `handle`. A classic record without `group_dead` is
    /// decided here: the last exit of its group this drain read, when no
    /// thread of the group is still running.
    pub fn drain(&mut self, mut handle: impl FnMut(Exit), undecoded: &mut u64) {
        if !self.classic {
            self.stream.drain(Exit::from_bytes, handle, undecoded);
            return;
        }
        let mut batch = Vec::new();
        self.stream
            .drain(Exit::from_bytes, |exit| batch.push(exit), undecoded);
        let mut last: BTreeMap<u32, usize> = BTreeMap::new();
        for (at, exit) in batch.iter().enumerate() {
            if exit.unknown & Exit::GROUP_UNKNOWN != 0 {
                last.insert(exit.tgid, at);
            }
        }
        for (at, mut exit) in batch.into_iter().enumerate() {
            if exit.unknown & Exit::GROUP_UNKNOWN != 0 {
                exit.group_dead =
                    u32::from(last.get(&exit.tgid) == Some(&at) && !has_live_thread(exit.tgid));
            }
            handle(exit);
        }
    }

    pub fn dropped(&self) -> u64 {
        self.stream.dropped()
    }
}

/// The `signal_generate` stream.
pub struct SignalProgram {
    stream: EventStream,
    classic: bool,
    /// The target's threads, for a process target on the classic road.
    _threads: Option<super::sched_common::Descendants>,
}

impl SignalProgram {
    /// See [`EventStream::skipped`].
    pub fn skipped(&self) -> u64 {
        self.stream.skipped()
    }

    pub fn load(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        let stream = EventStream::load(
            "signal_generate",
            "omn_signal",
            requested_ring_bytes,
            move |btf, output, drops| signal_instructions(btf, whose, output, drops),
        )?;
        Ok(Self {
            stream,
            classic: false,
            _threads: None,
        })
    }

    /// The classic tracepoint (`bpf:tp`). A process target's threads are
    /// kept by the classic follower, so a signal for one of them is known
    /// as the target's in the kernel.
    pub fn load_classic(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        let threads = match whose {
            Whose::Process(root) => Some(super::sched_common::Descendants::follow_threads(root)?),
            _ => None,
        };
        let threads_fd = threads.as_ref().and_then(|threads| threads.threads_fd());
        let stream =
            EventStream::load_classic("omn_signal", requested_ring_bytes, |output, drops| {
                let generate = Record::of("signal", "signal_generate")?;
                let programs = vec![ClassicProgram {
                    category: "signal",
                    tracepoint: "signal_generate",
                    instructions: classic_signal_instructions(
                        &generate, whose, threads_fd, output, drops,
                    )?,
                    name: "omn_signal",
                }];
                Ok((programs, Vec::new()))
            })?;
        Ok(Self {
            stream,
            classic: true,
            _threads: threads,
        })
    }

    pub fn drain(&mut self, mut handle: impl FnMut(Signal), undecoded: &mut u64) {
        let classic = self.classic;
        self.stream.drain(
            Signal::from_bytes,
            |mut signal: Signal| {
                if classic && signal.target_tgid == 0 {
                    signal.target_tgid = group_of(signal.target_tid).unwrap_or(0);
                }
                handle(signal)
            },
            undecoded,
        );
    }

    pub fn dropped(&self) -> u64 {
        self.stream.dropped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel_btf() -> Option<Btf> {
        match Btf::from_kernel() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    /// The program writes fields at stack offsets and `from_bytes` reads them at
    /// struct offsets. Nothing but this test connects the two, and a mismatch is
    /// a field quietly read from the wrong place -- which is a plausible number
    /// rather than an error.
    #[test]
    fn every_record_struct_matches_what_its_program_writes() {
        use std::mem::offset_of;
        assert_eq!(std::mem::size_of::<Exec>(), EXEC_BYTES);
        assert_eq!(std::mem::size_of::<Exit>(), EXIT_BYTES);
        assert_eq!(std::mem::size_of::<Signal>(), SIGNAL_BYTES);

        for (stack, field, name) in [
            (EXEC_TID, offset_of!(Exec, tid), "exec.tid"),
            (EXEC_TGID, offset_of!(Exec, tgid), "exec.tgid"),
            (EXEC_OLD_PID, offset_of!(Exec, old_pid), "exec.old_pid"),
            (EXEC_CPU, offset_of!(Exec, cpu), "exec.cpu"),
            (EXEC_COMM, offset_of!(Exec, comm), "exec.comm"),
            (EXEC_FILENAME, offset_of!(Exec, filename), "exec.filename"),
            (EXEC_PPID, offset_of!(Exec, ppid), "exec.ppid"),
            (EXEC_PAD, offset_of!(Exec, pad), "exec.pad"),
        ] {
            assert_eq!(
                (stack - EXEC_EVENT) as usize,
                field,
                "the program writes `{name}` where the struct does not read it"
            );
        }
        for (stack, field, name) in [
            (EXIT_TID, offset_of!(Exit, tid), "exit.tid"),
            (EXIT_TGID, offset_of!(Exit, tgid), "exit.tgid"),
            (EXIT_CODE, offset_of!(Exit, exit_code), "exit.exit_code"),
            (
                EXIT_GROUP_DEAD,
                offset_of!(Exit, group_dead),
                "exit.group_dead",
            ),
            (EXIT_COMM, offset_of!(Exit, comm), "exit.comm"),
            (EXIT_CPU, offset_of!(Exit, cpu), "exit.cpu"),
            (EXIT_UNKNOWN, offset_of!(Exit, unknown), "exit.unknown"),
        ] {
            assert_eq!(
                (stack - EXIT_EVENT) as usize,
                field,
                "the program writes `{name}` where the struct does not read it"
            );
        }
        for (stack, field, name) in [
            (
                SIG_TARGET_TID,
                offset_of!(Signal, target_tid),
                "signal.target_tid",
            ),
            (
                SIG_TARGET_TGID,
                offset_of!(Signal, target_tgid),
                "signal.target_tgid",
            ),
            (
                SIG_SENDER_TID,
                offset_of!(Signal, sender_tid),
                "signal.sender_tid",
            ),
            (
                SIG_SENDER_TGID,
                offset_of!(Signal, sender_tgid),
                "signal.sender_tgid",
            ),
            (SIG_COMM, offset_of!(Signal, comm), "signal.comm"),
            (SIG_NUMBER, offset_of!(Signal, sig), "signal.sig"),
            (SIG_RESULT, offset_of!(Signal, result), "signal.result"),
            (
                SIG_SENDER_COMM,
                offset_of!(Signal, sender_comm),
                "signal.sender_comm",
            ),
        ] {
            assert_eq!(
                (stack - SIG_EVENT) as usize,
                field,
                "the program writes `{name}` where the struct does not read it"
            );
        }
    }

    #[test]
    fn a_record_shorter_than_its_struct_is_refused_rather_than_padded() {
        assert!(Exec::from_bytes(&[0u8; 48]).is_none());
        assert!(Exit::from_bytes(&[0u8; 16]).is_none());
        assert!(Signal::from_bytes(&[0u8; 16]).is_none());
    }

    /// The two things `task_struct.exit_code` can be are not distinguishable by
    /// size, and reading the wrong one reports a program killed by `SIGKILL` as
    /// having exited cleanly.
    #[test]
    fn an_exit_code_is_taken_apart_the_way_the_kernel_packs_it() {
        let exited = |code: i32| Exit {
            ts_ns: 0,
            tid: 1,
            tgid: 1,
            exit_code: code,
            group_dead: 1,
            comm: [0; COMM_BYTES],
            cpu: 0,
            unknown: 0,
        };
        // `exit(7)` is 7 in the second byte and nothing in the first.
        assert_eq!(exited(7 << 8).status(), Some(7));
        assert_eq!(exited(7 << 8).signal(), None);
        // Killed by SIGKILL: the signal number is in the low seven bits, and
        // the status is not a status at all.
        assert_eq!(exited(9).signal(), Some(9));
        assert_eq!(exited(9).status(), None);
        assert!(!exited(9).core_dumped());
        assert!(exited(11 | 0x80).core_dumped());
        assert_eq!(exited(11 | 0x80).signal(), Some(11));
    }

    #[test]
    fn a_path_that_filled_the_record_says_it_is_truncated() {
        let mut filename = [b'a'; PATH_BYTES];
        let full = Exec {
            ts_ns: 0,
            tid: 1,
            tgid: 1,
            old_pid: 1,
            cpu: 0,
            comm: [0; COMM_BYTES],
            filename,
            ppid: 1,
            pad: 0,
        };
        assert!(full.filename_truncated());
        assert_eq!(full.filename().len(), PATH_BYTES);
        filename[PATH_BYTES - 1] = 0;
        let fits = Exec { filename, ..full };
        assert!(!fits.filename_truncated());
    }

    /// Every helper each program calls, in the order it calls them.
    fn helpers_called(instructions: &[Insn]) -> Vec<i32> {
        instructions
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect()
    }

    /// Invariant 6, checked in the instruction stream rather than trusted.
    ///
    /// `bpf_ktime_get_boot_ns` includes suspend time, so a program that called
    /// it would timestamp in a different domain from every other collector,
    /// every join across them would be wrong, and nothing would report an
    /// error.
    #[test]
    fn every_program_reads_the_monotonic_clock_and_no_other() {
        const KTIME_GET_BOOT_NS: i32 = 125;
        let Some(btf) = kernel_btf() else { return };
        for (name, program) in [
            (
                "exec",
                exec_instructions(&btf, Whose::Any, Output::Ring(3), 4).expect("a program"),
            ),
            (
                "exit",
                exit_instructions(&btf, Whose::Any, Output::Ring(3), 4).expect("a program"),
            ),
            (
                "signal",
                signal_instructions(&btf, Whose::Any, Output::Ring(3), 4).expect("a program"),
            ),
        ] {
            let called = helpers_called(&program);
            assert!(
                called.contains(&helper::KTIME_GET_NS),
                "the {name} program must timestamp its records"
            );
            assert!(
                !called.contains(&KTIME_GET_BOOT_NS),
                "boot time includes suspend, and mixing clock domains breaks every join silently"
            );
        }
    }

    /// The size handed to `bpf_ringbuf_output` is the size of the record the
    /// rest of the program wrote. A smaller one publishes a truncated record
    /// and a larger one publishes whatever is on the stack behind it.
    #[test]
    fn each_published_record_is_exactly_the_size_of_its_struct() {
        let Some(btf) = kernel_btf() else { return };
        for (program, bytes) in [
            (
                exec_instructions(&btf, Whose::Any, Output::Ring(3), 4).expect("a program"),
                EXEC_BYTES,
            ),
            (
                exit_instructions(&btf, Whose::Any, Output::Ring(3), 4).expect("a program"),
                EXIT_BYTES,
            ),
            (
                signal_instructions(&btf, Whose::Any, Output::Ring(3), 4).expect("a program"),
                SIGNAL_BYTES,
            ),
        ] {
            let output_at = program
                .iter()
                .position(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT)
                .expect("the program publishes its record");
            // r3 is the length argument, set just before the call.
            let length = program[..output_at]
                .iter()
                .rev()
                .find(|insn| insn.regs & 0xf == 3 && insn.code == 0xb7)
                .expect("a length is moved into r3");
            assert_eq!(length.imm as usize, bytes);
        }
    }

    /// The machine-wide signal program keeps every signal on the machine, so it
    /// carries no comparison against a process -- and the filtered one carries
    /// two, because a signal counts at either end.
    #[test]
    fn the_signal_filter_is_present_only_when_a_process_was_named() {
        let Some(btf) = kernel_btf() else { return };
        let machine_wide =
            signal_instructions(&btf, Whose::Any, Output::Ring(3), 4).expect("a program");
        assert!(
            !machine_wide.iter().any(|insn| insn.imm == 4242),
            "a machine-wide program compares against no process"
        );
        let filtered =
            signal_instructions(&btf, Whose::Process(4242), Output::Ring(3), 4).expect("a program");
        assert_eq!(
            filtered.iter().filter(|insn| insn.imm == 4242).count(),
            2,
            "a signal is kept when either end belongs to the target"
        );
    }

    /// The path is read with the helper that stops at the terminator, not with
    /// the fixed-size one. A fixed read of sixty-four bytes over a name that is
    /// shorter walks past the end of whatever allocation holds it.
    #[test]
    fn the_exec_path_is_read_as_a_string_rather_than_as_a_fixed_block() {
        let Some(btf) = kernel_btf() else { return };
        let program = exec_instructions(&btf, Whose::Any, Output::Ring(3), 4).expect("a program");
        let called = helpers_called(&program);
        assert!(
            called.contains(&helper::PROBE_READ_KERNEL_STR),
            "the filename must be read with bpf_probe_read_kernel_str"
        );
        assert_eq!(
            called
                .iter()
                .filter(|id| **id == helper::PROBE_READ_KERNEL)
                .count(),
            6,
            "one fixed read per field taken out of the task -- tid, tgid, comm -- two for the \
             parent's tgid (the pointer, then the field), and one for the path pointer out of \
             the binprm"
        );
    }

    /// The prototype is checked before an instruction is built, and a kernel
    /// with a different one is refused by name rather than read at the wrong
    /// offsets.
    #[test]
    fn a_tracepoint_of_another_shape_is_refused_with_both_prototypes() {
        let Some(btf) = kernel_btf() else { return };
        let Err(reason) = crate::bpf::program::prototype(
            &btf,
            "sched_process_exit",
            &[&EXEC_SIGNATURE],
            "its arguments",
        ) else {
            panic!("`sched_process_exit` does not have `sched_process_exec`'s prototype");
        };
        assert!(reason.contains("this kernel declares"), "{reason}");
        assert!(reason.contains("task_struct *"), "{reason}");
    }

    fn classic(text: &str) -> Record {
        Record::from_layout("test", super::super::attach::parse_layout(text))
    }

    /// The layouts Debian 10's 4.19 publishes, which the classic programs
    /// were written against.
    fn exec_419() -> Record {
        classic(
            "\tfield:__data_loc char[] filename;\toffset:8;\tsize:4;\tsigned:1;\n\
             \tfield:pid_t pid;\toffset:12;\tsize:4;\tsigned:1;\n\
             \tfield:pid_t old_pid;\toffset:16;\tsize:4;\tsigned:1;\n",
        )
    }

    fn exit_419() -> Record {
        classic(
            "\tfield:char comm[16];\toffset:8;\tsize:16;\tsigned:1;\n\
             \tfield:pid_t pid;\toffset:24;\tsize:4;\tsigned:1;\n\
             \tfield:int prio;\toffset:28;\tsize:4;\tsigned:1;\n",
        )
    }

    fn signal_419() -> Record {
        classic(
            "\tfield:int sig;\toffset:8;\tsize:4;\tsigned:1;\n\
             \tfield:int errno;\toffset:12;\tsize:4;\tsigned:1;\n\
             \tfield:int code;\toffset:16;\tsize:4;\tsigned:1;\n\
             \tfield:char comm[16];\toffset:20;\tsize:16;\tsigned:1;\n\
             \tfield:pid_t pid;\toffset:36;\tsize:4;\tsigned:1;\n\
             \tfield:int group;\toffset:40;\tsize:4;\tsigned:1;\n\
             \tfield:int result;\toffset:44;\tsize:4;\tsigned:1;\n",
        )
    }

    /// Every classic program builds against 4.19's layouts, passes the
    /// audit, publishes through the transport it is handed, the context
    /// first, and reads the monotonic clock.
    #[test]
    fn the_classic_programs_build_against_the_4_19_records() {
        let output = Output::PerfArray(3);
        for (name, program) in [
            (
                "exec",
                classic_exec_instructions(&exec_419(), Whose::Any, 5, output, 4),
            ),
            (
                "exit",
                classic_exit_instructions(&exit_419(), Whose::Any, 5, 6, output, 4),
            ),
            (
                "signal",
                classic_signal_instructions(
                    &signal_419(),
                    Whose::Process(4242),
                    Some(7),
                    output,
                    4,
                ),
            ),
        ] {
            let program = program.unwrap_or_else(|error| panic!("{name}: {error}"));
            let called = helpers_called(&program);
            assert!(called.contains(&helper::KTIME_GET_NS), "{name}");
            assert!(program.iter().any(Output::is_output_call), "{name}");
            assert!(!called.contains(&helper::RINGBUF_OUTPUT), "{name}");
        }
    }

    /// Without `group_dead` in its record the exit program says so in the
    /// record, for the drain to decide; with it the field is read.
    #[test]
    fn a_classic_exit_marks_what_its_record_lacks() {
        let output = Output::PerfArray(3);
        let lacking = classic_exit_instructions(&exit_419(), Whose::Any, 5, 6, output, 4).unwrap();
        assert!(lacking.contains(&Insn::st_imm(
            BPF_W,
            R10,
            EXIT_UNKNOWN,
            (Exit::GROUP_UNKNOWN | Exit::CORE_UNKNOWN) as i32
        )));
        let with_group = classic(
            "\tfield:char comm[16];\toffset:8;\tsize:16;\tsigned:1;\n\
             \tfield:pid_t pid;\toffset:24;\tsize:4;\tsigned:1;\n\
             \tfield:int prio;\toffset:28;\tsize:4;\tsigned:1;\n\
             \tfield:bool group_dead;\toffset:32;\tsize:1;\tsigned:0;\n",
        );
        let read = classic_exit_instructions(&with_group, Whose::Any, 5, 6, output, 4).unwrap();
        assert!(read.contains(&Insn::ldx(super::super::insn::BPF_B, 1, 6, 32)));
        assert!(read.contains(&Insn::st_imm(
            BPF_W,
            R10,
            EXIT_UNKNOWN,
            Exit::CORE_UNKNOWN as i32
        )));
    }

    /// A signal is fatal by default unless it is one of the eight that stop,
    /// continue or are ignored, and `SIGKILL` whatever its handler says.
    #[test]
    fn the_fatal_signal_program_skips_the_signals_a_task_survives() {
        let deliver = classic(
            "\tfield:int sig;\toffset:8;\tsize:4;\tsigned:1;\n\
             \tfield:unsigned long sa_handler;\toffset:24;\tsize:8;\tsigned:0;\n",
        );
        let program = fatal_signal_instructions(&deliver, 5, 6, 7).unwrap();
        for survived in [17, 18, 19, 20, 21, 22, 23, 28] {
            assert!(
                program
                    .iter()
                    .any(|insn| insn.code == 0x15 && insn.imm == survived),
                "{survived}"
            );
        }
        assert!(program
            .iter()
            .any(|insn| insn.code == 0x15 && insn.imm == 9));
    }
}
