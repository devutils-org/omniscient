// SPDX-License-Identifier: MIT OR GPL-2.0
//! The `sched_waking` program: who unblocks whom.
//!
//! It attaches to `sched_waking` rather than to `sched_wakeup`, and the
//! difference is the entire point of the source. `sched_waking` runs in the
//! context of the task doing the waking, so `bpf_get_current_pid_tgid` is the
//! waker. `sched_wakeup` can run on the woken task's cpu, after an inter
//! processor interrupt, where the current task is whatever happened to be
//! running there -- a waker attribution taken from it would be wrong exactly on
//! the cross-cpu wakeups that matter most.
//!
//! What comes out is one record per wakeup: when, who woke, who was woken, and
//! on which cpu the wakeup was issued. That is the raw material of the `WAKEUP`
//! join: the only way to carry latency across a block without instrumenting the
//! application.
//!
//! "The context of the task doing the waking" is not always a task's. A wakeup
//! issued from a hard interrupt, a softirq or an NMI runs on whatever task the
//! interrupt landed on, and `bpf_get_current_pid_tgid` then names a task that
//! had nothing to do with it (finding 48: a busy machine's disk, network and
//! timer wakeups were credited to a half-blocked workload by the thousand). So
//! the program reads the cpu's preempt count, and a wakeup issued with an
//! interrupt bit set is recorded with the interrupt as its waker -- tid and pid
//! 0, the context in the record -- and kept for a target only when its wakee is
//! the target's.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{
    helper, push_current_filter, Insn, Program, Subject, TaskFields, Whose, BPF_DW, BPF_W, R10,
};
use super::map::{map_failure, Counters};
use super::ring::{ring_bytes, RingBuffer};
use super::sched_common::{push_followed_filter, text_of, Descendants, Followed};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// One wakeup, as the program writes it and userspace reads it.
///
/// `repr(C)` on both sides of the ring is the contract: the program writes
/// these fields at these offsets, and `from_bytes` below reads them back.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wakeup {
    /// `bpf_ktime_get_ns`, which is `CLOCK_MONOTONIC` -- never
    /// `bpf_ktime_get_boot_ns`, which includes suspend time and would put this
    /// stream in a different domain from every other collector.
    pub ts_ns: u64,
    pub waker_tid: u32,
    pub waker_pid: u32,
    pub wakee_tid: u32,
    pub wakee_pid: u32,
    /// The cpu the wakeup was issued on, which is the waker's.
    pub cpu: u32,
    /// The interrupt bits of the cpu's preempt count when the wakeup was
    /// issued -- NMI, hard interrupt and *serving* a softirq -- or 0 in a
    /// task's context. Read through `context()`.
    context: u32,
    /// The woken task's name, read out of its `task_struct`.
    comm: [u8; 16],
    /// The current task's name: the waker's in a task's context, and in an
    /// interrupt's the task the interrupt was served on.
    current_comm: [u8; 16],
    /// In an interrupt's context, the thread the interrupt was served on --
    /// the one it landed on, or the one whose `local_bh_enable` served a
    /// softirq -- which is the waker's tid before it is cleared; 0 in a
    /// task's.
    pub served_in_tid: u32,
    /// The hard interrupt whose handler was running, plus one, or 0: no
    /// registered handler was (the local timer, an inter-processor
    /// interrupt), or the numbers were not read.
    irq: u32,
    /// The softirq vector being served, plus one, or 0.
    softirq: u32,
    /// Written as zero, so that no byte of the record is stack garbage.
    pad: u32,
}

/// Where a wakeup was issued from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakerContext {
    /// A task's own context: the waker is the task that was running.
    Task,
    /// Serving a softirq, either at an interrupt's exit or when a task
    /// re-enabled bottom halves; the task it ran on is not the waker.
    Softirq,
    /// A hard interrupt handler.
    Hardirq,
    /// A non-maskable interrupt.
    Nmi,
}

impl WakerContext {
    /// Whether the waker is an interrupt rather than a task.
    pub fn is_interrupt(self) -> bool {
        self != Self::Task
    }

    /// The name the event carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Softirq => "softirq",
            Self::Hardirq => "hardirq",
            Self::Nmi => "nmi",
        }
    }
}

/// The bits of `preempt_count` that say a cpu is in an interrupt, as
/// `include/linux/preempt.h` lays them out: `NMI_MASK` (bits 20-23),
/// `HARDIRQ_MASK` (bits 16-19), and of the softirq byte only
/// `SOFTIRQ_OFFSET` (bit 8), which is set while a softirq is *served*. The rest
/// of that byte counts `local_bh_disable`, which a task in its own context
/// holds around a socket lock, and a wakeup made there is the task's.
const NMI_MASK: u32 = 0x00f0_0000;
const HARDIRQ_MASK: u32 = 0x000f_0000;
const SOFTIRQ_OFFSET: u32 = 0x0000_0100;
const INTERRUPT_BITS: u32 = NMI_MASK | HARDIRQ_MASK | SOFTIRQ_OFFSET;

/// The size of the record the program writes. Stated once, used by the program
/// and checked against the struct.
const RECORD_BYTES: usize = 80;

