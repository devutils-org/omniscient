//! The `sched_stick_numa` tracepoint: the NUMA balancer deciding not to move a
//! task after all.
//!
//! Automatic NUMA balancing samples a task's memory accesses and tries to put
//! the task and the pages it touches on the same node. `sched_stick_numa`
//! fires on the decision *not* to move it -- the balancer found a better node
//! and left the task where it was, because moving it would have cost more than
//! the remote accesses do. A machine producing these in quantity is one whose
//! memory placement the balancer has given up on, which is a cause of slowness
//! that shows up in no cpu measurement at all: the task runs at full speed and
//! every load takes longer.
//!
//! Counted in the kernel rather than streamed, for the reason `migrate.rs`
//! gives. The key is `(task, source cpu, destination cpu)`, and both cpus are
//! kept because the pair is the decision: a task the balancer keeps wanting to
//! move to one particular node and keeps not moving is a different story from
//! one it is undecided about.
//!
//! **The node is not in the tracepoint and is not computed here.** The kernel
//! event carries cpus; a cpu belongs to a node, and that mapping is in sysfs
//! rather than in anything a program can read. It is applied on the way out,
//! in `collect/sched/numa_balance.rs`, where a file can be read.
//!
//! **This source cannot fire on a machine with one NUMA node**, which is what
//! the development machine is. There is no balancing to do when there is
//! nowhere else to be, so the scenario for it skips with that number rather
//! than asserting anything; `docs/families/sched.md` says so.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash};
use super::sched_common::{count_update_failure, read_field, text_of, E2BIG};
use super::syscall;

/// Arguments of `sched_stick_numa`, in the raw tracepoint's context.
const ARG_SRC_TASK: i16 = 0;
const ARG_SRC_CPU: i16 = 8;
const ARG_DST_CPU: i16 = 24;

/// The prototype this program is written against.
///
/// The third argument is the destination *task*, which the balancer passes as
/// null on this event -- it is a shared template with `sched_swap_numa`, where
/// there is one. Nothing here reads it, and the prototype is still checked in
/// full: a kernel that reordered these would hand this program a cpu number
/// where it expects a task.
const SIGNATURE: [&str; 4] = ["task_struct *", "int", "task_struct *", "int"];

/// How many `(task, source, destination)` triples one interval may hold.
const MAX_ROWS: u32 = 8192;

const DROP_ROWS_FULL: u32 = 0;
const DROP_ROWS_BUSY: u32 = 1;
const COUNTER_SLOTS: u32 = 2;

/// Where the map's key and value live on the program's stack.
const ROW_VALUE: i16 = -48;
const ROW_KEY: i16 = -24;
const DROP_KEY: i16 = -8;

/// The layout of a key.
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
const KEY_SRC_CPU: i16 = 8;
const KEY_DST_CPU: i16 = 12;
const ROW_KEY_BYTES: usize = 16;

/// The layout of a value.
const VALUE_COUNT: i16 = 0;
const VALUE_COMM: i16 = 8;
const COMM_BYTES: usize = 16;
const ROW_VALUE_BYTES: usize = 24;

/// `bpf_probe_read_kernel` is GPL-only, and the task's name and ids come
/// through it.
const LICENSE: &str = "GPL";

/// One `(task, source cpu, destination cpu)` triple's decisions over one
/// drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumaStick {
    pub tid: u32,
    pub tgid: u32,
    /// The cpu the task was on, which is where it stayed.
    pub src_cpu: u32,
    /// The cpu the balancer would have moved it to.
    pub dst_cpu: u32,
    pub count: u64,
    pub comm: String,
}

impl NumaStick {
    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        let word =
            |bytes: &[u8], at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        Some(Self {
            tid: word(key, KEY_TID as usize),
            tgid: word(key, KEY_TGID as usize),
            src_cpu: word(key, KEY_SRC_CPU as usize),
            dst_cpu: word(key, KEY_DST_CPU as usize),
            count: u64::from_ne_bytes(
                value[VALUE_COUNT as usize..VALUE_COUNT as usize + 8]
                    .try_into()
                    .expect("8"),
            ),
            comm: text_of(&value[VALUE_COMM as usize..VALUE_COMM as usize + COMM_BYTES]),
        })
    }
}

