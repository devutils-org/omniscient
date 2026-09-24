//! The `sched_switch` program: how long a task was not running, and why.
//!
//! This is the layer's first aggregating program, and it exists in this shape
//! because the streaming one is not viable. A context switch on a busy machine
//! happens a few million times a minute; one ring record per switch is a
//! measurement whose cost scales with the workload, which means the numbers
//! describe a machine running `omn` rather than the machine anyone wanted to
//! look at. Here the kernel keeps the arithmetic: two maps, one holding the
//! moment each task left the cpu and one accumulating nanoseconds per task,
//! and userspace reads a few hundred entries instead of a few million events.
//!
//! **Off cpu is blocked, not preempted.** `sched_switch` fires for both, and
//! they are different measurements: a task the scheduler took the cpu away
//! from is still runnable and its wait is run queue latency, which is
//! `sched.runqlat`. A task that left because it was waiting for something is
//! blocked, and the time until it comes back is what this source reports. The
//! program tells them apart by `prev_state`, which is zero exactly when the
//! task was still runnable, and drops the runnable case in the kernel rather
//! than carrying it to userspace to be filtered out there.
//!
//! The state is kept, not reduced to a boolean, because the distinction it
//! draws is the useful one: `TASK_INTERRUPTIBLE` is a program waiting for
//! something to happen, `TASK_UNINTERRUPTIBLE` is usually a program waiting on
//! a disk, and telling someone their service spent four seconds in the second
//! is a different conversation from the first.
//!
//! Two honest edges, neither of which the numbers can show on their own:
//!
//! - A task that leaves the cpu to die has `TASK_DEAD` and never comes back.
//!   It is dropped in the kernel; otherwise every short-lived child of an
//!   observed program would sit in the in-flight map until the run ended and
//!   be reported as still blocked.
//! - `prev_state` is read before `__schedule` decides what to do with it, so a
//!   task that was about to sleep and had a signal pending is put back to
//!   runnable after the value was taken. Those appear here as very short
//!   blocks that were really run queue waits. They are rare and they are small,
//!   and inventing a correction for them would be worse than saying so.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{count_drop, helper, read_field, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash, StackTraces};
use super::syscall;

/// Arguments of `sched_switch`, in the raw tracepoint's context: one 64-bit
/// slot each, in the order the kernel declares them.
const ARG_PREV: i16 = 8;
const ARG_NEXT: i16 = 16;
const ARG_PREV_STATE: i16 = 24;

/// The prototype this program is written against, checked against the running
/// kernel before a single instruction is built. `sched_switch` gained its
/// fourth argument in 5.18; a program written for the older shape reads
/// `prev_state` out of a slot that holds nothing and a `task_struct *` out of
/// one that holds an integer, and what comes back is a plausible number.
const SIGNATURE: [&str; 4] = ["bool", "task_struct *", "task_struct *", "unsigned int"];

/// `sched_wakeup(struct task_struct *p)`, which the second program reads.
const WAKEUP_SIGNATURE: [&str; 1] = ["task_struct *"];

/// `BPF_F_USER_STACK`: walk the task's own frames rather than the kernel's.
///
/// The kernel half of an off-cpu stack is always the same few frames of
/// `__schedule`, and says nothing; what a reader wants is the line of their own
/// program that waited. The kernel frames are where the *reason* is -- a futex,
/// a page fault, a read -- but that is what the task state already says, in one
/// word instead of eight addresses.
const BPF_F_USER_STACK: i32 = 1 << 8;

/// `TASK_RUNNING`: the task left the cpu and is still runnable, which is
/// preemption and belongs to `sched.runqlat`.
const TASK_RUNNING: i32 = 0;
/// `TASK_DEAD`: the task left the cpu for the last time.
const TASK_DEAD: i32 = 0x80;

/// How many tasks may be off cpu at once before the program stops being able
/// to record new ones. A machine has a few thousand tasks in total.
const MAX_IN_FLIGHT: u32 = 16384;
/// How many distinct stacks may be interned before the kernel starts evicting.
/// A program blocks in a handful of places; the room is for the whole machine.
const MAX_STACKS: u32 = 4096;
/// How deep the kernel walks before it gives up. Deeper than the interpreter
/// loops that make up most of a real user stack, and bounded because the value
/// size of the map is this times eight and it is allocated per entry.
const MAX_STACK_DEPTH: u32 = 64;

/// How many distinct `(task, state)` pairs one drain interval may hold. This
/// is the number the aggregation buys: the entries are pairs that blocked, not
/// switches that happened.
const MAX_TOTALS: u32 = 16384;