/// The softirq vectors, in `enum` order from `include/linux/interrupt.h`;
/// `softirq_to_name` in `kernel/softirq.c` spells them the same way.
const SOFTIRQ_NAMES: [&str; 10] = [
    "HI", "TIMER", "NET_TX", "NET_RX", "BLOCK", "IRQ_POLL", "TASKLET", "SCHED", "HRTIMER", "RCU",
];

impl Wakeup {
    /// The woken task's name, as the kernel holds it.
    pub fn wakee_comm(&self) -> String {
        text_of(&self.comm)
    }

    /// The name of the task that was current when the wakeup was issued: the
    /// waker in a task's context, the task an interrupt was served on in an
    /// interrupt's.
    pub fn current_comm(&self) -> String {
        text_of(&self.current_comm)
    }

    /// The hard interrupt whose handler issued the wakeup, when the context is
    /// a hard interrupt and the handler is a registered one.
    pub fn irq(&self) -> Option<u32> {
        (self.context() == WakerContext::Hardirq && self.irq != 0).then(|| self.irq - 1)
    }

    /// The softirq vector being served, by the kernel's name for it, when the
    /// context is a softirq.
    pub fn softirq(&self) -> Option<String> {
        if self.context() != WakerContext::Softirq || self.softirq == 0 {
            return None;
        }
        let vector = self.softirq - 1;
        Some(
            SOFTIRQ_NAMES
                .get(vector as usize)
                .map_or_else(|| vector.to_string(), |name| (*name).to_owned()),
        )
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().ok().unwrap());
        let mut comm = [0u8; 16];
        comm.copy_from_slice(&bytes[32..48]);
        let mut current_comm = [0u8; 16];
        current_comm.copy_from_slice(&bytes[48..64]);
        Some(Self {
            ts_ns: u64::from_ne_bytes(bytes[0..8].try_into().ok()?),
            waker_tid: word(8),
            waker_pid: word(12),
            wakee_tid: word(16),
            wakee_pid: word(20),
            cpu: word(24),
            context: word(28),
            comm,
            current_comm,
            served_in_tid: word(64),
            irq: word(68),
            softirq: word(72),
            pad: word(76),
        })
    }

    /// Where the wakeup was issued from. Anything but `Task` has a waker of
    /// tid and pid 0: the interrupt, not the task it interrupted.
    pub fn context(&self) -> WakerContext {
        if self.context & NMI_MASK != 0 {
            WakerContext::Nmi
        } else if self.context & HARDIRQ_MASK != 0 {
            WakerContext::Hardirq
        } else if self.context & SOFTIRQ_OFFSET != 0 {
            WakerContext::Softirq
        } else {
            WakerContext::Task
        }
    }
}

/// Where this kernel keeps the cpu's preempt count: a per-cpu variable's BTF
/// id, and the offset of the count inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreemptCount {
    btf_id: u32,
    offset: i32,
}

impl PreemptCount {
    /// Finds it on x86, in either of the two layouts the kernels this tool
    /// runs on have used: `__preempt_count` on its own (up to 6.1, and again
    /// since `pcpu_hot` was taken apart), or the `preempt_count` member of
    /// `pcpu_hot` (6.2 to 6.14). Another architecture keeps it in the task's
    /// `thread_info`, which this does not read, and gets `None`.
    pub fn locate(btf: &Btf) -> Option<Self> {
        if let Some(btf_id) = btf.var_id("__preempt_count") {
            return Some(Self { btf_id, offset: 0 });
        }
        let btf_id = btf.var_id("pcpu_hot")?;
        let offset = btf.member_offset("pcpu_hot", "preempt_count").ok()?;
        Some(Self { btf_id, offset })
    }
}

/// Stack offsets, negative from the frame pointer, matching `Wakeup`.
const EVENT: i16 = -(RECORD_BYTES as i16);
const WAKER_TID: i16 = EVENT + 8;
const WAKER_PID: i16 = EVENT + 12;
const WAKEE_TID: i16 = EVENT + 16;
const WAKEE_PID: i16 = EVENT + 20;
const CPU: i16 = EVENT + 24;
const CONTEXT: i16 = EVENT + 28;
const COMM: i16 = EVENT + 32;
const CURRENT_COMM: i16 = EVENT + 48;
const SERVED_IN: i16 = EVENT + 64;
const IRQ: i16 = EVENT + 68;
const SOFTIRQ: i16 = EVENT + 72;
const PAD: i16 = EVENT + 76;
/// The slots of the per-cpu map the interrupt programs write: the hard
/// interrupt whose handler is running, and the softirq vector being served.
const HARD_SLOT: i32 = 0;
const SOFT_SLOT: i32 = 1;
/// The one-word key of the drop counter, below the record, and of the
/// interrupt map's lookups.
const DROP_KEY: i16 = EVENT - 8;
/// Sixteen bytes for the target comparison, `insn::push_task_filter`.
const FILTER_SCRATCH: i16 = DROP_KEY - 16;

