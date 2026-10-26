// SPDX-License-Identifier: MIT OR GPL-2.0
//! The pieces the `sched` programs added in the sixth wave share.
//!
//! Four of them -- lock contention, futex waits, interrupt handlers and NUMA
//! balancing -- are the same shape seen four times: a pair of tracepoints that
//! open and close an interval, a per-key log2 histogram built in the kernel,
//! and a per-cpu counter of what could not be recorded. The parts that are
//! genuinely identical live here rather than four times over, because a log2
//! that disagrees with `emit::BucketLayout::Log2` in one of four copies is a
//! defect nobody would find by reading any single file.
//!
//! What is deliberately *not* here is the key layout, the filter and the
//! pairing. Those differ in each program for reasons the program explains --
//! a lock is keyed by the contending task, an interrupt by the cpu it landed
//! on -- and a generic version of them would hide exactly what a reader of
//! those files needs to see.

use std::os::fd::OwnedFd;

use super::btf::Btf;
use super::insn::{
    helper, push_task_filter, read_narrow, read_pointer, Insn, Program, Subject, TaskFields, Whose,
    BPF_DW, BPF_W, R10,
};
use super::map::{map_failure, Counters, Hash};
use super::syscall;

// The fragments these four programs share with every other family now live in
// `insn.rs`, where one copy decides what every kernel-built histogram in this
// repository does. They are re-exported here so that a `sched` program reads as
// one import, and because the seventh wave moved them rather than the sixth.
pub use super::insn::{bucket_low_ns, count_drop, push_log2_bucket, read_field, Log2Labels};

/// `BPF_F_USER_STACK`: walk the task's own stack rather than the kernel's.
///
/// The kernel side of a lock wait or a futex wait is the lock primitive, which
/// is the same handful of functions every time and says nothing about the
/// program. The user side is the call that took the lock, which is the answer
/// someone arrived with the question for. It is also the only side this build
/// can name: kernel frames would need `/proc/kallsyms`, which a normal machine
/// restricts to root, and `CAP_SYS_ADMIN` is more than the two capabilities
/// this project settled on.
pub const BPF_F_USER_STACK: i32 = 1 << 8;

/// Errno values `bpf_map_update_elem` returns, negated the way a program sees
/// them.
///
/// `-E2BIG` is a hash map with no room for a new key. `-EEXIST` is
/// `BPF_NOEXIST` refusing to overwrite an entry that is already there, which
/// for an in-flight map means the same key opened an interval that has not
/// closed. Anything else is the kernel declining the update at that instant --
/// for these programs almost always `-EBUSY`, the hash map's own bucket lock
/// already held on this cpu by an outer program.
pub const E2BIG: i32 = 7;
pub const EEXIST: i32 = 17;

/// `BPF_NOEXIST`: fail rather than overwrite.
pub const BPF_NOEXIST: i32 = 1;

/// Where aarch64 keeps the link register in the user registers a task entered
/// the kernel with: `x30` of `user_pt_regs.regs[31]`, the first member of the
/// kernel's `struct pt_regs`. The layout is the ptrace ABI's, not an internal
/// one.
#[cfg(target_arch = "aarch64")]
const PT_REGS_LR: i32 = 30 * 8;

/// Writes the current task's user link register to the stack at `slot`, or
/// zero where it is not asked for or this architecture has none.
///
/// Why a user stack needs it: the kernel's walk follows frame records, and a
/// function that sets none up -- glibc enters every blocking syscall through
/// one, its cancellation helper -- leaves its caller in the link register
/// alone, so the walk goes from the leaf to the caller's caller. Whether the
/// register *is* the caller is the leaf's unwind rule's to say, in userspace
/// (`collect::sched::LeafCallers`); a leaf that keeps a frame leaves a stale
/// value here, which splits a row by a number userspace then ignores.
///
/// `bpf_task_pt_regs` (5.15) of the current task is the user registers it
/// entered the kernel with, which is what the kernel's own user stack walk
/// starts from: the two agree about which frame is the leaf. Read with
/// `bpf_probe_read_kernel`, which writes zero where the read fails. Clobbers
/// `r0` to `r5`.
pub fn push_user_lr(program: &mut Program, slot: i16, leaf_lr: bool) {
    #[cfg(target_arch = "aarch64")]
    if leaf_lr {
        program.push(Insn::call(helper::GET_CURRENT_TASK_BTF));
        program.push(Insn::mov64_reg(1, 0));
        program.push(Insn::call(helper::TASK_PT_REGS));
        program.push(Insn::mov64_reg(3, 0));
        program.push(Insn::add64_imm(3, PT_REGS_LR));
        program.push(Insn::mov64_reg(1, R10));
        program.push(Insn::add64_imm(1, i32::from(slot)));
        program.push(Insn::mov64_imm(2, 8));
        program.push(Insn::call(helper::PROBE_READ_KERNEL));
        return;
    }
    let _ = leaf_lr;
    program.push(Insn::st_imm(BPF_DW, R10, slot, 0));
}