/// Slots of the drop counter, one per thing that can be lost.
const DROP_IN_FLIGHT: u32 = 0;
const DROP_TOTALS: u32 = 1;

/// Where each map's key and value live on the program's stack.
///
/// Laid out once here so the sizes below, the `#[repr(C)]` structs userspace
/// reads and the map descriptors all come from the same place. Every byte of
/// each is written before it is handed to a helper: an uninitialised byte in a
/// key makes two identical blocks land in two different buckets.
const TOTAL_VALUE: i16 = -96;
const TOTAL_KEY: i16 = -56;
const IN_FLIGHT_VALUE: i16 = -40;
const IN_FLIGHT_KEY: i16 = -8;
const DROP_KEY: i16 = -104;

/// The layout of a totals key, as byte offsets within it.
///
/// One set of numbers for the program, which writes the key onto its stack at
/// `TOTAL_KEY + <offset>`, and for `decode`, which reads it back out of the
/// bytes the kernel returns. Two sets would be two chances to disagree, and a
/// disagreement here is a field read from the wrong place -- a thread id that
/// is really half a process id, which is a number.
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
const KEY_STATE: i16 = 8;
/// The stack that blocked, as an id into the stack-trace map.
///
/// Part of the key rather than of the value, which is what makes the question
/// "where does this thread block" answerable: two blocks of the same thread in
/// the same state from different call sites are different entries, and summing
/// them would give back the number this source already had.
///
/// It also occupies the four bytes that used to be padding written as zero,
/// so the key is the same size it was. The padding mattered for the same
/// reason this does: the kernel hashes the whole key, and a byte the program
/// never wrote puts two identical blocks in two buckets.
const KEY_STACK: i16 = 12;
const TOTAL_KEY_BYTES: usize = 16;

/// The layout of a totals value, the same way.
const VALUE_TOTAL_NS: i16 = 0;
const VALUE_BLOCKS: i16 = 8;
const VALUE_COMM: i16 = 16;
const COMM_BYTES: usize = 16;
/// The part of `total_ns` the task spent **runnable**, waiting for a cpu after
/// whatever it was blocked on released it.
///
/// A block is timed from the switch that took the task off the cpu to the
/// switch that gave it one back, so the run queue wait after the wakeup is
/// inside it -- and inside `sched.runqlat` as well, which measures exactly that
/// wait from exactly the same tracepoint. Measured by the seventh wave's
/// compute crossing: 100 sleeps of 5 ms behind a spinning shell reported 582 ms
/// off cpu of which 77 ms was queue wait, and a decomposition adding the two
/// sources counted those 77 ms twice with nothing saying so.
const VALUE_QUEUED_NS: i16 = 32;
const TOTAL_VALUE_BYTES: usize = 40;

/// The in-flight map: thread id to when it left the cpu and in what state.
///
/// The state is carried across the block rather than read again, because by
/// the time the task is back on the cpu its state is `TASK_RUNNING` and what
/// it was waiting for is no longer written anywhere.
const IN_FLIGHT_TS: i16 = 0;
const IN_FLIGHT_STATE: i16 = 8;
/// The stack is captured when the task *leaves* the cpu, because that is the
/// stack that blocked. By the time it comes back it is inside the scheduler
/// and its own call site is gone, so the id is carried across the block the
/// same way the state is.
const IN_FLIGHT_STACK: i16 = 16;
/// When the task was made runnable again, written by the `sched_wakeup`
/// program into an entry the `sched_switch` one created.
///
/// Zero means this run did not see the wakeup: the task was woken before the
/// programs attached, or by a path that does not raise `sched_wakeup`. Zero is
/// carried as zero rather than guessed, and the collector says how many blocks
/// it applies to.
///
/// A task woken several times before it runs keeps the last stamp, which is
/// the one closest to the switch-in. `sched.runqlat` does the same with the
/// same tracepoint, so the two agree about what the wait was.
const IN_FLIGHT_WOKEN: i16 = 24;
const IN_FLIGHT_KEY_BYTES: usize = 4;
const IN_FLIGHT_VALUE_BYTES: usize = 32;

