// SPDX-License-Identifier: MIT OR GPL-2.0
//! The `lock:contention_begin` / `lock:contention_end` pair: a thread that had
//! to wait for a kernel lock.
//!
//! This is the source the kernel floor of 6.1 was chosen for. `DECISIONS.md`
//! settled on that floor so that kernel lock contention could be measured from
//! tracepoints the kernel itself maintains, rather than from a set of kprobes
//! on `mutex_lock`, `down_read`, `rt_spin_lock` and everything else that
//! blocks -- a fallback that would have had to be written, would have covered
//! a different set of locks on every kernel, and that nobody would have
//! exercised.
//!
//! **A thread waits for one lock at a time**, which is what makes the pairing
//! cheap: the in-flight map is keyed by thread id alone, and there is no
//! ambiguity about which wait an end closes. A thread blocked on a lock is not
//! running, so it cannot begin a second wait before finishing the first.
//!
//! **The histogram is built in the kernel**, for the reason `runqlat.rs`
//! explains at length: a contended lock on a busy machine fires these
//! tracepoints hundreds of thousands of times a second, and one ring record
//! per wait is a measurement whose cost grows with the contention it is
//! measuring. What crosses to userspace is a count and an exact total per
//! `(task, lock kind, stack, log2 bucket)`.
//!
//! **The stack is the user stack**, walked where the wait ends. The kernel
//! side of a lock wait is the lock primitive -- `__mutex_lock_slowpath` and
//! its neighbours, the same few functions every time -- and the side that says
//! something about the program is the call that took the lock. It is also the
//! only side this build can name: kernel frames would need `/proc/kallsyms`,
//! which a normal machine restricts to root.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, Insn, Program, Whose, BPF_DW, BPF_W, R10};
use super::map::{map_failure, row_map, select_rows, Counters, Hash, Rows, StackTraces};
use super::sched_common::{
    bucket_low_ns, count_drop, count_update_failure, load_reading_user_lr,
    push_current_followed_filter, push_log2_bucket, push_user_lr, text_of, Descendants, Followed,
    Log2Labels, BPF_F_USER_STACK, BPF_NOEXIST, E2BIG, EEXIST,
};
use super::syscall;

/// Arguments of the two tracepoints, in the raw tracepoint's context.
///
/// Slot zero holds the lock's address on both. A kernel address is recycled
/// the moment the structure holding it is freed, so it identifies a lock only
/// for as long as that lock exists -- the same argument the catalog makes
/// about block request pointers -- and it is carried as an attribute (`addr`,
/// what `perf lock contention -l` groups by), never as a key that would
/// outlive the lock. The begin program records it; the end program puts it in
/// the row.
const ARG_LOCK: i16 = 0;
const ARG_FLAGS: i16 = 8;

/// The prototypes these programs are written against, checked against the
/// running kernel before a single instruction is built.
const BEGIN_SIGNATURE: [&str; 2] = ["void *", "unsigned int"];
const END_SIGNATURE: [&str; 2] = ["void *", "int"];

/// How many threads may be waiting for a lock at once.
///
/// Every thread on the machine can be contending at the same instant, which is
/// what a lock convoy *is*, so this is sized for the machine rather than for
/// the expected case.
const MAX_WAITING: u32 = 16384;

/// How many distinct `(task, kind, stack, bucket)` rows one interval may hold.
const MAX_ROWS: u32 = 16384;

/// How many distinct stacks may be interned, and how deep each may go.
const MAX_STACKS: u32 = 4096;
const MAX_STACK_DEPTH: u32 = 64;

/// Slots of the counter, one per thing that can be lost *and* per reason it
/// was lost, plus the two things this program declines to measure. They are
/// separate slots because they are answered differently, and a loss reported
/// under the wrong reason sends a reader to change something that was never
/// the problem.
const DROP_WAITING_FULL: u32 = 0;
const DROP_WAITING_NESTED: u32 = 1;
const DROP_WAITING_BUSY: u32 = 2;
const DROP_ROWS_FULL: u32 = 3;
const DROP_ROWS_BUSY: u32 = 4;
/// Waits of the per-cpu idle task, which this program does not pair; see
/// `begin_instructions`.
const SKIPPED_IDLE: u32 = 5;
/// Ends that closed at a moment before their own beginning, which is proof the
/// pairing was wrong and never a duration to report.
const DROP_BACKWARDS: u32 = 6;
const COUNTER_SLOTS: u32 = 7;