/// Loads a program that walks a user stack, reading the user link register
/// with it where the architecture has one ([`push_user_lr`]), and without it
/// where the kernel refuses the read (before 5.15). The second answer is why
/// it is not read, for the run to say: a blocking call made from a function
/// that sets up no frame is then missing its caller from every stack.
pub fn load_reading_user_lr<T>(
    what: &str,
    build: impl Fn(bool) -> Result<Vec<super::insn::Insn>, String>,
    load: impl Fn(&[super::insn::Insn]) -> Result<T, String>,
) -> Result<(T, Option<String>), String> {
    if !cfg!(target_arch = "aarch64") {
        return Ok((load(&build(false)?)?, None));
    }
    match load(&build(true)?) {
        Ok(program) => Ok((program, None)),
        Err(refused) => {
            let program = load(&build(false)?)?;
            Ok((
                program,
                Some(format!(
                    "this kernel would not hand the {what} program a task's user link register \
                     ({refused}), so a blocking call made from a function that sets up no frame \
                     -- glibc's syscall wrappers -- is missing its caller from every stack: the \
                     kernel's walk goes from the leaf to the caller's caller"
                )),
            ))
        }
    }
}

/// Counts a failed `bpf_map_update_elem` under the reason the kernel gave.
///
/// The reasons are different facts about the run and are answered differently:
/// a full map is a size this build chose and can raise, a refused overwrite is
/// an interval inside another interval, and a busy bucket is the kernel
/// declining a re-entrant update on a path that by its nature re-enters. A
/// loss reported under the wrong one sends a reader to change something that
/// was never the problem, which costs more than a bare number would.
///
/// Expects the failed return value still in `r0`. Each reason carries the
/// label that skips it, because a program may contain more than one of these
/// and a label is defined once.
pub fn count_update_failure(
    program: &mut Program,
    drops_fd: i32,
    drop_key: i16,
    reasons: &[(i32, u32, &'static str)],
    fallback_slot: u32,
    resume: &'static str,
) {
    for (errno, slot, next) in reasons {
        program.jump_to(next, Insn::jne_imm(0, -errno, 0));
        count_drop(program, drops_fd, drop_key, *slot, resume);
        program.jump_to(resume, Insn::ja());
        program.label(next);
    }
    count_drop(program, drops_fd, drop_key, fallback_slot, resume);
}

/// Where a name ends inside a fixed-width field.
pub fn text_of(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Where the scheduler keeps its own clock and a task's queue stamp, on this
/// kernel.
///
/// **Why the scheduler's clock and not the tracepoints'.** A wait timed from
/// `sched_wakeup` to `sched_switch` with `bpf_ktime_get_ns` is a fact about two
/// instants, and the kernel's own account of the same wait, `sched_info`, is
/// another: `run_delay` is `rq->clock` when the task arrives on the cpu less
/// `sched_info.last_queued`, the same clock when it was enqueued. The two parted
/// by about 1.5 us a wait on this machine (`docs/fixes/w18-sched.md`): a
/// program sleeping 50 us at a time was 1 630 ns runnable a wait by the
/// tracepoints and 121 ns by its own `schedstat`, and its blocked time short by
/// the same amount. Every reference this project's cases hold a decomposition
/// to is the kernel's `schedstat`, and a row that reads its own clock across a
/// boundary the kernel draws elsewhere is short of it on every wait, by an
/// amount no tolerance on a total can name.
///
/// So the boundary between blocked and runnable is read where the kernel
/// draws it: the queue wait of a block is `rq->clock - last_queued`, both read
/// at the switch that ends it, which is the arithmetic `sched_info_arrive` does
/// a few instructions later with the same two values. Both are read with the
/// run queue's lock held by this cpu -- a `sched_switch` program runs inside
/// `__schedule` -- so neither can move under the read.
///
/// The run queue is reached through the task, `task->se.cfs_rq->rq`, which is
/// the run queue of the cpu the task is on: the one switching it in or out.
/// A kernel without `CONFIG_SCHED_INFO` or `CONFIG_FAIR_GROUP_SCHED` has no
/// such fields, and the programs then keep the tracepoints' clock and say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelClock {
    /// `task_struct.se.cfs_rq`, from the start of the task.
    task_cfs_rq: i32,
    /// `cfs_rq.rq`.
    cfs_rq_rq: i32,
    /// `rq.clock`, which `rq_clock()` returns.
    rq_clock: i32,
    /// `task_struct.sched_info.last_queued`, from the start of the task.
    last_queued: i32,
    /// `task_struct.sched_info.run_delay`: every wait the kernel has closed.
    run_delay: i32,
}

impl KernelClock {
    pub fn locate(btf: &Btf) -> Result<Self, String> {
        Ok(Self {
            task_cfs_rq: btf.member_offset("task_struct", "se")?
                + btf.member_offset("sched_entity", "cfs_rq")?,
            cfs_rq_rq: btf.member_offset("cfs_rq", "rq")?,
            rq_clock: btf.member_offset("rq", "clock")?,
            last_queued: btf.member_offset("task_struct", "sched_info")?
                + btf.member_offset("sched_info", "last_queued")?,
            run_delay: btf.member_offset("task_struct", "sched_info")?
                + btf.member_offset("sched_info", "run_delay")?,
        })
    }

    /// Why a kernel has no scheduler clock to read, in a sentence a note can
    /// carry.
    pub fn missing(reason: &str) -> String {
        format!(
            "the queue wait is timed between the `sched_wakeup` and `sched_switch` tracepoints \
             rather than read from the kernel's own `sched_info`, which this kernel does not \
             expose ({reason}); it differs from the task's `schedstat` by about a microsecond \
             a wait"
        )
    }
}

/// Reads the clock of the run queue the task in `task_reg` is on into the
/// eight bytes at `into`, jumping to `fail` when a pointer on the way reads as
/// null.
///
/// Clobbers `r1` to `r5` and the eight bytes at `scratch`.
pub fn push_rq_clock(
    program: &mut Program,
    clock: &KernelClock,
    task_reg: u8,
    scratch: i16,
    into: i16,
    fail: &'static str,
) {
    read_pointer(program, scratch, 4, task_reg, clock.task_cfs_rq);
    program.jump_to(fail, Insn::jeq_imm(4, 0, 0));
    read_pointer(program, scratch, 4, 4, clock.cfs_rq_rq);
    program.jump_to(fail, Insn::jeq_imm(4, 0, 0));
    program.push(Insn::st_imm(BPF_DW, R10, into, 0));
    read_field(program, into, 8, 4, clock.rq_clock);
}

/// Reads `sched_info.last_queued` of the task in `task_reg` into the eight
/// bytes at `into`: the run queue's clock when the task joined it, or zero
/// when the kernel holds no stamp.
///
/// Clobbers `r1` to `r5`.
pub fn push_last_queued(program: &mut Program, clock: &KernelClock, task_reg: u8, into: i16) {
    program.push(Insn::st_imm(BPF_DW, R10, into, 0));
    read_field(program, into, 8, task_reg, clock.last_queued);
}

/// Reads `sched_info.run_delay` of the task in `task_reg` into the eight
/// bytes at `into`: the waits the kernel has already closed.
///
/// One wait is not always closed once. A runnable task the load balancer moves
/// to another cpu is dequeued from the first, and `sched_info_dequeue` adds the
/// part of the wait spent there to `run_delay` and clears `last_queued`; the
/// second cpu stamps it again. `rq->clock - last_queued` at the switch is then
/// only the part on the cpu that ran it. Read when the wait begins and again
/// when it ends, `run_delay`'s growth is the rest: on this machine under ten
/// agents' load, the clock and stamp alone were 17 % short of the kernel's
/// `run_delay`.
///
/// Clobbers `r1` to `r5`.
pub fn push_run_delay(program: &mut Program, clock: &KernelClock, task_reg: u8, into: i16) {
    program.push(Insn::st_imm(BPF_DW, R10, into, 0));
    read_field(program, into, 8, task_reg, clock.run_delay);
}

/// Computes a wait as the kernel's `sched_info` closes it, into `r1`: the
/// run queue's clock less the queue stamp, both already on the stack, plus
/// what `run_delay` grew by since the wait began (see [`push_run_delay`]).
/// Jumps to `unstamped` when there is no stamp, or one ahead of the clock.
/// `grown` is a label of the call site's own, past the growth.
pub fn push_kernel_wait(
    program: &mut Program,
    [now, queued, delay_then, delay_now]: [i16; 4],
    unstamped: &'static str,
    grown: &'static str,
) {
    program.push(Insn::ldx(BPF_DW, 1, R10, now));
    program.push(Insn::ldx(BPF_DW, 2, R10, queued));
    program.jump_to(unstamped, Insn::jeq_imm(2, 0, 0));
    program.jump_to(unstamped, Insn::jgt_reg(2, 1, 0));
    program.push(Insn::sub64_reg(1, 2));
    program.push(Insn::ldx(BPF_DW, 2, R10, delay_now));
    program.push(Insn::ldx(BPF_DW, 3, R10, delay_then));
    // A `run_delay` that did not grow, or that reads lower than it did (it
    // never decreases; a zero where the read failed would), adds nothing.
    program.jump_to(grown, Insn::jge_reg(3, 2, 0));
    program.push(Insn::sub64_reg(2, 3));
    program.push(Insn::add64_reg(1, 2));
    program.label(grown);
}

/// A process target and the processes it starts, followed in the kernel.
///
/// `cpu.profile` follows the processes its target forks, and the sched
/// sources compared one thread group written into the instruction stream:
/// `make`'s table had `cc1`'s cpu and none of its waits -- no `runnable` row
/// where the kernel said one compile waited 2 059 ms for its cpu
/// (`docs/open-findings.md`, the seventeenth wave, stream build).
///
/// A program on `sched_process_fork` adds a child's thread group to a map when
/// its parent's is the target's or already in the map, before the child has
/// run an instruction: the fork tracepoint fires inside `kernel_clone`, ahead
/// of `wake_up_new_task`. So a descendant is followed from its first wait, not
/// from the next drain that finds it. A second program on
/// `sched_process_free` takes a thread group out again when its leader's
/// `task_struct` is freed, which is after the last of its threads is gone, so
/// a pid the machine hands to an unrelated process later is not followed.
///
/// The root is not in the map: it is compared as a constant first, which is
/// the case of every program that starts nothing, and costs the target's own
/// events no lookup.
///
/// **Without BTF** the same maps are kept by two classic tracepoints instead
/// (`Descendants::follow_classic`): `task:task_newtask`, which fires in the
/// parent's context inside `copy_process` -- also before the child runs --
/// and carries the child's id and its `clone_flags`, so a new thread is told
/// from a new process by `CLONE_THREAD`; and `sched:sched_process_free`,
/// whose `pid` is the freed task's own id, a process's when its leader goes.
/// A classic record names a task by its id alone and never its thread group,
/// so this follower also keeps every thread of the root and of what it
/// started (`Descendants::threads_fd`), for a program that must tell whose a
/// task is from a record's `pid`.
pub struct Descendants {
    root: u32,
    followed: Hash,
    /// Processes freed since the last read of [`Descendants::parents`], with
    /// their parents: the free program moves an entry here as it deletes it.
    exited: Hash,
    /// Every thread of the root and of the processes followed, with its
    /// thread group: the classic follower's.
    threads: Option<Hash>,
    missed: Counters,
    /// Processes already under the root that the map had no room for.
    missed_at_start: u64,
    programs: FollowPrograms,
}

/// The two programs that keep the maps, and what holds them attached.
enum FollowPrograms {
    Raw {
        fork_program: OwnedFd,
        _fork_link: OwnedFd,
        free_program: OwnedFd,
        _free_link: OwnedFd,
    },
    Classic {
        newtask: super::attach::PerfAttachment,
        free: super::attach::PerfAttachment,
    },
}

/// How many processes one run follows at once beside its root.
const MAX_DESCENDANTS: u32 = 8192;
const FOLLOW_KEY: i16 = -8;
const FOLLOW_VALUE: i16 = -16;
const FOLLOW_DROP_KEY: i16 = -24;
pub(super) const FORK_SIGNATURE: [&str; 2] = ["task_struct *", "task_struct *"];
pub(super) const FREE_SIGNATURE: [&str; 1] = ["task_struct *"];
const FOLLOW_LICENSE: &str = "Dual MIT/GPL";
/// How many threads the classic follower keeps, the root's included.
const MAX_FOLLOWED_THREADS: u32 = 65536;
/// `CLONE_THREAD`: the new task joins its parent's thread group.
const CLONE_THREAD: i32 = 0x0001_0000;

/// Whose tasks a program keeps: the target as [`Whose`] says, and, for a
/// process, the map of the processes it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Followed {
    pub whose: Whose,
    /// The descendants map, for a process target.
    pub descendants: Option<i32>,
}

impl Followed {
    /// The target alone, as before the eighteenth wave: what a thread, a
    /// cgroup or the machine is, and what the instruction tests build.
    pub fn only(whose: Whose) -> Self {
        Self {
            whose,
            descendants: None,
        }
    }
}

/// Falls through when the task `subject` names is one `followed` keeps, and
/// jumps to `skip` when it is not.
///
/// For a process with descendants, the task's thread group is compared with
/// the root first and looked up in the map only when it is not the root's.
/// Clobbers `r0` to `r5` and the sixteen bytes at `scratch`, as
/// [`push_task_filter`] does. `kept` is the label the root's comparison jumps
/// to, past the lookup; each call site names its own, because a label is
/// defined once in a program.
#[allow(clippy::too_many_arguments)]
pub fn push_followed_filter(
    program: &mut Program,
    fields: &TaskFields,
    followed: &Followed,
    subject: Subject,
    tgid_offset: i32,
    scratch: i16,
    skip: &'static str,
    kept: &'static str,
) {
    let (Whose::Process(root), Some(descendants)) = (followed.whose, followed.descendants) else {
        push_task_filter(program, fields, followed.whose, subject, scratch, skip);
        return;
    };
    let slot = match subject.ids {
        Some((_, tgid_slot)) => tgid_slot,
        None => {
            read_narrow(program, scratch, 4, subject.task_reg, tgid_offset);
            scratch
        }
    };
    program.push(Insn::ldx(BPF_W, 1, R10, slot));
    program.jump_to(kept, Insn::jeq_imm(1, root as i32, 0));
    program.extend(Insn::load_map_fd(1, descendants));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, slot as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to(skip, Insn::jeq_imm(0, 0, 0));
    program.label(kept);
}

/// Loads the current task's ids into `ids_reg` and falls through when the
/// task is one `followed` keeps, or jumps to `skip`.
///
/// The form of [`push_followed_filter`] for a program that runs in its
/// subject's own context -- `sched.lock`'s and `sched.futex`'s -- and so takes
/// the subject's ids from one helper call rather than out of a `task_struct`.
/// A process with descendants is compared with its root first, as a
/// constant, and looked up in the map only when it is not the root. Clobbers
/// `r0` to `r5` and the four bytes at `scratch`; `ids_reg` should be one of
/// `r6` to `r9`, since the filter may call helpers. `kept` is the label the
/// root's comparison jumps to, one per program.
pub fn push_current_followed_filter(
    program: &mut Program,
    followed: &Followed,
    ids_reg: u8,
    scratch: i16,
    skip: &'static str,
    kept: &'static str,
) {
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::mov64_reg(ids_reg, 0));
    match (followed.whose, followed.descendants) {
        (Whose::Any, _) => {}
        // The thread group is the high half. Known before the program loads
        // and never changing, so the root's comparison is in the instruction
        // stream; the processes it starts are in the map.
        (Whose::Process(root), descendants) => {
            program.push(Insn::mov64_reg(1, ids_reg));
            program.push(Insn::rsh64_imm(1, 32));
            match descendants {
                None => {
                    program.jump_to(skip, Insn::jne_imm(1, root as i32, 0));
                }
                Some(map) => {
                    program.push(Insn::stx(BPF_W, R10, scratch, 1));
                    program.jump_to(kept, Insn::jeq_imm(1, root as i32, 0));
                    program.extend(Insn::load_map_fd(1, map));
                    program.push(Insn::mov64_reg(2, R10));
                    program.push(Insn::add64_imm(2, scratch as i32));
                    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
                    program.jump_to(skip, Insn::jeq_imm(0, 0, 0));
                    program.label(kept);
                }
            }
        }
        // The low half, compared 32 bits wide: a 64-bit compare would include
        // the thread group and never match.
        (Whose::Thread(tid), _) => {
            program.push(Insn::stx(BPF_W, R10, scratch, ids_reg));
            program.push(Insn::ldx(BPF_W, 1, R10, scratch));
            program.jump_to(skip, Insn::jne_imm(1, tid as i32, 0));
        }
        // One helper call for the cgroup; `ids_reg` survives it.
        (whose @ Whose::Cgroup(_), _) => {
            super::insn::push_current_filter(program, whose, scratch, skip)
        }
    }
}

