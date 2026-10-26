// SPDX-License-Identifier: MIT OR GPL-2.0
//! The `irq_handler_entry` / `irq_handler_exit` pair, and `softirq_entry` /
//! `softirq_exit` beside it: time the machine spent
//! inside hardware interrupt handlers and inside softirqs.
//!
//! An interrupt handler runs on somebody else's time. It preempts whatever
//! thread was on the cpu, it is not scheduled, and the cpu time it costs is
//! charged to nobody -- a thread that took ten milliseconds longer than its
//! own cpu time explains may have spent that difference being interrupted, and
//! no per-task measurement in this tool can see it. That is what this source
//! is for.
//!
//! **The pairing is per cpu, not per task**, which is what makes it cheap: a
//! hardware interrupt handler on Linux runs with that line masked and does not
//! nest, so at most one handler is in flight per cpu and the in-flight
//! timestamp is one slot of a per-cpu array -- no hash, no key, no atomic. It
//! is also the only pairing that would be correct: an interrupt handler is not
//! running on behalf of the thread it interrupted, so keying it by the current
//! task would attribute a disk's interrupt to whatever happened to be on the
//! cpu.
//!
//! **And the soft half, since the thirteenth wave.** Softirq time -- the
//! network receive path, block completions, the timer wheel, RCU -- is
//! `softirq_entry` and `softirq_exit`, paired the same way: a softirq does not
//! nest in another on the same cpu, so its start is one more per-cpu slot. A
//! hard interrupt *can* land inside a softirq, and its time is taken out of
//! the softirq's: every hard exit adds its length to a per-cpu running total,
//! each softirq entry notes that total, and the exit subtracts what it grew by.
//! A row says which half it is (`kind`), and the two are never added together
//! by the program: a softirq is keyed by its vector, a hard interrupt by its
//! line, and the numbers overlap.

use super::btf::Btf;
use super::context::Record;
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, row_map, select_rows, Counters, Rows};
use super::proc_events::Attached;
use super::sched_common::{
    bucket_low_ns, count_drop, count_update_failure, push_log2_bucket, read_field, text_of,
    Log2Labels, E2BIG,
};
use crate::bpf::program::load_raw_tracepoint as load;

