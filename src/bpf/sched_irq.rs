//! The `irq_handler_entry` / `irq_handler_exit` pair: time the machine spent
//! inside a hardware interrupt handler.
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
//! **This is the hard half only.** The catalog entry's summary says "hard and
//! soft", and its requirements name `irq_handler_entry` and
//! `irq_handler_exit`, which are the hard ones; softirq time is
//! `softirq_entry` and `softirq_exit`, a requirement the entry does not
//! declare and which this stream does not own. Attaching to a tracepoint the
//! resolver never probed is how a backend comes to be reported available and
//! then fail, so the gap is left open and written down rather than closed
//! quietly. `docs/families/sched.md` says what it costs.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash};
use super::sched_common::{
    bucket_low_ns, count_drop, count_update_failure, push_log2_bucket, read_field, text_of,
    Log2Labels, E2BIG,
};
use super::syscall;

/// Arguments of the two tracepoints, in the raw tracepoint's context.
const ARG_IRQ: i16 = 0;
const ARG_ACTION: i16 = 8;

/// The prototypes these programs are written against.
const ENTRY_SIGNATURE: [&str; 2] = ["int", "irqaction *"];
const EXIT_SIGNATURE: [&str; 3] = ["int", "irqaction *", "int"];

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
const ROW_VALUE: i16 = -96;
const ROW_KEY: i16 = -40;
const DROP_KEY: i16 = -24;
/// The one slot of the in-flight array, which is per cpu.
const TS_KEY: i16 = -16;
/// Where the pointer to an interrupt's name is read before it is followed.
const SCRATCH: i16 = -8;

/// The layout of a row key.
const KEY_CPU: i16 = 0;
const KEY_IRQ: i16 = 4;
const KEY_BUCKET: i16 = 8;
const ROW_KEY_BYTES: usize = 12;

/// The layout of a row value.
const VALUE_COUNT: i16 = 0;
const VALUE_TOTAL_NS: i16 = 8;
/// The handler's name, as `/proc/interrupts` shows it: `nvme0q3`, `xhci_hcd`,
/// `eth0`. It is what makes a number actionable -- an interrupt line number is
/// a machine's private business and a device name is not.
const VALUE_NAME: i16 = 16;
const NAME_BYTES: usize = 40;
const ROW_VALUE_BYTES: usize = 56;

/// The in-flight array holds one `u64` per cpu, at index zero.
const IN_FLIGHT_SLOT: i32 = 0;

const LOG2: Log2Labels = [
    "irq_log2_32",
    "irq_log2_16",
    "irq_log2_8",
    "irq_log2_4",
    "irq_log2_2",
    "irq_log2_1",
    "irq_log2_done",
];

const LICENSE: &str = "GPL";

/// One `(cpu, irq, bucket)` row's handler runs over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrqRun {
    pub cpu: u32,
    pub irq: u32,
    pub bucket: u32,
    pub count: u64,
    pub total_ns: u64,
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
            bucket: word(key, KEY_BUCKET as usize),
            count: long(value, VALUE_COUNT as usize),
            total_ns: long(value, VALUE_TOTAL_NS as usize),
            name: text_of(&value[VALUE_NAME as usize..VALUE_NAME as usize + NAME_BYTES]),
        })
    }
}

fn check(btf: &Btf, tracepoint: &str, expected: &[&str]) -> Result<(), String> {
    let signature = btf.tracepoint_signature(tracepoint)?;
    if signature != expected {
        return Err(format!(
            "this kernel declares `{tracepoint}({})`, and this program is written for \
             `{tracepoint}({})`: it reads the interrupt number and its action out of fixed \
             slots, and against another prototype those slots hold something else",
            signature.join(", "),
            expected.join(", "),
        ));
    }
    Ok(())
}

