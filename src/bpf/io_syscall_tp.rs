// SPDX-License-Identifier: MIT OR GPL-2.0
//! `io.syscall` through the classic `raw_syscalls` tracepoints (`bpf:tp`),
//! for a kernel without BTF.
//!
//! The fentry road times the syscall table's own entry points, one pair per
//! syscall, and reads the descriptor out of the saved registers through BTF.
//! Neither is there without BTF; `raw_syscalls:sys_enter` and `sys_exit` are,
//! on every kernel, and their records carry the syscall's number, its
//! arguments and its return. So one program at each half times the same eight
//! calls -- read, write, pread64, pwrite64 and their vectored forms -- told
//! apart by their number, and counts them in the kernel into the rows the
//! fentry road fills, which the same collector reads.
//!
//! **The cost is the catalog's degradation**: these two tracepoints fire on
//! every syscall of the machine, and each run tests a number before it does
//! anything else.

use super::context::Record;
use super::fentry::{DrainLosses, Drained, Narrowing, Row};
use super::insn::{helper, Insn, Log2Labels, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, row_map, select_rows, Counters, Hash, Rows};
use super::net_program::count_drop;
use super::proc_events::Attached;
use super::sched_common::Descendants;

/// The eight calls, by the `io_vfs::Op` each is reported as, and their
/// numbers in the two syscall tables this build reads. Frozen ABI: a number
/// is never reused.
const CALLS: [(u32, i32, i32); 8] = [
    // (op, x86-64, aarch64)
    (0, 0, 63),   // read
    (1, 1, 64),   // write
    (2, 17, 67),  // pread64
    (3, 18, 68),  // pwrite64
    (4, 19, 65),  // readv
    (5, 20, 66),  // writev
    (6, 295, 69), // preadv
    (7, 296, 70), // pwritev
];
const CALL_LABELS: [&str; 8] = [
    "op_0", "op_1", "op_2", "op_3", "op_4", "op_5", "op_6", "op_7",
];

const LOG2: Log2Labels = [
    "sys_log2_32",
    "sys_log2_16",
    "sys_log2_8",
    "sys_log2_4",
    "sys_log2_2",
    "sys_log2_1",
    "sys_log2_done",
];

const MAX_PENDING: u32 = 16384;

/// The drop counter's slots.
const DROP_PENDING: u32 = 0;
const DROP_ROWS: u32 = 1;
const DROP_SLOTS: u32 = 2;

/// The pending map: a thread's call in flight, by its id.
const PEND_KEY: i16 = -8;
const PEND_VALUE: i16 = -32;
const PEND_TS: i16 = 0;
const PEND_FD: i16 = 8;
const PEND_OP: i16 = 16;
const PEND_VALUE_BYTES: usize = 24;

/// A row: `(tid, tgid, op, bucket, fd)` to `(count, total, bytes, errors,
/// comm)`.
const ROW_KEY: i16 = -56;
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
const KEY_OP: i16 = 8;
const KEY_BUCKET: i16 = 12;
const KEY_FD: i16 = 16;
const ROW_KEY_BYTES: usize = 24;
const ROW_VALUE: i16 = -104;
const VALUE_COUNT: i16 = 0;
const VALUE_TOTAL_NS: i16 = 8;
const VALUE_BYTES: i16 = 16;
const VALUE_ERRORS: i16 = 24;
const VALUE_COMM: i16 = 32;
const COMM_BYTES: usize = 16;
const ROW_VALUE_BYTES: usize = 48;

const DROP_KEY: i16 = -108;
const ROWS_SLOT: i16 = -120;
const SCRATCH: i16 = -128;

/// The syscall numbers of this build's table, or why there is none.
fn numbers() -> Result<[(u32, i32); 8], String> {
    let column = match super::fentry::syscall_arch() {
        Some("x64") => |call: &(u32, i32, i32)| call.1,
        Some(_) => |call: &(u32, i32, i32)| call.2,
        None => {
            return Err(format!(
                "`io.syscall` tells the calls it times apart by their syscall number, and this \
                 build knows x86-64's and aarch64's tables, not {}'s",
                std::env::consts::ARCH
            ))
        }
    };
    Ok(CALLS.map(|call| (call.0, column(&call))))
}

