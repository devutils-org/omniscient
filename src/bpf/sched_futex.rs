// SPDX-License-Identifier: MIT OR GPL-2.0
//! The `sys_enter` / `sys_exit` pair, narrowed to the futex waits: a thread
//! that went to sleep in userspace's own lock.
//!
//! A futex is what every userspace lock is underneath. `pthread_mutex_lock`
//! spins briefly and then asks the kernel to put the thread to sleep until
//! somebody wakes it; so does a condition variable, a Rust `Mutex`, a Go
//! channel and a Java monitor. The time between that call and its return is
//! the time the program spent waiting for another thread, and it is invisible
//! to a cpu profiler for the usual reason: a sleeping thread burns nothing.
//!
//! **Only the waiting commands are paired.** `futex()` is one syscall with a
//! dozen operations behind an `op` argument, and most of them return
//! immediately -- `FUTEX_WAKE` is the other half of every wait and takes a
//! microsecond. Timing all of them would produce a distribution whose left
//! half is the wake path, and the summary of that is not "how long this
//! program waits". The filter is in the program, where the catalog's `bpf`
//! backend promises it is.
//!
//! **The op is read as a 32-bit value on purpose.** The kernel takes `int op`
//! out of the second syscall register, so only its low half means anything;
//! reading the register whole would let whatever userspace left in the top
//! half turn `FUTEX_WAIT` into a value this program does not recognise.
//!
//! **The two flag bits are stripped rather than enumerated.**
//! `FUTEX_PRIVATE_FLAG` and `FUTEX_CLOCK_REALTIME` ride on top of the command
//! -- glibc sets the first on every private lock -- and the kernel itself
//! masks them off before switching on the command. Doing the same here is two
//! conditional subtractions; matching against the sixteen combinations instead
//! would be a list to extend every time the kernel adds a flag.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, push_current_filter, Insn, Program, Whose, BPF_DW, BPF_W, R10};
use super::map::{map_failure, row_map, select_rows, Counters, Hash, Rows, StackTraces};
use super::sched_common::{
    bucket_low_ns, count_drop, count_update_failure, push_log2_bucket, read_field, text_of,
    Log2Labels, BPF_F_USER_STACK, BPF_NOEXIST, E2BIG, EEXIST,
};
use super::syscall;

/// Arguments of `sys_enter` and `sys_exit`, in the raw tracepoint's context.
const ARG_REGS: i16 = 0;
const ARG_ID: i16 = 8;

/// The prototypes these programs are written against.
const ENTER_SIGNATURE: [&str; 2] = ["pt_regs *", "long int"];
const EXIT_SIGNATURE: [&str; 2] = ["pt_regs *", "long int"];

/// The `futex` syscall's number, which is per architecture and has no
/// architecture-independent name anywhere the kernel publishes.
///
/// A wrong number here does not fail: it silently measures a different
/// syscall, so the build refuses on an architecture whose number this does not
/// know rather than guessing one.
#[cfg(target_arch = "x86_64")]
const SYS_FUTEX: i32 = 202;
#[cfg(target_arch = "aarch64")]
const SYS_FUTEX: i32 = 98;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "`sched.futex` reads the futex syscall's number, which differs per architecture; add this \
     one's before building for it"
);

/// The `op` values that make a thread wait, after the flag bits are stripped.
///
/// `FUTEX_WAIT` and `FUTEX_WAIT_BITSET` are what an ordinary mutex and a
/// condition variable reach; the `LOCK_PI` family is priority inheritance,
/// which a real-time program uses and which blocks the same way.
/// `FUTEX_WAIT_REQUEUE_PI` is the second half of a `pthread_cond_wait` on a
/// PI mutex.
const FUTEX_WAIT: i32 = 0;
const FUTEX_LOCK_PI: i32 = 6;
const FUTEX_WAIT_BITSET: i32 = 9;
const FUTEX_WAIT_REQUEUE_PI: i32 = 11;
const FUTEX_LOCK_PI2: i32 = 13;
const WAITING_COMMANDS: [(i32, &str); 5] = [
    (FUTEX_WAIT, "wait"),
    (FUTEX_LOCK_PI, "lock_pi"),
    (FUTEX_WAIT_BITSET, "wait_bitset"),
    (FUTEX_WAIT_REQUEUE_PI, "wait_requeue_pi"),
    (FUTEX_LOCK_PI2, "lock_pi2"),
];

