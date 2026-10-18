// SPDX-License-Identifier: MIT OR GPL-2.0
//! `sched.runqlen`: each cpu's run queue length, sampled on its own clock.
//!
//! `/proc/stat`'s `procs_running` is the machine's sum, and the per-cpu queue
//! is exported only in debugfs. `runqlen` (bcc, bpftrace) answers it the way
//! this does: a program on a cpu-clock event, firing about 99 times a second
//! on every cpu, reads the cfs run queue of the task it interrupted --
//! `current->se.cfs_rq->nr_running` -- and counts the length in that cpu's
//! slot of a per-cpu histogram. One less than `nr_running` when it is not
//! zero, since the task running is in it: 0 is a cpu with nothing waiting, 1
//! one task waiting for it.
//!
//! Sampled, not traced: a queue that filled and drained between two ticks is
//! not seen, and the histogram is the share of ticks at each length -- what
//! runqlen prints.

use super::btf::Btf;
use super::insn::{helper, read_field, read_pointer, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters};

/// How many lengths are told apart; a longer queue is counted in the last.
pub const LENGTHS: u32 = 64;

/// How often each cpu is looked at.
pub const HZ: u64 = 99;

/// `bpf_get_current_task`.
const GET_CURRENT_TASK: i32 = 35;

const KEY: i16 = -8;
const SCRATCH: i16 = -16;
const LENGTH: i16 = -24;

/// Where the queue length is in a `cfs_rq`: `nr_running` until 6.14, which
/// renamed it `nr_queued`.
fn queue_length_at(btf: &Btf) -> Result<i32, String> {
    btf.member_offset("cfs_rq", "nr_running")
        .or_else(|_| btf.member_offset("cfs_rq", "nr_queued"))
}

fn instructions(btf: &Btf, histogram_fd: i32) -> Result<Vec<Insn>, String> {
    let cfs_rq_at =
        btf.member_offset("task_struct", "se")? + btf.member_offset("sched_entity", "cfs_rq")?;
    let length_at = queue_length_at(btf)?;

    let mut program = Program::new();
    program.push(Insn::call(GET_CURRENT_TASK));
    program.push(Insn::mov64_reg(7, 0));
    read_pointer(&mut program, SCRATCH, 8, 7, cfs_rq_at);
    // A task with no cfs run queue (a real-time or a deadline one) says
    // nothing about the cfs queue; the tick is not counted.
    program.jump_to("done", Insn::jeq_imm(8, 0, 0));
    program.push(Insn::st_imm(BPF_DW, R10, LENGTH, 0));
    read_field(&mut program, LENGTH, 4, 8, length_at);
    program.push(Insn::ldx(BPF_W, 1, R10, LENGTH));
    // The task running is one of `nr_running`.
    program.jump_to("counted", Insn::jeq_imm(1, 0, 0));
    program.push(Insn::add64_imm(1, -1));
    program.label("counted");
    program.push(Insn::mov64_imm(2, (LENGTHS - 1) as i32));
    program.jump_to("in_range", Insn::jge_reg(2, 1, 0));
    program.push(Insn::mov64_imm(1, (LENGTHS - 1) as i32));
    program.label("in_range");
    program.push(Insn::stx(BPF_W, R10, KEY, 1));
    program.extend(Insn::load_map_fd(1, histogram_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    // Per-cpu: this cpu's slot is its own, and the program cannot be
    // re-entered on it, so the increment needs no atomic.
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

/// The program on every cpu's clock, and the per-cpu histogram it fills.
pub struct RunqLenProgram {
    histogram: Counters,
    attachment: super::attach::PerfAttachment,
}

impl RunqLenProgram {
    pub fn load() -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let histogram = Counters::create(LENGTHS, "omn_runqlen")
            .map_err(|error| map_failure("run queue length histogram", error))?;
        let instructions = instructions(btf, histogram.as_raw_fd())?;
        let attachment = super::attach::cpu_clock(HZ, &instructions, "omn_runqlen")?;
        Ok(Self {
            histogram,
            attachment,
        })
    }

    /// Ticks so far, by length then by cpu: `ticks[length][cpu]`.
    pub fn ticks(&self) -> Vec<Vec<u64>> {
        (0..LENGTHS)
            .map(|length| self.histogram.read_per_cpu(length))
            .collect()
    }

    /// Runs the kernel skipped because the program was already running on
    /// that cpu.
    pub fn skipped(&self) -> u64 {
        super::attach::fentry::skipped([self.attachment.program()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program builds against this kernel's `task_struct` and `cfs_rq`,
    /// and caps the length before it looks the slot up.
    #[test]
    fn the_program_builds_against_this_kernel() {
        let Ok(btf) = super::super::kernel_btf() else {
            eprintln!("skipped: no kernel BTF");
            return;
        };
        queue_length_at(btf).expect("a queue length member");
        let program = instructions(btf, 3).expect("a program");
        let cap = program
            .iter()
            .position(|insn| insn.imm == (LENGTHS - 1) as i32)
            .expect("the cap");
        let lookup = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::MAP_LOOKUP_ELEM)
            .expect("the lookup");
        assert!(cap < lookup);
    }
}
