// SPDX-License-Identifier: MIT OR GPL-2.0
//! The writeback programs: how long the flusher spent, and what it wrote.
//!
//! Deferred writeback is the part of a write nobody's program is waiting for.
//! An application's `write` returns as soon as the page is dirty; minutes later
//! a kworker walks the dirty list and pushes it to the device, and the cost of
//! that round appears in no process's accounting. `writeback_start` and
//! `writeback_written` bracket one such round, and the gap between them is what
//! this measures.
//!
//! **The pairing is the work item, and the round is not the inode.** Both
//! tracepoints are handed the same `struct wb_writeback_work *` for one turn of
//! `wb_writeback`'s loop, so that pointer pairs them. What it does *not* give is
//! which file was written: a round walks a superblock's dirty inodes and the
//! tracepoints see the work item, not the inodes. So this source answers "how
//! long did the flusher run and how many pages did it move", per device, and
//! the `file` key the catalog lists for it has no producer here. The collector
//! says so rather than emitting events with a key nobody filled.
//!
//! **`nr_pages` is a budget being spent, not a total.** `wb_writeback`
//! subtracts each round's progress from `work->nr_pages` before firing
//! `writeback_written`, so the difference between the two readings is exactly
//! the pages that round wrote. Reading either one alone would report the
//! *remaining* budget, which for a `sync` is `LONG_MAX` and for background
//! writeback is a number the kernel picked.

use std::os::fd::{AsRawFd, OwnedFd};

use super::insn::{helper, read_field, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash};
use super::ring::{ring_bytes, RingBuffer};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};
use crate::bpf::program::load_raw_tracepoint as load;

/// The prototype both programs are written against.
const SIGNATURE: [&str; 2] = ["bdi_writeback *", "wb_writeback_work *"];

/// How many writeback rounds may be open at once.
///
/// One per flusher thread per superblock, which on any machine is tens.
const MAX_OPEN: u32 = 1024;

const DROP_OPEN: u32 = 0;
const DROP_RING: u32 = 1;
/// A `writeback_written` whose `writeback_start` this run never saw. Not a
/// loss: the round began before the programs attached.
const UNPAIRED: u32 = 2;
const DROP_SLOTS: u32 = 3;

/// One writeback round, as the programs write it and userspace reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Writeback {
    /// When the round ended: `bpf_ktime_get_ns`, `CLOCK_MONOTONIC`.
    pub ts_ns: u64,
    /// How long it ran.
    pub duration_ns: u64,
    /// Pages it wrote, as the budget it spent.
    pub pages: i64,
    /// `super_block.s_dev` of the filesystem being flushed, or zero for a
    /// round that names no superblock -- background writeback over a whole
    /// backing device does not.
    pub dev: u32,
    /// `enum wb_reason`: why the flusher woke up. Carried as the kernel's own
    /// number and named in userspace.
    pub reason: u32,
    /// `enum writeback_sync_modes`: zero is `WB_SYNC_NONE`, one is
    /// `WB_SYNC_ALL`. A round that waits for the device and one that does not
    /// are different measurements.
    pub sync_mode: u32,
    pub cpu: u32,
    /// The backing device the flusher belongs to, by the kernel's own id.
    pub bdi_id: u64,
}

const RECORD_BYTES: usize = 48;

impl Writeback {
    /// The device's major number, out of the kernel's packed `dev_t`.
    pub fn major(&self) -> u32 {
        (self.dev >> 20) & 0xfff
    }

    pub fn minor(&self) -> u32 {
        self.dev & 0xf_ffff
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let long = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().ok().unwrap());
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().ok().unwrap());
        Some(Self {
            ts_ns: long(0),
            duration_ns: long(8),
            pages: long(16) as i64,
            dev: word(24),
            reason: word(28),
            sync_mode: word(32),
            cpu: word(36),
            bdi_id: long(40),
        })
    }
}