/// The flag bits the kernel masks off before switching on the command.
const FUTEX_PRIVATE_FLAG: i32 = 128;
const FUTEX_CLOCK_REALTIME: i32 = 256;

/// How many threads may be inside a futex wait at once.
const MAX_WAITING: u32 = 16384;

/// How many distinct `(address, task, command, stack, bucket)` rows one
/// interval may hold.
const MAX_ROWS: u32 = 16384;

const MAX_STACKS: u32 = 4096;
const MAX_STACK_DEPTH: u32 = 64;

/// Slots of the counter, one per thing that can be lost and per reason.
const DROP_WAITING_FULL: u32 = 0;
const DROP_WAITING_NESTED: u32 = 1;
const DROP_WAITING_BUSY: u32 = 2;
const DROP_ROWS_FULL: u32 = 3;
const DROP_ROWS_BUSY: u32 = 4;
const DROP_BACKWARDS: u32 = 5;
const COUNTER_SLOTS: u32 = 6;

/// Where each map's key and value live on the program's stack.
const ROW_VALUE: i16 = -136;
const ROW_KEY: i16 = -96;
const WAIT_VALUE: i16 = -64;
const WAIT_KEY: i16 = -40;
const DROP_KEY: i16 = -32;
/// Where the program keeps the row map it looked up through the generation
/// slot, once per run; see `map::select_rows`.
const ROWS_SLOT: i16 = -144;
/// Where the raw second syscall register is read before its flag bits are
/// stripped. Its own slot, so that stripping does not overwrite a field the
/// value map is about to carry.
const SCRATCH: i16 = -24;

/// The layout of a row key.
///
/// The address first, because it is the thing a reader groups by: two threads
/// waiting on one futex is a contended lock, and two threads waiting on two is
/// two idle threads.
const KEY_ADDR: i16 = 0;
const KEY_TID: i16 = 8;
const KEY_TGID: i16 = 12;
const KEY_COMMAND: i16 = 16;
const KEY_BUCKET: i16 = 20;
const KEY_STACK: i16 = 24;
const ROW_KEY_BYTES: usize = 28;

/// The layout of a row value.
const VALUE_COUNT: i16 = 0;
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

/// The in-flight map: thread id to the wait it is inside.
const WAIT_KEY_BYTES: usize = 4;
const WAIT_TS: i16 = 0;
const WAIT_ADDR: i16 = 8;
const WAIT_COMMAND: i16 = 16;
const WAIT_PAD: i16 = 20;
const WAIT_VALUE_BYTES: usize = 24;

const LOG2: Log2Labels = [
    "futex_log2_32",
    "futex_log2_16",
    "futex_log2_8",
    "futex_log2_4",
    "futex_log2_2",
    "futex_log2_1",
    "futex_log2_done",
];

const LICENSE: &str = "Dual MIT/GPL";

/// One `(address, task, command, stack, bucket)` row's waits over one drain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FutexWait {
    /// The futex word's address in the waiting process, which is what makes
    /// two threads waiting on the same lock visible as such.
    pub addr: u64,
    pub tid: u32,
    pub tgid: u32,
    /// The `op` the thread called with, with the flag bits already stripped.
    pub command: u32,
    pub bucket: u32,
    pub stack: Option<u32>,
    pub count: u64,
    pub total_ns: u64,
    /// When the first of this row's window began, which is what the event is
    /// timestamped with. Zero means unknown, and falls back to the drain.
    pub first_ns: u64,
    pub comm: String,
}

