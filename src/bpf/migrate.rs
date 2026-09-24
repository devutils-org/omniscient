//! The `sched_migrate_task` program: a task moved from one cpu to another.
//!
//! A migration costs what the task left behind -- its cache footprint on the
//! old cpu, and on a NUMA machine possibly the locality of its memory. The
//! count alone does not say how much that cost, and nothing can; what it says
//! is how often it happened and to whom, which is the question a reader
//! actually arrives with when a program is slower than its cpu time explains.
//!
//! Counted in the kernel rather than streamed, for the same reason
//! `sched.offcpu` is: on a machine with more runnable tasks than cpus the
//! scheduler migrates constantly, and a measurement whose cost grows with the
//! load balancer's enthusiasm is one that changes the thing it measures. The
//! key is `(task, destination cpu)`, which keeps the one thing a count would
//! otherwise throw away -- a task bouncing between two cpus and a task walking
//! across all of them are the same number and different problems.
//!
//! **The destination is in the tracepoint; the origin is not.** The raw
//! tracepoint is handed the task and the cpu it is going to, and the cpu it is
//! leaving is not among its arguments -- the classic tracepoint's `orig_cpu`
//! field is computed by the trace handler from the task's own state, which is
//! already gone by the time a program could read it here. So an event says
//! where a task arrived, never where it came from, and it says so rather than
//! reporting the cpu the program happened to run on, which is neither.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, read_field, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash};
use super::syscall;

/// Arguments of `sched_migrate_task`, in the raw tracepoint's context.
const ARG_TASK: i16 = 0;
const ARG_DEST_CPU: i16 = 8;

/// The prototype this program is written against, checked against the running
/// kernel before a single instruction is built.
const SIGNATURE: [&str; 2] = ["task_struct *", "int"];

/// How many `(task, destination)` pairs one drain interval may hold.
const MAX_ROWS: u32 = 16384;

/// The one slot of the drop counter.
const DROP_ROWS: u32 = 0;

/// Where the map's key and value live on the program's stack.
const ROW_VALUE: i16 = -48;
const ROW_KEY: i16 = -24;
const DROP_KEY: i16 = -12;

/// The layout of a key.
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
/// The cpu the task is going to. In the key because a task pinned to two cpus
/// and a task wandering over sixteen are the same total and different stories.
const KEY_DEST_CPU: i16 = 8;
const ROW_KEY_BYTES: usize = 12;

/// The layout of a value.
const VALUE_COUNT: i16 = 0;
const VALUE_COMM: i16 = 8;
const COMM_BYTES: usize = 16;
const ROW_VALUE_BYTES: usize = 24;

/// `bpf_probe_read_kernel` is GPL-only, and the task's name and ids come
/// through it.
const LICENSE: &str = "GPL";

/// One `(task, destination cpu)` pair's migrations over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub tid: u32,
    pub tgid: u32,
    /// The cpu the task was moved *to*. There is no "from" here; see the
    /// module's own note on why.
    pub dest_cpu: u32,
    pub count: u64,
    pub comm: String,
}

impl Migration {
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
            dest_cpu: word(key, KEY_DEST_CPU as usize),
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
    target_tgid: Option<u32>,
    rows_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let signature = btf.tracepoint_signature("sched_migrate_task")?;
    if signature != SIGNATURE {
        return Err(format!(
            "this kernel declares `sched_migrate_task({})`, and this program is written for \
             `sched_migrate_task({})`: it reads the task and the destination cpu out of fixed \
             slots, and against another prototype those slots hold something else",
            signature.join(", "),
            SIGNATURE.join(", "),
        ));
    }
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;

    let mut program = Program::new();
    // r6 keeps the context, r7 the task being moved and r8 the cpu it is going
    // to. All three survive the helper calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_TASK));
    program.push(Insn::ldx(BPF_DW, 8, 6, ARG_DEST_CPU));

    read_field(&mut program, ROW_KEY + KEY_TID, 4, 7, pid_offset);
    read_field(&mut program, ROW_KEY + KEY_TGID, 4, 7, tgid_offset);