/// One `(task, state)` pair's blocked time over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffCpu {
    pub tid: u32,
    pub tgid: u32,
    pub state: u32,
    pub total_ns: u64,
    /// The part of `total_ns` that was runnable rather than blocked: the run
    /// queue wait between the wakeup and the switch back onto a cpu.
    ///
    /// `total_ns - queued_ns` is the time the task was actually waiting for
    /// something, which is what a decomposition wants; `queued_ns` is the part
    /// `sched.runqlat` also reports, which is what makes adding the two sources
    /// a double count unless one of them subtracts it.
    pub queued_ns: u64,
    /// How many separate blocks that time was spread over.
    pub blocks: u64,
    pub comm: String,
    /// The stack that blocked, as an id into the program's stack-trace map.
    ///
    /// `None` when the walk failed, which is the ordinary case for a thread
    /// blocked in a kernel worker with no user stack to walk, and for one whose
    /// frames the kernel could not follow. The kernel returns a negative errno
    /// there, and a failed walk is not stack zero.
    pub stack: Option<u32>,
}

impl OffCpu {
    /// The task state as `/proc` would letter it, for a reader rather than for
    /// arithmetic.
    ///
    /// The raw value is kept beside this one: the set is open, the kernel adds
    /// to it, and a state this does not recognise must not be rendered as one
    /// it does.
    pub fn state_name(state: u32) -> &'static str {
        const INTERRUPTIBLE: u32 = 0x0001;
        const UNINTERRUPTIBLE: u32 = 0x0002;
        const STOPPED: u32 = 0x0004;
        const TRACED: u32 = 0x0008;
        const NOLOAD: u32 = 0x0400;
        match state {
            0 => "runnable",
            // `TASK_IDLE` is uninterruptible sleep the kernel excludes from
            // the load average: an idle worker thread, not a program stuck on
            // a disk. Reporting it as uninterruptible is how a machine doing
            // nothing looks like a machine in trouble.
            state if state & (UNINTERRUPTIBLE | NOLOAD) == UNINTERRUPTIBLE | NOLOAD => "idle",
            state if state & UNINTERRUPTIBLE != 0 => "uninterruptible",
            state if state & INTERRUPTIBLE != 0 => "sleeping",
            state if state & STOPPED != 0 => "stopped",
            state if state & TRACED != 0 => "traced",
            _ => "other",
        }
    }

    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < TOTAL_KEY_BYTES || value.len() < TOTAL_VALUE_BYTES {
            return None;
        }
        let word =
            |bytes: &[u8], at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long =
            |bytes: &[u8], at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        let comm = &value[VALUE_COMM as usize..VALUE_COMM as usize + COMM_BYTES];
        let end = comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(comm.len());
        Some(Self {
            tid: word(key, KEY_TID as usize),
            tgid: word(key, KEY_TGID as usize),
            state: word(key, KEY_STATE as usize),
            total_ns: long(value, VALUE_TOTAL_NS as usize),
            queued_ns: long(value, VALUE_QUEUED_NS as usize),
            blocks: long(value, VALUE_BLOCKS as usize),
            comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
            stack: {
                // Signed on the wire: `bpf_get_stackid` answers with an id or
                // with a negative errno, and reading the latter as an unsigned
                // id would look up a stack four billion entries in.
                let raw = word(key, KEY_STACK as usize) as i32;
                (raw >= 0).then_some(raw as u32)
            },
        })
    }
}