impl FutexWait {
    pub fn bucket_low_ns(&self) -> u64 {
        bucket_low_ns(self.bucket)
    }

    /// The command in words, or its number when this build does not know it.
    ///
    /// A number rather than "unknown": the set of futex operations has grown
    /// several times, and a command this build cannot name is still one a
    /// reader can look up.
    pub fn command_name(&self) -> String {
        WAITING_COMMANDS
            .iter()
            .find(|(value, _)| *value as u32 == self.command)
            .map(|(_, name)| (*name).to_owned())
            .unwrap_or_else(|| format!("op {}", self.command))
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
        Some(Self {
            addr: long(key, KEY_ADDR as usize),
            tid: word(key, KEY_TID as usize),
            tgid: word(key, KEY_TGID as usize),
            command: word(key, KEY_COMMAND as usize),
            bucket: word(key, KEY_BUCKET as usize),
            stack: (stack >= 0).then_some(stack as u32),
            count: long(value, VALUE_COUNT as usize),
            total_ns: long(value, VALUE_TOTAL_NS as usize),
            first_ns: long(value, VALUE_FIRST_NS as usize),
            comm: text_of(&value[VALUE_COMM as usize..VALUE_COMM as usize + COMM_BYTES]),
        })
    }
}

fn check(btf: &Btf, tracepoint: &str, expected: &[&str]) -> Result<(), String> {
    let signature = btf.tracepoint_signature(tracepoint)?;
    if signature != expected {
        return Err(format!(
            "this kernel declares `{tracepoint}({})`, and this program is written for \
             `{tracepoint}({})`: it reads the register block and the syscall number out of fixed \
             slots, and against another prototype those slots hold something else",
            signature.join(", "),
            expected.join(", "),
        ));
    }
    Ok(())
}

/// Loads the caller's ids into `r7` and leaves unless they are the run's.
///
/// Both tracepoints fire on every syscall of every process on the machine,
/// which is what the catalog entry's `med` cost and its `raw_syscalls fires on
/// everything` neighbours are about. This comparison is the first thing either
/// program does, so the cost of a syscall that is not the run's is four
/// instructions.
fn current_and_filter(program: &mut Program, whose: Whose, resume: &'static str) {
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::mov64_reg(7, 0));
    match whose {
        Whose::Process(tgid) => {
            // The thread group is the high half of what the helper returns.
            program.push(Insn::mov64_reg(1, 7));
            program.push(Insn::rsh64_imm(1, 32));
            program.jump_to(resume, Insn::jne_imm(1, tgid as i32, 0));
        }
        Whose::Thread(tid) => {
            // The low half, compared 32 bits wide: the high half holds the
            // thread group and a 64-bit compare would never match.
            program.push(Insn::stx(BPF_W, R10, SCRATCH, 7));
            program.push(Insn::ldx(BPF_W, 1, R10, SCRATCH));
            program.jump_to(resume, Insn::jne_imm(1, tid as i32, 0));
        }
        // One helper call for the cgroup, and below it; `r7` survives it.
        Whose::Cgroup(_) => push_current_filter(program, whose, SCRATCH, resume),
        // Not a target the collector builds: this source has no machine-wide
        // form. Were one asked for, it would be every syscall on the machine.
        Whose::Any => {}
    }
}