    // A run observes one process. The thread group is known before the program
    // loads and never changes, so the filter is in the instruction stream
    // rather than in a map.
    if let Some(tgid) = target_tgid {
        program.push(Insn::ldx(BPF_W, 1, R10, ROW_KEY + KEY_TGID));
        program.jump_to("done", Insn::jne_imm(1, tgid as i32, 0));
    }
    // The last four bytes of the key, so every byte of it is written: the
    // kernel hashes all twelve, and one nobody wrote puts two identical
    // migrations in two buckets.
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_DEST_CPU, 8));

    program.extend(Insn::load_map_fd(1, rows_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first_migration", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu, and a migration is precisely an
    // event two cpus are involved in, so the increment cannot be a plain
    // load-add-store.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.jump_to("done", Insn::ja());

    program.label("first_migration");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    // Read once, when the row is created: a task's name does not change, and
    // this path runs every time the load balancer moves anything.
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
    // The map is full: this migration will never be counted, and nothing else
    // anywhere would record that it happened.
    program.push(Insn::st_imm(BPF_W, R10, DROP_KEY, DROP_ROWS as i32));
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
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// A loaded, attached migration program and the map it fills.
pub struct MigrationProgram {
    rows: Hash,
    drops: Counters,
    program: OwnedFd,
    _link: OwnedFd,
}

impl MigrationProgram {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.program])
    }

    pub fn load(target_tgid: Option<u32>) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let rows = Hash::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_migr_rows")
            .map_err(|error| map_failure("migration map", error))?;
        let drops = Counters::create(1, "omn_migr_drop")
            .map_err(|error| map_failure("migration drop counter", error))?;

        let instructions = instructions(btf, target_tgid, rows.as_raw_fd(), drops.as_raw_fd())?;
        let program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &instructions,
            "omn_migrate",
            LICENSE,
        )
        .map_err(|error| {
            if error.is_permission() {
                super::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the migration program: {error}")
            }
        })?;
        let link = syscall::raw_tracepoint_open("sched_migrate_task", program.as_raw_fd())
            .map_err(|error| {
                format!("cannot attach to the `sched_migrate_task` tracepoint: {error}")
            })?;

        Ok(Self {
            rows,
            drops,
            program,
            _link: link,
        })
    }

    /// Takes every row the kernel has counted since the last drain.
    pub fn drain(&self) -> Result<(Vec<Migration>, super::DrainLosses), String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the migration counts: {error}"))?;
        let mut losses = super::DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match Migration::decode(key, value) {
                Some(row) => rows.push(row),
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    /// Migrations the program could not count because the map was full.
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

    /// A migration is an event two cpus take part in, and the count lives in
    /// one allocation they share. A plain load-add-store loses whichever
    /// increment lands inside another cpu's, and loses most of them on exactly
    /// the machine whose load balancer is busiest.
    #[test]
    fn the_count_is_accumulated_atomically() {
        let Some(btf) = kernel_btf() else { return };
        let program = instructions(&btf, None, 3, 4).expect("a program");
        assert_eq!(
            program.iter().filter(|insn| insn.code == 0xdb).count(),
            1,
            "the migration count, atomically"
        );
    }

    /// This program takes no timestamp at all -- a migration is a count, not a
    /// duration -- so the clock invariant is checked by its absence. A reading
    /// nothing subtracts from is a number on an event that means nothing, and
    /// worse, invites someone to subtract two of them later.
    #[test]
    fn the_program_reads_no_clock_because_it_measures_no_duration() {
        let Some(btf) = kernel_btf() else { return };
        let called: Vec<i32> = instructions(&btf, None, 3, 4)
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
        assert!(!instructions(&btf, None, 3, 4)
            .expect("a program")
            .iter()
            .any(|insn| insn.imm == 4242));
        assert_eq!(
            instructions(&btf, Some(4242), 3, 4)
                .expect("a program")
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
            btf.tracepoint_signature("sched_migrate_task")
                .expect("a prototype"),
            SIGNATURE
        );
        assert!(instructions(&btf, None, 3, 4).is_ok());
    }

    /// The kernel hashes every byte of a key, so one the program never writes
    /// puts two identical migrations in two buckets and halves both counts.
    #[test]
    fn every_byte_of_a_key_and_a_value_is_accounted_for() {
        let key = [(KEY_TID, 4), (KEY_TGID, 4), (KEY_DEST_CPU, 4)];
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
        assert!(Migration::decode(&[0u8; 4], &[0u8; ROW_VALUE_BYTES]).is_none());
        assert!(Migration::decode(&[0u8; ROW_KEY_BYTES], &[0u8; 8]).is_none());
    }

    #[test]
    fn a_name_stops_at_its_terminator() {
        let mut key = [0u8; ROW_KEY_BYTES];
        key[KEY_DEST_CPU as usize..KEY_DEST_CPU as usize + 4].copy_from_slice(&7u32.to_ne_bytes());
        let mut value = [0u8; ROW_VALUE_BYTES];
        value[..8].copy_from_slice(&5u64.to_ne_bytes());
        value[VALUE_COMM as usize..VALUE_COMM as usize + 6].copy_from_slice(b"spinne");
        let row = Migration::decode(&key, &value).expect("a full record");
        assert_eq!(
            (row.dest_cpu, row.count, row.comm.as_str()),
            (7, 5, "spinne")
        );
    }
}