/// Builds the program for a target, or for the whole machine.
fn instructions(
    btf: &Btf,
    target_tgid: Option<u32>,
    in_flight_fd: i32,
    totals_fd: i32,
    drops_fd: i32,
    stacks_fd: i32,
) -> Result<Vec<Insn>, String> {
    let signature = btf.tracepoint_signature("sched_switch")?;
    if signature != SIGNATURE {
        return Err(format!(
            "this kernel declares `sched_switch({})`, and this program is written for \
             `sched_switch({})`: it reads the task that left the cpu, the one that took it and \
             the state of the first out of fixed slots, and against another prototype those \
             slots hold something else",
            signature.join(", "),
            SIGNATURE.join(", "),
        ));
    }
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;

    let mut program = Program::new();
    // r6 keeps the context, r7 whichever task is being looked at and r8 the
    // one timestamp both halves of the program use. All three survive the
    // helper calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    // ---- the task leaving the cpu ----
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_PREV));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_PREV_STATE));
    // Still runnable: this is preemption, and the wait that follows is run
    // queue latency rather than blocked time.
    program.jump_to("left", Insn::jeq_imm(1, TASK_RUNNING, 0));
    // Leaving for the last time: it will never come back to be measured.
    program.jump_to("left", Insn::jset_imm(1, TASK_DEAD, 0));
    program.push(Insn::stx(BPF_DW, R10, IN_FLIGHT_VALUE + IN_FLIGHT_TS, 8));
    program.push(Insn::stx(BPF_DW, R10, IN_FLIGHT_VALUE + IN_FLIGHT_STATE, 1));
    // No wakeup has been seen for this block yet. Written rather than left
    // alone: every byte of a value handed to a helper has to be one this
    // program put there, and a stale stamp from the stack would be read as a
    // queue wait that did not happen.
    program.push(Insn::st_imm(
        BPF_DW,
        R10,
        IN_FLIGHT_VALUE + IN_FLIGHT_WOKEN,
        0,
    ));
    // The stack of the task that is leaving, taken here because here is the
    // only place it exists. `sched_switch` runs in the outgoing task's context
    // -- the scheduler has not switched the stack yet -- so the walk goes
    // through its own frames; by the time it is scheduled back in, what is
    // under it is the scheduler.
    //
    // The state was written to the stack first, because this call clobbers
    // r1 through r5 and the state lives in r1.
    program.push(Insn::mov64_reg(1, 6));
    program.extend(Insn::load_map_fd(2, stacks_fd));
    program.push(Insn::mov64_imm(3, BPF_F_USER_STACK));
    program.push(Insn::call(helper::GET_STACKID));
    // Stored whatever it is. A failure is a negative errno, kept as it came so
    // that userspace can tell "no stack" from "stack zero" -- a task with no
    // user stack to walk, which is every kernel worker, is the ordinary case
    // and not an error to hide.
    program.push(Insn::stx(BPF_DW, R10, IN_FLIGHT_VALUE + IN_FLIGHT_STACK, 0));
    // The whole slot is cleared first: the key is four bytes in an eight-byte
    // slot, and the four the program does not write are still hashed.
    program.push(Insn::st_imm(BPF_DW, R10, IN_FLIGHT_KEY, 0));
    read_field(&mut program, IN_FLIGHT_KEY, 4, 7, pid_offset);

    // A run observes one process. The filter is written into the instruction
    // stream rather than kept in a map: the thread group is known before the
    // program loads and never changes, so a lookup on a path that runs tens of
    // thousands of times a second would buy nothing.
    if let Some(tgid) = target_tgid {
        read_field(&mut program, TOTAL_KEY + KEY_TGID, 4, 7, tgid_offset);
        program.push(Insn::ldx(BPF_W, 1, R10, TOTAL_KEY + KEY_TGID));
        program.jump_to("left", Insn::jne_imm(1, tgid as i32, 0));
    }

    program.extend(Insn::load_map_fd(1, in_flight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, IN_FLIGHT_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, IN_FLIGHT_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("left", Insn::jsge_imm(0, 0, 0));
    // The map is full: this block will never be attributed, and nothing else
    // anywhere would record that it happened.
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_IN_FLIGHT, "left");
    program.label("left");

    // ---- the task coming back ----
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_NEXT));
    program.push(Insn::st_imm(BPF_DW, R10, IN_FLIGHT_KEY, 0));
    read_field(&mut program, IN_FLIGHT_KEY, 4, 7, pid_offset);
    program.extend(Insn::load_map_fd(1, in_flight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, IN_FLIGHT_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // Not a task this run saw block: it was already running when the program
    // was attached, or it belongs to another process.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, IN_FLIGHT_TS));
    program.push(Insn::ldx(BPF_DW, 2, 0, IN_FLIGHT_STATE));
    // r9 = now - when it left. Kept in a callee-saved register because the
    // calls between here and the accumulation would clobber anything else.
    program.push(Insn::mov64_reg(9, 8));
    program.push(Insn::sub64_reg(9, 1));
    program.push(Insn::stx(BPF_W, R10, TOTAL_KEY + KEY_STATE, 2));
    // Read out of the in-flight value while the lookup's pointer is still in
    // r0, and written to the stack straight away: the delete below clobbers
    // r1 through r5 and there is no callee-saved register left to hold it.
    program.push(Insn::ldx(BPF_W, 3, 0, IN_FLIGHT_STACK));
    program.push(Insn::stx(BPF_W, R10, TOTAL_KEY + KEY_STACK, 3));
    // The wakeup stamp, and the queue wait it makes measurable. The slot it
    // lands in is the same one the first-block template writes from, so both
    // paths below read one place.
    program.push(Insn::ldx(BPF_DW, 4, 0, IN_FLIGHT_WOKEN));
    program.jump_to("no_wakeup", Insn::jeq_imm(4, 0, 0));
    program.push(Insn::mov64_reg(5, 8));
    program.push(Insn::sub64_reg(5, 4));
    program.push(Insn::stx(BPF_DW, R10, TOTAL_VALUE + VALUE_QUEUED_NS, 5));
    program.jump_to("queued_known", Insn::ja());
    program.label("no_wakeup");
    // A block whose wakeup this run did not see contributes nothing to the
    // queue wait rather than an estimate of one. The collector counts these and
    // says so, because a zero that means "not seen" and a zero that means "no
    // wait" are different facts.
    program.push(Insn::st_imm(BPF_DW, R10, TOTAL_VALUE + VALUE_QUEUED_NS, 0));
    program.label("queued_known");

    program.extend(Insn::load_map_fd(1, in_flight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, IN_FLIGHT_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    // The thread id is already on the stack from the lookup just done.
    program.push(Insn::ldx(BPF_W, 1, R10, IN_FLIGHT_KEY));
    program.push(Insn::stx(BPF_W, R10, TOTAL_KEY + KEY_TID, 1));
    read_field(&mut program, TOTAL_KEY + KEY_TGID, 4, 7, tgid_offset);

    program.extend(Insn::load_map_fd(1, totals_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, TOTAL_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first_block", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu, so the increment has to be one the
    // other cpus cannot interleave with.
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_TOTAL_NS, 9));
    program.push(Insn::ldx(BPF_DW, 1, R10, TOTAL_VALUE + VALUE_QUEUED_NS));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_QUEUED_NS, 1));
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_BLOCKS, 1));
    program.jump_to("done", Insn::ja());

    program.label("first_block");
    program.push(Insn::stx(BPF_DW, R10, TOTAL_VALUE + VALUE_TOTAL_NS, 9));
    program.push(Insn::st_imm(BPF_DW, R10, TOTAL_VALUE + VALUE_BLOCKS, 1));
    // `VALUE_QUEUED_NS` is already in the template: the switch-in path wrote it
    // there, which is why it was written to that slot rather than to a scratch
    // one.
    // The name is read once, when the entry is created, rather than on every
    // block: it does not change, and this is the hot path.
    read_field(
        &mut program,
        TOTAL_VALUE + VALUE_COMM,
        COMM_BYTES as i32,
        7,
        comm_offset,
    );
    program.extend(Insn::load_map_fd(1, totals_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, TOTAL_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, TOTAL_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_TOTALS, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `sched_wakeup`: a blocked task has been made runnable.
///
/// It writes the moment into the in-flight entry the `sched_switch` program
/// created and does nothing else -- it never inserts, because a wakeup for a
/// task this run did not see block is not this source's business, and inserting
/// there would make the map grow with every wakeup on the machine.
///
/// `sched_wakeup` rather than `sched_waking` deliberately: `sched.runqlat`
/// measures the run queue wait from `sched_wakeup`, and a `queued_ns` measured
/// from the other one would be a different number that looked comparable.
fn wakeup_instructions(btf: &Btf, in_flight_fd: i32) -> Result<Vec<Insn>, String> {
    let signature = btf.tracepoint_signature("sched_wakeup")?;
    if signature != WAKEUP_SIGNATURE {
        return Err(format!(
            "this kernel declares `sched_wakeup({})`, and this program is written for \
             `sched_wakeup({})`: it reads the task being woken out of the first slot, and \
             against another prototype that slot holds something else",
            signature.join(", "),
            WAKEUP_SIGNATURE.join(", "),
        ));
    }
    let pid_offset = btf.member_offset("task_struct", "pid")?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    program.push(Insn::st_imm(BPF_DW, R10, IN_FLIGHT_KEY, 0));
    read_field(&mut program, IN_FLIGHT_KEY, 4, 7, pid_offset);
    program.extend(Insn::load_map_fd(1, in_flight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, IN_FLIGHT_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // Not blocked as far as this run knows: an ordinary wakeup of a task that
    // was never off cpu inside the window, or one belonging to another process.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::stx(BPF_DW, 0, IN_FLIGHT_WOKEN, 8));

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// A loaded, attached off-cpu program and the maps it fills.
///
/// Dropping it detaches: the link, the program and the maps are file
/// descriptors, and the kernel unloads the program when the last one closes.
pub struct OffCpuProgram {
    in_flight: Hash,
    totals: Hash,
    drops: Counters,
    stacks: StackTraces,
    program: OwnedFd,
    _link: OwnedFd,
    wakeup_program: OwnedFd,
    _wakeup_link: OwnedFd,
}

/// `bpf_probe_read_kernel` is GPL-only, and reading a field out of a
/// `task_struct` is not possible without it. The declaration covers the
/// instruction stream in this file, which is what the kernel is asking about.
const LICENSE: &str = "GPL";

/// What one drain found, beside the measurements.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainLosses {
    /// Entries the walk returned and the read no longer found.
    pub vanished: u64,
    /// The walk stopped at the map's capacity rather than at its end.
    pub truncated: bool,
}

impl OffCpuProgram {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.program, &self.wakeup_program])
    }

    pub fn load(target_tgid: Option<u32>) -> Result<Self, String> {
        let btf = super::kernel_btf()?;

        let in_flight = Hash::create(
            IN_FLIGHT_KEY_BYTES,
            IN_FLIGHT_VALUE_BYTES,
            MAX_IN_FLIGHT,
            "omn_offcpu_run",
        )
        .map_err(|error| map_failure("off-cpu in-flight map", error))?;
        let totals = Hash::create(
            TOTAL_KEY_BYTES,
            TOTAL_VALUE_BYTES,
            MAX_TOTALS,
            "omn_offcpu_sum",
        )
        .map_err(|error| map_failure("off-cpu total map", error))?;
        let drops = Counters::create(2, "omn_offcpu_drop")
            .map_err(|error| map_failure("off-cpu drop counter", error))?;
        let stacks = StackTraces::create(MAX_STACKS, MAX_STACK_DEPTH, "omn_offcpu_stk")
            .map_err(|error| map_failure("off-cpu stack map", error))?;

        let instructions = instructions(
            btf,
            target_tgid,
            in_flight.as_raw_fd(),
            totals.as_raw_fd(),
            drops.as_raw_fd(),
            stacks.as_raw_fd(),
        )?;
        let program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &instructions,
            "omn_offcpu",
            LICENSE,
        )
        .map_err(|error| {
            if error.is_permission() {
                super::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the off-cpu program: {error}")
            }
        })?;

        let link = syscall::raw_tracepoint_open("sched_switch", program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sched_switch` tracepoint: {error}"))?;

        let wakeup_instructions = wakeup_instructions(btf, in_flight.as_raw_fd())?;
        let wakeup_program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &wakeup_instructions,
            "omn_offcpu_wake",
            LICENSE,
        )
        .map_err(|error| {
            if error.is_permission() {
                super::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the off-cpu wakeup program: {error}")
            }
        })?;
        let wakeup_link = syscall::raw_tracepoint_open("sched_wakeup", wakeup_program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sched_wakeup` tracepoint: {error}"))?;

        Ok(Self {
            in_flight,
            totals,
            drops,
            stacks,
            program,
            _link: link,
            wakeup_program,
            _wakeup_link: wakeup_link,
        })
    }

    /// Takes everything the kernel has accumulated since the last drain.
    ///
    /// Each entry is a delta over the interval between two drains, not a
    /// running total: the entries are removed as they are read, so what comes
    /// back next time is what happened in between. A caller that wants the
    /// total adds them up, and one that wants a series does not have to
    /// subtract consecutive readings to get one.
    pub fn drain(&self) -> Result<(Vec<OffCpu>, DrainLosses), String> {
        let drained = self
            .totals
            .drain()
            .map_err(|error| format!("cannot read the off-cpu totals: {error}"))?;
        let mut losses = DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut entries = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match OffCpu::decode(key, value) {
                Some(entry) => entries.push(entry),
                // A record the wrong size means this reader and the program
                // disagree about the layout, which is a bug rather than a
                // machine condition -- but counting it beats reinterpreting it.
                None => losses.vanished += 1,
            }
        }
        Ok((entries, losses))
    }

    /// The addresses behind one stack id, leaf first.
    ///
    /// Read rather than taken: many blocks resolve to the same stack, and the
    /// map outlives one drain. `None` means the kernel no longer has it, which
    /// happens under eviction and leaves a measurement with an id and no
    /// explanation -- worth reporting as that rather than as an empty stack.
    pub fn stack(&self, id: u32) -> Option<Vec<u64>> {
        self.stacks.get(id).ok().flatten()
    }

    /// How deep the kernel was asked to walk, which is what a stack of exactly
    /// this many frames was probably truncated at.
    pub fn stack_depth(&self) -> usize {
        self.stacks.depth() as usize
    }

    /// How many tasks were off cpu and had not come back.
    ///
    /// Their blocked time is real and is not in any total, because it has no
    /// end yet. That is a fact about the window rather than a loss, and it is
    /// reported as one.
    pub fn still_blocked(&self) -> usize {
        self.in_flight.len().unwrap_or(0)
    }

    /// Blocks the program could not record because a map was full, as
    /// `(in flight, totals)`.
    pub fn dropped(&self) -> (u64, u64) {
        (
            self.drops.read(DROP_IN_FLIGHT),
            self.drops.read(DROP_TOTALS),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key or a value the program leaves a hole in is not a smaller record,
    /// it is a record with stack garbage in it: the kernel hashes every byte
    /// of a key, so one byte nobody wrote puts two identical blocks in two
    /// buckets and halves both totals.
    #[test]
    fn every_byte_of_a_key_and_a_value_is_accounted_for() {
        let key = [(KEY_TID, 4), (KEY_TGID, 4), (KEY_STATE, 4), (KEY_STACK, 4)];
        let value = [
            (VALUE_TOTAL_NS, 8),
            (VALUE_BLOCKS, 8),
            (VALUE_COMM, COMM_BYTES as i16),
            (VALUE_QUEUED_NS, 8),
        ];
        for (fields, bytes, what) in [
            (&key[..], TOTAL_KEY_BYTES, "the totals key"),
            (&value[..], TOTAL_VALUE_BYTES, "the totals value"),
        ] {
            let mut covered = vec![false; bytes];
            for (at, size) in fields {
                for byte in *at..*at + *size {
                    let byte = byte as usize;
                    assert!(byte < bytes, "{what} is written past its own end");
                    assert!(!covered[byte], "{what} writes byte {byte} twice");
                    covered[byte] = true;
                }
            }
            assert!(
                covered.iter().all(|byte| *byte),
                "{what} has a byte the program never writes"
            );
        }
    }

    /// The stack regions the program writes must not overlap. They are written
    /// by offset arithmetic, and two that overlap produce a key built partly
    /// out of a value -- which hashes to a bucket nobody looks in, so the
    /// numbers are simply short.
    #[test]
    fn the_stack_regions_do_not_overlap_and_stay_inside_the_frame() {
        let regions = [
            (DROP_KEY, 4, "drop key"),
            (TOTAL_VALUE, TOTAL_VALUE_BYTES as i16, "total value"),
            (TOTAL_KEY, TOTAL_KEY_BYTES as i16, "total key"),
            (
                IN_FLIGHT_VALUE,
                IN_FLIGHT_VALUE_BYTES as i16,
                "in-flight value",
            ),
            (IN_FLIGHT_KEY, 8, "in-flight key slot"),
        ];
        for (at, size, name) in regions {
            assert!(at >= -512 && at + size <= 0, "`{name}` is off the frame");
            for (other_at, other_size, other) in regions {
                if name == other {
                    continue;
                }
                assert!(
                    at + size <= other_at || other_at + other_size <= at,
                    "`{name}` overlaps `{other}`"
                );
            }
        }
    }

    fn kernel_btf() -> Option<Btf> {
        match Btf::from_kernel() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    fn helpers_called(instructions: &[Insn]) -> Vec<i32> {
        instructions
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect()
    }

    /// Invariant 6, checked in the instruction stream rather than trusted.
    /// `bpf_ktime_get_boot_ns` includes suspend time, and a duration computed
    /// from two readings of it would be right until the machine slept.
    #[test]
    fn the_program_reads_the_monotonic_clock_and_no_other() {
        let Some(btf) = kernel_btf() else { return };
        let called = helpers_called(&instructions(&btf, Some(1), 3, 4, 5, 6).expect("a program"));
        const KTIME_GET_BOOT_NS: i32 = 125;
        assert!(called.contains(&helper::KTIME_GET_NS));
        assert!(!called.contains(&KTIME_GET_BOOT_NS));
        assert_eq!(
            called
                .iter()
                .filter(|id| **id == helper::KTIME_GET_NS)
                .count(),
            1,
            "both halves of a switch must be stamped with one reading, or a block measured \
             across the two is short by whatever happened between them"
        );
    }

    /// A block's duration is the difference between two timestamps, and the
    /// program must subtract them rather than store the second: a total built
    /// from absolute timestamps is the uptime, and it looks like a number.
    #[test]
    fn the_duration_is_a_difference_and_not_a_timestamp() {
        let Some(btf) = kernel_btf() else { return };
        let program = instructions(&btf, None, 3, 4, 5, 6).expect("a program");
        assert!(
            program.iter().any(|insn| *insn == Insn::sub64_reg(9, 1)),
            "the program must subtract the moment the task left from the moment it came back"
        );
    }

    /// Preemption is not blocked time, and a task leaving the cpu to die is
    /// not blocked time either. Both are dropped in the kernel, which is the
    /// only place dropping them is free.
    #[test]
    fn a_runnable_or_dying_task_is_not_recorded_as_blocked() {
        let Some(btf) = kernel_btf() else { return };
        let program = instructions(&btf, None, 3, 4, 5, 6).expect("a program");
        assert!(
            program
                .iter()
                .any(|insn| insn.code == 0x15 && insn.imm == TASK_RUNNING),
            "a switch away from a runnable task must be skipped: that is run queue wait"
        );
        assert!(
            program
                .iter()
                .any(|insn| insn.code == 0x45 && insn.imm == TASK_DEAD),
            "a task leaving the cpu to die never comes back and must not be held in flight"
        );
    }

    /// The accumulation is shared by every cpu, so it has to be atomic. A
    /// plain load-add-store loses whichever increment lands between the two
    /// halves of another cpu's, and the loss is proportional to how busy the
    /// machine is -- which is to say, largest exactly when the number matters.
    #[test]
    fn the_totals_are_accumulated_atomically() {
        let Some(btf) = kernel_btf() else { return };
        let program = instructions(&btf, None, 3, 4, 5, 6).expect("a program");
        let atomics = program.iter().filter(|insn| insn.code == 0xdb).count();
        assert_eq!(
            atomics, 3,
            "the blocked nanoseconds, the queue wait inside them, and the block count"
        );
    }

    #[test]
    fn the_filter_is_present_only_when_a_process_was_named() {
        let Some(btf) = kernel_btf() else { return };
        let machine_wide = instructions(&btf, None, 3, 4, 5, 6).expect("a program");
        assert!(!machine_wide.iter().any(|insn| insn.imm == 4242));
        let filtered = instructions(&btf, Some(4242), 3, 4, 5, 6).expect("a program");
        assert_eq!(
            filtered.iter().filter(|insn| insn.imm == 4242).count(),
            1,
            "a task is filtered where it blocks; where it comes back it is already known"
        );
    }

    /// A kernel whose `sched_switch` is not the one this program reads must be
    /// refused by name. The alternative is reading a `task_struct *` out of a
    /// slot that holds an integer, which produces numbers.
    #[test]
    fn a_kernel_with_another_prototype_is_refused_rather_than_read_wrong() {
        let Some(btf) = kernel_btf() else { return };
        assert!(
            instructions(&btf, None, 3, 4, 5, 6).is_ok(),
            "this kernel's prototype is the one the program is written for"
        );
        assert_eq!(
            btf.tracepoint_signature("sched_switch")
                .expect("a prototype"),
            SIGNATURE,
            "and that is what the refusal compares against"
        );
    }

    #[test]
    fn a_state_the_kernel_excludes_from_the_load_average_is_not_reported_as_a_disk_wait() {
        assert_eq!(OffCpu::state_name(0x0402), "idle");
        assert_eq!(OffCpu::state_name(0x0002), "uninterruptible");
        assert_eq!(OffCpu::state_name(0x0001), "sleeping");
        assert_eq!(OffCpu::state_name(0x0102), "uninterruptible", "killable");
        assert_eq!(OffCpu::state_name(0), "runnable");
        assert_eq!(OffCpu::state_name(0x8000), "other");
    }

    #[test]
    fn a_record_the_wrong_size_is_refused_rather_than_padded() {
        assert!(OffCpu::decode(&[0u8; 8], &[0u8; TOTAL_VALUE_BYTES]).is_none());
        assert!(OffCpu::decode(&[0u8; TOTAL_KEY_BYTES], &[0u8; 16]).is_none());
    }

    #[test]
    fn a_name_stops_at_its_terminator() {
        let mut key = [0u8; TOTAL_KEY_BYTES];
        key[0..4].copy_from_slice(&99u32.to_ne_bytes());
        let mut value = [0u8; TOTAL_VALUE_BYTES];
        value[0..8].copy_from_slice(&1234u64.to_ne_bytes());
        value[16..20].copy_from_slice(b"true");
        let entry = OffCpu::decode(&key, &value).expect("a full record");
        assert_eq!(
            (entry.tid, entry.total_ns, entry.comm.as_str()),
            (99, 1234, "true")
        );
    }
}