/// What a program reads its interrupt out of: a raw tracepoint's arguments,
/// checked against BTF, or a classic `irq` record (`bpf:tp`).
#[derive(Clone, Copy)]
enum Source<'a> {
    Raw(&'a Btf),
    Classic(&'a Record),
}

/// Arguments of the tracepoints, in the raw tracepoint's context. A softirq's
/// only argument is its vector, in the same first slot.
const ARG_IRQ: i16 = 0;
const ARG_ACTION: i16 = 8;

/// The prototypes these programs are written against.
pub(super) const ENTRY_SIGNATURE: [&str; 2] = ["int", "irqaction *"];
pub(super) const EXIT_SIGNATURE: [&str; 3] = ["int", "irqaction *", "int"];
pub(super) const SOFT_SIGNATURE: [&str; 1] = ["unsigned int"];

/// The two halves, as a row's `kind`.
pub const KIND_HARD: u32 = 0;
pub const KIND_SOFT: u32 = 1;

/// How many distinct `(cpu, irq, bucket)` rows one interval may hold.
///
/// Small on purpose: the cardinality is bounded by the machine rather than by
/// the workload -- the cpus times the interrupt lines times the few buckets a
/// handler's duration falls in.
const MAX_ROWS: u32 = 8192;

/// Slots of the counter.
const DROP_ROWS_FULL: u32 = 0;
const DROP_ROWS_BUSY: u32 = 1;
const DROP_BACKWARDS: u32 = 2;
/// Exits that found no entry: the run attached in the middle of a handler.
const DROP_UNOPENED: u32 = 3;
const COUNTER_SLOTS: u32 = 4;

/// Where the maps' keys and values live on the program's stack.
const ROW_VALUE: i16 = -104;
const ROW_KEY: i16 = -40;
const DROP_KEY: i16 = -24;
/// Where the program keeps the row map it looked up through the generation
/// slot, once per run; see `map::select_rows`.
const ROWS_SLOT: i16 = -112;
/// The one slot of the in-flight array, which is per cpu.
const TS_KEY: i16 = -16;
/// Where the pointer to an interrupt's name is read before it is followed.
const SCRATCH: i16 = -8;
/// Where a softirq's start is kept while its length is worked out, to stamp
/// the row with.
const SOFT_START: i16 = -120;

/// The layout of a row key.
const KEY_CPU: i16 = 0;
const KEY_IRQ: i16 = 4;
const KEY_BUCKET: i16 = 8;
/// Which half: [`KIND_HARD`] or [`KIND_SOFT`]. In the key because a vector and
/// a line are both small integers, and `NET_RX` is 3 as surely as some
/// device's line is.
const KEY_KIND: i16 = 12;
const ROW_KEY_BYTES: usize = 16;

/// The layout of a row value.
const VALUE_COUNT: i16 = 0;
const VALUE_TOTAL_NS: i16 = 8;
/// The handler's name, as `/proc/interrupts` shows it: `nvme0q3`, `xhci_hcd`,
/// `eth0`. It is what makes a number actionable -- an interrupt line number is
/// a machine's private business and a device name is not.
const VALUE_NAME: i16 = 16;
const NAME_BYTES: usize = 40;
/// When the first run of this row's window *began*.
///
/// A bucketed source emits one event per row per window, and stamping that
/// event at the drain puts it at the window's end -- so a join that *opens* on
/// this source opens after everything that happened in the same window as the
/// first run. Finding 44 has the argument; `mem.reclaim` and
/// `sched.runqlat` carried it first.
///
/// Written once, where the row is created, which is that key's first run of
/// the window: the rows map is drained and emptied every window.
const VALUE_FIRST_NS: i16 = 56;
const ROW_VALUE_BYTES: usize = 64;

/// The in-flight array holds four `u64`s per cpu: the running hard handler's
/// start, the running softirq's start, the cpu's running total of hard
/// handler time, and that total as it was when the running softirq began.
const IN_FLIGHT_SLOT: i32 = 0;
const SOFT_SLOT: i32 = 1;
const HARD_TOTAL_SLOT: i32 = 2;
const HARD_AT_SOFT_SLOT: i32 = 3;
const IN_FLIGHT_SLOTS: u32 = 4;

const LOG2: Log2Labels = [
    "irq_log2_32",
    "irq_log2_16",
    "irq_log2_8",
    "irq_log2_4",
    "irq_log2_2",
    "irq_log2_1",
    "irq_log2_done",
];

/// One `(cpu, irq, bucket)` row's handler runs over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrqRun {
    pub cpu: u32,
    /// The interrupt line, or for a softirq its vector.
    pub irq: u32,
    /// [`KIND_HARD`] or [`KIND_SOFT`].
    pub kind: u32,
    pub bucket: u32,
    pub count: u64,
    pub total_ns: u64,
    /// When the first of this row's window began, which is what the event is
    /// timestamped with. Zero means unknown, and falls back to the drain.
    pub first_ns: u64,
    /// The handler's name, as `/proc/interrupts` shows it.
    pub name: String,
}

impl IrqRun {
    pub fn bucket_low_ns(&self) -> u64 {
        bucket_low_ns(self.bucket)
    }

    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        let word =
            |bytes: &[u8], at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long =
            |bytes: &[u8], at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        Some(Self {
            cpu: word(key, KEY_CPU as usize),
            irq: word(key, KEY_IRQ as usize),
            kind: word(key, KEY_KIND as usize),
            bucket: word(key, KEY_BUCKET as usize),
            count: long(value, VALUE_COUNT as usize),
            total_ns: long(value, VALUE_TOTAL_NS as usize),
            first_ns: long(value, VALUE_FIRST_NS as usize),
            name: text_of(&value[VALUE_NAME as usize..VALUE_NAME as usize + NAME_BYTES]),
        })
    }
}