/// The program on `sys_enter`: a thread is about to wait, if this is a wait.
fn enter_instructions(
    btf: &Btf,
    whose: Whose,
    waiting_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    check(btf, "sys_enter", &ENTER_SIGNATURE)?;
    let di = btf.member_offset("pt_regs", "di")?;
    let si = btf.member_offset("pt_regs", "si")?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));

    // The syscall number is the cheapest test there is and it is first: every
    // other syscall on the machine leaves after two instructions.
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_ID));
    program.jump_to("done", Insn::jne_imm(1, SYS_FUTEX, 0));

    current_and_filter(&mut program, whose, "done");

    // r9 keeps the register block the syscall arrived with.
    program.push(Insn::ldx(BPF_DW, 9, 6, ARG_REGS));

    // The second argument is `int op`. Read eight bytes and then four: the
    // register holds sixty-four bits and the kernel reads thirty-two of them,
    // so anything above that is userspace's leftovers and not part of the
    // command.
    read_field(&mut program, SCRATCH, 8, 9, si);
    program.push(Insn::ldx(BPF_W, 1, R10, SCRATCH));

    // The two flag bits, stripped the way the kernel strips them.
    for (bit, strip, after) in [
        (FUTEX_PRIVATE_FLAG, "strip_private", "after_private"),
        (FUTEX_CLOCK_REALTIME, "strip_realtime", "after_realtime"),
    ] {
        program.jump_to(strip, Insn::jset_imm(1, bit, 0));
        program.jump_to(after, Insn::ja());
        program.label(strip);
        program.push(Insn::add64_imm(1, -bit));
        program.label(after);
    }

    // Only the commands that make a thread wait are timed; everything else is
    // the other half of somebody's wait and returns at once.
    for (command, _) in WAITING_COMMANDS {
        program.jump_to("is_a_wait", Insn::jeq_imm(1, command, 0));
    }
    program.jump_to("done", Insn::ja());
    program.label("is_a_wait");
    program.push(Insn::stx(BPF_W, R10, WAIT_VALUE + WAIT_COMMAND, 1));
    // Every byte of the value is written: the four this layout does not use
    // would otherwise carry whatever was on the stack.
    program.push(Insn::st_imm(BPF_W, R10, WAIT_VALUE + WAIT_PAD, 0));

    // The futex word's address is the first argument.
    read_field(&mut program, WAIT_VALUE + WAIT_ADDR, 8, 9, di);

    program.push(Insn::st_imm(BPF_DW, R10, WAIT_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, WAIT_KEY, 7));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, WAIT_VALUE + WAIT_TS, 0));

    program.extend(Insn::load_map_fd(1, waiting_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, WAIT_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, WAIT_VALUE as i32));
    // `BPF_NOEXIST`: a thread inside a futex wait cannot enter a second one,
    // so an entry already there is this program's assumption being wrong, and
    // counting it beats overwriting a wait that is still open.
    program.push(Insn::mov64_imm(4, BPF_NOEXIST));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
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

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `sys_exit`: the wait, if there was one, is over.
///
/// The syscall number is not tested here and does not need to be. A thread is
/// in the in-flight map only while it is inside a futex wait, and a thread
/// inside a syscall is not making another one, so the first `sys_exit` that
/// finds an entry is that syscall's own return.
fn exit_instructions(
    btf: &Btf,
    whose: Whose,
    waiting_fd: i32,
    rows_fd: i32,
    drops_fd: i32,
    stacks_fd: i32,
) -> Result<Vec<Insn>, String> {
    check(btf, "sys_exit", &EXIT_SIGNATURE)?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    // The clock first, so nothing this program does is counted as part of the
    // wait it is closing.
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));
    current_and_filter(&mut program, whose, "done");

    program.push(Insn::st_imm(BPF_DW, R10, WAIT_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, WAIT_KEY, 7));
    program.extend(Insn::load_map_fd(1, waiting_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, WAIT_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // Not inside a futex wait: this is one of the other few thousand syscalls
    // a second this program is attached to.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    // Read before the delete, which invalidates the pointer.
    program.push(Insn::ldx(BPF_DW, 9, 0, WAIT_TS));
    program.push(Insn::ldx(BPF_DW, 1, 0, WAIT_ADDR));
    program.push(Insn::stx(BPF_DW, R10, ROW_KEY + KEY_ADDR, 1));
    program.push(Insn::ldx(BPF_W, 1, 0, WAIT_COMMAND));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_COMMAND, 1));

    program.extend(Insn::load_map_fd(1, waiting_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, WAIT_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    // r9 = now - when the wait began.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::mov64_reg(9, 1));
    // A wait that ended before it began is proof the two events were not the
    // same wait, and read as the unsigned number a duration is it would be
    // nine billion billion nanoseconds rendered as calmly as any other value.
    program.jump_to("forwards", Insn::jsge_imm(9, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_BACKWARDS, "done");
    program.jump_to("done", Insn::ja());
    program.label("forwards");

    // The stack is walked where the wait ends, which is inside the same
    // `pthread_mutex_lock` that began it: the user frames are the call that
    // took the lock.
    program.push(Insn::mov64_reg(1, 6));
    program.extend(Insn::load_map_fd(2, stacks_fd));
    program.push(Insn::mov64_imm(3, BPF_F_USER_STACK));
    program.push(Insn::call(helper::GET_STACKID));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_STACK, 0));

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
    // One allocation reachable from every cpu, and the threads sharing a futex
    // are on different cpus by construction.
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

/// Waits neither map could take, under the kernel's own reason.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FutexDrops {
    pub waiting_full: u64,
    pub waiting_nested: u64,
    pub waiting_busy: u64,
    pub rows_full: u64,
    pub rows_busy: u64,
    pub backwards: u64,
}