/// Where each map's key and value live on the program's stack.
const ROW_VALUE: i16 = -128;
const ROW_KEY: i16 = -168;
const WAIT_VALUE: i16 = -64;
const WAIT_KEY: i16 = -40;
const DROP_KEY: i16 = -32;
/// Where the program keeps the row map it looked up through the generation
/// slot, once per run; see `map::select_rows`.
const ROWS_SLOT: i16 = -176;
/// Eight bytes for a cgroup target's comparison, `insn::push_current_filter`.
const FILTER_SCRATCH: i16 = -184;

/// The layout of a row key, as byte offsets within it.
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
/// The `LCB_F_*` bits the kernel passes with `contention_begin`. In the key
/// rather than the value because a spinlock wait and a mutex wait are
/// different problems with different fixes, and a field that only ever holds
/// its last-seen value cannot be told apart from one that never changed.
const KEY_FLAGS: i16 = 8;
/// The log2 bucket of the wait. Part of the key is what makes this a histogram
/// rather than a mean.
const KEY_BUCKET: i16 = 12;
/// What `bpf_get_stackid` returned, kept as it came: a negative value is the
/// kernel saying it could not walk the stack, which is the ordinary answer for
/// a kernel worker and not an error to hide.
const KEY_STACK: i16 = 16;
/// The kernel stack of the wait, as `bpf_get_stackid` returned it: where the
/// caller of the lock primitive is (`perf lock contention`'s caller column).
const KEY_KSTACK: i16 = 20;
/// The user link register the wait ended with, on aarch64, or zero; the
/// caller a frameless leaf left there. See `sched_common::push_user_lr`.
const KEY_LR: i16 = 24;
/// The lock's address, from `contention_begin`.
const KEY_ADDR: i16 = 32;
const ROW_KEY_BYTES: usize = 40;

/// The layout of a row value.
const VALUE_COUNT: i16 = 0;
/// The exact sum of the waits in this bucket, kept beside the count because a
/// log2 bucket is a factor of two wide and a total built from its bounds would
/// inherit that error.
const VALUE_TOTAL_NS: i16 = 8;
const VALUE_COMM: i16 = 16;
const COMM_BYTES: usize = 16;
/// When the first wait of this row's window *began*.
///
/// A bucketed source emits one event per row per window, and stamping that
/// event at the drain puts it at the window's end -- so a join that *opens* on
/// this source opens after everything that happened in the same window as the
/// first wait. Finding 44 has the argument; `mem.reclaim` and
/// `sched.runqlat` carried it first.
///
/// Written once, where the row is created, which is that key's first wait of
/// the window: the rows map is drained and emptied every window.
const VALUE_FIRST_NS: i16 = 32;
const ROW_VALUE_BYTES: usize = 40;

/// The in-flight map: thread id to when its wait began and what it is waiting
/// on.
const WAIT_KEY_BYTES: usize = 4;
const WAIT_TS: i16 = 0;
const WAIT_FLAGS: i16 = 8;
const WAIT_PAD: i16 = 12;
const WAIT_ADDR: i16 = 16;
const WAIT_VALUE_BYTES: usize = 24;

/// Labels for the unrolled log2, which must be unique within one program.
const LOG2: Log2Labels = [
    "lock_log2_32",
    "lock_log2_16",
    "lock_log2_8",
    "lock_log2_4",
    "lock_log2_2",
    "lock_log2_1",
    "lock_log2_done",
];

/// `bpf_get_current_comm` and the stack walker are GPL-only.
const LICENSE: &str = "Dual MIT/GPL";

/// One `(task, lock kind, stack, bucket)` row's waits over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockWait {
    pub tid: u32,
    pub tgid: u32,
    /// The `LCB_F_*` bits, as the kernel passed them.
    pub flags: u32,
    /// The log2 bucket, in `emit::BucketLayout::Log2`'s numbering.
    pub bucket: u32,
    /// The stack the kernel interned, or none when it could not walk one.
    pub stack: Option<u32>,
    /// The kernel stack of the wait, or none.
    pub kernel_stack: Option<u32>,
    /// The lock's kernel address.
    pub addr: u64,
    /// The user link register the wait ended with, on aarch64, or zero; see
    /// `KEY_LR`.
    pub lr: u64,
    pub count: u64,
    pub total_ns: u64,
    /// When the first of this row's window began, which is what the event is
    /// timestamped with. Zero means unknown, and falls back to the drain.
    pub first_ns: u64,
    pub comm: String,
}