impl Descendants {
    /// Loads the two programs for `root`, attaches them, and adds the
    /// processes `root` already has under it.
    ///
    /// Attached before the existing ones are read, so that a fork between the
    /// read and the attach is not missed: it is seen by the program, and the
    /// read finds it again, which is harmless.
    ///
    /// Through the raw tracepoints where the kernel has them, the classic ones
    /// otherwise -- no BTF, or a BTF without the tracepoints' types (5.4's,
    /// before `btf_trace_*` was published) -- and the raw refusal is the
    /// one said when neither attaches.
    pub fn follow(root: u32) -> Result<Self, String> {
        let raw = super::kernel_btf().and_then(|btf| Self::follow_raw(btf, root));
        match raw {
            Ok(descendants) => Ok(descendants),
            Err(raw) => Self::follow_classic(root).map_err(|classic| {
                format!("{raw}; and through the classic tracepoints, {classic}")
            }),
        }
    }

    /// The classic follower whatever the kernel has: what a program that
    /// tells a task's owner from a record's `pid` needs, for its threads map.
    pub fn follow_threads(root: u32) -> Result<Self, String> {
        Self::follow_classic(root)
    }

    fn maps() -> Result<(Hash, Hash, Counters), String> {
        let followed = Hash::create(4, 4, MAX_DESCENDANTS, "omn_sched_desc")
            .map_err(|error| map_failure("descendants map", error))?;
        let exited = Hash::create(4, 4, MAX_DESCENDANTS, "omn_sched_dexit")
            .map_err(|error| map_failure("exited descendants map", error))?;
        let missed = Counters::create(1, "omn_sched_dmiss")
            .map_err(|error| map_failure("descendants drop counter", error))?;
        Ok((followed, exited, missed))
    }

