// SPDX-License-Identifier: MIT OR GPL-2.0
//! The `sched_stat_runtime` program behind `cpu.oncpu_time` through
//! `bpf:raw_tp`: each thread's on-cpu time, to the nanosecond the scheduler
//! itself keeps.
//!
//! **The scheduler's own increments, not a clock read here.** Every task
//! carries `se.sum_exec_runtime`, the nanoseconds it has run -- the first field
//! of `/proc/<tid>/schedstat` -- and the scheduler adds to it in one place,
//! `update_curr`, which hands the same increment to the `sched_stat_runtime`
//! tracepoint. It runs on every scheduler tick of a running task and when the
//! task leaves the cpu. Adding the increments up per thread gives what
//! `schedstat` moved by over the same window, with nothing to round, and
//! leaves out the interrupt time a kernel built with `IRQ_TIME_ACCOUNTING`
//! leaves out. No `bpf_ktime_get_ns` is called.
//!
//! Why not `sched_switch`, which a first version used: a slice is only known
//! when it ends, so a thread spinning alone on its core -- the busiest thread
//! of the machine -- reported nothing for as long as it was not preempted.
//! The tick's increments credit a running thread within one tick, and the
//! window's edges cost at most a tick each: the first increment after the
//! attach can reach back to the tick before it, and the last tick before the
//! end is the last one credited.
//!
//! Counted in the kernel, one row per thread per drain, like
//! `sched.switches`: a stream of increments costs more than the answer is
//! worth.

use std::os::fd::OwnedFd;

use super::btf::Btf;
use super::insn::{
    count_drop, helper, read_field, Insn, Program, Subject, TaskFields, Whose, BPF_DW, BPF_W, R10,
};
use super::map::{map_failure, row_map, select_rows, Counters, Rows};
use super::sched_common::{push_followed_filter, Descendants, Followed};
use super::syscall;
use crate::bpf::program::LICENSE;

/// Arguments of `sched_stat_runtime`, in the raw tracepoint's context: one
/// 64-bit slot each.
const ARG_TASK: i16 = 0;
const ARG_RUNTIME: i16 = 8;

/// The prototype the slots above are read from. Kernels before 6.8 passed a
/// third argument, the new `vruntime`, which this program would not read; it
/// is refused there rather than assumed, like every prototype here.
pub(crate) const SIGNATURE: [&str; 2] = ["task_struct *", "u64"];

/// How many threads one drain interval may hold rows for.
const MAX_ROWS: u32 = 16384;

/// The one slot of the drop counter.
const DROP_ROWS: u32 = 0;

/// Where things live on the program's stack.
const ROW_KEY: i16 = -8;
const DROP_KEY: i16 = -12;
/// The row map this run looked up through the generation slot.
const ROWS_SLOT: i16 = -24;
const ROW_VALUE: i16 = -56;
/// Sixteen bytes for the target comparison, `insn::push_task_filter`.
const FILTER_SCRATCH: i16 = -72;

/// The layout of a key: the thread, and the process it belongs to.
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
const ROW_KEY_BYTES: usize = 8;

/// The layout of a value.
const VALUE_NS: i16 = 0;
const VALUE_UPDATES: i16 = 8;
const VALUE_COMM: i16 = 16;
const COMM_BYTES: usize = 16;
const ROW_VALUE_BYTES: usize = 32;

/// One thread's on-cpu time over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnCpu {
    pub tid: u32,
    pub tgid: u32,
    /// Nanoseconds on a cpu, by the scheduler's own account.
    pub ns: u64,
    /// How many times the scheduler brought the thread's total up to date:
    /// once a tick while it runs, and once when it leaves the cpu.
    pub updates: u64,
    pub comm: String,
}