/// Puts a pointer to one of this cpu's in-flight slots in `r0`.
fn open_slot(program: &mut Program, in_flight_fd: i32, slot: i32) {
    program.push(Insn::st_imm(BPF_W, R10, TS_KEY, slot));
    program.extend(Insn::load_map_fd(1, in_flight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, TS_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
}

/// The program on `irq_handler_entry`: a handler has the cpu.
fn entry_instructions(source: Source, in_flight_fd: i32) -> Result<Vec<Insn>, String> {
    if let Source::Raw(btf) = source {
        crate::bpf::program::prototype(
            btf,
            "irq_handler_entry",
            &[&ENTRY_SIGNATURE],
            "the interrupt number and its action",
        )?;
    }

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(7, 0));

    open_slot(&mut program, in_flight_fd, IN_FLIGHT_SLOT);
    // Index zero of a per-cpu array is always there; the check is what the
    // verifier requires rather than a case this expects.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    // No atomic: this slot belongs to this cpu, and a hardware interrupt
    // handler does not nest on the cpu it is running on.
    program.push(Insn::stx(BPF_DW, 0, 0, 7));

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `irq_handler_exit`: the handler is done and has a length.
fn exit_instructions(
    source: Source,
    in_flight_fd: i32,
    rows_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    // The classic exit record has the line and not its action: such a row is
    // named at the drain, from `/proc/interrupts`.
    let name_offset = match source {
        Source::Raw(btf) => {
            crate::bpf::program::prototype(
                btf,
                "irq_handler_exit",
                &[&EXIT_SIGNATURE],
                "the interrupt number and its action",
            )?;
            Some(btf.member_offset("irqaction", "name")?)
        }
        Source::Classic(_) => None,
    };

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    open_slot(&mut program, in_flight_fd, IN_FLIGHT_SLOT);
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 9, 0, 0));
    // Zero means no entry was recorded for this handler: the programs were
    // attached while it was already running. Counted, because an exit with no
    // beginning is the one thing that would otherwise be timed from the epoch.
    program.jump_to("unopened", Insn::jeq_imm(9, 0, 0));
    // Cleared so that the next exit on this cpu cannot reuse this timestamp.
    program.push(Insn::st_imm(BPF_DW, 0, 0, 0));

    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::mov64_reg(9, 1));
    program.jump_to("forwards", Insn::jsge_imm(9, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_BACKWARDS, "done");
    program.jump_to("done", Insn::ja());

    program.label("unopened");
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_UNOPENED, "done");
    program.jump_to("done", Insn::ja());

    program.label("forwards");
    // This cpu's running total of hard handler time, which a softirq this
    // handler interrupted takes its own length out of.
    open_slot(&mut program, in_flight_fd, HARD_TOTAL_SLOT);
    program.jump_to("totalled", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, 0));
    program.push(Insn::add64_reg(1, 9));
    program.push(Insn::stx(BPF_DW, 0, 0, 1));
    program.label("totalled");
    match source {
        Source::Raw(_) => {
            // r7 keeps the action, which is where the handler's name is.
            program.push(Insn::ldx(BPF_DW, 7, 6, ARG_ACTION));
            program.push(Insn::ldx(BPF_DW, 1, 6, ARG_IRQ));
        }
        Source::Classic(record) => record.load(&mut program, 1, 6, "irq", SCRATCH)?,
    }
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_IRQ, 1));
    program.push(Insn::st_imm(
        BPF_W,
        R10,
        ROW_KEY + KEY_KIND,
        KIND_HARD as i32,
    ));

    push_row(&mut program, rows_fd, drops_fd, name_offset);

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Adds the run whose length is in `r9` to its row, from the cpu it ran on
/// and the key fields already on the stack; `r8` is the program's reading of
/// the clock, and `r7` the `irqaction` whose name a hard row carries.
///
/// Shared by the two exits, which differ only in how the length and the key
/// were found: a hard row is named from its action, a soft row by the
/// collector from its vector.
fn push_row(program: &mut Program, rows_fd: i32, drops_fd: i32, name_offset: Option<i32>) {
    // The cpu goes in the key before the bucket does: this call clobbers r1
    // through r5, and the bucket is computed in them. It is the cpu the
    // handler ran on, which is the one whose work it interrupted.
    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_CPU, 0));

    push_log2_bucket(program, 9, &LOG2);
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_BUCKET, 3));

    // The row map of this interval, taken once: a drain may swap it at any
    // instruction, and a lookup and an insert that named two maps would put
    // one occurrence in each. See `map::Rows`.
    select_rows(program, rows_fd, ROWS_SLOT, "no_rows");
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first_run", Insn::jeq_imm(0, 0, 0));
    // One allocation reachable from every cpu -- the key names a cpu, but the
    // map does not -- so both increments are atomic.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_TOTAL_NS, 9));
    program.jump_to("done", Insn::ja());

    program.label("first_run");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_TOTAL_NS, 9));
    // When this one began: now, less how long it lasted. r8 is the reading
    // taken at the top of the program and r9 is the duration, so the beginning
    // is recovered rather than carried through a second map field.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_FIRST_NS, 1));
    // The name field is cleared before it is filled. `bpf_probe_read_kernel_str`
    // stops at the terminator and leaves the rest of the span as it found it,
    // and what it found is this program's own stack -- which would then travel
    // into a map userspace reads.
    for offset in (0..NAME_BYTES as i16).step_by(8) {
        program.push(Insn::st_imm(
            BPF_DW,
            R10,
            ROW_VALUE + VALUE_NAME + offset,
            0,
        ));
    }
    // `irqaction.name` is a pointer to a string, so it takes two reads: the
    // pointer out of the structure, then the characters it points at.
    if let Some(name_offset) = name_offset {
        read_field(program, SCRATCH, 8, 7, name_offset);
        program.push(Insn::mov64_reg(1, R10));
        program.push(Insn::add64_imm(1, (ROW_VALUE + VALUE_NAME) as i32));
        program.push(Insn::mov64_imm(2, NAME_BYTES as i32));
        program.push(Insn::ldx(BPF_DW, 3, R10, SCRATCH));
        program.push(Insn::call(helper::PROBE_READ_KERNEL_STR));
    }

    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_update_failure(
        program,
        drops_fd,
        DROP_KEY,
        &[(E2BIG, DROP_ROWS_FULL, "rows_not_full")],
        DROP_ROWS_BUSY,
        "done",
    );

    program.jump_to("done", Insn::ja());
    // The slot held no map, which it always does; counted rather than assumed.
    program.label("no_rows");
    count_drop(program, drops_fd, DROP_KEY, DROP_ROWS_BUSY, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
}