/// Builds the program for a target, or for the whole machine.
///
/// The target's thread group id is written into the instruction stream rather
/// than kept in a map: it is known before the program is loaded and never
/// changes during a run, so a map lookup per wakeup would buy nothing and cost
/// something on a path that fires tens of thousands of times a second.
fn instructions(
    btf: &Btf,
    preempt: Option<PreemptCount>,
    followed: Followed,
    ring_fd: i32,
    drops_fd: i32,
    interrupts_fd: Option<i32>,
) -> Result<Vec<Insn>, String> {
    let whose = followed.whose;
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;
    let fields = TaskFields::locate(btf, whose)?;

    let mut program = Program::new();
    // r6 keeps the context and r7 the task being woken; both survive helper
    // calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    // The first argument of `sched_waking` is the task_struct being woken.
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, EVENT, 0));

    // The low half is the thread id, the high half the process id.
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, WAKER_TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, WAKER_PID, 0));

    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, CPU, 0));

    // The current task's name, which is the waker's in a task's context.
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, CURRENT_COMM as i32));
    program.push(Insn::mov64_imm(2, 16));
    program.push(Insn::call(helper::GET_CURRENT_COMM));
    for slot in [SERVED_IN, IRQ, SOFTIRQ, PAD] {
        program.push(Insn::st_imm(BPF_W, R10, slot, 0));
    }

    // In an interrupt the current task is only where the interrupt landed.
    // The waker becomes the interrupt -- tid and pid 0, which also no target
    // filter below can match, so such a wakeup is kept only for its wakee.
    match preempt {
        Some(preempt) => {
            program.extend(Insn::load_btf_id(1, preempt.btf_id));
            program.push(Insn::call(helper::THIS_CPU_PTR));
            // `bpf_this_cpu_ptr` never returns null for a per-cpu variable.
            program.push(Insn::ldx(BPF_W, 1, 0, preempt.offset as i16));
            program.push(Insn::and64_imm(1, INTERRUPT_BITS as i32));
            program.push(Insn::stx(BPF_W, R10, CONTEXT, 1));
            program.jump_to("in_a_task", Insn::jeq_imm(1, 0, 0));
            // The task the interrupt was served on is kept beside it, under
            // its own name: not the waker, but where a softirq served in a
            // task's `local_bh_enable` came from.
            program.push(Insn::ldx(BPF_W, 1, R10, WAKER_TID));
            program.push(Insn::stx(BPF_W, R10, SERVED_IN, 1));
            program.push(Insn::st_imm(BPF_W, R10, WAKER_TID, 0));
            program.push(Insn::st_imm(BPF_W, R10, WAKER_PID, 0));
            // Which interrupt, as the interrupt programs left it on this cpu.
            if let Some(interrupts_fd) = interrupts_fd {
                for (slot, into, next) in [
                    (HARD_SLOT, IRQ, "hard_read"),
                    (SOFT_SLOT, SOFTIRQ, "soft_read"),
                ] {
                    program.push(Insn::st_imm(BPF_W, R10, DROP_KEY, slot));
                    program.extend(Insn::load_map_fd(1, interrupts_fd));
                    program.push(Insn::mov64_reg(2, R10));
                    program.push(Insn::add64_imm(2, DROP_KEY as i32));
                    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
                    program.jump_to(next, Insn::jeq_imm(0, 0, 0));
                    program.push(Insn::ldx(BPF_W, 1, 0, 0));
                    program.push(Insn::stx(BPF_W, R10, into, 1));
                    program.label(next);
                }
            }
            program.label("in_a_task");
        }
        // Every byte of the record is written before it is published; a word
        // left uninitialised is stack garbage from whatever ran before.
        None => {
            program.push(Insn::st_imm(BPF_W, R10, CONTEXT, 0));
        }
    }

    for (into, size, field_offset) in [
        (WAKEE_TID, 4, pid_offset),
        (WAKEE_PID, 4, tgid_offset),
        (COMM, 16, comm_offset),
    ] {
        program.push(Insn::mov64_reg(1, R10));
        program.push(Insn::add64_imm(1, into as i32));
        program.push(Insn::mov64_imm(2, size));
        program.push(Insn::mov64_reg(3, 7));
        program.push(Insn::add64_imm(3, field_offset));
        program.push(Insn::call(helper::PROBE_READ_KERNEL));
    }

    // A run observes one process, one thread or one cgroup, so a wakeup is
    // kept when either end of it belongs to the target: the wakeups it
    // receives are its blocked time ending, the ones it sends are the blocked
    // time it ends for others. An interrupt's wakeup has a waker of 0 by now
    // and is kept only as received.
    if whose != Whose::Any {
        push_followed_filter(
            &mut program,
            &fields,
            &followed,
            Subject::with_ids(7, WAKEE_TID, WAKEE_PID),
            tgid_offset,
            FILTER_SCRATCH,
            "not_the_wakee",
            "wakee_kept",
        );
        program.jump_to("emit", Insn::ja());
        program.label("not_the_wakee");
        match whose {
            // The waker's ids are on the stack already, and zero for an
            // interrupt, which no target's ids are.
            Whose::Process(_) | Whose::Thread(_) => push_followed_filter(
                &mut program,
                &fields,
                &followed,
                Subject::with_ids(7, WAKER_TID, WAKER_PID),
                tgid_offset,
                FILTER_SCRATCH,
                "done",
                "waker_kept",
            ),
            // The waker is the current task, whose cgroup one helper answers
            // for -- except in an interrupt, where the current task is only
            // the one the interrupt landed on.
            _ => {
                program.push(Insn::ldx(BPF_W, 1, R10, WAKER_TID));
                program.jump_to("done", Insn::jeq_imm(1, 0, 0));
                push_current_filter(&mut program, whose, FILTER_SCRATCH, "done");
            }
        }
    }

    program.label("emit");
    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EVENT as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));

    // A full ring returns an error and the wakeup is gone. Nothing else would
    // ever record that, so the program counts it: a correlation missing a third
    // of its evidence must not look like a correlation that found nothing.
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    program.push(Insn::st_imm(BPF_W, R10, DROP_KEY, 0));
    program.extend(Insn::load_map_fd(1, drops_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, DROP_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, 0));
    program.push(Insn::add64_imm(1, 1));
    program.push(Insn::stx(BPF_DW, 0, 0, 1));

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    // Checked before the kernel sees it: a structural mistake here comes back
    // from the verifier as a register dump, and this is a program nobody
    // compiled, so nothing else would have caught it.
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The prototypes the interrupt programs read their one argument out of.
const IRQ_ENTRY_SIGNATURE: [&str; 2] = ["int", "irqaction *"];
const IRQ_EXIT_SIGNATURE: [&str; 3] = ["int", "irqaction *", "int"];
const SOFTIRQ_SIGNATURE: [&str; 1] = ["unsigned int"];