    fn follow_raw(btf: &Btf, root: u32) -> Result<Self, String> {
        let (followed, exited, missed) = Self::maps()?;
        let fork = fork_instructions(btf, root, followed.as_raw_fd(), missed.as_raw_fd())?;
        let free = free_instructions(btf, followed.as_raw_fd(), exited.as_raw_fd())?;
        let fork_program = load_raw(&fork, "omn_sched_fork")?;
        let fork_link = crate::bpf::program::attach_raw("sched_process_fork", &fork_program)?;
        let free_program = load_raw(&free, "omn_sched_free")?;
        let free_link = crate::bpf::program::attach_raw("sched_process_free", &free_program)?;
        let programs = FollowPrograms::Raw {
            fork_program,
            _fork_link: fork_link,
            free_program,
            _free_link: free_link,
        };
        Ok(Self::populated(
            root, followed, exited, None, missed, programs,
        ))
    }

    fn follow_classic(root: u32) -> Result<Self, String> {
        let (followed, exited, missed) = Self::maps()?;
        let threads = Hash::create(4, 4, MAX_FOLLOWED_THREADS, "omn_sched_thr")
            .map_err(|error| map_failure("followed threads map", error))?;
        let newtask = super::attach::tracepoint_layout("task", "task_newtask")?;
        let free = super::attach::tracepoint_layout("sched", "sched_process_free")?;
        let at = |layout: &super::attach::Layout, field: &str, tracepoint: &str| {
            layout
                .get(field)
                .map(|field| field.offset)
                .ok_or_else(|| format!("`{tracepoint}` has no `{field}` field"))
        };
        let newtask_fields = NewTaskFields {
            pid: at(&newtask, "pid", "task:task_newtask")?,
            clone_flags: at(&newtask, "clone_flags", "task:task_newtask")?,
        };
        let freed_pid = at(&free, "pid", "sched:sched_process_free")?;
        let maps = ClassicMaps {
            followed: followed.as_raw_fd(),
            exited: exited.as_raw_fd(),
            threads: threads.as_raw_fd(),
            missed: missed.as_raw_fd(),
        };
        let newtask_program = newtask_instructions(root, newtask_fields, maps)?;
        let free_program = classic_free_instructions(freed_pid, maps)?;
        let scope = super::attach::Scope::System;
        let newtask = super::attach::tracepoint(
            "task",
            "task_newtask",
            &newtask_program,
            "omn_sched_newt",
            scope,
        )?;
        let free = super::attach::tracepoint(
            "sched",
            "sched_process_free",
            &free_program,
            "omn_sched_free",
            scope,
        )?;
        let programs = FollowPrograms::Classic { newtask, free };
        Ok(Self::populated(
            root,
            followed,
            exited,
            Some(threads),
            missed,
            programs,
        ))
    }

