// SPDX-License-Identifier: MIT OR GPL-2.0
//! The `sched_switch` program that counts: how often a task left the cpu, and
//! whether it went or was taken off.
//!
//! `sched.switches` is served first by `pmu:sw`, an inherited software counter
//! that is exact and costs nothing, and that follows threads and cpus but not
//! a cgroup. This program is the second backend of the chain
//! (`docs/DECISIONS.md`, the fourteenth wave): it serves every target, a cgroup
//! included, in `count` mode, and it is the one that can tell the two kinds of
//! switch apart, which the entry's caveat says only a `sched_switch` backend
//! can.
//!
//! **Voluntary is the kernel's own rule, not an approximation of it.**
//! `__schedule` credits a switch to `nvcsw` when it was not a preemption and
//! the task's state was not `TASK_RUNNING` -- it blocked, or asked to -- and to
//! `nivcsw` otherwise, and it hands the tracepoint the same two facts: the
//! `preempt` flag and the `prev_state` it read. So the rows here add up to the
//! `voluntary_ctxt_switches` and `nonvoluntary_ctxt_switches` of
//! `/proc/<pid>/status`, over the window, and that is what the scenario holds
//! them to.
//!
//! Counted in the kernel, one row per `(task, kind)` per drain, like
//! `sched.migrations`: a switch-heavy machine switches hundreds of thousands
//! of times a second, and a stream of them costs more than the answer is
//! worth. The trade is that there is no per-switch event, which is why `trace`
//! stays `pmu:sw`'s: the `woke` join from `sched.wakeup` needs the switch
//! itself.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::context::Record;
use super::insn::{
    count_drop, helper, read_field, Insn, Program, Subject, TaskFields, Whose, BPF_DW, BPF_W, R10,
};
use super::map::{map_failure, row_map, select_rows, Counters, Rows};
use super::proc_events::Attached;
use crate::bpf::program::LICENSE;

/// `BPF_MAP_TYPE_LRU_HASH`: a wakeup stamp whose task never ran under the
/// program is evicted rather than kept forever.
const BPF_MAP_TYPE_LRU_HASH: u32 = 9;

/// How many woken, not yet running tasks are stamped at once.
const MAX_WOKEN: u32 = 16384;

/// `sched_wakeup(task_struct *)`, and `sched_wakeup_new` the same.
const WAKEUP_SIGNATURE: [&str; 1] = ["task_struct *"];
use super::ring::{ring_bytes, RingBuffer};
use super::sched_common::{
    push_followed_filter, push_last_queued, push_rq_clock, push_run_delay, Descendants, Followed,
    KernelClock,
};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// Arguments of `sched_switch`, in the raw tracepoint's context: one 64-bit
/// slot each, in the order the kernel declares them.
const ARG_PREEMPT: i16 = 0;
const ARG_PREV: i16 = 8;
const ARG_PREV_STATE: i16 = 24;

/// The prototype this program is written against, checked against the running
/// kernel before a single instruction is built. The fourth argument arrived in
/// 5.18, below this project's floor; without it the rule above cannot be
/// applied, and a program that read slot 24 there would read nothing.
pub(crate) const SIGNATURE: [&str; 4] = ["bool", "task_struct *", "task_struct *", "unsigned int"];

/// How many `(task, kind)` rows one drain interval may hold.
const MAX_ROWS: u32 = 16384;

/// The one slot of the drop counter.
const DROP_ROWS: u32 = 0;

/// Where the map's key and value live on the program's stack.
const ROW_VALUE: i16 = -48;
const ROW_KEY: i16 = -24;
const DROP_KEY: i16 = -12;
/// The row map this run looked up through the generation slot; see
/// `map::select_rows`.
const ROWS_SLOT: i16 = -56;
/// Sixteen bytes for the target comparison, `insn::push_task_filter`.
const FILTER_SCRATCH: i16 = -72;

/// The layout of a key.
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
/// 1 for a voluntary switch, 0 for an involuntary one.
const KEY_VOLUNTARY: i16 = 8;
const ROW_KEY_BYTES: usize = 12;

/// The layout of a value.
const VALUE_COUNT: i16 = 0;
const VALUE_COMM: i16 = 8;
const COMM_BYTES: usize = 16;
const ROW_VALUE_BYTES: usize = 24;

/// One task's switches of one kind over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchCount {
    pub tid: u32,
    pub tgid: u32,
    /// The kernel's `nvcsw` rule: not a preemption, and the task was not
    /// runnable when it left.
    pub voluntary: bool,
    pub count: u64,
    pub comm: String,
}

impl SwitchCount {
    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        let word =
            |bytes: &[u8], at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let comm = &value[VALUE_COMM as usize..VALUE_COMM as usize + COMM_BYTES];
        let end = comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(comm.len());
        Some(Self {
            tid: word(key, KEY_TID as usize),
            tgid: word(key, KEY_TGID as usize),
            voluntary: word(key, KEY_VOLUNTARY as usize) != 0,
            count: u64::from_ne_bytes(
                value[VALUE_COUNT as usize..VALUE_COUNT as usize + 8]
                    .try_into()
                    .expect("8"),
            ),
            comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
        })
    }
}