impl OnCpu {
    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        let word = |at: i16| u32::from_ne_bytes(key[at as usize..][..4].try_into().expect("4"));
        let dword = |at: i16| u64::from_ne_bytes(value[at as usize..][..8].try_into().expect("8"));
        let comm = &value[VALUE_COMM as usize..VALUE_COMM as usize + COMM_BYTES];
        let end = comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(comm.len());
        Some(Self {
            tid: word(KEY_TID),
            tgid: word(KEY_TGID),
            ns: dword(VALUE_NS),
            updates: dword(VALUE_UPDATES),
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
    let signature = btf.tracepoint_signature("sched_stat_runtime")?;
    if signature != SIGNATURE {
        return Err(format!(
            "this kernel declares `sched_stat_runtime({})`, and this program is written for \
             `sched_stat_runtime({})`: it reads the task and the increment out of fixed slots, \
             and against another prototype those slots hold something else",
            signature.join(", "),
            SIGNATURE.join(", "),
        ));
    }
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;
    let fields = TaskFields::locate(btf, followed.whose)?;

    let mut program = Program::new();
    // r6 keeps the context, r7 the task and r8 the increment; all three
    // survive the helper calls.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_TASK));
    program.push(Insn::ldx(BPF_DW, 8, 6, ARG_RUNTIME));

    read_field(&mut program, ROW_KEY + KEY_TID, 4, 7, pid_offset);
    read_field(&mut program, ROW_KEY + KEY_TGID, 4, 7, tgid_offset);
    // The idle task is not a thread anyone runs; the scheduler does not
    // account it here, and a zero is refused in case one ever is.
    program.push(Insn::ldx(BPF_W, 1, R10, ROW_KEY + KEY_TID));
    program.jump_to("done", Insn::jeq_imm(1, 0, 0));
    push_followed_filter(
        &mut program,
        &fields,
        &followed,
        Subject::with_ids(7, ROW_KEY + KEY_TID, ROW_KEY + KEY_TGID),
        tgid_offset,
        FILTER_SCRATCH,
        "done",
        "kept",
    );

    select_rows(&mut program, rows_fd, ROWS_SLOT, "no_rows");
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first_update", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu: a thread's two increments in a row
    // can come from two cpus.
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_NS, 8));
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_UPDATES, 1));
    program.jump_to("done", Insn::ja());

    program.label("first_update");
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_NS, 8));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_UPDATES, 1));
    read_field(
        &mut program,
        ROW_VALUE + VALUE_COMM,
        COMM_BYTES as i32,
        7,
        comm_offset,
    );
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    // The row map is full: this increment is in no row.
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_ROWS, "done");
    program.jump_to("done", Insn::ja());

    // The slot held no map, which it always does; counted rather than assumed.
    program.label("no_rows");
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_ROWS, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// A loaded, attached on-cpu program and the map it fills.
pub struct OnCpuProgram {
    /// The processes a process target started, followed in the kernel.
    descendants: Option<Descendants>,
    rows: Rows,
    drops: Counters,
    program: OwnedFd,
    _link: OwnedFd,
}