    /// Adds the processes already under `root`, and their threads where the
    /// follower keeps them.
    ///
    /// The programs are attached before the existing ones are read, so that a
    /// fork between the read and the attach is not missed: it is seen by the
    /// program, and the read finds it again, which is harmless.
    fn populated(
        root: u32,
        followed: Hash,
        exited: Hash,
        threads: Option<Hash>,
        missed: Counters,
        programs: FollowPrograms,
    ) -> Self {
        let mut descendants = Self {
            root,
            followed,
            exited,
            threads,
            missed,
            missed_at_start: 0,
            programs,
        };
        for (tgid, parent) in processes_under(root) {
            if tgid != root {
                // A process already there that cannot be added is followed by
                // nobody, and is counted where a fork the map had no room for
                // is.
                if update_elem(descendants.followed.as_raw_fd(), tgid, parent).is_err() {
                    descendants.missed_at_start += 1;
                }
            }
            if let Some(threads) = &descendants.threads {
                for tid in threads_of(tgid) {
                    if update_elem(threads.as_raw_fd(), tid, tgid).is_err() {
                        descendants.missed_at_start += 1;
                    }
                }
            }
        }
        descendants
    }

    /// The map of every followed thread to its thread group, the classic
    /// follower's.
    pub fn threads_fd(&self) -> Option<i32> {
        self.threads.as_ref().map(Hash::as_raw_fd)
    }