/// Stack offsets of the record.
const EVENT: i16 = -48;
const EV_TS: i16 = EVENT;
const EV_DURATION: i16 = EVENT + 8;
const EV_PAGES: i16 = EVENT + 16;
const EV_DEV: i16 = EVENT + 24;
const EV_REASON: i16 = EVENT + 28;
const EV_SYNC_MODE: i16 = EVENT + 32;
const EV_CPU: i16 = EVENT + 36;
const EV_BDI: i16 = EVENT + 40;

/// The open entry: when the round started and what its budget was then.
const ENTRY_VALUE: i16 = EVENT - 16;
const VAL_START_TS: i16 = 0;
const VAL_PAGES: i16 = 8;
const ENTRY_KEY: i16 = EVENT - 24;
const DROP_KEY: i16 = EVENT - 32;
/// Scratch for a pointer read out of a kernel structure.
const SCRATCH: i16 = EVENT - 40;

const ENTRY_KEY_BYTES: usize = 8;
const ENTRY_VALUE_BYTES: usize = 16;

fn count(program: &mut Program, counters_fd: i32, slot: u32, resume: &'static str) {
    program.push(Insn::st_imm(BPF_W, R10, DROP_KEY, slot as i32));
    program.extend(Insn::load_map_fd(1, counters_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, DROP_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to(resume, Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, 0));
    program.push(Insn::add64_imm(1, 1));
    program.push(Insn::stx(BPF_DW, 0, 0, 1));
}

/// `writeback_start`: the round begins. Remember when, and what it may spend.
fn start_instructions(
    btf: &super::btf::Btf,
    open_fd: i32,
    counters_fd: i32,
) -> Result<Vec<Insn>, String> {
    crate::bpf::program::prototype(
        btf,
        "writeback_start",
        &[&SIGNATURE],
        "the flusher and the work item",
    )?;
    let nr_pages = btf.member_offset("wb_writeback_work", "nr_pages")?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    // The work item is the second argument and the identity of the round.
    program.push(Insn::ldx(BPF_DW, 7, 6, 8));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, ENTRY_VALUE + VAL_START_TS, 0));
    read_field(&mut program, ENTRY_VALUE + VAL_PAGES, 8, 7, nr_pages);

    program.push(Insn::stx(BPF_DW, R10, ENTRY_KEY, 7));
    program.extend(Insn::load_map_fd(1, open_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ENTRY_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ENTRY_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count(&mut program, counters_fd, DROP_OPEN, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// `writeback_written`: the round ends. Subtract, describe, publish.
fn written_instructions(
    btf: &super::btf::Btf,
    open_fd: i32,
    ring_fd: i32,
    counters_fd: i32,
) -> Result<Vec<Insn>, String> {
    crate::bpf::program::prototype(
        btf,
        "writeback_written",
        &[&SIGNATURE],
        "the flusher and the work item",
    )?;
    let nr_pages = btf.member_offset("wb_writeback_work", "nr_pages")?;
    let sb = btf.member_offset("wb_writeback_work", "sb")?;
    let reason = btf.member_offset("wb_writeback_work", "reason")?;
    let sync_mode = btf.member_offset("wb_writeback_work", "sync_mode")?;
    let s_dev = btf.member_offset("super_block", "s_dev")?;
    let wb_bdi = btf.member_offset("bdi_writeback", "bdi")?;
    let bdi_id = btf.member_offset("backing_dev_info", "id")?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 8));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    for slot in 0..(RECORD_BYTES / 8) as i16 {
        program.push(Insn::st_imm(BPF_DW, R10, EVENT + slot * 8, 0));
    }
    program.push(Insn::stx(BPF_DW, R10, EV_TS, 8));
    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, EV_CPU, 0));

    read_field(&mut program, EV_REASON, 4, 7, reason);
    read_field(&mut program, EV_SYNC_MODE, 4, 7, sync_mode);

    // The superblock is null for a round that flushes a whole backing device
    // rather than one filesystem, which is what background writeback does.
    read_field(&mut program, SCRATCH, 8, 7, sb);
    program.push(Insn::ldx(BPF_DW, 9, R10, SCRATCH));
    program.jump_to("no_super_block", Insn::jeq_imm(9, 0, 0));
    read_field(&mut program, EV_DEV, 4, 9, s_dev);
    program.label("no_super_block");

    // The backing device, through the flusher in the first argument.
    program.push(Insn::ldx(BPF_DW, 9, 6, 0));
    read_field(&mut program, SCRATCH, 8, 9, wb_bdi);
    program.push(Insn::ldx(BPF_DW, 9, R10, SCRATCH));
    program.jump_to("no_backing_device", Insn::jeq_imm(9, 0, 0));
    read_field(&mut program, EV_BDI, 8, 9, bdi_id);
    program.label("no_backing_device");

    program.push(Insn::stx(BPF_DW, R10, ENTRY_KEY, 7));
    program.extend(Insn::load_map_fd(1, open_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ENTRY_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("unpaired", Insn::jeq_imm(0, 0, 0));

    // Copied out before the delete, which invalidates the pointer.
    program.push(Insn::ldx(BPF_DW, 9, 0, VAL_START_TS));
    program.push(Insn::ldx(BPF_DW, 1, 0, VAL_PAGES));
    program.push(Insn::stx(BPF_DW, R10, EV_PAGES, 1));

    program.extend(Insn::load_map_fd(1, open_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ENTRY_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::stx(BPF_DW, R10, EV_DURATION, 1));

    // The pages the round actually wrote: the budget it had at the start,
    // minus what is left now. Reading either alone reports the remainder,
    // which for a `sync` is LONG_MAX.
    read_field(&mut program, SCRATCH, 8, 7, nr_pages);
    program.push(Insn::ldx(BPF_DW, 1, R10, EV_PAGES));
    program.push(Insn::ldx(BPF_DW, 2, R10, SCRATCH));
    program.push(Insn::sub64_reg(1, 2));
    program.push(Insn::stx(BPF_DW, R10, EV_PAGES, 1));

    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EVENT as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count(&mut program, counters_fd, DROP_RING, "done");
    program.jump_to("done", Insn::ja());

    program.label("unpaired");
    count(&mut program, counters_fd, UNPAIRED, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Two loaded, attached programs and the maps between them.
pub struct WritebackProgram {
    ring: RingBuffer,
    counters: Counters,
    open: Hash,
    start_program: OwnedFd,
    _start_link: OwnedFd,
    written_program: OwnedFd,
    _written_link: OwnedFd,
}

impl WritebackProgram {
    pub fn load(requested_ring_bytes: usize) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let bytes = ring_bytes(requested_ring_bytes);

        let ring_map = syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_wb_ring")
            .map_err(|error| map_failure("writeback ring buffer", error))?;
        let counters = Counters::create(DROP_SLOTS, "omn_wb_count")
            .map_err(|error| map_failure("writeback drop counter", error))?;
        let open = Hash::create(ENTRY_KEY_BYTES, ENTRY_VALUE_BYTES, MAX_OPEN, "omn_wb_open")
            .map_err(|error| map_failure("writeback round map", error))?;

        let start = start_instructions(btf, open.as_raw_fd(), counters.as_raw_fd())?;
        let written = written_instructions(
            btf,
            open.as_raw_fd(),
            ring_map.as_raw_fd(),
            counters.as_raw_fd(),
        )?;

        let start_program = load(&start, "omn_wb_start")?;
        let written_program = load(&written, "omn_wb_end")?;

        // The start is attached first: a round recorded with no end attached
        // ages out of the map, whereas an end attached first would look for a
        // start nobody wrote -- which is the lookup that fails, and the only
        // thing between that and a duration measured from nothing.
        let start_link = crate::bpf::program::attach_raw("writeback_start", &start_program)?;
        let written_link = crate::bpf::program::attach_raw("writeback_written", &written_program)?;

        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the writeback ring buffer: {error}"))?;

        Ok(Self {
            ring,
            counters,
            open,
            start_program,
            _start_link: start_link,
            written_program,
            _written_link: written_link,
        })
    }

    pub fn drain(&mut self, mut handle: impl FnMut(Writeback), undecoded: &mut u64) {
        self.ring.drain(|bytes| match Writeback::from_bytes(bytes) {
            Some(record) => handle(record),
            None => *undecoded += 1,
        });
    }

    /// Runs of these programs the kernel skipped because one was already
    /// running on that cpu; see `BlockProgram::skipped`, where it was measured.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.start_program, &self.written_program])
    }

    /// Rounds the map could not hold, and records the ring could not hold.
    pub fn dropped(&self) -> (u64, u64) {
        (self.counters.read(DROP_OPEN), self.counters.read(DROP_RING))
    }

    /// Ends whose start this run never saw.
    pub fn unpaired(&self) -> u64 {
        self.counters.read(UNPAIRED)
    }

    /// Rounds still running when the run ended.
    pub fn still_running(&self) -> usize {
        self.open.len().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel_btf() -> Option<&'static super::super::btf::Btf> {
        match super::super::kernel_btf() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    /// The program writes fields at stack offsets and `from_bytes` reads them
    /// at struct offsets, and nothing else connects the two.
    #[test]
    fn the_record_struct_matches_what_the_programs_write() {
        use std::mem::offset_of;
        assert_eq!(std::mem::size_of::<Writeback>(), RECORD_BYTES);
        for (stack, field, name) in [
            (EV_TS, offset_of!(Writeback, ts_ns), "ts_ns"),
            (EV_DURATION, offset_of!(Writeback, duration_ns), "duration"),
            (EV_PAGES, offset_of!(Writeback, pages), "pages"),
            (EV_DEV, offset_of!(Writeback, dev), "dev"),
            (EV_REASON, offset_of!(Writeback, reason), "reason"),
            (EV_SYNC_MODE, offset_of!(Writeback, sync_mode), "sync_mode"),
            (EV_CPU, offset_of!(Writeback, cpu), "cpu"),
            (EV_BDI, offset_of!(Writeback, bdi_id), "bdi_id"),
        ] {
            assert_eq!(
                (stack - EVENT) as usize,
                field,
                "the programs write `{name}` where the struct does not read it"
            );
        }
    }

    #[test]
    fn the_running_kernel_declares_the_prototype_these_programs_assume() {
        let Some(btf) = kernel_btf() else { return };
        for tracepoint in ["writeback_start", "writeback_written"] {
            crate::bpf::program::prototype(
                btf,
                tracepoint,
                &[&SIGNATURE],
                "the flusher and the work item",
            )
            .unwrap_or_else(|reason| panic!("{tracepoint}: {reason}"));
        }
    }

    /// Invariant 6: one clock domain, and it is not the boot clock.
    #[test]
    fn the_programs_read_the_monotonic_clock_and_no_other() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        for program in [
            start_instructions(btf, 3, 4).expect("a program"),
            written_instructions(btf, 3, 4, 5).expect("a program"),
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

    /// `nr_pages` is a budget the round spends, so it is read twice and
    /// subtracted. Reading it once would report the remainder -- `LONG_MAX`
    /// for a `sync` -- as the pages written.
    #[test]
    fn the_pages_written_are_a_difference_of_two_readings_of_the_budget() {
        let Some(btf) = kernel_btf() else { return };
        let program = written_instructions(btf, 3, 4, 5).expect("a program");
        let subtractions = program.iter().filter(|insn| insn.code == 0x1f).count();
        assert_eq!(
            subtractions, 2,
            "one subtraction for the duration and one for the budget spent"
        );
    }

    #[test]
    fn the_published_record_is_exactly_the_size_of_the_struct() {
        let Some(btf) = kernel_btf() else { return };
        let program = written_instructions(btf, 3, 4, 5).expect("a program");
        let output_at = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT)
            .expect("the program publishes its record");
        let length = program[..output_at]
            .iter()
            .rev()
            .find(|insn| insn.regs & 0xf == 3 && insn.code == 0xb7)
            .expect("a length is moved into r3");
        assert_eq!(length.imm as usize, std::mem::size_of::<Writeback>());
    }
}