/// The program on `raw_syscalls:sys_enter`: one of the eight calls begins.
fn enter_instructions(
    record: &Record,
    numbers: &[(u32, i32); 8],
    narrowing: Narrowing,
    followed: Option<i32>,
    pending_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let args_at = record.field("args")?.offset;
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    record.load(&mut program, 1, 6, "id", SCRATCH)?;
    for ((_, number), label) in numbers.iter().zip(CALL_LABELS) {
        program.jump_to(label, Insn::jeq_imm(1, *number, 0));
    }
    program.jump_to("done", Insn::ja());
    for ((op, _), label) in numbers.iter().zip(CALL_LABELS) {
        program.label(label);
        program.push(Insn::mov64_imm(8, *op as i32));
        program.jump_to("timed", Insn::ja());
    }
    program.label("timed");

    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::mov64_reg(7, 0));
    if let Some(tgid) = narrowing.tgid {
        program.push(Insn::mov64_reg(1, 7));
        program.push(Insn::rsh64_imm(1, 32));
        match followed {
            None => {
                program.jump_to("done", Insn::jne_imm(1, tgid as i32, 0));
            }
            Some(map) => {
                program.push(Insn::stx(BPF_W, R10, SCRATCH, 1));
                program.jump_to("followed", Insn::jeq_imm(1, tgid as i32, 0));
                program.extend(Insn::load_map_fd(1, map));
                program.push(Insn::mov64_reg(2, R10));
                program.push(Insn::add64_imm(2, SCRATCH as i32));
                program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
                program.jump_to("done", Insn::jeq_imm(0, 0, 0));
                program.label("followed");
            }
        }
    }
    program.push(Insn::st_imm(BPF_DW, R10, PEND_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, PEND_KEY, 7));
    if let Some(tid) = narrowing.tid {
        program.push(Insn::ldx(BPF_W, 1, R10, PEND_KEY));
        program.jump_to("done", Insn::jne_imm(1, tid as i32, 0));
    }

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + PEND_TS, 0));
    // The descriptor is every one of the eight calls' first argument.
    program.push(Insn::ldx(BPF_DW, 1, 6, args_at));
    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + PEND_FD, 1));
    program.push(Insn::st_imm(BPF_DW, R10, PEND_VALUE + PEND_OP, 0));
    program.push(Insn::stx(BPF_W, R10, PEND_VALUE + PEND_OP, 8));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, PEND_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    // The map is full: this call will never be timed, and is counted.
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_PENDING, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `raw_syscalls:sys_exit`: the thread's timed call, when it
/// has one in flight, ends, and is counted in its row.
fn exit_instructions(
    record: &Record,
    pending_fd: i32,
    rows_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::mov64_reg(7, 0));
    program.push(Insn::st_imm(BPF_DW, R10, PEND_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, PEND_KEY, 7));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // Most syscalls are none of the eight, and their exit stops here.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 9, 0, PEND_TS));
    program.push(Insn::ldx(BPF_DW, 1, 0, PEND_FD));
    program.push(Insn::stx(BPF_DW, R10, ROW_KEY + KEY_FD, 1));
    program.push(Insn::ldx(BPF_W, 1, 0, PEND_OP));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_OP, 1));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::sub64_reg(0, 9));
    program.push(Insn::mov64_reg(9, 0));
    // r8 keeps the return: bytes moved, or a negative errno.
    record.load(&mut program, 8, 6, "ret", SCRATCH)?;

    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TID, 7));
    program.push(Insn::mov64_reg(1, 7));
    program.push(Insn::rsh64_imm(1, 32));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TGID, 1));
    super::insn::push_log2_bucket(&mut program, 9, &LOG2);
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_BUCKET, 3));

    // The row map of this interval, taken once; see `map::Rows`.
    select_rows(&mut program, rows_fd, ROWS_SLOT, "no_rows");
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first_call", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu, so every accumulation is atomic.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_TOTAL_NS, 9));
    program.jump_to("moved", Insn::jsge_imm(8, 1, 0));
    program.jump_to("done", Insn::jsge_imm(8, 0, 0));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_ERRORS, 1));
    program.jump_to("done", Insn::ja());
    program.label("moved");
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_BYTES, 8));
    program.jump_to("done", Insn::ja());

    program.label("first_call");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_TOTAL_NS, 9));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_BYTES, 0));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_ERRORS, 0));
    program.jump_to("first_moved", Insn::jsge_imm(8, 1, 0));
    program.jump_to("first_counted", Insn::jsge_imm(8, 0, 0));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_ERRORS, 1));
    program.jump_to("first_counted", Insn::ja());
    program.label("first_moved");
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_BYTES, 8));
    program.label("first_counted");
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