    pub fn map_fd(&self) -> i32 {
        self.followed.as_raw_fd()
    }

    pub fn followed(&self, whose: Whose) -> Followed {
        Followed {
            whose,
            descendants: Some(self.map_fd()),
        }
    }

    /// The thread groups followed now, the root first.
    pub fn processes(&self) -> Vec<u32> {
        let mut processes = vec![self.root];
        if let Ok((keys, _)) = self.followed.keys() {
            processes.extend(
                keys.iter()
                    .filter_map(|key| key.get(..4)?.try_into().ok())
                    .map(u32::from_ne_bytes)
                    .filter(|tgid| *tgid != self.root),
            );
        }
        processes
    }

    /// The parent of every process the target started, as `(tgid, ppid)`:
    /// the ones followed now, and the ones freed since the last call, which
    /// this call forgets. The root is not here; its parent is in `/proc`.
    pub fn parents(&self) -> Vec<(u32, u32)> {
        let pair = |key: &[u8], value: &[u8]| -> Option<(u32, u32)> {
            Some((
                u32::from_ne_bytes(key.get(..4)?.try_into().ok()?),
                u32::from_ne_bytes(value.get(..4)?.try_into().ok()?),
            ))
        };
        let mut parents = Vec::new();
        if let Ok((keys, _)) = self.followed.keys() {
            for key in keys {
                if let Ok(Some(value)) = syscall::map_lookup(self.followed.as_raw_fd(), &key, 4) {
                    parents.extend(pair(&key, &value));
                }
            }
        }
        if let Ok(drained) = self.exited.drain() {
            parents.extend(
                drained
                    .entries
                    .iter()
                    .filter_map(|(key, value)| pair(key, value)),
            );
        }
        parents
    }

    /// Processes the target started that the map had no room for, so whose
    /// tasks no program here kept.
    pub fn missed(&self) -> u64 {
        self.missed.read(0) + self.missed_at_start
    }

    /// Runs of the two programs the kernel skipped, as every program here
    /// reports them.
    pub fn skipped(&self) -> u64 {
        match &self.programs {
            FollowPrograms::Raw {
                fork_program,
                free_program,
                ..
            } => crate::bpf::program::skipped([fork_program, free_program]),
            FollowPrograms::Classic { newtask, free } => {
                crate::bpf::program::skipped([newtask.program(), free.program()])
            }
        }
    }
}

fn load_raw(instructions: &[Insn], name: &str) -> Result<OwnedFd, String> {
    syscall::prog_load(
        syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
        instructions,
        name,
        FOLLOW_LICENSE,
    )
    .map_err(|error| {
        if error.is_permission() {
            super::permission_advice("loading a raw tracepoint program")
        } else {
            format!("the kernel refused the `{name}` program: {error}")
        }
    })
}