/// The `LCB_F_*` bits, as `include/trace/events/lock.h` defines them.
const LCB_F_SPIN: u32 = 1 << 0;
const LCB_F_READ: u32 = 1 << 1;
const LCB_F_WRITE: u32 = 1 << 2;
const LCB_F_RT: u32 = 1 << 3;
const LCB_F_PERCPU: u32 = 1 << 4;
const LCB_F_MUTEX: u32 = 1 << 5;

impl LockWait {
    /// The low bound of this row's bucket, in nanoseconds.
    pub fn bucket_low_ns(&self) -> u64 {
        bucket_low_ns(self.bucket)
    }

    /// What kind of lock this wait was on, in words.
    ///
    /// The bits are not exclusive -- a reader-writer semaphore arrives as
    /// `read` or `write`, a PREEMPT_RT machine adds `rt` to a spinlock -- so
    /// this names every bit that is set rather than picking one. A wait whose
    /// flags say nothing is reported as `other` rather than as a guess: the
    /// set of bits has grown once already and will again.
    pub fn kind(&self) -> String {
        let mut parts = Vec::new();
        for (bit, name) in [
            (LCB_F_SPIN, "spin"),
            (LCB_F_MUTEX, "mutex"),
            (LCB_F_RT, "rt"),
            (LCB_F_PERCPU, "percpu"),
            (LCB_F_READ, "read"),
            (LCB_F_WRITE, "write"),
        ] {
            if self.flags & bit != 0 {
                parts.push(name);
            }
        }
        if parts.is_empty() {
            return "other".to_owned();
        }
        parts.join("+")
    }

    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        let word =
            |bytes: &[u8], at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long =
            |bytes: &[u8], at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        let stack = word(key, KEY_STACK as usize) as i32;
        let kernel_stack = word(key, KEY_KSTACK as usize) as i32;
        Some(Self {
            tid: word(key, KEY_TID as usize),
            tgid: word(key, KEY_TGID as usize),
            flags: word(key, KEY_FLAGS as usize),
            bucket: word(key, KEY_BUCKET as usize),
            // A negative id is the kernel's errno for a stack it could not
            // walk, and it must not be handed on as a stack number.
            stack: (stack >= 0).then_some(stack as u32),
            kernel_stack: (kernel_stack >= 0).then_some(kernel_stack as u32),
            addr: long(key, KEY_ADDR as usize),
            lr: long(key, KEY_LR as usize),
            count: long(value, VALUE_COUNT as usize),
            total_ns: long(value, VALUE_TOTAL_NS as usize),
            first_ns: long(value, VALUE_FIRST_NS as usize),
            comm: text_of(&value[VALUE_COMM as usize..VALUE_COMM as usize + COMM_BYTES]),
        })
    }
}

/// Checks a tracepoint's prototype before any instruction is built.
fn check(btf: &Btf, tracepoint: &str, expected: &[&str]) -> Result<(), String> {
    let signature = btf.tracepoint_signature(tracepoint)?;
    if signature != expected {
        return Err(format!(
            "this kernel declares `{tracepoint}({})`, and this program is written for \
             `{tracepoint}({})`: it reads fixed slots of the tracepoint's context, and against \
             another prototype those slots hold something else",
            signature.join(", "),
            expected.join(", "),
        ));
    }
    Ok(())
}

/// Loads the caller's thread id and thread group into `r7`, and filters.
///
/// `bpf_get_current_pid_tgid` rather than a `task_struct` read: both
/// tracepoints run in the contending thread's own context, so the current task
/// *is* the subject, and one helper call beats two probe reads on a path this
/// hot.
fn current_and_filter(
    program: &mut Program,
    followed: &Followed,
    resume: &'static str,
    kept: &'static str,
) {
    push_current_followed_filter(program, followed, 7, FILTER_SCRATCH, resume, kept);
}