/// The program on `softirq_entry`: a softirq vector has the cpu.
fn soft_entry_instructions(source: Source, in_flight_fd: i32) -> Result<Vec<Insn>, String> {
    if let Source::Raw(btf) = source {
        crate::bpf::program::prototype(
            btf,
            "softirq_entry",
            &[&SOFT_SIGNATURE],
            "the interrupt number and its action",
        )?;
    }

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(7, 0));

    open_slot(&mut program, in_flight_fd, SOFT_SLOT);
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::stx(BPF_DW, 0, 0, 7));
    // The cpu's hard handler time so far: what the exit takes this softirq's
    // share of that time against.
    open_slot(&mut program, in_flight_fd, HARD_TOTAL_SLOT);
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 8, 0, 0));
    open_slot(&mut program, in_flight_fd, HARD_AT_SOFT_SLOT);
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::stx(BPF_DW, 0, 0, 8));

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `softirq_exit`: the vector is done, and its length is the
/// time since its entry less the hard handlers that ran inside it.
fn soft_exit_instructions(
    source: Source,
    in_flight_fd: i32,
    rows_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    if let Source::Raw(btf) = source {
        crate::bpf::program::prototype(
            btf,
            "softirq_exit",
            &[&SOFT_SIGNATURE],
            "the interrupt number and its action",
        )?;
    }

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    open_slot(&mut program, in_flight_fd, SOFT_SLOT);
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 9, 0, 0));
    program.jump_to("unopened", Insn::jeq_imm(9, 0, 0));
    program.push(Insn::st_imm(BPF_DW, 0, 0, 0));
    program.push(Insn::stx(BPF_DW, R10, SOFT_START, 9));

    // r7 = the hard handler time that landed inside this softirq.
    open_slot(&mut program, in_flight_fd, HARD_TOTAL_SLOT);
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 7, 0, 0));
    open_slot(&mut program, in_flight_fd, HARD_AT_SOFT_SLOT);
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, 0));
    program.push(Insn::sub64_reg(7, 1));

    // r9 = now - start - nested.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::sub64_reg(1, 7));
    program.push(Insn::mov64_reg(9, 1));
    program.jump_to("forwards", Insn::jsge_imm(9, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_BACKWARDS, "done");
    program.jump_to("done", Insn::ja());

    program.label("unopened");
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_UNOPENED, "done");
    program.jump_to("done", Insn::ja());

    program.label("forwards");
    match source {
        Source::Raw(_) => {
            program.push(Insn::ldx(BPF_DW, 1, 6, ARG_IRQ));
        }
        Source::Classic(record) => record.load(&mut program, 1, 6, "vec", SCRATCH)?,
    }
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_IRQ, 1));
    program.push(Insn::st_imm(
        BPF_W,
        R10,
        ROW_KEY + KEY_KIND,
        KIND_SOFT as i32,
    ));
    // The row's stamp is the softirq's start: `push_row` recovers it as the
    // clock less the length, and the length has the nested hard time taken
    // out, so the clock is moved back by that much for it.
    program.push(Insn::sub64_reg(8, 7));
    push_row(&mut program, rows_fd, drops_fd, None);

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Each line's handler names from `/proc/interrupts`, by line: what a classic
/// hard row is named by, where the raw program reads its action's. The names
/// follow the line's type after a run of spaces, and a shared line's are
/// separated by a comma and one space, so the last run of two or more spaces
/// is where they begin.
fn interrupt_names() -> std::collections::BTreeMap<u32, String> {
    let text = std::fs::read_to_string("/proc/interrupts").unwrap_or_default();
    parse_interrupt_names(&text)
}

