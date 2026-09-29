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

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{
    helper, push_current_filter, push_task_filter, read_field, read_narrow, read_pointer, Insn,
    Program, Subject, TaskFields, Whose, BPF_DW, BPF_W, R10,
};
use super::map::{map_failure, Counters};
use super::ring::{ring_bytes, RingBuffer};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// `bpf_probe_read_kernel` is GPL-only, and every one of these programs reads a
/// field out of a `task_struct` through it.
const LICENSE: &str = "GPL";

/// The prototypes these programs are written against, as the kernel publishes
/// them in its own BTF, with the tracepoint's leading data pointer dropped.
const EXEC_SIGNATURE: [&str; 3] = ["task_struct *", "pid_t", "linux_binprm *"];
const EXIT_SIGNATURE: [&str; 2] = ["task_struct *", "bool"];
/// `sched_process_exit` before it was handed whether the thread was its
/// process's last (6.6 and the kernels before it). The program reads the same
/// fact out of `task->signal->live` there; see [`exit_instructions`].
const EXIT_SIGNATURE_WITHOUT_GROUP: [&str; 1] = ["task_struct *"];
const SIGNAL_SIGNATURE: [&str; 5] = ["int", "kernel_siginfo *", "task_struct *", "int", "int"];

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
}

const EXEC_BYTES: usize = 104;

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
    pub(crate) pad: u32,
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
}

const SIGNAL_BYTES: usize = 48;

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
        })
    }
}

impl Exit {
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
            pad: 0,
        })
    }
}

impl Signal {
    /// The name of the task the signal is for.
    pub fn comm(&self) -> String {
        text(&self.comm)
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < SIGNAL_BYTES {
            return None;
        }
        let mut comm = [0u8; COMM_BYTES];
        comm.copy_from_slice(&bytes[24..40]);
        Some(Self {
            ts_ns: long(bytes, 0),
            target_tid: word(bytes, 8),
            target_tgid: word(bytes, 12),
            sender_tid: word(bytes, 16),
            sender_tgid: word(bytes, 20),
            comm,
            sig: word(bytes, 40) as i32,
            result: word(bytes, 44) as i32,
        })
    }
}

// ---- stack layout ----
//
// Stated once per record, as offsets from the frame pointer, so that the
// program's stores and the struct's fields come from the same place. A test
// below crosses the two, because nothing else connects them and a field written
// where the struct does not read it is a plausible number rather than an error.

const EXEC_EVENT: i16 = -112;
const EXEC_TID: i16 = EXEC_EVENT + 8;
const EXEC_TGID: i16 = EXEC_EVENT + 12;
const EXEC_OLD_PID: i16 = EXEC_EVENT + 16;
const EXEC_CPU: i16 = EXEC_EVENT + 20;
const EXEC_COMM: i16 = EXEC_EVENT + 24;
const EXEC_FILENAME: i16 = EXEC_EVENT + 40;
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
const EXIT_PAD: i16 = EXIT_EVENT + 44;
const EXIT_DROP_KEY: i16 = EXIT_EVENT - 8;
const EXIT_FILTER: i16 = EXIT_EVENT - 24;
/// Eight bytes for the walk to `signal->live` on a kernel whose tracepoint
/// does not pass `group_dead`.
const EXIT_SCRATCH: i16 = EXIT_EVENT - 32;

const SIG_EVENT: i16 = -48;
const SIG_TARGET_TID: i16 = SIG_EVENT + 8;
const SIG_TARGET_TGID: i16 = SIG_EVENT + 12;
const SIG_SENDER_TID: i16 = SIG_EVENT + 16;
const SIG_SENDER_TGID: i16 = SIG_EVENT + 20;
const SIG_COMM: i16 = SIG_EVENT + 24;
const SIG_NUMBER: i16 = SIG_EVENT + 40;
const SIG_RESULT: i16 = SIG_EVENT + 44;
const SIG_DROP_KEY: i16 = SIG_EVENT - 8;
const SIG_FILTER: i16 = SIG_EVENT - 24;