/// Which interrupt a cpu is in, for the wakeup program to read.
///
/// Four small programs on `irq_handler_entry`/`irq_handler_exit` and
/// `softirq_entry`/`softirq_exit` keep, per cpu, the hard interrupt whose
/// handler is running and the softirq vector being served, each plus one so
/// that zero is "none". A hard handler does not nest on its cpu and a softirq
/// does not either, so one slot each is the whole state. What they cannot
/// name is a hard interrupt with no registered handler -- the local timer,
/// which ends most sleeps, and inter-processor interrupts -- and the wakeup a
/// threaded handler makes of its own thread, which the kernel issues after
/// `irq_handler_exit`: those wakeups carry no `irq`, and the caveat says so.
struct InterruptNumbers {
    map: Counters,
    programs: Vec<OwnedFd>,
    _links: Vec<OwnedFd>,
}

/// A program that sets one slot of the interrupt map from its first argument,
/// plus one, or clears it.
fn interrupt_instructions(slot: i32, map_fd: i32, set: bool) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    if set {
        // A raw tracepoint's arguments are eight bytes each; the number is in
        // the low half, and the store below takes only that half.
        program.push(Insn::ldx(BPF_DW, 7, 6, 0));
        program.push(Insn::add64_imm(7, 1));
    }
    program.push(Insn::st_imm(BPF_W, R10, -8, slot));
    program.extend(Insn::load_map_fd(1, map_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, -8));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    if set {
        program.push(Insn::stx(BPF_W, 0, 0, 7));
    } else {
        program.push(Insn::st_imm(BPF_W, 0, 0, 0));
    }
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

impl InterruptNumbers {
    fn load(btf: &Btf) -> Result<Self, String> {
        for (tracepoint, expected) in [
            ("irq_handler_entry", &IRQ_ENTRY_SIGNATURE[..]),
            ("irq_handler_exit", &IRQ_EXIT_SIGNATURE[..]),
            ("softirq_entry", &SOFTIRQ_SIGNATURE[..]),
            ("softirq_exit", &SOFTIRQ_SIGNATURE[..]),
        ] {
            let signature = btf.tracepoint_signature(tracepoint)?;
            if signature != expected {
                return Err(format!(
                    "this kernel declares `{tracepoint}({})` where `({})` was expected",
                    signature.join(", "),
                    expected.join(", "),
                ));
            }
        }
        let map = Counters::create(2, "omn_wake_irq")
            .map_err(|error| map_failure("interrupt map", error))?;
        let mut programs = Vec::new();
        let mut links = Vec::new();
        // The exits before the entries: an exit with no entry before it clears
        // a slot that was clear, where an entry with no exit attached would
        // leave a number behind that the next interrupt's wakeups would read.
        for (tracepoint, slot, set, name) in [
            ("irq_handler_exit", HARD_SLOT, false, "omn_wake_irqout"),
            ("softirq_exit", SOFT_SLOT, false, "omn_wake_sftout"),
            ("irq_handler_entry", HARD_SLOT, true, "omn_wake_irqin"),
            ("softirq_entry", SOFT_SLOT, true, "omn_wake_sftin"),
        ] {
            let instructions = interrupt_instructions(slot, map.as_raw_fd(), set)?;
            let program = syscall::prog_load(
                syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
                &instructions,
                name,
                LICENSE,
            )
            .map_err(|error| format!("the kernel refused the `{tracepoint}` program: {error}"))?;
            let link = syscall::raw_tracepoint_open(tracepoint, program.as_raw_fd())
                .map_err(|error| format!("cannot attach to `{tracepoint}`: {error}"))?;
            programs.push(program);
            links.push(link);
        }
        Ok(Self {
            map,
            programs,
            _links: links,
        })
    }
}