fn instructions(btf: &Btf, rows_fd: i32, drops_fd: i32) -> Result<Vec<Insn>, String> {
    let signature = btf.tracepoint_signature("sched_stick_numa")?;
    if signature != SIGNATURE {
        return Err(format!(
            "this kernel declares `sched_stick_numa({})`, and this program is written for \
             `sched_stick_numa({})`: it reads the task and the two cpus out of fixed slots, and \
             against another prototype those slots hold something else",
            signature.join(", "),
            SIGNATURE.join(", "),
        ));
    }
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;

    let mut program = Program::new();
    // r6 keeps the context and r7 the task that was left where it was. Both
    // survive the helper calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_SRC_TASK));

    read_field(&mut program, ROW_KEY + KEY_TID, 4, 7, pid_offset);
    read_field(&mut program, ROW_KEY + KEY_TGID, 4, 7, tgid_offset);
    // The rest of the key, so every byte of it is written: the kernel hashes
    // all sixteen, and one nobody wrote puts two identical decisions in two
    // buckets.
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_SRC_CPU));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_SRC_CPU, 1));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_DST_CPU));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_DST_CPU, 1));

    program.extend(Insn::load_map_fd(1, rows_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first_decision", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu, and the balancer runs on all of them.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.jump_to("done", Insn::ja());

    program.label("first_decision");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    // Read once, when the row is created: a task's name does not change.
    read_field(
        &mut program,
        ROW_VALUE + VALUE_COMM,
        COMM_BYTES as i32,
        7,
        comm_offset,
    );
    program.extend(Insn::load_map_fd(1, rows_fd));
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

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Decisions the map could not take, under the kernel's own reason.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NumaDrops {
    pub rows_full: u64,
    pub rows_busy: u64,
}

/// A loaded, attached program and the map it fills.
pub struct NumaProgram {
    rows: Hash,
    drops: Counters,
    program: OwnedFd,
    _link: OwnedFd,
}

impl NumaProgram {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.program])
    }

    pub fn load() -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let rows = Hash::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_numa_rows")
            .map_err(|error| map_failure("NUMA balancing map", error))?;
        let drops = Counters::create(COUNTER_SLOTS, "omn_numa_drop")
            .map_err(|error| map_failure("NUMA balancing drop counter", error))?;

        let instructions = instructions(btf, rows.as_raw_fd(), drops.as_raw_fd())?;
        let program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &instructions,
            "omn_numa_stick",
            LICENSE,
        )
        .map_err(|error| {
            if error.is_permission() {
                super::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the NUMA balancing program: {error}")
            }
        })?;
        let link = syscall::raw_tracepoint_open("sched_stick_numa", program.as_raw_fd()).map_err(
            |error| format!("cannot attach to the `sched_stick_numa` tracepoint: {error}"),
        )?;

        Ok(Self {
            rows,
            drops,
            program,
            _link: link,
        })
    }

    pub fn drain(&self) -> Result<(Vec<NumaStick>, super::DrainLosses), String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the NUMA balancing counts: {error}"))?;
        let mut losses = super::DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match NumaStick::decode(key, value) {
                Some(row) => rows.push(row),
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    pub fn dropped(&self) -> NumaDrops {
        NumaDrops {
            rows_full: self.drops.read(DROP_ROWS_FULL),
            rows_busy: self.drops.read(DROP_ROWS_BUSY),
        }
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

    /// A decision is a count, not a duration, so this program takes no
    /// timestamp -- and the clock invariant is checked by its absence. A
    /// reading nothing subtracts from is a number on an event that means
    /// nothing, and worse, invites someone to subtract two of them later.
    #[test]
    fn the_program_reads_no_clock_because_it_measures_no_duration() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        let called: Vec<i32> = instructions(&btf, 3, 4)
            .expect("a program")
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect();
        assert!(!called.contains(&helper::KTIME_GET_NS));
        assert!(!called.contains(&KTIME_GET_BOOT_NS));
    }

    /// The balancer runs on every cpu and the row is one allocation they
    /// share, so the increment cannot be a plain load-add-store.
    #[test]
    fn the_count_is_accumulated_atomically() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            instructions(&btf, 3, 4)
                .expect("a program")
                .iter()
                .filter(|insn| insn.code == 0xdb)
                .count(),
            1
        );
    }

    /// The third argument is the destination task, which this event passes as
    /// null. Reading it would dereference nothing and produce a plausible
    /// number; the slot is never touched, and this is what says so.
    #[test]
    fn the_null_task_in_the_third_slot_is_never_read() {
        let Some(btf) = kernel_btf() else { return };
        const ARG_DST_TASK: i16 = 16;
        assert!(
            !instructions(&btf, 3, 4)
                .expect("a program")
                .iter()
                .any(|insn| insn.off == ARG_DST_TASK && (insn.regs >> 4) == 6),
            "the destination task slot is read, and on this event it is null"
        );
    }

    #[test]
    fn a_kernel_with_another_prototype_is_refused_rather_than_read_wrong() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            btf.tracepoint_signature("sched_stick_numa")
                .expect("a prototype"),
            SIGNATURE
        );
        assert!(instructions(&btf, 3, 4).is_ok());
    }

    #[test]
    fn every_byte_of_the_key_and_value_is_accounted_for() {
        let key = [
            (KEY_TID, 4),
            (KEY_TGID, 4),
            (KEY_SRC_CPU, 4),
            (KEY_DST_CPU, 4),
        ];
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
        assert!(NumaStick::decode(&[0u8; 4], &[0u8; ROW_VALUE_BYTES]).is_none());
        assert!(NumaStick::decode(&[0u8; ROW_KEY_BYTES], &[0u8; 8]).is_none());
    }

    #[test]
    fn both_cpus_survive_the_round_trip() {
        let mut key = [0u8; ROW_KEY_BYTES];
        key[KEY_SRC_CPU as usize..KEY_SRC_CPU as usize + 4].copy_from_slice(&3u32.to_ne_bytes());
        key[KEY_DST_CPU as usize..KEY_DST_CPU as usize + 4].copy_from_slice(&19u32.to_ne_bytes());
        let mut value = [0u8; ROW_VALUE_BYTES];
        value[..8].copy_from_slice(&11u64.to_ne_bytes());
        value[VALUE_COMM as usize..VALUE_COMM as usize + 6].copy_from_slice(b"walker");
        let row = NumaStick::decode(&key, &value).expect("a full record");
        assert_eq!(
            (row.src_cpu, row.dst_cpu, row.count, row.comm.as_str()),
            (3, 19, 11, "walker")
        );
    }
}
