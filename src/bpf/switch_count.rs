//! The `sched_switch` program that counts: how often a task left the cpu, and
//! whether it went or was taken off.
//!
//! `sched.switches` is served first by `pmu:sw`, an inherited software counter
//! that is exact and costs nothing, and that follows threads and cpus but not
//! a cgroup. This program is the second backend of the chain
//! (`DECISIONS.md`, the fourteenth wave): it serves every target, a cgroup
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
use super::insn::{
    count_drop, helper, push_task_filter, read_field, Insn, Program, Subject, TaskFields, Whose,
    BPF_DW, BPF_W, R10,
};
use super::map::{map_failure, row_map, select_rows, Counters, Rows};
use super::syscall;

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

/// `bpf_probe_read_kernel` is GPL-only, and the task's name and ids come
/// through it.
const LICENSE: &str = "GPL";

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

fn instructions(btf: &Btf, whose: Whose, rows_fd: i32, drops_fd: i32) -> Result<Vec<Insn>, String> {
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
    push_task_filter(
        &mut program,
        &fields,
        whose,
        Subject::with_ids(7, ROW_KEY + KEY_TID, ROW_KEY + KEY_TGID),
        FILTER_SCRATCH,
        "done",
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

    // The row map of this interval, taken once; see `map::Rows`.
    select_rows(&mut program, rows_fd, ROWS_SLOT, "no_rows");
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
    // The map is full: this switch will never be counted.
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

/// A loaded, attached switch-counting program and the map it fills.
pub struct SwitchCountProgram {
    rows: Rows,
    drops: Counters,
    program: OwnedFd,
    _link: OwnedFd,
}

impl SwitchCountProgram {
    /// Runs the kernel skipped because this program was already running on
    /// that cpu; see `MigrationProgram::skipped`.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.program])
    }

    pub fn load(whose: Whose) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_swc_rows")
            .map_err(|error| map_failure("switch count map", error))?;
        let drops = Counters::create(1, "omn_swc_drop")
            .map_err(|error| map_failure("switch count drop counter", error))?;

        let instructions = instructions(btf, whose, rows.as_raw_fd(), drops.as_raw_fd())?;
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
        let link = syscall::raw_tracepoint_open("sched_switch", program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sched_switch` tracepoint: {error}"))?;

        Ok(Self {
            rows,
            drops,
            program,
            _link: link,
        })
    }

    /// Takes every row the kernel has counted since the last drain.
    pub fn drain(&self) -> Result<(Vec<SwitchCount>, super::DrainLosses), String> {
        let drained = self
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
                Some(row) => rows.push(row),
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    /// Switches the program could not count because the map was full.
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

    #[test]
    fn the_count_is_accumulated_atomically() {
        let Some(btf) = kernel_btf() else { return };
        let program = instructions(&btf, Whose::Any, 3, 4).expect("a program");
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
        let called: Vec<i32> = instructions(&btf, Whose::Any, 3, 4)
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
        assert!(!instructions(&btf, Whose::Any, 3, 4)
            .expect("a program")
            .iter()
            .any(|insn| insn.imm == 4242));
        assert_eq!(
            instructions(&btf, Whose::Process(4242), 3, 4)
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
        let program = instructions(&btf, Whose::Any, 3, 4).expect("a program");
        crate::bpf::map::assert_rows_taken_once(&program, 3);
    }
}