/// `sched_process_fork(parent, child)`: the child's thread group joins the map
/// when the parent's is the root's or is in it.
///
/// A new thread is a fork too, with the parent's thread group, and writes
/// nothing: the value is the parent's thread group, and a thread's would be
/// its own.
fn fork_instructions(
    btf: &Btf,
    root: u32,
    followed_fd: i32,
    missed_fd: i32,
) -> Result<Vec<Insn>, String> {
    crate::bpf::program::prototype(btf, "sched_process_fork", &[&FORK_SIGNATURE], "tasks")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    program.push(Insn::ldx(BPF_DW, 8, 6, 8));

    read_narrow(&mut program, FOLLOW_KEY, 4, 7, tgid_offset);
    program.push(Insn::ldx(BPF_W, 1, R10, FOLLOW_KEY));
    program.jump_to("parent_followed", Insn::jeq_imm(1, root as i32, 0));
    program.extend(Insn::load_map_fd(1, followed_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.label("parent_followed");

    // The value is the parent's thread group, which is the child's `ppid`
    // for as long as the map holds it (`Descendants::parents`).
    program.push(Insn::ldx(BPF_W, 1, R10, FOLLOW_KEY));
    program.push(Insn::stx(BPF_W, R10, FOLLOW_VALUE, 1));
    read_narrow(&mut program, FOLLOW_KEY, 4, 8, tgid_offset);
    // A thread of the root is the root.
    program.push(Insn::ldx(BPF_W, 1, R10, FOLLOW_KEY));
    program.jump_to("done", Insn::jeq_imm(1, root as i32, 0));
    // A new thread of a followed process is that process, and writing its
    // own thread group as its parent would overwrite the real one.
    program.push(Insn::ldx(BPF_W, 2, R10, FOLLOW_VALUE));
    program.jump_to("new_process", Insn::jne_reg(1, 2, 0));
    program.jump_to("done", Insn::ja());
    program.label("new_process");
    program.extend(Insn::load_map_fd(1, followed_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, FOLLOW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    // The map is full: this child, and everything it starts, is followed by
    // nobody, and nothing else would say so.
    count_drop(&mut program, missed_fd, FOLLOW_DROP_KEY, 0, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// `sched_process_free(task)`: a thread group leaves the map when its
/// leader's `task_struct` is freed, which is after every thread of it is.
fn free_instructions(btf: &Btf, followed_fd: i32, exited_fd: i32) -> Result<Vec<Insn>, String> {
    crate::bpf::program::prototype(btf, "sched_process_free", &[&FREE_SIGNATURE], "tasks")?;
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    read_narrow(&mut program, FOLLOW_VALUE, 4, 7, pid_offset);
    read_narrow(&mut program, FOLLOW_KEY, 4, 7, tgid_offset);
    program.push(Insn::ldx(BPF_W, 1, R10, FOLLOW_VALUE));
    program.push(Insn::ldx(BPF_W, 2, R10, FOLLOW_KEY));
    // Not the leader: the group is still alive.
    program.jump_to("done", Insn::jne_reg(1, 2, 0));
    // Its parent is kept in the exited map until the next drain reads it, so
    // a process born and gone between two drains still has its `ppid`.
    program.extend(Insn::load_map_fd(1, followed_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_W, 1, 0, 0));
    program.push(Insn::stx(BPF_W, R10, FOLLOW_VALUE, 1));
    program.extend(Insn::load_map_fd(1, exited_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, FOLLOW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.extend(Insn::load_map_fd(1, followed_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Where `task:task_newtask` keeps the new task's id and its clone flags.
#[derive(Debug, Clone, Copy)]
struct NewTaskFields {
    pid: i16,
    clone_flags: i16,
}

/// The classic follower's maps, as its programs load them.
#[derive(Debug, Clone, Copy)]
struct ClassicMaps {
    followed: i32,
    exited: i32,
    threads: i32,
    missed: i32,
}

/// `task:task_newtask`, in the parent's context: a new task whose parent's
/// thread group is the root's or followed joins the threads map, and joins
/// the processes when it is not a thread (`CLONE_THREAD`).
fn newtask_instructions(
    root: u32,
    fields: NewTaskFields,
    maps: ClassicMaps,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, FOLLOW_VALUE, 0));
    program.jump_to("parent_followed", Insn::jeq_imm(0, root as i32, 0));
    program.extend(Insn::load_map_fd(1, maps.followed));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_VALUE as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.label("parent_followed");

    // The new task's id, and whether it is a thread of its parent's group.
    program.push(Insn::ldx(BPF_W, 7, 6, fields.pid));
    program.push(Insn::stx(BPF_W, R10, FOLLOW_KEY, 7));
    program.push(Insn::ldx(BPF_DW, 8, 6, fields.clone_flags));
    // A thread's group is its parent's, already in `FOLLOW_VALUE`; a new
    // process is a group of its own.
    program.jump_to("thread_value", Insn::jset_imm(8, CLONE_THREAD, 0));
    program.push(Insn::stx(BPF_W, R10, FOLLOW_DROP_KEY, 7));
    program.jump_to("thread_update", Insn::ja());
    program.label("thread_value");
    program.push(Insn::ldx(BPF_W, 1, R10, FOLLOW_VALUE));
    program.push(Insn::stx(BPF_W, R10, FOLLOW_DROP_KEY, 1));
    program.label("thread_update");
    program.extend(Insn::load_map_fd(1, maps.threads));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, FOLLOW_DROP_KEY as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("thread_kept", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, maps.missed, FOLLOW_DROP_KEY, 0, "thread_kept");
    program.label("thread_kept");

    program.jump_to("done", Insn::jset_imm(8, CLONE_THREAD, 0));
    // The value is the parent's thread group, the child's `ppid` for as long
    // as the map holds it (`Descendants::parents`).
    program.extend(Insn::load_map_fd(1, maps.followed));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, FOLLOW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, maps.missed, FOLLOW_DROP_KEY, 0, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// `sched:sched_process_free`: the freed task leaves the threads map, and a
/// process leaves the followed ones when the task is its leader, whose id is
/// the group's. A thread that is not a leader has an id no process is keyed
/// by, so the lookup misses and nothing else changes.
fn classic_free_instructions(pid_at: i16, maps: ClassicMaps) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::ldx(BPF_W, 1, 1, pid_at));
    program.push(Insn::stx(BPF_W, R10, FOLLOW_KEY, 1));
    program.extend(Insn::load_map_fd(1, maps.threads));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    program.extend(Insn::load_map_fd(1, maps.followed));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    // Kept in the exited map until the next drain reads it, as the raw
    // follower keeps it.
    program.push(Insn::ldx(BPF_W, 1, 0, 0));
    program.push(Insn::stx(BPF_W, R10, FOLLOW_VALUE, 1));
    program.extend(Insn::load_map_fd(1, maps.exited));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, FOLLOW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.extend(Insn::load_map_fd(1, maps.followed));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, FOLLOW_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The threads of `tgid` now, by id.
fn threads_of(tgid: u32) -> Vec<u32> {
    std::fs::read_dir(format!("/proc/{tgid}/task"))
        .map(|threads| {
            threads
                .flatten()
                .filter_map(|thread| thread.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The processes under `root` now, root included, each with the process it
/// was found under (the root's is 0): each thread's
/// `/proc/<tgid>/task/<tid>/children`, walked down.
fn processes_under(root: u32) -> Vec<(u32, u32)> {
    let mut found = Vec::new();
    let mut pending = vec![(root, 0)];
    let mut seen = std::collections::BTreeSet::new();
    while let Some((tgid, parent)) = pending.pop() {
        if !seen.insert(tgid) {
            continue;
        }
        found.push((tgid, parent));
        let Ok(threads) = std::fs::read_dir(format!("/proc/{tgid}/task")) else {
            continue;
        };
        for thread in threads.flatten() {
            let children =
                std::fs::read_to_string(thread.path().join("children")).unwrap_or_default();
            pending.extend(
                children
                    .split_whitespace()
                    .filter_map(|pid| pid.parse::<u32>().ok())
                    .map(|child| (child, tgid)),
            );
        }
    }
    found
}

/// `BPF_MAP_UPDATE_ELEM` of a four-byte key with its parent as the value.
fn update_elem(map_fd: i32, tgid: u32, parent: u32) -> std::io::Result<()> {
    #[repr(C)]
    struct ElemAttr {
        map_fd: u32,
        pad: u32,
        key: u64,
        value: u64,
        flags: u64,
    }
    const BPF_MAP_UPDATE_ELEM: i64 = 2;
    let key = tgid.to_ne_bytes();
    let value = parent.to_ne_bytes();
    let mut attr = ElemAttr {
        map_fd: map_fd as u32,
        pad: 0,
        key: key.as_ptr() as u64,
        value: value.as_ptr() as u64,
        flags: 0,
    };
    // SAFETY: `attr` is the prefix of `union bpf_attr` the command reads, and
    // the key and value it points at outlive the call.
    let result = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_MAP_UPDATE_ELEM,
            &mut attr as *mut ElemAttr,
            std::mem::size_of::<ElemAttr>() as u32,
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout's own definition, written out rather than called: bucket *k*
    /// holds `[2^(k-1), 2^k)`, so the index of a value is `floor(log2(v)) + 1`.
    /// An independent expression of it is the only thing that can catch the
    /// unrolled search, or this table, being off by one.
    fn reference_index(value: u64) -> u32 {
        u64::BITS - value.leading_zeros()
    }

    /// The whole point of building a histogram in the kernel is that the emit
    /// layer can bucket the events again and land in the same places. If these
    /// two definitions ever part, every kernel-built distribution in this
    /// family is quietly reported one bucket -- a factor of two -- away from
    /// where it belongs, and nothing says so.
    #[test]
    fn a_buckets_low_bound_belongs_to_that_bucket_and_not_the_one_below() {
        for bucket in 0..48u32 {
            let low = bucket_low_ns(bucket);
            assert_eq!(
                reference_index(low),
                bucket,
                "bucket {bucket} starts at {low}, which the layout puts elsewhere"
            );
            if bucket > 0 {
                assert_eq!(
                    reference_index(low - 1),
                    bucket - 1,
                    "bucket {bucket} and the one below it overlap"
                );
            }
        }
    }

    /// Every value has to land in the bucket whose half-open range contains
    /// it, including the two ends the unrolled search treats specially: zero,
    /// which is bucket zero rather than the log2 of nothing, and one.
    #[test]
    fn every_value_falls_in_the_bucket_that_contains_it() {
        for value in [
            0u64,
            1,
            2,
            3,
            4,
            7,
            8,
            1023,
            1024,
            300_000_000,
            u64::MAX / 2,
        ] {
            let bucket = reference_index(value);
            assert!(
                bucket_low_ns(bucket) <= value,
                "{value} is below its own bucket's low bound"
            );
            assert!(
                value < bucket_low_ns(bucket + 1),
                "{value} reaches into the next bucket"
            );
        }
        assert_eq!(bucket_low_ns(0), 0);
    }

    #[test]
    fn a_name_stops_at_its_terminator_and_survives_a_full_field() {
        assert_eq!(text_of(b"eth0\0\0\0\0"), "eth0");
        assert_eq!(text_of(b"abcd"), "abcd");
        assert_eq!(text_of(b"\0rest"), "");
    }
}