/// A loaded, attached wakeup program and the two maps it writes.
///
/// Dropping it detaches: the link, the program and the maps are file
/// descriptors, and the kernel unloads the program when the last one closes.
pub struct WakeupProgram {
    /// The processes a process target started, followed in the kernel.
    descendants: Option<Descendants>,
    /// Whether the program can tell an interrupt's wakeup from a task's.
    interrupts_told_apart: bool,
    /// The programs that say which interrupt, or why there are none.
    interrupts: Result<InterruptNumbers, String>,
    ring: RingBuffer,
    drops: Counters,
    program: OwnedFd,
    _link: OwnedFd,
}

/// The license the program declares.
///
/// `bpf_probe_read_kernel` is one of the helpers the kernel marks GPL-only, and
/// reading a field out of a `task_struct` is not possible without it. The
/// declaration covers the instruction stream in this file, which is what the
/// kernel is asking about; `omn` itself stays under its own license.
const LICENSE: &str = "Dual MIT/GPL";

impl WakeupProgram {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.program])
            + self.descendants.as_ref().map_or(0, Descendants::skipped)
            + self.interrupts.as_ref().map_or(0, |numbers| {
                crate::bpf::attach::fentry::skipped(numbers.programs.iter())
            })
    }

    /// Processes the target started that no program here followed, because
    /// the map of them was full.
    pub fn unfollowed(&self) -> u64 {
        self.descendants.as_ref().map_or(0, Descendants::missed)
    }

    /// Loads and attaches the program, or says why the kernel refused.
    pub fn load(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let bytes = ring_bytes(requested_ring_bytes);

        let ring_map = syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_wakeups")
            .map_err(|error| map_failure("wakeup ring buffer", error))?;
        let drops = Counters::create(1, "omn_wake_drop")
            .map_err(|error| map_failure("drop counter", error))?;

        let preempt = PreemptCount::locate(btf);
        let descendants = match whose {
            Whose::Process(root) => Some(Descendants::follow(root)?),
            _ => None,
        };
        let followed = descendants
            .as_ref()
            .map_or(Followed::only(whose), |descendants| {
                descendants.followed(whose)
            });
        // Only where an interrupt's wakeup is told apart at all: elsewhere
        // there is no interrupt context to name.
        let interrupts = match preempt {
            Some(_) => InterruptNumbers::load(btf),
            None => Err("this kernel's preempt count was not found".to_owned()),
        };
        let instructions = instructions(
            btf,
            preempt,
            followed,
            ring_map.as_raw_fd(),
            drops.as_raw_fd(),
            interrupts
                .as_ref()
                .ok()
                .map(|numbers| numbers.map.as_raw_fd()),
        )?;
        let program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &instructions,
            "omn_waking",
            LICENSE,
        )
        .map_err(|error| {
            if error.is_permission() {
                super::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the wakeup program: {error}")
            }
        })?;

        let link = syscall::raw_tracepoint_open("sched_waking", program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sched_waking` tracepoint: {error}"))?;

        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the wakeup ring buffer: {error}"))?;

        Ok(Self {
            descendants,
            interrupts_told_apart: preempt.is_some(),
            interrupts,
            ring,
            drops,
            program,
            _link: link,
        })
    }

    /// Whether a wakeup issued from an interrupt is recorded as the
    /// interrupt's. `false` on a kernel whose preempt count this program cannot
    /// find, where such a wakeup is credited to the interrupted task as before.
    pub fn interrupts_told_apart(&self) -> bool {
        self.interrupts_told_apart
    }

    /// Why an interrupt's wakeup carries no `irq` or `softirq`, when the
    /// programs that read them could not be loaded.
    pub fn interrupts_unnamed(&self) -> Option<&str> {
        self.interrupts.as_ref().err().map(String::as_str)
    }

    /// Hands every complete wakeup to `handle`, and counts the records that
    /// were not the size this reader expects rather than reinterpreting them.
    pub fn drain(&mut self, mut handle: impl FnMut(Wakeup), undecoded: &mut u64) {
        self.ring.drain(|bytes| match Wakeup::from_bytes(bytes) {
            Some(wakeup) => handle(wakeup),
            None => *undecoded += 1,
        });
    }

    /// Wakeups the program could not publish because the ring was full.
    ///
    /// Summed across cpus, because the counter is per-cpu to keep the fast path
    /// free of atomics.
    pub fn dropped(&self) -> u64 {
        self.drops.read(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program writes fields at stack offsets and `from_bytes` reads them
    /// at struct offsets. Nothing but this test connects the two, and a
    /// mismatch would be a field quietly read from the wrong place.
    #[test]
    fn the_record_struct_matches_what_the_program_writes() {
        use std::mem::offset_of;
        assert_eq!(std::mem::size_of::<Wakeup>(), RECORD_BYTES);
        for (stack, field, name) in [
            (WAKER_TID, offset_of!(Wakeup, waker_tid), "waker_tid"),
            (WAKER_PID, offset_of!(Wakeup, waker_pid), "waker_pid"),
            (WAKEE_TID, offset_of!(Wakeup, wakee_tid), "wakee_tid"),
            (WAKEE_PID, offset_of!(Wakeup, wakee_pid), "wakee_pid"),
            (CPU, offset_of!(Wakeup, cpu), "cpu"),
            (CONTEXT, offset_of!(Wakeup, context), "context"),
            (COMM, offset_of!(Wakeup, comm), "comm"),
            (
                CURRENT_COMM,
                offset_of!(Wakeup, current_comm),
                "current_comm",
            ),
            (
                SERVED_IN,
                offset_of!(Wakeup, served_in_tid),
                "served_in_tid",
            ),
            (IRQ, offset_of!(Wakeup, irq), "irq"),
            (SOFTIRQ, offset_of!(Wakeup, softirq), "softirq"),
            (PAD, offset_of!(Wakeup, pad), "pad"),
        ] {
            assert_eq!(
                (stack - EVENT) as usize,
                field,
                "the program writes `{name}` where the struct does not read it"
            );
        }
    }

    #[test]
    fn a_record_shorter_than_the_struct_is_refused_rather_than_padded() {
        assert!(Wakeup::from_bytes(&[0u8; 16]).is_none());
    }

    #[test]
    fn a_name_stops_at_its_terminator() {
        let mut bytes = [0u8; RECORD_BYTES];
        bytes[32..36].copy_from_slice(b"true");
        bytes[48..50].copy_from_slice(b"sh");
        let wakeup = Wakeup::from_bytes(&bytes).expect("a full record");
        assert_eq!(wakeup.wakee_comm(), "true");
        assert_eq!(wakeup.current_comm(), "sh", "the two ends are two names");
    }

    /// The interrupt's number is read only in the context it belongs to, and
    /// zero is "none" rather than interrupt 0 (the legacy timer, on x86).
    #[test]
    fn an_interrupt_is_named_only_in_its_own_context() {
        let record = |context: u32, irq: u32, softirq: u32| {
            let mut bytes = [0u8; RECORD_BYTES];
            bytes[28..32].copy_from_slice(&context.to_ne_bytes());
            bytes[64..68].copy_from_slice(&77u32.to_ne_bytes());
            bytes[68..72].copy_from_slice(&irq.to_ne_bytes());
            bytes[72..76].copy_from_slice(&softirq.to_ne_bytes());
            Wakeup::from_bytes(&bytes).expect("a full record")
        };
        let hard = record(0x1_0000, 1, 4);
        assert_eq!(
            hard.irq(),
            Some(0),
            "stored plus one: interrupt 0 is a number"
        );
        assert_eq!(
            hard.softirq(),
            None,
            "a hard interrupt over a softirq is the hard one's"
        );
        assert_eq!(hard.served_in_tid, 77);
        assert_eq!(
            record(0x1_0000, 0, 0).irq(),
            None,
            "no registered handler ran"
        );
        let soft = record(0x100, 9, 4);
        assert_eq!(soft.irq(), None);
        assert_eq!(soft.softirq().as_deref(), Some("NET_RX"));
        assert_eq!(record(0x100, 0, 42).softirq().as_deref(), Some("41"));
        let task = record(0, 9, 4);
        assert_eq!((task.irq(), task.softirq()), (None, None));
    }

    /// The interrupt programs read one argument and touch one slot; nothing
    /// else, on a path that runs on every interrupt of the machine.
    #[test]
    fn the_interrupt_programs_set_and_clear_one_slot() {
        let set = interrupt_instructions(SOFT_SLOT, 3, true).expect("a program");
        let clear = interrupt_instructions(HARD_SLOT, 3, false).expect("a program");
        for program in [&set, &clear] {
            assert_eq!(helpers_called(program), vec![helper::MAP_LOOKUP_ELEM]);
        }
        assert!(set
            .iter()
            .any(|insn| insn.imm == SOFT_SLOT && insn.code == 0x62));
        assert!(clear
            .iter()
            .any(|insn| insn.imm == HARD_SLOT && insn.code == 0x62));
    }

    /// Every helper the program calls, in the order it calls them.
    fn helpers_called(instructions: &[Insn]) -> Vec<i32> {
        instructions
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect()
    }

    /// Invariant 6, checked in the instruction stream rather than trusted.
    ///
    /// `bpf_ktime_get_boot_ns` includes suspend time. A program that called it
    /// would produce timestamps in a different domain from every other
    /// collector, every join across them would be wrong, and nothing anywhere
    /// would report an error -- which is exactly why this is asserted against
    /// the built program and not left to a reviewer noticing the helper number.
    #[test]
    fn the_program_reads_the_monotonic_clock_and_no_other() {
        let Ok(btf) = Btf::from_kernel() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let program = instructions(
            &btf,
            PreemptCount::locate(&btf),
            Followed::only(Whose::Process(1)),
            3,
            4,
            Some(5),
        )
        .expect("a program");
        let called = helpers_called(&program);
        assert!(
            called.contains(&helper::KTIME_GET_NS),
            "the program must timestamp its records"
        );
        const KTIME_GET_BOOT_NS: i32 = 125;
        assert!(
            !called.contains(&KTIME_GET_BOOT_NS),
            "boot time includes suspend, and mixing clock domains breaks every join silently"
        );
    }

    /// The program calls these and nothing else. A helper appearing here that
    /// nobody intended is either dead weight on a path that runs tens of
    /// thousands of times a second, or a fact being read that the record does
    /// not carry.
    #[test]
    fn the_program_calls_only_the_helpers_it_needs() {
        let Ok(btf) = Btf::from_kernel() else { return };
        let program = instructions(
            &btf,
            PreemptCount::locate(&btf),
            Followed::only(Whose::Process(1)),
            3,
            4,
            Some(5),
        )
        .expect("a program");
        let called = helpers_called(&program);
        let expected = [
            helper::KTIME_GET_NS,
            helper::GET_CURRENT_PID_TGID,
            helper::GET_SMP_PROCESSOR_ID,
            helper::PROBE_READ_KERNEL,
            helper::RINGBUF_OUTPUT,
            helper::MAP_LOOKUP_ELEM,
            helper::THIS_CPU_PTR,
            helper::GET_CURRENT_COMM,
        ];
        for helper in &called {
            assert!(
                expected.contains(helper),
                "the program calls helper {helper}, which it was never meant to"
            );
        }
        assert_eq!(
            called
                .iter()
                .filter(|id| **id == helper::PROBE_READ_KERNEL)
                .count(),
            3,
            "one read per field taken out of the task: tid, pid and comm"
        );
    }

    /// The machine-wide program keeps every wakeup on the machine, so it must
    /// carry no comparison against a process -- and the filtered one must carry
    /// two, because a wakeup counts at either end.
    #[test]
    fn the_filter_is_present_only_when_a_process_was_named() {
        let Ok(btf) = Btf::from_kernel() else { return };
        let machine_wide = instructions(
            &btf,
            PreemptCount::locate(&btf),
            Followed::only(Whose::Any),
            3,
            4,
            Some(5),
        )
        .expect("a program");
        assert!(
            !machine_wide.iter().any(|insn| insn.imm == 4242),
            "a machine-wide program compares against no process"
        );
        let filtered = instructions(
            &btf,
            PreemptCount::locate(&btf),
            Followed::only(Whose::Process(4242)),
            3,
            4,
            Some(5),
        )
        .expect("a program");
        assert_eq!(
            filtered.iter().filter(|insn| insn.imm == 4242).count(),
            2,
            "a wakeup is kept when either end belongs to the target"
        );
    }

    /// The size handed to `bpf_ringbuf_output` is the size of the record the
    /// rest of the program wrote. A smaller one publishes a truncated record
    /// and a larger one publishes whatever is on the stack behind it.
    #[test]
    fn the_published_record_is_exactly_the_size_of_the_struct() {
        let Ok(btf) = Btf::from_kernel() else { return };
        let program = instructions(
            &btf,
            PreemptCount::locate(&btf),
            Followed::only(Whose::Any),
            3,
            4,
            Some(5),
        )
        .expect("a program");
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
        assert_eq!(length.imm as usize, std::mem::size_of::<Wakeup>());
    }

    /// The program is built without any privilege, so this checks the shape
    /// even where the kernel would refuse to load it.
    #[test]
    fn the_program_is_built_against_the_running_kernels_offsets() {
        let Ok(btf) = Btf::from_kernel() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let filtered = instructions(
            &btf,
            PreemptCount::locate(&btf),
            Followed::only(Whose::Process(4242)),
            3,
            4,
            Some(5),
        )
        .expect("a program");
        let machine_wide = instructions(
            &btf,
            PreemptCount::locate(&btf),
            Followed::only(Whose::Any),
            3,
            4,
            Some(5),
        )
        .expect("a program");
        assert!(
            filtered.len() > machine_wide.len(),
            "the filtered program carries the comparisons the machine-wide one does not"
        );
        assert_eq!(
            filtered.last().copied(),
            Some(Insn::exit()),
            "a program must end in exit"
        );
        assert!(
            filtered.iter().any(|insn| insn.imm == 4242),
            "the target pid is written into the instruction stream"
        );
    }

    /// The context word is the preempt count's interrupt bits, and only the
    /// served-softirq bit of the softirq byte counts: a task holding
    /// `local_bh_disable` (bits 9 and up of that byte) is in its own context.
    #[test]
    fn the_context_is_read_off_the_interrupt_bits_alone() {
        let with = |context: u32| {
            let mut bytes = [0u8; RECORD_BYTES];
            bytes[28..32].copy_from_slice(&context.to_ne_bytes());
            Wakeup::from_bytes(&bytes).expect("a full record").context()
        };
        assert_eq!(with(0), WakerContext::Task);
        assert_eq!(with(0x200), WakerContext::Task, "bottom halves disabled");
        assert_eq!(with(0x100), WakerContext::Softirq);
        assert_eq!(
            with(0x1_0100),
            WakerContext::Hardirq,
            "a hard irq over a softirq"
        );
        assert_eq!(with(0x10_0000), WakerContext::Nmi);
        // The program masks before it stores, so these never reach a record;
        // the mask itself is what keeps them out.
        assert_eq!(
            0x8000_0001 & INTERRUPT_BITS,
            0,
            "need-resched and preempt depth"
        );
    }

    /// `Btf::var_id` finds a variable the kernel declares, and nothing for a
    /// name that is a type but not a variable, or no name at all -- and gives
    /// the same id as a walk of the blob written apart from `btf.rs`.
    #[test]
    fn a_variables_id_is_found_and_a_types_is_not() {
        let (Ok(blob), Ok(btf)) = (std::fs::read("/sys/kernel/btf/vmlinux"), Btf::from_kernel())
        else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        assert!(btf.var_id("task_struct").is_none(), "a struct is not a VAR");
        assert!(btf.var_id("no_such_variable_anywhere").is_none());
        // `runqueues` is a per-cpu VAR on every kernel with BTF.
        let id = btf
            .var_id("runqueues")
            .expect("the scheduler's per-cpu run queues");
        assert_eq!(
            Some(id),
            var_id(&blob, "runqueues"),
            "the two readings disagree"
        );
        for name in ["__preempt_count", "pcpu_hot"] {
            assert_eq!(btf.var_id(name), var_id(&blob, name), "{name}");
        }
    }

    /// The type id of the `VAR` named `name` in a BTF blob.
    ///
    /// Kept in the tests as a second reading of the blob, written apart from
    /// `btf.rs`, to check `Btf::var_id` against: same layout, same per-kind sizes,
    /// native endian.
    fn var_id(blob: &[u8], name: &str) -> Option<u32> {
        const KIND_VAR: u32 = 14;
        let word = |at: usize| -> Option<u32> {
            Some(u32::from_ne_bytes(blob.get(at..at + 4)?.try_into().ok()?))
        };
        if blob.len() < 24 || u16::from_ne_bytes([blob[0], blob[1]]) != 0xeb9f {
            return None;
        }
        let header = word(4)? as usize;
        let types = header + word(8)? as usize;
        let end = types + word(12)? as usize;
        let strings = header + word(16)? as usize;
        let named = |offset: usize| -> bool {
            let start = strings + offset;
            let Some(tail) = blob.get(start..) else {
                return false;
            };
            tail.len() > name.len()
                && &tail[..name.len()] == name.as_bytes()
                && tail[name.len()] == 0
        };
        let (mut at, mut id) = (types, 1u32);
        while at + 12 <= end {
            let info = word(at + 4)?;
            let (kind, vlen) = ((info >> 24) & 0x1f, (info & 0xffff) as usize);
            if kind == KIND_VAR && named(word(at)? as usize) {
                return Some(id);
            }
            let extra = match kind {
                1 | 14 | 17 => 4,     // INT, VAR, DECL_TAG
                3 => 12,              // ARRAY
                4 | 5 => vlen * 12,   // STRUCT, UNION
                6 | 13 => vlen * 8,   // ENUM, FUNC_PROTO
                15 | 19 => vlen * 12, // DATASEC, ENUM64
                _ => 0,
            };
            at += 12 + extra;
            id += 1;
        }
        None
    }

    /// On x86 the program reads the preempt count and, in an interrupt,
    /// replaces the waker before any comparison against the target -- so an
    /// interrupt's wakeup can only be kept as its wakee's.
    #[test]
    fn an_interrupts_waker_is_replaced_before_the_target_is_compared() {
        let Ok(btf) = Btf::from_kernel() else { return };
        let Some(preempt) = PreemptCount::locate(&btf) else {
            if cfg!(target_arch = "x86_64") {
                panic!("an x86 kernel with BTF declares `__preempt_count` or `pcpu_hot`");
            }
            eprintln!("skipped: this kernel's preempt count is not a per-cpu variable");
            return;
        };
        let program = instructions(
            &btf,
            Some(preempt),
            Followed::only(Whose::Process(4242)),
            3,
            4,
            Some(5),
        )
        .expect("a program");
        let load = program
            .iter()
            .position(|insn| {
                insn.code == 0x18 && insn.regs >> 4 == crate::bpf::insn::BPF_PSEUDO_BTF_ID
            })
            .expect("the preempt count is loaded by its BTF id");
        assert_eq!(program[load].imm as u32, preempt.btf_id);
        let zeroed = program
            .iter()
            .position(|insn| insn.code == 0x62 && insn.off == WAKER_PID && insn.imm == 0)
            .expect("an interrupt's waker pid is written as 0");
        let compared = program
            .iter()
            .position(|insn| insn.imm == 4242)
            .expect("the target is compared");
        assert!(
            load < zeroed && zeroed < compared,
            "{load} {zeroed} {compared}"
        );
    }
}