fn instructions(
    btf: &Btf,
    followed: Followed,
    rows_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let whose = followed.whose;
    let signature = btf.tracepoint_signature("sched_switch")?;
    if signature != SIGNATURE {
        return Err(format!(
            "this kernel declares `sched_switch({})`, and this program is written for \
             `sched_switch({})`: it reads the preemption flag, the task that left and its state \
             out of fixed slots, and against another prototype those slots hold something else",
            signature.join(", "),
            SIGNATURE.join(", "),
        ));
    }
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;
    let fields = TaskFields::locate(btf, whose)?;

    let mut program = Program::new();
    // r6 keeps the context, r7 the task leaving the cpu and r8 the kind of
    // switch. All three survive the helper calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_PREV));

    read_field(&mut program, ROW_KEY + KEY_TID, 4, 7, pid_offset);
    read_field(&mut program, ROW_KEY + KEY_TGID, 4, 7, tgid_offset);

    // A run observes one process, one thread or one cgroup, compared on the
    // task that is leaving: the switch is its own.
    push_followed_filter(
        &mut program,
        &fields,
        &followed,
        Subject::with_ids(7, ROW_KEY + KEY_TID, ROW_KEY + KEY_TGID),
        tgid_offset,
        FILTER_SCRATCH,
        "done",
        "switched_kept",
    );

    // The kind, by `__schedule`'s own rule: voluntary when it was not a
    // preemption and the state was not `TASK_RUNNING` (zero). The kernel
    // widens each argument to its 64-bit slot with a cast, so the `bool` and
    // the `unsigned int` arrive zero-extended; and a raw tracepoint's context
    // may only be read a whole slot at a time.
    program.push(Insn::mov64_imm(8, 0));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_PREEMPT));
    program.jump_to("kind", Insn::jne_imm(1, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_PREV_STATE));
    program.jump_to("kind", Insn::jeq_imm(1, 0, 0));
    program.push(Insn::mov64_imm(8, 1));
    program.label("kind");
    // Every byte of the key written: the kernel hashes all twelve.
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_VOLUNTARY, 8));

    count_row(&mut program, rows_fd, drops_fd, |program| {
        read_field(
            program,
            ROW_VALUE + VALUE_COMM,
            COMM_BYTES as i32,
            7,
            comm_offset,
        );
        Ok(())
    })?;

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Counts one switch in the row the key on the stack names, creating it,
/// with the comm `write_comm` writes, when it is the first; exits through
/// `done`, which this emits.
fn count_row(
    program: &mut Program,
    rows_fd: i32,
    drops_fd: i32,
    write_comm: impl FnOnce(&mut Program) -> Result<(), String>,
) -> Result<(), String> {
    // The row map of this interval, taken once; see `map::Rows`.
    select_rows(program, rows_fd, ROWS_SLOT, "no_rows");
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first_switch", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu: a task's two switches in a row can
    // be on two cpus, and a plain load-add-store loses one of them.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.jump_to("done", Insn::ja());

    program.label("first_switch");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    write_comm(program)?;
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    // The map is full: this switch will never be counted.
    count_drop(program, drops_fd, DROP_KEY, DROP_ROWS, "done");
    program.jump_to("done", Insn::ja());

    // The slot held no map, which it always does; counted rather than assumed.
    program.label("no_rows");
    count_drop(program, drops_fd, DROP_KEY, DROP_ROWS, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    Ok(())
}

/// `sched_switch`'s classic `prev_state` for a preempted task:
/// `TASK_REPORT_MAX`, where the low byte is the state's report letter.
const REPORTED_PREEMPTED: i32 = 0x100;
const REPORTED_STATES: i32 = 0xff;

/// The classic program on `sched:sched_switch` (`bpf:tp`), for a kernel
/// without BTF: the leaving task by its id and comm, its kind by the
/// report-format `prev_state` -- voluntary when it is not the preempted
/// value and names a state other than running, `__schedule`'s rule -- and its
/// thread group the followed threads' map's, or `/proc`'s at the drain.
fn classic_instructions(
    record: &Record,
    whose: Whose,
    threads: Option<i32>,
    rows_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_KEY, 0));
    program.push(Insn::st_imm(BPF_W, R10, ROW_KEY + KEY_VOLUNTARY, 0));
    record.load(&mut program, 1, 6, "prev_pid", FILTER_SCRATCH)?;
    // The idle task leaves the cpu too, and is no task's switch.
    program.jump_to("done", Insn::jeq_imm(1, 0, 0));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TID, 1));
    match (whose, threads) {
        (Whose::Thread(tid), _) => {
            program.jump_to("done", Insn::jne_imm(1, tid as i32, 0));
        }
        (_, Some(threads)) => {
            program.extend(Insn::load_map_fd(1, threads));
            program.push(Insn::mov64_reg(2, R10));
            program.push(Insn::add64_imm(2, (ROW_KEY + KEY_TID) as i32));
            program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
            program.jump_to("done", Insn::jeq_imm(0, 0, 0));
            program.push(Insn::ldx(BPF_W, 1, 0, 0));
            program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TGID, 1));
        }
        (Whose::Any, None) => {}
        (other, None) => {
            return Err(format!(
                "the classic `sched_switch` counting program cannot keep {other:?}: its record \
                 names a task by its id alone"
            ))
        }
    }
    record.load(&mut program, 1, 6, "prev_state", FILTER_SCRATCH)?;
    program.jump_to("kind", Insn::jset_imm(1, REPORTED_PREEMPTED, 0));
    program.push(Insn::and64_imm(1, REPORTED_STATES));
    program.jump_to("kind", Insn::jeq_imm(1, 0, 0));
    program.push(Insn::st_imm(BPF_W, R10, ROW_KEY + KEY_VOLUNTARY, 1));
    program.label("kind");
    count_row(&mut program, rows_fd, drops_fd, |program| {
        record.copy(program, 6, "prev_comm", ROW_VALUE + VALUE_COMM, COMM_BYTES)
    })?;
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// A loaded, attached `sched_switch` program: the one that counts and the
/// map it fills, or the one that streams each switch (`load_stream`).
pub struct SwitchCountProgram(Loaded);

enum Loaded {
    Counting(Counting),
    Stream(SwitchStreamProgram),
}