/// Two loaded, attached programs and the maps they share.
pub struct FutexProgram {
    waiting: Hash,
    rows: Rows,
    drops: Counters,
    stacks: StackTraces,
    enter_program: OwnedFd,
    _enter_link: OwnedFd,
    exit_program: OwnedFd,
    _exit_link: OwnedFd,
}

impl FutexProgram {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.enter_program, &self.exit_program])
    }

    pub fn load(whose: Whose) -> Result<Self, String> {
        let btf = super::kernel_btf()?;

        let waiting = Hash::create(
            WAIT_KEY_BYTES,
            WAIT_VALUE_BYTES,
            MAX_WAITING,
            "omn_futex_wait",
        )
        .map_err(|error| map_failure("futex wait map", error))?;
        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_futex_hist")
            .map_err(|error| map_failure("futex histogram map", error))?;
        let drops = Counters::create(COUNTER_SLOTS, "omn_futex_drop")
            .map_err(|error| map_failure("futex drop counter", error))?;
        let stacks = StackTraces::create(MAX_STACKS, MAX_STACK_DEPTH, "omn_futex_stk")
            .map_err(|error| map_failure("futex stack map", error))?;

        let enter = enter_instructions(btf, whose, waiting.as_raw_fd(), drops.as_raw_fd())?;
        let exit = exit_instructions(
            btf,
            whose,
            waiting.as_raw_fd(),
            rows.as_raw_fd(),
            drops.as_raw_fd(),
            stacks.as_raw_fd(),
        )?;

        let enter_program = load(&enter, "omn_futex_in")?;
        let exit_program = load(&exit, "omn_futex_out")?;

        // Entries before exits: a wait opened by a program whose partner is not
        // attached yet ages out of the map, whereas an exit seen before any
        // entry was recorded would close a wait nobody opened.
        let enter_link = syscall::raw_tracepoint_open("sys_enter", enter_program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sys_enter` tracepoint: {error}"))?;
        let exit_link = syscall::raw_tracepoint_open("sys_exit", exit_program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sys_exit` tracepoint: {error}"))?;

        Ok(Self {
            waiting,
            rows,
            drops,
            stacks,
            enter_program,
            _enter_link: enter_link,
            exit_program,
            _exit_link: exit_link,
        })
    }

    pub fn drain(&self) -> Result<(Vec<FutexWait>, super::DrainLosses), String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the futex histogram: {error}"))?;
        let mut losses = super::DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match FutexWait::decode(key, value) {
                Some(row) => rows.push(row),
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    pub fn stack(&self, id: u32) -> Option<Vec<u64>> {
        self.stacks.get(id).ok().flatten()
    }

    pub fn stack_depth(&self) -> usize {
        self.stacks.depth() as usize
    }

    /// How many threads were inside a futex wait when the run ended. Their
    /// wait is real and is in no bucket, because it has no end yet -- and for
    /// this source that is the normal state of an idle thread pool.
    pub fn still_waiting(&self) -> usize {
        self.waiting.len().unwrap_or(0)
    }

    pub fn dropped(&self) -> FutexDrops {
        FutexDrops {
            waiting_full: self.drops.read(DROP_WAITING_FULL),
            waiting_nested: self.drops.read(DROP_WAITING_NESTED),
            waiting_busy: self.drops.read(DROP_WAITING_BUSY),
            rows_full: self.drops.read(DROP_ROWS_FULL),
            rows_busy: self.drops.read(DROP_ROWS_BUSY),
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

    fn enter(btf: &Btf, whose: Whose) -> Vec<Insn> {
        enter_instructions(btf, whose, 3, 4).expect("a program")
    }

    fn exit(btf: &Btf, whose: Whose) -> Vec<Insn> {
        exit_instructions(btf, whose, 3, 4, 5, 6).expect("a program")
    }

    /// Both tracepoints fire on every syscall of every process on the machine.
    /// The syscall number has to be the first thing tested, or the cost of
    /// this source is paid by every `read` and `write` on the box.
    #[test]
    fn the_syscall_number_is_tested_before_anything_else_is_read() {
        let Some(btf) = kernel_btf() else { return };
        let program = enter(&btf, Whose::Process(1));
        let test = program
            .iter()
            .position(|insn| insn.imm == SYS_FUTEX)
            .expect("the syscall number");
        assert!(
            test < 3,
            "the futex test is at instruction {test}, after something else has already run"
        );
        assert!(
            !program[..test]
                .iter()
                .any(|insn| insn.code == 0x85 && insn.imm == helper::PROBE_READ_KERNEL),
            "a kernel read happens before the syscall is known to be futex"
        );
    }

    /// The kernel masks the two flag bits off before switching on the command,
    /// and glibc sets `FUTEX_PRIVATE_FLAG` on every private lock. A program
    /// that compared the raw `op` would match no wait that any ordinary
    /// program makes.
    #[test]
    fn the_flag_bits_are_stripped_before_the_command_is_matched() {
        let Some(btf) = kernel_btf() else { return };
        let program = enter(&btf, Whose::Process(1));
        let stripped: Vec<i32> = program
            .iter()
            .filter(|insn| insn.imm == -FUTEX_PRIVATE_FLAG || insn.imm == -FUTEX_CLOCK_REALTIME)
            .map(|insn| insn.imm)
            .collect();
        assert_eq!(stripped.len(), 2, "{stripped:?}");
        let first_match = program
            .iter()
            .position(|insn| insn.imm == FUTEX_WAIT_BITSET)
            .expect("a command match");
        let last_strip = program
            .iter()
            .rposition(|insn| insn.imm == -FUTEX_CLOCK_REALTIME)
            .expect("the realtime strip");
        assert!(
            last_strip < first_match,
            "a command is matched before the flags are stripped"
        );
    }

    /// Timing every futex operation would put the wake path -- the other half
    /// of every wait, and a microsecond long -- in the same distribution as
    /// the waits, and the summary of that is not how long this program waits.
    #[test]
    fn only_the_commands_that_make_a_thread_wait_are_paired() {
        let Some(btf) = kernel_btf() else { return };
        let program = enter(&btf, Whose::Process(1));
        for (command, name) in WAITING_COMMANDS {
            assert!(
                program.iter().any(|insn| insn.imm == command),
                "`{name}` is not matched"
            );
        }
        // FUTEX_WAKE and FUTEX_CMP_REQUEUE return at once and must not be
        // among the commands the program opens a wait for.
        for absent in [1i32, 4] {
            assert!(
                !WAITING_COMMANDS
                    .iter()
                    .any(|(command, _)| *command == absent),
                "a command that does not wait is in the waiting set"
            );
        }
    }

    /// A `--tid` run asks about one thread. Comparing thread groups would
    /// answer for every thread of its process, which is the question next to
    /// the one that was asked.
    #[test]
    fn a_thread_target_and_a_process_target_compare_different_halves() {
        let Some(btf) = kernel_btf() else { return };
        let by_process = enter(&btf, Whose::Process(4242));
        let by_thread = enter(&btf, Whose::Thread(4242));
        // The thread group is the high half, reached with a 32-bit shift.
        assert!(by_process.iter().any(|insn| insn.imm == 32));
        assert!(!by_thread
            .iter()
            .take_while(|insn| insn.imm != 4242)
            .any(|insn| insn.imm == 32));
        for program in [by_process, by_thread] {
            assert_eq!(
                program.iter().filter(|insn| insn.imm == 4242).count(),
                1,
                "the filter is written once"
            );
        }
    }

    #[test]
    fn both_programs_read_clock_monotonic_and_nothing_else() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        for program in [
            enter(&btf, Whose::Process(1)),
            exit(&btf, Whose::Process(1)),
        ] {
            let called: Vec<i32> = program
                .iter()
                .filter(|insn| insn.code == 0x85)
                .map(|insn| insn.imm)
                .collect();
            assert!(called.contains(&helper::KTIME_GET_NS));
            assert!(!called.contains(&KTIME_GET_BOOT_NS));
        }
    }

    #[test]
    fn the_count_and_the_total_are_accumulated_atomically() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            exit(&btf, Whose::Process(1))
                .iter()
                .filter(|insn| insn.code == 0xdb)
                .count(),
            2
        );
    }

    #[test]
    fn a_kernel_with_another_prototype_is_refused_rather_than_read_wrong() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            btf.tracepoint_signature("sys_enter").expect("a prototype"),
            ENTER_SIGNATURE
        );
        assert!(check(&btf, "sys_enter", &["long int"]).is_err());
    }

    #[test]
    fn every_byte_of_every_key_and_value_is_accounted_for() {
        let row_key = [
            (KEY_ADDR, 8),
            (KEY_TID, 4),
            (KEY_TGID, 4),
            (KEY_COMMAND, 4),
            (KEY_BUCKET, 4),
            (KEY_STACK, 4),
        ];
        let row_value = [
            (VALUE_COUNT, 8),
            (VALUE_TOTAL_NS, 8),
            (VALUE_FIRST_NS, 8),
            (VALUE_COMM, COMM_BYTES as i16),
        ];
        let wait_value = [
            (WAIT_TS, 8),
            (WAIT_ADDR, 8),
            (WAIT_COMMAND, 4),
            (WAIT_PAD, 4),
        ];
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
            (SCRATCH, 8, "scratch"),
            (ROWS_SLOT, 8, "row map slot"),
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
        assert!(FutexWait::decode(&[0u8; 4], &[0u8; ROW_VALUE_BYTES]).is_none());
        assert!(FutexWait::decode(&[0u8; ROW_KEY_BYTES], &[0u8; 8]).is_none());
    }

    #[test]
    fn a_command_this_build_cannot_name_is_reported_as_its_number() {
        let row = |command: u32| FutexWait {
            addr: 0x1000,
            tid: 1,
            tgid: 1,
            command,
            bucket: 2,
            stack: None,
            count: 1,
            total_ns: 1,
            first_ns: 0,
            comm: String::new(),
        };
        assert_eq!(row(FUTEX_WAIT as u32).command_name(), "wait");
        assert_eq!(row(FUTEX_WAIT_BITSET as u32).command_name(), "wait_bitset");
        assert_eq!(row(77).command_name(), "op 77");
    }

    /// The row map is found through the generation slot once per run, and the
    /// lookup and the insert both use what it found -- the property that makes
    /// the drain's swap safe; see `map::Rows`.
    #[test]
    fn the_row_map_is_taken_from_the_slot_once_per_run() {
        let Some(btf) = kernel_btf() else { return };
        let program = exit_instructions(&btf, Whose::Process(1), 3, 4, 5, 6).expect("a program");
        crate::bpf::map::assert_rows_taken_once(&program, 4);
    }
}