impl OnCpuProgram {
    pub fn load(whose: Whose) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_oncpu_rows")
            .map_err(|error| map_failure("on-cpu row map", error))?;
        let drops = Counters::create(1, "omn_oncpu_drop")
            .map_err(|error| map_failure("on-cpu drop counter", error))?;
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
            "omn_oncpu",
            LICENSE,
        )
        .map_err(|error| {
            if error.is_permission() {
                super::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the on-cpu program: {error}")
            }
        })?;
        let link = crate::bpf::program::attach_raw("sched_stat_runtime", &program)?;
        Ok(Self {
            descendants,
            rows,
            drops,
            program,
            _link: link,
        })
    }

    /// Takes every row the kernel has added up since the last drain.
    pub fn drain(&self) -> Result<(Vec<OnCpu>, super::DrainLosses), String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the on-cpu rows: {error}"))?;
        let mut losses = super::DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match OnCpu::decode(key, value) {
                Some(row) => rows.push(row),
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    /// Runs the kernel skipped because this program was already running on
    /// that cpu.
    pub fn skipped(&self) -> u64 {
        crate::bpf::program::skipped([&self.program])
            + self.descendants.as_ref().map_or(0, Descendants::skipped)
    }

    /// Processes the target started that no program here followed.
    pub fn unfollowed(&self) -> u64 {
        self.descendants.as_ref().map_or(0, Descendants::missed)
    }

    /// Increments that could not be put in a row, the row map full.
    pub fn dropped(&self) -> u64 {
        self.drops.read(DROP_ROWS)
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

    fn program(btf: &Btf, whose: Whose) -> Vec<Insn> {
        instructions(btf, Followed::only(whose), 3, 4).expect("a program")
    }

    /// The increment and the count, each added atomically: a thread's two
    /// increments in a row can come from two cpus.
    #[test]
    fn the_time_and_the_updates_are_accumulated_atomically() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            program(&btf, Whose::Any)
                .iter()
                .filter(|insn| insn.code == 0xdb)
                .count(),
            2
        );
    }

    /// The length is the scheduler's own difference: no clock is read.
    #[test]
    fn the_program_reads_no_clock_because_the_scheduler_keeps_the_time() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        let called: Vec<i32> = program(&btf, Whose::Any)
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect();
        assert!(!called.contains(&helper::KTIME_GET_NS));
        assert!(!called.contains(&KTIME_GET_BOOT_NS));
    }

    #[test]
    fn the_filter_is_present_only_when_a_process_was_named() {
        let Some(btf) = kernel_btf() else { return };
        assert!(!program(&btf, Whose::Any)
            .iter()
            .any(|insn| insn.imm == 4242));
        assert_eq!(
            program(&btf, Whose::Process(4242))
                .iter()
                .filter(|insn| insn.imm == 4242)
                .count(),
            1
        );
    }

    #[test]
    fn the_running_kernel_declares_the_prototype_this_program_reads() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            btf.tracepoint_signature("sched_stat_runtime")
                .expect("a prototype"),
            SIGNATURE
        );
        crate::bpf::mem_program::assert_catalog_lists(
            "cpu.oncpu_time",
            "sched_stat_runtime",
            &[&SIGNATURE],
        );
    }

    #[test]
    fn every_byte_of_a_key_and_a_value_is_accounted_for() {
        let key = [(KEY_TID, 4), (KEY_TGID, 4)];
        let value = [
            (VALUE_NS, 8),
            (VALUE_UPDATES, 8),
            (VALUE_COMM, COMM_BYTES as i16),
        ];
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
            (ROW_KEY, ROW_KEY_BYTES as i16, "key"),
            (DROP_KEY, 4, "drop key"),
            (ROWS_SLOT, 8, "row map slot"),
            (ROW_VALUE, ROW_VALUE_BYTES as i16, "value"),
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
    fn a_record_reads_back_its_task_its_time_and_its_name() {
        assert!(OnCpu::decode(&[0u8; 4], &[0u8; ROW_VALUE_BYTES]).is_none());
        let mut key = [0u8; ROW_KEY_BYTES];
        key[..4].copy_from_slice(&7u32.to_ne_bytes());
        key[4..8].copy_from_slice(&5u32.to_ne_bytes());
        let mut value = [0u8; ROW_VALUE_BYTES];
        value[..8].copy_from_slice(&1_234_567u64.to_ne_bytes());
        value[8..16].copy_from_slice(&3u64.to_ne_bytes());
        value[16..20].copy_from_slice(b"spin");
        let row = OnCpu::decode(&key, &value).expect("a full record");
        assert_eq!(
            (row.tid, row.tgid, row.ns, row.updates, row.comm.as_str()),
            (7, 5, 1_234_567, 3, "spin")
        );
    }

    #[test]
    fn the_row_map_is_taken_from_the_slot_once_per_run() {
        let Some(btf) = kernel_btf() else { return };
        crate::bpf::map::assert_rows_taken_once(&program(&btf, Whose::Any), 3);
    }
}