/// The program on `contention_begin`: a thread has started waiting.
fn begin_instructions(
    btf: &Btf,
    followed: &Followed,
    waiting_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    check(btf, "contention_begin", &BEGIN_SIGNATURE)?;

    let mut program = Program::new();
    // r6 keeps the context and r7 the caller's ids; both survive the helper
    // calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    current_and_filter(&mut program, followed, "done", "followed");

    // The whole slot is cleared first: the key is four bytes in an eight-byte
    // slot, and the four the program does not write are still on the stack.
    program.push(Insn::st_imm(BPF_DW, R10, WAIT_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, WAIT_KEY, 7));

    // Thread zero is not one thread. Every cpu has an idle task and all of
    // them are numbered zero, so an in-flight map keyed by thread id puts
    // twenty-four of them in one slot: a wait opened on one cpu is then closed
    // by another cpu's idle task, and the duration that comes out is the gap
    // between two unrelated instants -- measured here before this line, a
    // negative one, rendered as a wait of a hundred and fifty million minutes.
    // The idle task's contention is real and is in interrupt context; what is
    // not real is any number this pairing could give for it. Counted rather
    // than dropped in silence.
    program.push(Insn::ldx(BPF_W, 1, R10, WAIT_KEY));
    program.jump_to("idle", Insn::jeq_imm(1, 0, 0));

    // The flags are read before the clock: `ldx` clobbers nothing the call
    // would, but keeping the context read next to the context makes the
    // register discipline visible.
    program.push(Insn::ldx(BPF_DW, 8, 6, ARG_FLAGS));
    program.push(Insn::stx(BPF_W, R10, WAIT_VALUE + WAIT_FLAGS, 8));
    // Every byte of the value is written: the four this layout does not use
    // would otherwise travel whatever was on the stack.
    program.push(Insn::st_imm(BPF_W, R10, WAIT_VALUE + WAIT_PAD, 0));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_LOCK));
    program.push(Insn::stx(BPF_DW, R10, WAIT_VALUE + WAIT_ADDR, 1));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, WAIT_VALUE + WAIT_TS, 0));

    program.extend(Insn::load_map_fd(1, waiting_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, WAIT_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, WAIT_VALUE as i32));
    // `BPF_NOEXIST` rather than an overwrite. A second `contention_begin` for
    // a thread that has not ended its first is contention *inside* contention
    // -- an interrupt arriving while a thread spins, taking a lock of its own
    // -- and overwriting would silently replace the outer wait, which is the
    // longer of the two and the one the thread actually experiences. The outer
    // one is kept and the inner one is counted as unmeasured.
    program.push(Insn::mov64_imm(4, BPF_NOEXIST));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    // The update did not happen: this wait will never be timed, and nothing
    // else anywhere would record that it happened.
    count_update_failure(
        &mut program,
        drops_fd,
        DROP_KEY,
        &[
            (E2BIG, DROP_WAITING_FULL, "waiting_not_full"),
            (EEXIST, DROP_WAITING_NESTED, "waiting_not_nested"),
        ],
        DROP_WAITING_BUSY,
        "done",
    );
    program.jump_to("done", Insn::ja());

    program.label("idle");
    count_drop(&mut program, drops_fd, DROP_KEY, SKIPPED_IDLE, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `contention_end`: the thread has the lock, and the wait has
/// a length.
#[allow(clippy::too_many_arguments)]
fn end_instructions(
    btf: &Btf,
    followed: &Followed,
    waiting_fd: i32,
    rows_fd: i32,
    drops_fd: i32,
    stacks_fd: i32,
    kernel_stacks_fd: i32,
    leaf_lr: bool,
) -> Result<Vec<Insn>, String> {
    check(btf, "contention_end", &END_SIGNATURE)?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    // The clock is read first, so that nothing this program does is counted as
    // part of the wait it is closing.
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));
    current_and_filter(&mut program, followed, "done", "followed");

    program.push(Insn::st_imm(BPF_DW, R10, WAIT_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, WAIT_KEY, 7));
    program.extend(Insn::load_map_fd(1, waiting_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, WAIT_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // A thread this run never saw begin a wait: it was already blocked when
    // the programs were attached, or it belongs to another process.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    // Both fields are read before the entry is deleted, because deleting
    // invalidates the pointer the lookup returned.
    program.push(Insn::ldx(BPF_DW, 9, 0, WAIT_TS));
    program.push(Insn::ldx(BPF_W, 1, 0, WAIT_FLAGS));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_FLAGS, 1));
    program.push(Insn::ldx(BPF_DW, 1, 0, WAIT_ADDR));
    program.push(Insn::stx(BPF_DW, R10, ROW_KEY + KEY_ADDR, 1));

    program.extend(Insn::load_map_fd(1, waiting_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, WAIT_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    // r9 = now - when the wait began. Computed through r1, which nothing
    // between here and the next call reads.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::mov64_reg(9, 1));

    // A wait that ended before it began is not a long wait, it is proof that
    // the two events were not the same wait. Read as unsigned -- which is what
    // a duration is -- the difference is nine billion billion nanoseconds, and
    // it would be bucketed and rendered as calmly as any other number. The
    // pairing this program uses should make it impossible; it is checked
    // anyway, because the cost is one compare and the alternative is a
    // histogram nobody can tell is wrong.
    program.jump_to("forwards", Insn::jsge_imm(9, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_BACKWARDS, "done");
    program.jump_to("done", Insn::ja());
    program.label("forwards");

    // The stack goes in the key before the bucket does: this call clobbers r1
    // through r5, and the bucket is computed in them.
    program.push(Insn::mov64_reg(1, 6));
    program.extend(Insn::load_map_fd(2, stacks_fd));
    program.push(Insn::mov64_imm(3, BPF_F_USER_STACK));
    program.push(Insn::call(helper::GET_STACKID));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_STACK, 0));
    // The kernel half, for the caller of the lock primitive.
    program.push(Insn::mov64_reg(1, 6));
    program.extend(Insn::load_map_fd(2, kernel_stacks_fd));
    program.push(Insn::mov64_imm(3, 0));
    program.push(Insn::call(helper::GET_STACKID));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_KSTACK, 0));
    push_user_lr(&mut program, ROW_KEY + KEY_LR, leaf_lr);

    push_log2_bucket(&mut program, 9, &LOG2);
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_BUCKET, 3));

    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TID, 7));
    program.push(Insn::mov64_reg(1, 7));
    program.push(Insn::rsh64_imm(1, 32));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TGID, 1));

    // The row map of this interval, taken once: a drain may swap it at any
    // instruction, and a lookup and an insert that named two maps would put
    // one occurrence in each. See `map::Rows`.
    select_rows(&mut program, rows_fd, ROWS_SLOT, "no_rows");
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first_wait", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu, and a lock is precisely the thing
    // several cpus reach at once, so neither increment can be a plain
    // load-add-store.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_TOTAL_NS, 9));
    program.jump_to("done", Insn::ja());

    program.label("first_wait");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_TOTAL_NS, 9));
    // When this one began: now, less how long it lasted. r8 is the reading
    // taken at the top of the program and r9 is the duration, so the beginning
    // is recovered rather than carried through a second map field.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_FIRST_NS, 1));
    // Read once, when the row is created rather than on every wait: a thread's
    // name does not change, and this path runs as often as the lock is
    // contended.
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (ROW_VALUE + VALUE_COMM) as i32));
    program.push(Insn::mov64_imm(2, COMM_BYTES as i32));
    program.push(Insn::call(helper::GET_CURRENT_COMM));

    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_update_failure(
        &mut program,
        drops_fd,
        DROP_KEY,
        &[(E2BIG, DROP_ROWS_FULL, "rows_not_full")],
        DROP_ROWS_BUSY,
        "done",
    );

    program.jump_to("done", Insn::ja());
    // The slot held no map, which it always does; counted rather than assumed.
    program.label("no_rows");
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_ROWS_BUSY, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Waits neither map could take, counted under the kernel's own reason.
///
/// Kept apart because they are answered differently: a full map is a size this
/// build chose and can raise, and a busy one is the kernel declining a
/// re-entrant update on a path that by its nature re-enters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LockDrops {
    pub waiting_full: u64,
    /// A thread that began a second wait without ending its first.
    pub waiting_nested: u64,
    pub waiting_busy: u64,
    pub rows_full: u64,
    pub rows_busy: u64,
    /// Waits of the per-cpu idle task, which this program declines to pair.
    /// Not a loss: a number this pairing cannot produce correctly is not one
    /// that was lost on the way out.
    pub skipped_idle: u64,
    /// Ends whose beginning was later than themselves.
    pub backwards: u64,
}