/// Publishes the record on the stack, and counts it when the ring is full.
///
/// A full ring returns an error, the event is gone, and nothing anywhere else
/// records that it existed -- the BPF ring buffer does not report its own
/// overflows. So the program counts them itself, which is the only reason a
/// stream missing a third of its events does not look like a quiet machine.
fn publish(
    program: &mut Program,
    ring_fd: i32,
    drops_fd: i32,
    event: i16,
    bytes: usize,
    drop_key: i16,
) {
    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, event as i32));
    program.push(Insn::mov64_imm(3, bytes as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));

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

/// Refuses a kernel whose tracepoint is not the shape the program reads.
fn check_signature(btf: &Btf, tracepoint: &str, expected: &[&str]) -> Result<(), String> {
    let signature = btf.tracepoint_signature(tracepoint)?;
    if signature != expected {
        return Err(format!(
            "this kernel declares `{tracepoint}({})`, and this program is written for \
             `{tracepoint}({})`: it reads its arguments out of fixed slots, and against another \
             prototype those slots hold something else",
            signature.join(", "),
            expected.join(", "),
        ));
    }
    Ok(())
}

/// The program on `sched_process_exec`: a task has just become another program.
fn exec_instructions(
    btf: &Btf,
    whose: Whose,
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    check_signature(btf, "sched_process_exec", &EXEC_SIGNATURE)?;
    let fields = TaskFields::locate(btf, whose)?;
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;
    let filename_offset = btf.member_offset("linux_binprm", "filename")?;

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

    publish(
        &mut program,
        ring_fd,
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
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let signature = btf.tracepoint_signature("sched_process_exit")?;
    let passes_group_dead = if signature == EXIT_SIGNATURE {
        true
    } else if signature == EXIT_SIGNATURE_WITHOUT_GROUP {
        false
    } else {
        return Err(format!(
            "this kernel declares `sched_process_exit({})`, and this program is written for \
             `sched_process_exit({})` or `sched_process_exit({})`: it reads its arguments out \
             of fixed slots, and against another prototype those slots hold something else",
            signature.join(", "),
            EXIT_SIGNATURE.join(", "),
            EXIT_SIGNATURE_WITHOUT_GROUP.join(", "),
        ));
    };
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
    program.push(Insn::st_imm(BPF_W, R10, EXIT_PAD, 0));

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

    publish(
        &mut program,
        ring_fd,
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
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    check_signature(btf, "signal_generate", &SIGNAL_SIGNATURE)?;
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
    publish(
        &mut program,
        ring_fd,
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
    ring: RingBuffer,
    drops: Counters,
    program: OwnedFd,
    _link: OwnedFd,
}

impl EventStream {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.program])
    }

    /// Creates the two maps, builds the program with their descriptors, loads
    /// it and attaches it to `tracepoint`.
    fn load(
        tracepoint: &str,
        name: &str,
        requested_ring_bytes: usize,
        build: impl FnOnce(&Btf, i32, i32) -> Result<Vec<Insn>, String>,
    ) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let bytes = ring_bytes(requested_ring_bytes);

        let ring_map = syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, name)
            .map_err(|error| map_failure(&format!("`{tracepoint}` ring buffer"), error))?;
        let drops = Counters::create(1, name)
            .map_err(|error| map_failure(&format!("`{tracepoint}` drop counter"), error))?;

        let instructions = build(btf, ring_map.as_raw_fd(), drops.as_raw_fd())?;
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

        let link = syscall::raw_tracepoint_open(tracepoint, program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `{tracepoint}` tracepoint: {error}"))?;

        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the `{tracepoint}` ring buffer: {error}"))?;

        Ok(Self {
            ring,
            drops,
            program,
            _link: link,
        })
    }

    /// Hands every record this reader recognises to `handle`, and counts the
    /// ones it does not rather than reinterpreting them.
    fn drain<T>(
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

/// The `sched_process_exec` stream.
pub struct ExecProgram(EventStream);

impl ExecProgram {
    /// See [`EventStream::skipped`].
    pub fn skipped(&self) -> u64 {
        self.0.skipped()
    }

    pub fn load(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        EventStream::load(
            "sched_process_exec",
            "omn_exec",
            requested_ring_bytes,
            move |btf, ring, drops| exec_instructions(btf, whose, ring, drops),
        )
        .map(Self)
    }

    pub fn drain(&mut self, handle: impl FnMut(Exec), undecoded: &mut u64) {
        self.0.drain(Exec::from_bytes, handle, undecoded);
    }

    pub fn dropped(&self) -> u64 {
        self.0.dropped()
    }
}

/// The `sched_process_exit` stream.
pub struct ExitProgram(EventStream);

impl ExitProgram {
    /// See [`EventStream::skipped`].
    pub fn skipped(&self) -> u64 {
        self.0.skipped()
    }

    pub fn load(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        EventStream::load(
            "sched_process_exit",
            "omn_exit",
            requested_ring_bytes,
            move |btf, ring, drops| exit_instructions(btf, whose, ring, drops),
        )
        .map(Self)
    }

    /// Whether this kernel's tracepoint passes `group_dead`, rather than the
    /// program reading it out of `signal->live`.
    pub fn group_dead_is_passed() -> bool {
        super::kernel_btf()
            .and_then(|btf| btf.tracepoint_signature("sched_process_exit"))
            .is_ok_and(|signature| signature == EXIT_SIGNATURE)
    }

    pub fn drain(&mut self, handle: impl FnMut(Exit), undecoded: &mut u64) {
        self.0.drain(Exit::from_bytes, handle, undecoded);
    }

    pub fn dropped(&self) -> u64 {
        self.0.dropped()
    }
}

/// The `signal_generate` stream.
pub struct SignalProgram(EventStream);

impl SignalProgram {
    /// See [`EventStream::skipped`].
    pub fn skipped(&self) -> u64 {
        self.0.skipped()
    }

    pub fn load(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        EventStream::load(
            "signal_generate",
            "omn_signal",
            requested_ring_bytes,
            move |btf, ring, drops| signal_instructions(btf, whose, ring, drops),
        )
        .map(Self)
    }

    pub fn drain(&mut self, handle: impl FnMut(Signal), undecoded: &mut u64) {
        self.0.drain(Signal::from_bytes, handle, undecoded);
    }

    pub fn dropped(&self) -> u64 {
        self.0.dropped()
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
            (EXIT_PAD, offset_of!(Exit, pad), "exit.pad"),
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
            pad: 0,
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
                exec_instructions(&btf, Whose::Any, 3, 4).expect("a program"),
            ),
            (
                "exit",
                exit_instructions(&btf, Whose::Any, 3, 4).expect("a program"),
            ),
            (
                "signal",
                signal_instructions(&btf, Whose::Any, 3, 4).expect("a program"),
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
                exec_instructions(&btf, Whose::Any, 3, 4).expect("a program"),
                EXEC_BYTES,
            ),
            (
                exit_instructions(&btf, Whose::Any, 3, 4).expect("a program"),
                EXIT_BYTES,
            ),
            (
                signal_instructions(&btf, Whose::Any, 3, 4).expect("a program"),
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
        let machine_wide = signal_instructions(&btf, Whose::Any, 3, 4).expect("a program");
        assert!(
            !machine_wide.iter().any(|insn| insn.imm == 4242),
            "a machine-wide program compares against no process"
        );
        let filtered = signal_instructions(&btf, Whose::Process(4242), 3, 4).expect("a program");
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
        let program = exec_instructions(&btf, Whose::Any, 3, 4).expect("a program");
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
            4,
            "one fixed read per field taken out of the task -- tid, tgid, comm -- and one for \
             the path pointer out of the binprm"
        );
    }

    /// The prototype is checked before an instruction is built, and a kernel
    /// with a different one is refused by name rather than read at the wrong
    /// offsets.
    #[test]
    fn a_tracepoint_of_another_shape_is_refused_with_both_prototypes() {
        let Some(btf) = kernel_btf() else { return };
        let Err(reason) = check_signature(&btf, "sched_process_exit", &EXEC_SIGNATURE) else {
            panic!("`sched_process_exit` does not have `sched_process_exec`'s prototype");
        };
        assert!(reason.contains("this kernel declares"), "{reason}");
        assert!(reason.contains("task_struct *"), "{reason}");
    }
}