fn decode(key: &[u8], value: &[u8]) -> Option<Row> {
    if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
        return None;
    }
    let word = |bytes: &[u8], at: i16| {
        u32::from_ne_bytes(bytes[at as usize..][..4].try_into().expect("4"))
    };
    let long = |bytes: &[u8], at: i16| {
        u64::from_ne_bytes(bytes[at as usize..][..8].try_into().expect("8"))
    };
    let comm = &value[VALUE_COMM as usize..][..COMM_BYTES];
    let end = comm
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(COMM_BYTES);
    Some(Row {
        tid: word(key, KEY_TID),
        tgid: word(key, KEY_TGID),
        op: word(key, KEY_OP),
        bucket: word(key, KEY_BUCKET),
        // The descriptor is an `unsigned int` in a 64-bit slot.
        object: long(key, KEY_FD) & u64::from(u32::MAX),
        dev: 0,
        aux: 0,
        count: long(value, VALUE_COUNT),
        total_ns: long(value, VALUE_TOTAL_NS),
        bytes: long(value, VALUE_BYTES),
        errors: long(value, VALUE_ERRORS),
        comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
    })
}

/// The two loaded, attached programs and the maps they count into.
pub struct SyscallTpProgram {
    pending: Hash,
    rows: Rows,
    drops: Counters,
    /// The exit first, then the entry.
    attached: Vec<Attached>,
    descendants: Option<Descendants>,
    /// When the interval the next drain closes opened.
    opened_ns: std::cell::Cell<u64>,
}

impl SyscallTpProgram {
    pub fn load(narrowing: Narrowing) -> Result<Self, String> {
        let numbers = numbers()?;
        let enter = Record::of("raw_syscalls", "sys_enter")?;
        let exit = Record::of("raw_syscalls", "sys_exit")?;
        let pending = Hash::create(4, PEND_VALUE_BYTES, MAX_PENDING, "omn_iosys_pend")
            .map_err(|error| map_failure("io.syscall pending map", error))?;
        let rows = Rows::create(
            ROW_KEY_BYTES,
            ROW_VALUE_BYTES,
            super::io_vfs::MAX_ROWS,
            "omn_iosys_rows",
        )
        .map_err(|error| map_failure("io.syscall histogram map", error))?;
        let drops = Counters::create(DROP_SLOTS, "omn_iosys_drop")
            .map_err(|error| map_failure("io.syscall drop counter", error))?;
        let descendants = match narrowing.tgid {
            Some(root) => Some(Descendants::follow(root)?),
            None => None,
        };
        let enter_program = enter_instructions(
            &enter,
            &numbers,
            narrowing,
            descendants.as_ref().map(Descendants::map_fd),
            pending.as_raw_fd(),
            drops.as_raw_fd(),
        )?;
        let exit_program = exit_instructions(
            &exit,
            pending.as_raw_fd(),
            rows.as_raw_fd(),
            drops.as_raw_fd(),
        )?;
        // The exit first: an entry whose exit is not attached yet would leave
        // a stamp nothing closes.
        let attached = vec![
            Attached::classic("raw_syscalls", "sys_exit", &exit_program, "omn_iosys_x")?,
            Attached::classic("raw_syscalls", "sys_enter", &enter_program, "omn_iosys_e")?,
        ];
        Ok(Self {
            pending,
            rows,
            drops,
            attached,
            descendants,
            opened_ns: std::cell::Cell::new(crate::clock::monotonic_ns()),
        })
    }