/// Two loaded, attached programs and the maps they share.
///
/// Dropping it detaches both: the links, the programs and the maps are file
/// descriptors, and the kernel unloads a program when the last one closes.
pub struct LockProgram {
    /// The processes a process target started, followed in the kernel.
    descendants: Option<Descendants>,
    waiting: Hash,
    rows: Rows,
    drops: Counters,
    stacks: StackTraces,
    kernel_stacks: StackTraces,
    /// Why the user link register is not read, where the architecture has
    /// one.
    leaf_lr_missing: Option<String>,
    begin_program: OwnedFd,
    _begin_link: OwnedFd,
    end_program: OwnedFd,
    _end_link: OwnedFd,
}

impl LockProgram {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.begin_program, &self.end_program])
            + self.descendants.as_ref().map_or(0, Descendants::skipped)
    }

    /// The processes the target started and this run follows now, beside the
    /// target itself; empty for any other target.
    pub fn processes(&self) -> Vec<u32> {
        self.descendants
            .as_ref()
            .map_or_else(Vec::new, Descendants::processes)
    }

    /// Processes the target started that no program here followed, because
    /// the map of them was full.
    pub fn unfollowed(&self) -> u64 {
        self.descendants.as_ref().map_or(0, Descendants::missed)
    }

    pub fn load(whose: Whose) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        // A process target is followed into every process it starts, as
        // `sched.offcpu` and `sched.wakeup` follow it: a program on
        // `sched_process_fork` adds a child to the set before it has run.
        let descendants = match whose {
            Whose::Process(root) => Some(Descendants::follow(root)?),
            _ => None,
        };
        let followed = descendants
            .as_ref()
            .map_or(Followed::only(whose), |descendants| {
                descendants.followed(whose)
            });

        let waiting = Hash::create(
            WAIT_KEY_BYTES,
            WAIT_VALUE_BYTES,
            MAX_WAITING,
            "omn_lock_wait",
        )
        .map_err(|error| map_failure("lock wait map", error))?;
        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_lock_hist")
            .map_err(|error| map_failure("lock histogram map", error))?;
        let drops = Counters::create(COUNTER_SLOTS, "omn_lock_drop")
            .map_err(|error| map_failure("lock drop counter", error))?;
        let stacks = StackTraces::create(MAX_STACKS, MAX_STACK_DEPTH, "omn_lock_stk")
            .map_err(|error| map_failure("lock stack map", error))?;
        let kernel_stacks = StackTraces::create(MAX_STACKS, MAX_STACK_DEPTH, "omn_lock_kstk")
            .map_err(|error| map_failure("lock kernel stack map", error))?;

        let begin = begin_instructions(btf, &followed, waiting.as_raw_fd(), drops.as_raw_fd())?;
        let begin_program = load(&begin, "omn_lock_begin")?;
        let (end_program, leaf_lr_missing) = load_reading_user_lr(
            "lock",
            |leaf_lr| {
                end_instructions(
                    btf,
                    &followed,
                    waiting.as_raw_fd(),
                    rows.as_raw_fd(),
                    drops.as_raw_fd(),
                    stacks.as_raw_fd(),
                    kernel_stacks.as_raw_fd(),
                    leaf_lr,
                )
            },
            |instructions| load(instructions, "omn_lock_end"),
        )?;

        // Attached in this order on purpose. A wait recorded by a program whose
        // partner is not attached yet is a wait that is never closed -- it ages
        // out of the map -- whereas an end seen before any begin was recorded
        // would close a wait nobody opened, and the lookup that fails is the
        // only thing standing between that and a duration measured from an
        // uninitialised timestamp.
        let begin_link =
            syscall::raw_tracepoint_open("contention_begin", begin_program.as_raw_fd()).map_err(
                |error| format!("cannot attach to the `contention_begin` tracepoint: {error}"),
            )?;
        let end_link = syscall::raw_tracepoint_open("contention_end", end_program.as_raw_fd())
            .map_err(|error| {
                format!("cannot attach to the `contention_end` tracepoint: {error}")
            })?;

        Ok(Self {
            descendants,
            waiting,
            rows,
            drops,
            stacks,
            kernel_stacks,
            leaf_lr_missing,
            begin_program,
            _begin_link: begin_link,
            end_program,
            _end_link: end_link,
        })
    }

    /// Takes every row the kernel has accumulated since the last drain.
    pub fn drain(&self) -> Result<(Vec<LockWait>, super::DrainLosses), String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the lock histogram: {error}"))?;
        let mut losses = super::DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match LockWait::decode(key, value) {
                Some(row) => rows.push(row),
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    /// The addresses behind a stack id, or none if the kernel has evicted it.
    pub fn stack(&self, id: u32) -> Option<Vec<u64>> {
        self.stacks.get(id).ok().flatten()
    }

    /// The kernel addresses behind a kernel stack id, leaf first.
    pub fn kernel_stack(&self, id: u32) -> Option<Vec<u64>> {
        self.kernel_stacks.get(id).ok().flatten()
    }

    /// How deep a stack this build asked the kernel to walk, so that a stack
    /// which came back exactly that deep can be reported as truncated rather
    /// than as complete.
    /// Why the user link register is not read, on aarch64 where it is the
    /// only place a frameless leaf's caller is.
    pub fn leaf_lr_missing(&self) -> Option<&str> {
        self.leaf_lr_missing.as_deref()
    }

    pub fn stack_depth(&self) -> usize {
        self.stacks.depth() as usize
    }

    /// How many threads were waiting for a lock when the run ended.
    ///
    /// Their wait is real and is in no bucket, because it has no end yet.
    pub fn still_waiting(&self) -> usize {
        self.waiting.len().unwrap_or(0)
    }

    /// Waits the programs could not record, by which map refused and why.
    pub fn dropped(&self) -> LockDrops {
        LockDrops {
            waiting_full: self.drops.read(DROP_WAITING_FULL),
            waiting_nested: self.drops.read(DROP_WAITING_NESTED),
            waiting_busy: self.drops.read(DROP_WAITING_BUSY),
            rows_full: self.drops.read(DROP_ROWS_FULL),
            rows_busy: self.drops.read(DROP_ROWS_BUSY),
            skipped_idle: self.drops.read(SKIPPED_IDLE),
            backwards: self.drops.read(DROP_BACKWARDS),
        }
    }
}