struct Counting {
    /// The processes a process target started, followed in the kernel.
    descendants: Option<Descendants>,
    rows: Rows,
    drops: Counters,
    attached: Attached,
    /// Whether the program is on the classic tracepoint, whose rows are
    /// given their thread group at the drain.
    classic: bool,
}

impl SwitchCountProgram {
    /// Runs the kernel skipped because this program was already running on
    /// that cpu; see `MigrationProgram::skipped`.
    pub fn skipped(&self) -> u64 {
        match &self.0 {
            Loaded::Counting(counting) => {
                crate::bpf::program::skipped(counting.attached.program())
                    + counting
                        .descendants
                        .as_ref()
                        .map_or(0, Descendants::skipped)
            }
            Loaded::Stream(stream) => stream.skipped(),
        }
    }

    /// Processes the target started that no program here followed, because
    /// the map of them was full.
    pub fn unfollowed(&self) -> u64 {
        match &self.0 {
            Loaded::Counting(counting) => {
                counting.descendants.as_ref().map_or(0, Descendants::missed)
            }
            Loaded::Stream(stream) => stream.unfollowed(),
        }
    }

    /// The per-switch program instead of the counting one: a ring record per
    /// switch either end of which is the target's.
    pub fn load_stream(whose: Whose, ring_bytes: usize) -> Result<Self, String> {
        SwitchStreamProgram::load(whose, ring_bytes).map(|stream| Self(Loaded::Stream(stream)))
    }

    /// Hands every switch the per-switch program published to `handle`;
    /// nothing for the counting program.
    pub fn drain_switches(&mut self, handle: impl FnMut(Switch), undecoded: &mut u64) {
        if let Loaded::Stream(stream) = &mut self.0 {
            stream.drain(handle, undecoded);
        }
    }

    /// The parents of the processes the target started.
    pub fn parents(&self) -> Vec<(u32, u32)> {
        match &self.0 {
            Loaded::Counting(counting) => counting
                .descendants
                .as_ref()
                .map_or_else(Vec::new, Descendants::parents),
            Loaded::Stream(stream) => stream.parents(),
        }
    }

    pub fn load(whose: Whose) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_swc_rows")
            .map_err(|error| map_failure("switch count map", error))?;
        let drops = Counters::create(1, "omn_swc_drop")
            .map_err(|error| map_failure("switch count drop counter", error))?;

        let descendants = match whose {
            Whose::Process(root) => Some(Descendants::follow(root)?),
            _ => None,
        };
        let followed = descendants
            .as_ref()
            .map_or(Followed::only(whose), |descendants| {
                descendants.followed(whose)
            });
        let instructions = instructions(btf, followed, rows.as_raw_fd(), drops.as_raw_fd())?;
        let program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &instructions,
            "omn_switch_cnt",
            LICENSE,
        )
        .map_err(|error| {
            if error.is_permission() {
                super::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the switch-counting program: {error}")
            }
        })?;
        let link = crate::bpf::program::attach_raw("sched_switch", &program)?;

        Ok(Self(Loaded::Counting(Counting {
            descendants,
            rows,
            drops,
            attached: Attached::Raw {
                program,
                _link: link,
            },
            classic: false,
        })))
    }

    /// The counting program on the classic tracepoint (`bpf:tp`), for a
    /// kernel without BTF. A cgroup cannot be kept: the record names a task
    /// by its id alone.
    pub fn load_classic(whose: Whose) -> Result<Self, String> {
        if let Whose::Cgroup(_) = whose {
            return Err(
                "`sched.switches` through the classic tracepoint cannot keep a cgroup: the task \
                 leaving the cpu is named by its id alone in `sched_switch`'s record, and the \
                 cgroup it is in is nowhere in it"
                    .to_owned(),
            );
        }
        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_swc_rows")
            .map_err(|error| map_failure("switch count map", error))?;
        let drops = Counters::create(1, "omn_swc_drop")
            .map_err(|error| map_failure("switch count drop counter", error))?;
        let descendants = match whose {
            Whose::Process(root) => Some(Descendants::follow_threads(root)?),
            _ => None,
        };
        let record = Record::of("sched", "sched_switch")?;
        let instructions = classic_instructions(
            &record,
            whose,
            descendants.as_ref().and_then(Descendants::threads_fd),
            rows.as_raw_fd(),
            drops.as_raw_fd(),
        )?;
        let attached = Attached::classic("sched", "sched_switch", &instructions, "omn_switch_cnt")?;
        Ok(Self(Loaded::Counting(Counting {
            descendants,
            rows,
            drops,
            attached,
            classic: true,
        })))
    }

    /// Takes every row the kernel has counted since the last drain.
    pub fn drain(&self) -> Result<(Vec<SwitchCount>, super::DrainLosses), String> {
        let Loaded::Counting(counting) = &self.0 else {
            return Ok((Vec::new(), super::DrainLosses::default()));
        };
        let drained = counting
            .rows
            .drain()
            .map_err(|error| format!("cannot read the switch counts: {error}"))?;
        let mut losses = super::DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match SwitchCount::decode(key, value) {
                Some(mut row) => {
                    if counting.classic && row.tgid == 0 {
                        row.tgid = super::proc_events::group_of(row.tid).unwrap_or(0);
                    }
                    rows.push(row)
                }
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    /// Switches the program could not count because the map was full, or
    /// publish because the ring was.
    pub fn dropped(&self) -> u64 {
        match &self.0 {
            Loaded::Counting(counting) => counting.drops.read(DROP_ROWS),
            Loaded::Stream(stream) => stream.dropped(),
        }
    }
}

/// One context switch, as the per-switch program writes it and userspace
/// reads it: `sched.slices`' raw material.
///
/// `repr(C)` on both sides of the ring is the contract: the program writes
/// these fields at these offsets and `from_bytes` reads them back.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switch {
    /// `bpf_ktime_get_ns`, `CLOCK_MONOTONIC`.
    pub ts_ns: u64,
    pub cpu: u32,
    pub prev_tid: u32,
    pub prev_pid: u32,
    pub next_tid: u32,
    pub next_pid: u32,
    /// The state the task leaving the cpu was in, as the tracepoint gives it:
    /// zero is `TASK_RUNNING`, a preemption or a yield.
    pub prev_state: u32,
    /// The tracepoint's `preempt` flag.
    pub preempt: u32,
    /// Which end the target is: 1 the task coming in, 2 the task leaving;
    /// and [`QUEUE_STAMPED`] when `queue_ns` was read from a stamp.
    pub matched: u32,
    /// How long the task leaving ran, by the run queue's clock:
    /// `rq->clock - sched_info.last_arrival`, or 0 with no stamp.
    pub run_ns: u64,
    /// How long the task coming in waited on the queue, by the same clock:
    /// `rq->clock - sched_info.last_queued`, or 0 with no stamp, which
    /// `matched` tells from a real zero by [`QUEUE_STAMPED`].
    pub queue_ns: u64,
    prev_comm: [u8; 16],
    next_comm: [u8; 16],
    /// `sched_info.run_delay` of the task leaving: every wait the kernel had
    /// closed for it, read as its time off begins.
    pub prev_delay: u64,
    /// The same of the task coming in, read before `sched_info_arrive` closes
    /// this wait: what grew since it left is the part of the wait spent on a
    /// cpu it was moved away from, which `queue_ns` alone does not see.
    pub next_delay: u64,
    /// How long the task coming in waited since the tracepoints saw it become
    /// runnable -- its `sched_wakeup`, or its preemption -- which
    /// [`WAKE_STAMPED`] tells from no stamp: `perf sched timehist`'s `sch
    /// delay`, timed where timehist times it.
    pub wake_ns: u64,
}