fn parse_interrupt_names(text: &str) -> std::collections::BTreeMap<u32, String> {
    let mut names = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some((line_number, rest)) = line.trim_start().split_once(':') else {
            continue;
        };
        let Ok(line_number) = line_number.parse::<u32>() else {
            continue;
        };
        let Some(at) = rest.trim_end().rfind("  ") else {
            continue;
        };
        let name = rest.trim_end()[at..].trim();
        if !name.is_empty() {
            names.insert(line_number, name.to_owned());
        }
    }
    names
}

/// Handler runs neither map could take, under the kernel's own reason.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IrqDrops {
    pub rows_full: u64,
    pub rows_busy: u64,
    pub backwards: u64,
    /// Exits whose entry this run never saw.
    pub unopened: u64,
}

/// Four loaded, attached programs and the maps they share.
pub struct IrqProgram {
    rows: Rows,
    drops: Counters,
    _in_flight: Counters,
    /// Each entry before its exit.
    attached: Vec<Attached>,
    /// Whether the programs are on the classic tracepoints, whose hard rows
    /// are named at the drain.
    classic: bool,
}

impl IrqProgram {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::program::skipped(self.attached.iter().filter_map(Attached::program))
    }

    pub fn load() -> Result<Self, String> {
        Self::load_with(Some(super::kernel_btf()?))
    }

    /// The same four programs on the classic `irq` tracepoints (`bpf:tp`),
    /// for a kernel without BTF.
    pub fn load_classic() -> Result<Self, String> {
        Self::load_with(None)
    }

    /// `btf` is the raw road's; without it, the classic one.
    fn load_with(btf: Option<&Btf>) -> Result<Self, String> {
        let records = match btf {
            Some(_) => None,
            None => Some([
                Record::of("irq", "irq_handler_entry")?,
                Record::of("irq", "irq_handler_exit")?,
                Record::of("irq", "softirq_entry")?,
                Record::of("irq", "softirq_exit")?,
            ]),
        };
        let source = |index: usize| match (&records, btf) {
            (Some(records), _) => Source::Classic(&records[index]),
            (None, Some(btf)) => Source::Raw(btf),
            (None, None) => unreachable!("records are read wherever there is no BTF"),
        };

        // A per-cpu array of one `u64`: this build's in-flight timestamp, one
        // per cpu, which is the whole state these two programs share.
        let in_flight = Counters::create(IN_FLIGHT_SLOTS, "omn_irq_open")
            .map_err(|error| map_failure("interrupt in-flight slot", error))?;
        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_irq_hist")
            .map_err(|error| map_failure("interrupt histogram map", error))?;
        let drops = Counters::create(COUNTER_SLOTS, "omn_irq_drop")
            .map_err(|error| map_failure("interrupt drop counter", error))?;

        let built = [
            (
                "irq_handler_entry",
                "omn_irq_in",
                entry_instructions(source(0), in_flight.as_raw_fd())?,
            ),
            (
                "irq_handler_exit",
                "omn_irq_out",
                exit_instructions(
                    source(1),
                    in_flight.as_raw_fd(),
                    rows.as_raw_fd(),
                    drops.as_raw_fd(),
                )?,
            ),
            (
                "softirq_entry",
                "omn_softirq_in",
                soft_entry_instructions(source(2), in_flight.as_raw_fd())?,
            ),
            (
                "softirq_exit",
                "omn_softirq_out",
                soft_exit_instructions(
                    source(3),
                    in_flight.as_raw_fd(),
                    rows.as_raw_fd(),
                    drops.as_raw_fd(),
                )?,
            ),
        ];
        // Each entry before its exit: an exit that finds no timestamp is
        // counted and skipped, where the reverse order would have nothing to
        // stop a handler's length being measured from zero.
        let mut attached = Vec::with_capacity(built.len());
        for (tracepoint, name, instructions) in &built {
            attached.push(if records.is_some() {
                Attached::classic("irq", tracepoint, instructions, name)?
            } else {
                let program = load(instructions, name)?;
                let link = crate::bpf::program::attach_raw(tracepoint, &program)?;
                Attached::Raw {
                    program,
                    _link: link,
                }
            });
        }

        Ok(Self {
            rows,
            drops,
            _in_flight: in_flight,
            attached,
            classic: records.is_some(),
        })
    }

    pub fn drain(&self) -> Result<(Vec<IrqRun>, super::DrainLosses), String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the interrupt histogram: {error}"))?;
        let mut losses = super::DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        let names = if self.classic {
            interrupt_names()
        } else {
            Default::default()
        };
        for (key, value) in &drained.entries {
            match IrqRun::decode(key, value) {
                Some(mut row) => {
                    if self.classic && row.kind == KIND_HARD {
                        row.name = names.get(&row.irq).cloned().unwrap_or_default();
                    }
                    rows.push(row)
                }
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    pub fn dropped(&self) -> IrqDrops {
        IrqDrops {
            rows_full: self.drops.read(DROP_ROWS_FULL),
            rows_busy: self.drops.read(DROP_ROWS_BUSY),
            backwards: self.drops.read(DROP_BACKWARDS),
            unopened: self.drops.read(DROP_UNOPENED),
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

    /// An interrupt handler is not running on behalf of the thread it
    /// interrupted, so nothing here may read the current task: doing so would
    /// charge a disk's interrupt to whatever happened to be on the cpu, which
    /// is the one attribution this source exists to avoid.
    /// A classic hard row is named from `/proc/interrupts`: the names after
    /// the line's type, a shared line's all of them.
    #[test]
    fn a_line_is_named_as_proc_interrupts_names_it() {
        let names = parse_interrupt_names(
            "           CPU0       CPU1\n\
               0:         38          0   IO-APIC   2-edge      timer\n\
              16:          0         12   IO-APIC  16-fasteoi   ehci_hcd:usb1, uhci_hcd:usb3\n\
              24:        900          0   PCI-MSI 512000-edge      ahci[0000:00:1f.2]\n\
             NMI:          0          0   Non-maskable interrupts\n",
        );
        assert_eq!(names.get(&0).map(String::as_str), Some("timer"));
        assert_eq!(
            names.get(&16).map(String::as_str),
            Some("ehci_hcd:usb1, uhci_hcd:usb3")
        );
        assert_eq!(
            names.get(&24).map(String::as_str),
            Some("ahci[0000:00:1f.2]")
        );
        assert_eq!(names.len(), 3, "NMI is no line");
    }

    /// The classic programs read the line and the vector out of 4.19's
    /// records.
    #[test]
    fn the_classic_programs_read_the_records() {
        let exit = Record::from_layout(
            "irq:irq_handler_exit",
            super::super::attach::parse_layout(
                "\tfield:int irq;\toffset:8;\tsize:4;\tsigned:1;\n\
                 \tfield:int ret;\toffset:12;\tsize:4;\tsigned:1;\n",
            ),
        );
        let soft = Record::from_layout(
            "irq:softirq_exit",
            super::super::attach::parse_layout(
                "\tfield:unsigned int vec;\toffset:8;\tsize:4;\tsigned:0;\n",
            ),
        );
        let hard = exit_instructions(Source::Classic(&exit), 3, 4, 5).expect("a hard exit");
        let soft = soft_exit_instructions(Source::Classic(&soft), 3, 4, 5).expect("a soft exit");
        for program in [hard, soft] {
            assert!(program.contains(&Insn::ldx(BPF_W, 1, 6, 8)));
        }
        entry_instructions(Source::Classic(&exit), 3).expect("an entry");
    }

    #[test]
    fn neither_program_asks_who_was_running() {
        let Some(btf) = kernel_btf() else { return };
        for program in [
            entry_instructions(Source::Raw(&btf), 3).expect("a program"),
            exit_instructions(Source::Raw(&btf), 3, 4, 5).expect("a program"),
        ] {
            let called: Vec<i32> = program
                .iter()
                .filter(|insn| insn.code == 0x85)
                .map(|insn| insn.imm)
                .collect();
            assert!(
                !called.contains(&helper::GET_CURRENT_PID_TGID),
                "the current task is read in a context it does not belong to"
            );
        }
    }

    /// The in-flight timestamp lives in a per-cpu array, one slot, and a
    /// handler does not nest on its own cpu -- so there is no hash lookup and
    /// no atomic on the entry path, which is the hottest path in this build.
    #[test]
    fn the_entry_path_is_a_clock_read_and_one_store() {
        let Some(btf) = kernel_btf() else { return };
        let program = entry_instructions(Source::Raw(&btf), 3).expect("a program");
        let calls: Vec<i32> = program
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect();
        assert_eq!(calls, vec![helper::KTIME_GET_NS, helper::MAP_LOOKUP_ELEM]);
        assert!(
            !program.iter().any(|insn| insn.code == 0xdb),
            "the entry path takes an atomic it does not need"
        );
    }

    /// The handler's name is a pointer inside the action, so it takes two
    /// reads -- and the second is the string reader rather than a fixed span,
    /// because a fixed span past the end of a string can fault.
    #[test]
    fn the_handlers_name_is_followed_rather_than_copied_from_the_structure() {
        let Some(btf) = kernel_btf() else { return };
        let program = exit_instructions(Source::Raw(&btf), 3, 4, 5).expect("a program");
        let calls: Vec<i32> = program
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect();
        assert!(calls.contains(&helper::PROBE_READ_KERNEL));
        assert!(calls.contains(&helper::PROBE_READ_KERNEL_STR));
    }

    /// `bpf_probe_read_kernel_str` stops at the terminator and leaves the rest
    /// of the span as it found it, and what it found is this program's stack.
    /// Every byte of the name has to be cleared first or the map carries
    /// kernel stack contents to userspace.
    #[test]
    fn the_name_field_is_cleared_before_it_is_filled() {
        let Some(btf) = kernel_btf() else { return };
        let program = exit_instructions(Source::Raw(&btf), 3, 4, 5).expect("a program");
        let cleared: Vec<i16> = program
            .iter()
            .filter(|insn| {
                // `BPF_ST | BPF_DW | BPF_MEM` with the frame pointer as the
                // base and zero as the value.
                insn.code == 0x7a && (insn.regs & 0xf) == 10 && insn.imm == 0
            })
            .map(|insn| insn.off)
            .collect();
        for offset in (0..NAME_BYTES as i16).step_by(8) {
            assert!(
                cleared.contains(&(ROW_VALUE + VALUE_NAME + offset)),
                "byte {offset} of the name is never cleared"
            );
        }
    }

    #[test]
    fn a_kernel_with_another_prototype_is_refused_rather_than_read_wrong() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            btf.tracepoint_signature("irq_handler_entry")
                .expect("a prototype"),
            ENTRY_SIGNATURE
        );
        assert_eq!(
            btf.tracepoint_signature("irq_handler_exit")
                .expect("a prototype"),
            EXIT_SIGNATURE
        );
        assert!(crate::bpf::program::prototype(
            &btf,
            "irq_handler_exit",
            &[&["int"]],
            "the interrupt number and its action"
        )
        .is_err());
    }

    #[test]
    fn every_byte_of_every_key_and_value_is_accounted_for() {
        let key = [(KEY_CPU, 4), (KEY_IRQ, 4), (KEY_BUCKET, 4), (KEY_KIND, 4)];
        let value = [
            (VALUE_COUNT, 8),
            (VALUE_TOTAL_NS, 8),
            (VALUE_FIRST_NS, 8),
            (VALUE_NAME, NAME_BYTES as i16),
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
            (ROW_VALUE, ROW_VALUE_BYTES as i16, "row value"),
            (ROW_KEY, ROW_KEY_BYTES as i16, "row key"),
            (DROP_KEY, 4, "drop key"),
            (TS_KEY, 4, "in-flight key"),
            (SCRATCH, 8, "scratch"),
            (ROWS_SLOT, 8, "row map slot"),
            (SOFT_START, 8, "softirq start"),
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
        assert!(IrqRun::decode(&[0u8; 4], &[0u8; ROW_VALUE_BYTES]).is_none());
        assert!(IrqRun::decode(&[0u8; ROW_KEY_BYTES], &[0u8; 8]).is_none());
    }

    #[test]
    fn a_name_that_fills_its_field_is_still_read() {
        let mut key = [0u8; ROW_KEY_BYTES];
        key[KEY_IRQ as usize..KEY_IRQ as usize + 4].copy_from_slice(&142u32.to_ne_bytes());
        let mut value = [0u8; ROW_VALUE_BYTES];
        value[VALUE_COUNT as usize..VALUE_COUNT as usize + 8].copy_from_slice(&9u64.to_ne_bytes());
        value[VALUE_NAME as usize..VALUE_NAME as usize + 7].copy_from_slice(b"nvme0q3");
        let row = IrqRun::decode(&key, &value).expect("a full record");
        assert_eq!((row.irq, row.count, row.name.as_str()), (142, 9, "nvme0q3"));
    }

    /// The row map is found through the generation slot once per run, and the
    /// lookup and the insert both use what it found -- the property that makes
    /// the drain's swap safe; see `map::Rows`.
    #[test]
    fn the_row_map_is_taken_from_the_slot_once_per_run() {
        let Some(btf) = kernel_btf() else { return };
        let program = exit_instructions(Source::Raw(&btf), 3, 4, 5).expect("a program");
        crate::bpf::map::assert_rows_taken_once(&program, 4);
    }
}