/// Puts a pointer to this cpu's in-flight slot in `r0`.
fn open_in_flight(program: &mut Program, in_flight_fd: i32) {
    program.push(Insn::st_imm(BPF_W, R10, TS_KEY, IN_FLIGHT_SLOT));
    program.extend(Insn::load_map_fd(1, in_flight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, TS_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
}

/// The program on `irq_handler_entry`: a handler has the cpu.
fn entry_instructions(btf: &Btf, in_flight_fd: i32) -> Result<Vec<Insn>, String> {
    check(btf, "irq_handler_entry", &ENTRY_SIGNATURE)?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(7, 0));

    open_in_flight(&mut program, in_flight_fd);
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
    btf: &Btf,
    in_flight_fd: i32,
    rows_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    check(btf, "irq_handler_exit", &EXIT_SIGNATURE)?;
    let name_offset = btf.member_offset("irqaction", "name")?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    open_in_flight(&mut program, in_flight_fd);
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
    // r7 keeps the action, which is where the handler's name is.
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_ACTION));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_IRQ));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_IRQ, 1));

    // The cpu goes in the key before the bucket does: this call clobbers r1
    // through r5, and the bucket is computed in them. It is the cpu the
    // handler ran on, which is the one whose work it interrupted.
    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_CPU, 0));

    push_log2_bucket(&mut program, 9, &LOG2);
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_BUCKET, 3));

    program.extend(Insn::load_map_fd(1, rows_fd));
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
    read_field(&mut program, SCRATCH, 8, 7, name_offset);
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (ROW_VALUE + VALUE_NAME) as i32));
    program.push(Insn::mov64_imm(2, NAME_BYTES as i32));
    program.push(Insn::ldx(BPF_DW, 3, R10, SCRATCH));
    program.push(Insn::call(helper::PROBE_READ_KERNEL_STR));

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

/// Handler runs neither map could take, under the kernel's own reason.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IrqDrops {
    pub rows_full: u64,
    pub rows_busy: u64,
    pub backwards: u64,
    /// Exits whose entry this run never saw.
    pub unopened: u64,
}

/// Two loaded, attached programs and the maps they share.
pub struct IrqProgram {
    rows: Hash,
    drops: Counters,
    _in_flight: Counters,
    _entry_program: OwnedFd,
    _entry_link: OwnedFd,
    _exit_program: OwnedFd,
    _exit_link: OwnedFd,
}

impl IrqProgram {
    pub fn load() -> Result<Self, String> {
        let btf = super::kernel_btf()?;

        // A per-cpu array of one `u64`: this build's in-flight timestamp, one
        // per cpu, which is the whole state these two programs share.
        let in_flight = Counters::create(1, "omn_irq_open")
            .map_err(|error| map_failure("interrupt in-flight slot", error))?;
        let rows = Hash::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_irq_hist")
            .map_err(|error| map_failure("interrupt histogram map", error))?;
        let drops = Counters::create(COUNTER_SLOTS, "omn_irq_drop")
            .map_err(|error| map_failure("interrupt drop counter", error))?;

        let entry = entry_instructions(btf, in_flight.as_raw_fd())?;
        let exit = exit_instructions(
            btf,
            in_flight.as_raw_fd(),
            rows.as_raw_fd(),
            drops.as_raw_fd(),
        )?;

        let entry_program = load(&entry, "omn_irq_in")?;
        let exit_program = load(&exit, "omn_irq_out")?;

        // Entry first: an exit that finds no timestamp is counted and skipped,
        // where the reverse order would have nothing to stop a handler's
        // length being measured from zero.
        let entry_link =
            syscall::raw_tracepoint_open("irq_handler_entry", entry_program.as_raw_fd()).map_err(
                |error| format!("cannot attach to the `irq_handler_entry` tracepoint: {error}"),
            )?;
        let exit_link = syscall::raw_tracepoint_open("irq_handler_exit", exit_program.as_raw_fd())
            .map_err(|error| {
                format!("cannot attach to the `irq_handler_exit` tracepoint: {error}")
            })?;

        Ok(Self {
            rows,
            drops,
            _in_flight: in_flight,
            _entry_program: entry_program,
            _entry_link: entry_link,
            _exit_program: exit_program,
            _exit_link: exit_link,
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
        for (key, value) in &drained.entries {
            match IrqRun::decode(key, value) {
                Some(row) => rows.push(row),
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

    /// An interrupt handler is not running on behalf of the thread it
    /// interrupted, so nothing here may read the current task: doing so would
    /// charge a disk's interrupt to whatever happened to be on the cpu, which
    /// is the one attribution this source exists to avoid.
    #[test]
    fn neither_program_asks_who_was_running() {
        let Some(btf) = kernel_btf() else { return };
        for program in [
            entry_instructions(&btf, 3).expect("a program"),
            exit_instructions(&btf, 3, 4, 5).expect("a program"),
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
        let program = entry_instructions(&btf, 3).expect("a program");
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
        let program = exit_instructions(&btf, 3, 4, 5).expect("a program");
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
        let program = exit_instructions(&btf, 3, 4, 5).expect("a program");
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
        assert!(check(&btf, "irq_handler_exit", &["int"]).is_err());
    }

    #[test]
    fn every_byte_of_every_key_and_value_is_accounted_for() {
        let key = [(KEY_CPU, 4), (KEY_IRQ, 4), (KEY_BUCKET, 4)];
        let value = [
            (VALUE_COUNT, 8),
            (VALUE_TOTAL_NS, 8),
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
}