    pub fn drain(&self, _last: bool) -> Result<Drained, String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the io.syscall histogram: {error}"))?;
        let closed_ns = crate::clock::monotonic_ns();
        let opened_ns = self.opened_ns.replace(closed_ns);
        let mut losses = DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match decode(key, value) {
                Some(row) => rows.push(row),
                None => losses.vanished += 1,
            }
        }
        Ok(Drained {
            rows,
            losses,
            opened_ns,
            closed_ns,
        })
    }

    pub fn still_pending(&self) -> (usize, usize) {
        super::fentry::split_stamps(&self.pending)
    }

    /// Calls the programs could not record, as `(pending, rows, orphaned)`.
    pub fn dropped(&self) -> (u64, u64, u64) {
        (self.drops.read(DROP_PENDING), self.drops.read(DROP_ROWS), 0)
    }

    pub fn missed(&self) -> u64 {
        crate::bpf::program::skipped(self.attached.iter().filter_map(Attached::program))
            + self.descendants.as_ref().map_or(0, Descendants::skipped)
    }

    pub fn unfollowed(&self) -> u64 {
        self.descendants.as_ref().map_or(0, Descendants::missed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records() -> (Record, Record) {
        let enter = Record::from_layout(
            "raw_syscalls:sys_enter",
            super::super::attach::parse_layout(
                "\tfield:long id;\toffset:8;\tsize:8;\tsigned:1;\n\
                 \tfield:unsigned long args[6];\toffset:16;\tsize:48;\tsigned:0;\n",
            ),
        );
        let exit = Record::from_layout(
            "raw_syscalls:sys_exit",
            super::super::attach::parse_layout(
                "\tfield:long id;\toffset:8;\tsize:8;\tsigned:1;\n\
                 \tfield:long ret;\toffset:16;\tsize:8;\tsigned:1;\n",
            ),
        );
        (enter, exit)
    }

    /// The entry compares each of the eight numbers and reads the descriptor
    /// out of the first argument; the exit reads the return.
    #[test]
    fn the_programs_read_the_number_the_descriptor_and_the_return() {
        let Ok(numbers) = numbers() else { return };
        let (enter, exit) = records();
        for narrowing in [
            Narrowing::default(),
            Narrowing {
                tgid: Some(42),
                tid: None,
            },
            Narrowing {
                tgid: None,
                tid: Some(42),
            },
        ] {
            let program =
                enter_instructions(&enter, &numbers, narrowing, Some(9), 3, 4).expect("an entry");
            assert!(program.contains(&Insn::ldx(BPF_DW, 1, 6, 8)), "the number");
            assert!(
                program.contains(&Insn::ldx(BPF_DW, 1, 6, 16)),
                "the descriptor"
            );
            for (_, number) in numbers {
                assert!(program
                    .iter()
                    .any(|insn| insn.code == 0x15 && insn.imm == number));
            }
        }
        let program = exit_instructions(&exit, 3, 4, 5).expect("an exit");
        assert!(program.contains(&Insn::ldx(BPF_DW, 8, 6, 16)), "the return");
        crate::bpf::map::assert_rows_taken_once(&program, 4);
    }

    /// The row the program writes is the row the reader decodes.
    #[test]
    fn a_row_decodes_into_the_histograms_row() {
        let mut key = vec![0u8; ROW_KEY_BYTES];
        key[KEY_TID as usize..][..4].copy_from_slice(&7u32.to_ne_bytes());
        key[KEY_TGID as usize..][..4].copy_from_slice(&6u32.to_ne_bytes());
        key[KEY_OP as usize..][..4].copy_from_slice(&3u32.to_ne_bytes());
        key[KEY_BUCKET as usize..][..4].copy_from_slice(&11u32.to_ne_bytes());
        key[KEY_FD as usize..][..8].copy_from_slice(&5u64.to_ne_bytes());
        let mut value = vec![0u8; ROW_VALUE_BYTES];
        value[VALUE_COUNT as usize..][..8].copy_from_slice(&2u64.to_ne_bytes());
        value[VALUE_BYTES as usize..][..8].copy_from_slice(&8192u64.to_ne_bytes());
        value[VALUE_COMM as usize..][..2].copy_from_slice(b"dd");
        let row = decode(&key, &value).expect("a row");
        assert_eq!(
            (row.tid, row.tgid, row.op, row.bucket, row.object, row.count, row.bytes),
            (7, 6, 3, 11, 5, 2, 8192)
        );
        assert_eq!(row.comm, "dd");
    }

    #[test]
    fn the_stack_regions_do_not_overlap_and_stay_inside_the_frame() {
        let regions = [
            (PEND_KEY, 8, "pending key"),
            (PEND_VALUE, PEND_VALUE_BYTES as i16, "pending value"),
            (ROW_KEY, ROW_KEY_BYTES as i16, "row key"),
            (ROW_VALUE, ROW_VALUE_BYTES as i16, "row value"),
            (DROP_KEY, 4, "drop key"),
            (ROWS_SLOT, 8, "row map slot"),
            (SCRATCH, 8, "scratch"),
        ];
        for (at, size, name) in regions {
            assert!(at >= -512 && at + size <= 0, "`{name}` is off the frame");
            for (other_at, other_size, other) in regions {
                if name != other {
                    assert!(
                        at + size <= other_at || other_at + other_size <= at,
                        "`{name}` overlaps `{other}`"
                    );
                }
            }
        }
    }
}