fn load(instructions: &[Insn], name: &str) -> Result<OwnedFd, String> {
    syscall::prog_load(
        syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
        instructions,
        name,
        LICENSE,
    )
    .map_err(|error| {
        if error.is_permission() {
            super::permission_advice("loading a raw tracepoint program")
        } else {
            format!("the kernel refused the `{name}` program: {error}")
        }
    })
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

    fn end_program(btf: &Btf, target: Whose) -> Vec<Insn> {
        end_instructions(btf, &Followed::only(target), 3, 4, 5, 6, 7, true).expect("a program")
    }

    /// A wait is the gap between two events, so both programs have to read the
    /// same clock -- and it has to be the one this project's whole timeline is
    /// in. `bpf_ktime_get_boot_ns` would be a different domain and a silent
    /// offset against every other source in a session.
    #[test]
    fn both_programs_read_clock_monotonic_and_nothing_else() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        for program in [
            begin_instructions(&btf, &Followed::only(Whose::Any), 3, 4).expect("a program"),
            end_program(&btf, Whose::Any),
        ] {
            let called: Vec<i32> = program
                .iter()
                .filter(|insn| insn.code == 0x85)
                .map(|insn| insn.imm)
                .collect();
            assert!(called.contains(&helper::KTIME_GET_NS), "{called:?}");
            assert!(!called.contains(&KTIME_GET_BOOT_NS), "{called:?}");
        }
    }

    /// A lock is the one thing several cpus reach at the same instant, so a
    /// plain load-add-store on a shared row loses whichever increment lands
    /// inside another cpu's -- and loses most of them on exactly the machine
    /// whose locks are busiest.
    #[test]
    fn the_count_and_the_total_are_accumulated_atomically() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            end_program(&btf, Whose::Any)
                .iter()
                .filter(|insn| insn.code == 0xdb)
                .count(),
            2,
            "the count and the total, both atomically"
        );
    }

    /// The stack is asked for at the end of the wait, where the thread is
    /// running again and its user stack is the call that took the lock. It has
    /// to be the *user* stack: the kernel side is the lock primitive, and this
    /// build cannot name kernel frames at all.
    #[test]
    fn the_stack_walked_is_the_tasks_own() {
        let Some(btf) = kernel_btf() else { return };
        let program = end_program(&btf, Whose::Any);
        let walk = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::GET_STACKID)
            .expect("the stack walk");
        assert!(
            program[..walk]
                .iter()
                .any(|insn| insn.imm == BPF_F_USER_STACK),
            "the walk is asked for without BPF_F_USER_STACK"
        );
    }

    #[test]
    fn the_filter_is_present_only_when_a_process_was_named() {
        let Some(btf) = kernel_btf() else { return };
        assert!(!end_program(&btf, Whose::Any)
            .iter()
            .any(|insn| insn.imm == 4242));
        assert_eq!(
            end_program(&btf, Whose::Process(4242))
                .iter()
                .filter(|insn| insn.imm == 4242)
                .count(),
            1
        );
    }

    #[test]
    fn a_kernel_with_another_prototype_is_refused_rather_than_read_wrong() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            btf.tracepoint_signature("contention_begin")
                .expect("a prototype"),
            BEGIN_SIGNATURE
        );
        assert_eq!(
            btf.tracepoint_signature("contention_end")
                .expect("a prototype"),
            END_SIGNATURE
        );
        assert!(check(&btf, "contention_begin", &["void *"]).is_err());
    }

    /// The kernel hashes every byte of a key, so one the program never writes
    /// puts two identical waits in two buckets and halves both counts.
    #[test]
    fn every_byte_of_every_key_and_value_is_accounted_for() {
        let row_key = [
            (KEY_TID, 4),
            (KEY_TGID, 4),
            (KEY_FLAGS, 4),
            (KEY_BUCKET, 4),
            (KEY_STACK, 4),
            (KEY_KSTACK, 4),
            (KEY_LR, 8),
            (KEY_ADDR, 8),
        ];
        let row_value = [
            (VALUE_COUNT, 8),
            (VALUE_TOTAL_NS, 8),
            (VALUE_FIRST_NS, 8),
            (VALUE_COMM, COMM_BYTES as i16),
        ];
        let wait_value = [(WAIT_TS, 8), (WAIT_FLAGS, 4), (WAIT_PAD, 4), (WAIT_ADDR, 8)];
        for (fields, bytes, what) in [
            (&row_key[..], ROW_KEY_BYTES, "the row key"),
            (&row_value[..], ROW_VALUE_BYTES, "the row value"),
            (&wait_value[..], WAIT_VALUE_BYTES, "the in-flight value"),
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

    #[test]
    fn the_stack_regions_do_not_overlap_and_stay_inside_the_frame() {
        let regions = [
            (ROW_VALUE, ROW_VALUE_BYTES as i16, "row value"),
            (ROW_KEY, ROW_KEY_BYTES as i16, "row key"),
            (WAIT_VALUE, WAIT_VALUE_BYTES as i16, "in-flight value"),
            (WAIT_KEY, 8, "in-flight key"),
            (DROP_KEY, 4, "drop key"),
            (ROWS_SLOT, 8, "row map slot"),
            (FILTER_SCRATCH, 8, "filter scratch"),
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

    #[test]
    fn a_record_the_wrong_size_is_refused_rather_than_padded() {
        assert!(LockWait::decode(&[0u8; 4], &[0u8; ROW_VALUE_BYTES]).is_none());
        assert!(LockWait::decode(&[0u8; ROW_KEY_BYTES], &[0u8; 8]).is_none());
    }

    /// The kernel returns a negative errno when it cannot walk a stack, and
    /// that value shares the field with a real id. Handing `-14` on as stack
    /// 4294967282 would point every unwalkable wait at one invented call site.
    #[test]
    fn a_stack_the_kernel_could_not_walk_is_no_stack_rather_than_a_huge_one() {
        let mut key = [0u8; ROW_KEY_BYTES];
        key[KEY_STACK as usize..KEY_STACK as usize + 4].copy_from_slice(&(-14i32).to_ne_bytes());
        let value = [0u8; ROW_VALUE_BYTES];
        assert_eq!(LockWait::decode(&key, &value).expect("a row").stack, None);
        key[KEY_STACK as usize..KEY_STACK as usize + 4].copy_from_slice(&7i32.to_ne_bytes());
        assert_eq!(
            LockWait::decode(&key, &value).expect("a row").stack,
            Some(7)
        );
    }

    /// The flag bits are not exclusive, and naming only the first one set
    /// would report a PREEMPT_RT spinlock and an ordinary one as the same
    /// thing.
    #[test]
    fn a_lock_kind_names_every_bit_the_kernel_set() {
        let of = |flags: u32| {
            LockWait {
                tid: 1,
                tgid: 1,
                flags,
                bucket: 3,
                stack: None,
                kernel_stack: None,
                addr: 0xffff_8881_0000_1000,
                lr: 0,
                count: 1,
                total_ns: 4,
                first_ns: 0,
                comm: String::new(),
            }
            .kind()
        };
        assert_eq!(of(LCB_F_SPIN), "spin");
        assert_eq!(of(LCB_F_MUTEX), "mutex");
        assert_eq!(of(LCB_F_SPIN | LCB_F_RT), "spin+rt");
        assert_eq!(of(LCB_F_READ), "read");
        assert_eq!(of(LCB_F_WRITE | LCB_F_PERCPU), "percpu+write");
        // A bit this build has never seen is not a guess at one it has.
        assert_eq!(of(1 << 20), "other");
    }

    /// The row map is found through the generation slot once per run, and the
    /// lookup and the insert both use what it found -- the property that makes
    /// the drain's swap safe; see `map::Rows`.
    #[test]
    fn the_row_map_is_taken_from_the_slot_once_per_run() {
        let Some(btf) = kernel_btf() else { return };
        let program = end_instructions(&btf, &Followed::only(Whose::Any), 3, 4, 5, 6, 7, true)
            .expect("a program");
        crate::bpf::map::assert_rows_taken_once(&program, 4);
    }
}