const SWITCH_BYTES: usize = 112;

/// The bit of [`Switch::matched`] set when the task coming in carried a
/// wakeup (or preemption) stamp, so `wake_ns` is a wait.
const WAKE_STAMPED: u32 = 8;

/// The bit of [`Switch::matched`] set when the task coming in held a queue
/// stamp no later than its run queue's clock, so `queue_ns` is a wait and not
/// the absence of one.
const QUEUE_STAMPED: u32 = 4;

impl Switch {
    pub fn prev_comm(&self) -> String {
        super::sched_common::text_of(&self.prev_comm)
    }

    pub fn next_comm(&self) -> String {
        super::sched_common::text_of(&self.next_comm)
    }

    /// How long the task coming in waited on the queue, or `None` when it
    /// held no stamp to tell: not the same as a zero wait.
    pub fn queue(&self) -> Option<u64> {
        (self.matched & QUEUE_STAMPED != 0).then_some(self.queue_ns)
    }

    /// How long the task coming in waited from its wakeup by the
    /// tracepoints, or `None` when the run saw it become runnable nowhere.
    pub fn wake(&self) -> Option<u64> {
        (self.matched & WAKE_STAMPED != 0).then_some(self.wake_ns)
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < SWITCH_BYTES {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        let mut prev_comm = [0u8; 16];
        prev_comm.copy_from_slice(&bytes[56..72]);
        let mut next_comm = [0u8; 16];
        next_comm.copy_from_slice(&bytes[72..88]);
        Some(Self {
            ts_ns: long(0),
            cpu: word(8),
            prev_tid: word(12),
            prev_pid: word(16),
            next_tid: word(20),
            next_pid: word(24),
            prev_state: word(28),
            preempt: word(32),
            matched: word(36),
            run_ns: long(40),
            queue_ns: long(48),
            prev_comm,
            next_comm,
            prev_delay: long(88),
            next_delay: long(96),
            wake_ns: long(104),
        })
    }
}

/// Where the per-switch program keeps the record on its stack.
const SW_EVENT: i16 = -(SWITCH_BYTES as i16);
const SW_CPU: i16 = SW_EVENT + 8;
const SW_PREV_TID: i16 = SW_EVENT + 12;
const SW_PREV_PID: i16 = SW_EVENT + 16;
const SW_NEXT_TID: i16 = SW_EVENT + 20;
const SW_NEXT_PID: i16 = SW_EVENT + 24;
const SW_STATE: i16 = SW_EVENT + 28;
const SW_PREEMPT: i16 = SW_EVENT + 32;
const SW_MATCHED: i16 = SW_EVENT + 36;
const SW_RUN: i16 = SW_EVENT + 40;
const SW_QUEUE: i16 = SW_EVENT + 48;
const SW_PREV_COMM: i16 = SW_EVENT + 56;
const SW_NEXT_COMM: i16 = SW_EVENT + 72;
const SW_PREV_DELAY: i16 = SW_EVENT + 88;
const SW_NEXT_DELAY: i16 = SW_EVENT + 96;
const SW_WAKE: i16 = SW_EVENT + 104;
const SW_NOW: i16 = SW_EVENT - 8;
const SW_STAMP: i16 = SW_NOW - 8;
const SW_CLOCK_SCRATCH: i16 = SW_STAMP - 8;
const SW_DROP_KEY: i16 = SW_CLOCK_SCRATCH - 8;
const SW_FILTER_SCRATCH: i16 = SW_DROP_KEY - 16;
const SW_WAKE_KEY: i16 = SW_FILTER_SCRATCH - 8;
const SW_WAKE_VALUE: i16 = SW_WAKE_KEY - 8;
const ARG_NEXT: i16 = 16;

/// The per-switch program: one ring record per switch either end of which
/// the target is, with the leaving task's run and the coming task's queue
/// wait by the kernel's own `sched_info` stamps, read where
/// `sched_info_switch` reads them a few instructions later.
fn stream_instructions(
    btf: &Btf,
    followed: Followed,
    ring_fd: i32,
    drops_fd: i32,
    woken_fd: i32,
) -> Result<Vec<Insn>, String> {
    let whose = followed.whose;
    let signature = btf.tracepoint_signature("sched_switch")?;
    if signature != SIGNATURE {
        return Err(format!(
            "this kernel declares `sched_switch({})`, and the per-switch program is written for \
             `sched_switch({})`",
            signature.join(", "),
            SIGNATURE.join(", "),
        ));
    }
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;
    let last_arrival = btf.member_offset("task_struct", "sched_info")?
        + btf.member_offset("sched_info", "last_arrival")?;
    let clock = KernelClock::locate(btf)?;
    let fields = TaskFields::locate(btf, whose)?;

    let mut program = Program::new();
    // r6 the context, r7 the task leaving, r8 the task coming in.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_PREV));
    program.push(Insn::ldx(BPF_DW, 8, 6, ARG_NEXT));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, SW_EVENT, 0));
    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, SW_CPU, 0));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_PREEMPT));
    program.push(Insn::stx(BPF_W, R10, SW_PREEMPT, 1));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_PREV_STATE));
    program.push(Insn::stx(BPF_W, R10, SW_STATE, 1));
    for slot in [SW_RUN, SW_QUEUE, SW_WAKE] {
        program.push(Insn::st_imm(BPF_DW, R10, slot, 0));
    }
    for (into, size, task, offset) in [
        (SW_PREV_TID, 4, 7, pid_offset),
        (SW_PREV_PID, 4, 7, tgid_offset),
        (SW_PREV_COMM, 16, 7, comm_offset),
        (SW_NEXT_TID, 4, 8, pid_offset),
        (SW_NEXT_PID, 4, 8, tgid_offset),
        (SW_NEXT_COMM, 16, 8, comm_offset),
    ] {
        program.push(Insn::mov64_reg(1, R10));
        program.push(Insn::add64_imm(1, into as i32));
        program.push(Insn::mov64_imm(2, size));
        program.push(Insn::mov64_reg(3, task));
        program.push(Insn::add64_imm(3, offset));
        program.push(Insn::call(helper::PROBE_READ_KERNEL));
    }

    // Which end is the target's, both asked: a switch between two of its
    // own tasks is both ends of one slice.
    if whose == Whose::Any {
        program.push(Insn::st_imm(BPF_W, R10, SW_MATCHED, 3));
    } else {
        program.push(Insn::st_imm(BPF_W, R10, SW_MATCHED, 0));
        push_followed_filter(
            &mut program,
            &fields,
            &followed,
            Subject::with_ids(8, SW_NEXT_TID, SW_NEXT_PID),
            tgid_offset,
            SW_FILTER_SCRATCH,
            "next_not_followed",
            "next_kept",
        );
        program.push(Insn::st_imm(BPF_W, R10, SW_MATCHED, 1));
        program.label("next_not_followed");
        push_followed_filter(
            &mut program,
            &fields,
            &followed,
            Subject::with_ids(7, SW_PREV_TID, SW_PREV_PID),
            tgid_offset,
            SW_FILTER_SCRATCH,
            "prev_not_followed",
            "prev_kept",
        );
        program.push(Insn::ldx(BPF_W, 1, R10, SW_MATCHED));
        program.push(Insn::or64_imm(1, 2));
        program.push(Insn::stx(BPF_W, R10, SW_MATCHED, 1));
        program.label("prev_not_followed");
        program.push(Insn::ldx(BPF_W, 1, R10, SW_MATCHED));
        program.jump_to("done", Insn::jeq_imm(1, 0, 0));
    }

    // Both ends' closed waits, before anything below can jump to the emit.
    push_run_delay(&mut program, &clock, 7, SW_PREV_DELAY);
    push_run_delay(&mut program, &clock, 8, SW_NEXT_DELAY);

    // The run queue's clock, and each end's stamp against it. A stamp of
    // zero, or one ahead of the clock, is no stamp, and its field stays 0.
    // Reached through a task's `se.cfs_rq`, which the idle task has none of:
    // the task coming in first, since a switch out of idle is the common case
    // of a wakeup, and the task leaving when the one coming in is idle.
    push_rq_clock(
        &mut program,
        &clock,
        8,
        SW_CLOCK_SCRATCH,
        SW_NOW,
        "clock_by_prev",
    );
    program.jump_to("have_clock", Insn::ja());
    program.label("clock_by_prev");
    push_rq_clock(&mut program, &clock, 7, SW_CLOCK_SCRATCH, SW_NOW, "emit");
    program.label("have_clock");
    program.push(Insn::st_imm(BPF_DW, R10, SW_STAMP, 0));
    read_field(&mut program, SW_STAMP, 8, 7, last_arrival);
    for (into, next) in [(SW_RUN, "queued"), (SW_QUEUE, "emit")] {
        if into == SW_QUEUE {
            push_last_queued(&mut program, &clock, 8, SW_STAMP);
        }
        program.push(Insn::ldx(BPF_DW, 1, R10, SW_NOW));
        program.push(Insn::ldx(BPF_DW, 2, R10, SW_STAMP));
        program.jump_to(next, Insn::jeq_imm(2, 0, 0));
        program.jump_to(next, Insn::jgt_reg(2, 1, 0));
        program.push(Insn::sub64_reg(1, 2));
        program.push(Insn::stx(BPF_DW, R10, into, 1));
        if into == SW_RUN {
            program.label("queued");
        } else {
            // A real zero wait and no stamp both leave 0 in the field: the
            // flag is what tells them apart.
            program.push(Insn::ldx(BPF_W, 1, R10, SW_MATCHED));
            program.push(Insn::or64_imm(1, QUEUE_STAMPED as i32));
            program.push(Insn::stx(BPF_W, R10, SW_MATCHED, 1));
        }
    }

    program.label("emit");
    // The tracepoints' wait of the task coming in, when it is the target's:
    // from the stamp its wakeup (or its preemption) left, to now.
    program.push(Insn::ldx(BPF_W, 1, R10, SW_MATCHED));
    program.push(Insn::and64_imm(1, 1));
    program.jump_to("wake_done", Insn::jeq_imm(1, 0, 0));
    program.push(Insn::st_imm(BPF_DW, R10, SW_WAKE_KEY, 0));
    program.push(Insn::ldx(BPF_W, 1, R10, SW_NEXT_TID));
    program.push(Insn::stx(BPF_W, R10, SW_WAKE_KEY, 1));
    program.extend(Insn::load_map_fd(1, woken_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, SW_WAKE_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("wake_done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 2, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, R10, SW_EVENT));
    // A stamp later than now is not this wait's; it is dropped unread.
    program.jump_to("wake_delete", Insn::jgt_reg(2, 1, 0));
    program.push(Insn::sub64_reg(1, 2));
    program.push(Insn::stx(BPF_DW, R10, SW_WAKE, 1));
    program.push(Insn::ldx(BPF_W, 1, R10, SW_MATCHED));
    program.push(Insn::or64_imm(1, WAKE_STAMPED as i32));
    program.push(Insn::stx(BPF_W, R10, SW_MATCHED, 1));
    program.label("wake_delete");
    program.extend(Insn::load_map_fd(1, woken_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, SW_WAKE_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    program.label("wake_done");
    // A target's task leaving still runnable is preempted, and its wait for
    // the cpu begins now, as `perf sched timehist` times it.
    program.push(Insn::ldx(BPF_W, 1, R10, SW_MATCHED));
    program.push(Insn::and64_imm(1, 2));
    program.jump_to("stamp_done", Insn::jeq_imm(1, 0, 0));
    program.push(Insn::ldx(BPF_W, 1, R10, SW_STATE));
    program.jump_to("stamp_done", Insn::jne_imm(1, 0, 0));
    program.push(Insn::st_imm(BPF_DW, R10, SW_WAKE_KEY, 0));
    program.push(Insn::ldx(BPF_W, 1, R10, SW_PREV_TID));
    program.jump_to("stamp_done", Insn::jeq_imm(1, 0, 0));
    program.push(Insn::stx(BPF_W, R10, SW_WAKE_KEY, 1));
    program.push(Insn::ldx(BPF_DW, 1, R10, SW_EVENT));
    program.push(Insn::stx(BPF_DW, R10, SW_WAKE_VALUE, 1));
    program.extend(Insn::load_map_fd(1, woken_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, SW_WAKE_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, SW_WAKE_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.label("stamp_done");

    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, SW_EVENT as i32));
    program.push(Insn::mov64_imm(3, SWITCH_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    // A full ring: the switch is gone, and counted.
    count_drop(&mut program, drops_fd, SW_DROP_KEY, DROP_ROWS, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `sched_wakeup` and `sched_wakeup_new`: a target's task
/// became runnable, and the moment is stamped for the switch that runs it.
fn wakeup_instructions(btf: &Btf, followed: Followed, woken_fd: i32) -> Result<Vec<Insn>, String> {
    for tracepoint in ["sched_wakeup", "sched_wakeup_new"] {
        let signature = btf.tracepoint_signature(tracepoint)?;
        if signature != WAKEUP_SIGNATURE {
            return Err(format!(
                "this kernel declares `{tracepoint}({})`, and the wakeup stamp is written for \
                 `{tracepoint}({})`",
                signature.join(", "),
                WAKEUP_SIGNATURE.join(", "),
            ));
        }
    }
    const KEY: i16 = -8;
    const VALUE: i16 = -16;
    const SCRATCH: i16 = -32;
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let fields = TaskFields::locate(btf, followed.whose)?;
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    program.push(Insn::st_imm(BPF_DW, R10, KEY, 0));
    read_field(&mut program, KEY, 4, 7, pid_offset);
    // Thread zero is every cpu's idle task, never a woken one.
    program.push(Insn::ldx(BPF_W, 1, R10, KEY));
    program.jump_to("done", Insn::jeq_imm(1, 0, 0));
    push_followed_filter(
        &mut program,
        &fields,
        &followed,
        Subject::task(7),
        tgid_offset,
        SCRATCH,
        "done",
        "woken_kept",
    );
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, VALUE, 0));
    program.extend(Insn::load_map_fd(1, woken_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// A loaded, attached per-switch program and the ring it writes.
struct SwitchStreamProgram {
    descendants: Option<Descendants>,
    ring: RingBuffer,
    drops: Counters,
    program: OwnedFd,
    _link: OwnedFd,
    /// The wakeup stamps and the program writing them, attached to both
    /// wakeup tracepoints.
    _woken: OwnedFd,
    wakeup_program: OwnedFd,
    _wakeup_links: [OwnedFd; 2],
}

impl SwitchStreamProgram {
    pub fn load(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let bytes = ring_bytes(requested_ring_bytes);
        let ring_map =
            syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_switches")
                .map_err(|error| map_failure("switch ring buffer", error))?;
        let drops = Counters::create(1, "omn_sws_drop")
            .map_err(|error| map_failure("switch stream drop counter", error))?;
        let descendants = match whose {
            Whose::Process(root) => Some(Descendants::follow(root)?),
            _ => None,
        };
        let followed = descendants
            .as_ref()
            .map_or(Followed::only(whose), |descendants| {
                descendants.followed(whose)
            });
        let woken = syscall::map_create(BPF_MAP_TYPE_LRU_HASH, 4, 8, MAX_WOKEN, "omn_sws_woken")
            .map_err(|error| map_failure("wakeup stamp map", error))?;
        let instructions = stream_instructions(
            btf,
            followed,
            ring_map.as_raw_fd(),
            drops.as_raw_fd(),
            woken.as_raw_fd(),
        )?;
        let wakeup = wakeup_instructions(btf, followed, woken.as_raw_fd())?;
        let wakeup_program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &wakeup,
            "omn_sws_wake",
            LICENSE,
        )
        .map_err(|error| {
            if error.is_permission() {
                super::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the wakeup stamp program: {error}")
            }
        })?;
        let program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &instructions,
            "omn_switch_str",
            LICENSE,
        )
        .map_err(|error| {
            if error.is_permission() {
                super::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the per-switch program: {error}")
            }
        })?;
        // The wakeups first, so a switch never looks for a stamp nobody could
        // have written yet.
        let wakeup_link =
            |tracepoint: &str| crate::bpf::program::attach_raw(tracepoint, &wakeup_program);
        let wakeup_links = [
            wakeup_link("sched_wakeup")?,
            wakeup_link("sched_wakeup_new")?,
        ];
        let link = crate::bpf::program::attach_raw("sched_switch", &program)?;
        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the switch ring buffer: {error}"))?;
        Ok(Self {
            descendants,
            ring,
            drops,
            program,
            _link: link,
            _woken: woken,
            wakeup_program,
            _wakeup_links: wakeup_links,
        })
    }

    /// Hands every complete record to `handle`, and counts the ones of the
    /// wrong size rather than reinterpreting them.
    pub fn drain(&mut self, mut handle: impl FnMut(Switch), undecoded: &mut u64) {
        self.ring.drain(|bytes| match Switch::from_bytes(bytes) {
            Some(switch) => handle(switch),
            None => *undecoded += 1,
        });
    }

    /// Switches the program could not publish because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.drops.read(DROP_ROWS)
    }

    /// Runs the kernel skipped; see `SwitchCountProgram::skipped`.
    pub fn skipped(&self) -> u64 {
        crate::bpf::program::skipped([&self.program, &self.wakeup_program])
            + self.descendants.as_ref().map_or(0, Descendants::skipped)
    }

    /// Processes the target started that no program here followed.
    pub fn unfollowed(&self) -> u64 {
        self.descendants.as_ref().map_or(0, Descendants::missed)
    }

    /// The parents of the processes the target started.
    pub fn parents(&self) -> Vec<(u32, u32)> {
        self.descendants
            .as_ref()
            .map_or_else(Vec::new, Descendants::parents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-switch program and the wakeup stamp build against this kernel;
    /// the switch looks a stamp up and deletes it, and the wakeup program
    /// compares the target before it stamps.
    /// The classic counting program reads 4.19's `sched_switch` record and
    /// tells the two kinds apart by its report-format `prev_state`.
    #[test]
    fn the_classic_counting_program_reads_the_leaving_task_and_its_kind() {
        let record = Record::from_layout(
            "sched:sched_switch",
            super::super::attach::parse_layout(
                "\tfield:char prev_comm[16];\toffset:8;\tsize:16;\tsigned:1;\n\
                 \tfield:pid_t prev_pid;\toffset:24;\tsize:4;\tsigned:1;\n\
                 \tfield:int prev_prio;\toffset:28;\tsize:4;\tsigned:1;\n\
                 \tfield:long prev_state;\toffset:32;\tsize:8;\tsigned:1;\n\
                 \tfield:char next_comm[16];\toffset:40;\tsize:16;\tsigned:1;\n\
                 \tfield:pid_t next_pid;\toffset:56;\tsize:4;\tsigned:1;\n\
                 \tfield:int next_prio;\toffset:60;\tsize:4;\tsigned:1;\n",
            ),
        );
        for (whose, threads) in [
            (Whose::Any, None),
            (Whose::Process(42), Some(9)),
            (Whose::Thread(42), None),
        ] {
            let program = classic_instructions(&record, whose, threads, 3, 4)
                .unwrap_or_else(|error| panic!("{whose:?}: {error}"));
            assert!(
                program.contains(&Insn::ldx(BPF_W, 1, 6, 24)),
                "the leaving task"
            );
            assert!(program.contains(&Insn::ldx(BPF_DW, 1, 6, 32)), "its state");
            assert!(
                program
                    .iter()
                    .any(|insn| insn.code == 0x45 && insn.imm == REPORTED_PREEMPTED),
                "a preemption is told apart"
            );
        }
    }

    #[test]
    fn the_wakeup_stamp_and_its_reader_build_against_this_kernel() {
        let Ok(btf) = crate::bpf::kernel_btf() else {
            eprintln!("skipped: no kernel BTF");
            return;
        };
        let stream = stream_instructions(btf, Followed::only(Whose::Any), 3, 4, 5)
            .expect("the per-switch program");
        let calls = |program: &[Insn], helper: i32| {
            program
                .iter()
                .filter(|insn| insn.code == 0x85 && insn.imm == helper)
                .count()
        };
        assert!(calls(&stream, helper::MAP_LOOKUP_ELEM) >= 1);
        assert_eq!(calls(&stream, helper::MAP_DELETE_ELEM), 1);
        let wakeup = wakeup_instructions(btf, Followed::only(Whose::Process(4242)), 5)
            .expect("the wakeup program");
        let compared = wakeup
            .iter()
            .position(|insn| insn.imm == 4242)
            .expect("the comparison");
        let stamped = wakeup
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::MAP_UPDATE_ELEM)
            .expect("the stamp");
        assert!(compared < stamped);
    }

    /// The program writes the record at stack offsets and `from_bytes`
    /// reads it at struct offsets; nothing else connects the two.
    #[test]
    fn the_switch_record_matches_what_the_program_writes() {
        use std::mem::offset_of;
        assert_eq!(std::mem::size_of::<Switch>(), SWITCH_BYTES);
        for (stack, field) in [
            (SW_CPU, offset_of!(Switch, cpu)),
            (SW_PREV_TID, offset_of!(Switch, prev_tid)),
            (SW_PREV_PID, offset_of!(Switch, prev_pid)),
            (SW_NEXT_TID, offset_of!(Switch, next_tid)),
            (SW_NEXT_PID, offset_of!(Switch, next_pid)),
            (SW_STATE, offset_of!(Switch, prev_state)),
            (SW_PREEMPT, offset_of!(Switch, preempt)),
            (SW_MATCHED, offset_of!(Switch, matched)),
            (SW_RUN, offset_of!(Switch, run_ns)),
            (SW_QUEUE, offset_of!(Switch, queue_ns)),
            (SW_PREV_COMM, offset_of!(Switch, prev_comm)),
            (SW_NEXT_COMM, offset_of!(Switch, next_comm)),
            (SW_PREV_DELAY, offset_of!(Switch, prev_delay)),
            (SW_NEXT_DELAY, offset_of!(Switch, next_delay)),
            (SW_WAKE, offset_of!(Switch, wake_ns)),
        ] {
            assert_eq!((stack - SW_EVENT) as usize, field);
        }
        assert!(Switch::from_bytes(&[0u8; 16]).is_none());
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

    #[test]
    fn the_count_is_accumulated_atomically() {
        let Some(btf) = kernel_btf() else { return };
        let program = instructions(&btf, Followed::only(Whose::Any), 3, 4).expect("a program");
        assert_eq!(
            program.iter().filter(|insn| insn.code == 0xdb).count(),
            1,
            "the switch count, atomically"
        );
    }

    /// A count is not a duration: the program reads no clock at all.
    #[test]
    fn the_program_reads_no_clock_because_it_measures_no_duration() {
        let Some(btf) = kernel_btf() else { return };
        let called: Vec<i32> = instructions(&btf, Followed::only(Whose::Any), 3, 4)
            .expect("a program")
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect();
        const KTIME_GET_BOOT_NS: i32 = 125;
        assert!(!called.contains(&helper::KTIME_GET_NS));
        assert!(!called.contains(&KTIME_GET_BOOT_NS));
    }

    #[test]
    fn the_filter_is_present_only_when_a_process_was_named() {
        let Some(btf) = kernel_btf() else { return };
        assert!(!instructions(&btf, Followed::only(Whose::Any), 3, 4)
            .expect("a program")
            .iter()
            .any(|insn| insn.imm == 4242));
        assert_eq!(
            instructions(&btf, Followed::only(Whose::Process(4242)), 3, 4)
                .expect("a program")
                .iter()
                .filter(|insn| insn.imm == 4242)
                .count(),
            1
        );
    }

    /// The prototype, read back from the running kernel, and held to the
    /// catalog's line: the probe answers from the line, the attach from here.
    #[test]
    fn the_running_kernel_and_the_catalog_declare_the_prototype_this_program_reads() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            btf.tracepoint_signature("sched_switch")
                .expect("a prototype"),
            SIGNATURE
        );
        crate::bpf::mem_program::assert_catalog_lists(
            "sched.switches",
            "sched_switch",
            &[&SIGNATURE],
        );
    }

    #[test]
    fn every_byte_of_a_key_and_a_value_is_accounted_for() {
        let key = [(KEY_TID, 4), (KEY_TGID, 4), (KEY_VOLUNTARY, 4)];
        let value = [(VALUE_COUNT, 8), (VALUE_COMM, COMM_BYTES as i16)];
        for (fields, bytes, what) in [
            (&key[..], ROW_KEY_BYTES, "the key"),
            (&value[..], ROW_VALUE_BYTES, "the value"),
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
            (ROW_VALUE, ROW_VALUE_BYTES as i16, "value"),
            (ROW_KEY, ROW_KEY_BYTES as i16, "key"),
            (DROP_KEY, 4, "drop key"),
            (ROWS_SLOT, 8, "row map slot"),
            (FILTER_SCRATCH, 16, "filter scratch"),
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
    fn a_record_reads_back_its_task_its_kind_and_its_name() {
        assert!(SwitchCount::decode(&[0u8; 4], &[0u8; ROW_VALUE_BYTES]).is_none());
        let mut key = [0u8; ROW_KEY_BYTES];
        key[..4].copy_from_slice(&7u32.to_ne_bytes());
        key[4..8].copy_from_slice(&5u32.to_ne_bytes());
        key[8..12].copy_from_slice(&1u32.to_ne_bytes());
        let mut value = [0u8; ROW_VALUE_BYTES];
        value[..8].copy_from_slice(&9u64.to_ne_bytes());
        value[8..13].copy_from_slice(b"sleep");
        let row = SwitchCount::decode(&key, &value).expect("a full record");
        assert_eq!(
            (
                row.tid,
                row.tgid,
                row.voluntary,
                row.count,
                row.comm.as_str()
            ),
            (7, 5, true, 9, "sleep")
        );
    }

    #[test]
    fn the_row_map_is_taken_from_the_slot_once_per_run() {
        let Some(btf) = kernel_btf() else { return };
        let program = instructions(&btf, Followed::only(Whose::Any), 3, 4).expect("a program");
        crate::bpf::map::assert_rows_taken_once(&program, 3);
    }
}
