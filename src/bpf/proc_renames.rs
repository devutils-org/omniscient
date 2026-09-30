// SPDX-License-Identifier: MIT OR GPL-2.0
//! The names tasks took at `exec`, for the programs that read a name once.
//!
//! `sched.offcpu`, `sched.runqlat` and `sched.migrations` read `task_struct.comm`
//! once, when a row is first put in their map, rather than on every event --
//! deliberately, because that path runs tens of thousands of times a second and
//! a name does not change. Except at `exec`, which is exactly the moment a
//! launched target acquires the name anyone would recognise: the child is held
//! at the launch gate before its first instruction, so it is still a fork of
//! `omn` when it first joins a run queue, and a run shorter than a drain tick
//! reported every one of its rows as `comm=omn` (`docs/open-findings.md`,
//! finding 20).
//!
//! Of the three ways out the finding lists, this is the one it calls correct:
//! `sched_process_exec` is attached beside the program, and records the new
//! name of every task that exec'd, by thread id, in a small LRU map. The
//! collector drains it after its rows and gives each row of a renamed task the
//! name it had at the end of the window. Reading the name on every event
//! would cost the hot path a helper call for a change that happens once in a
//! task's life; reading `/proc/<tid>/comm` at the drain answers for whoever
//! holds the id then, and for a short-lived program -- the finding's own
//! example -- for nobody.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{
    helper, push_task_filter, read_field, Insn, Program, Subject, TaskFields, Whose, BPF_DW, R10,
};
use super::syscall;

/// `BPF_MAP_TYPE_LRU_HASH`: a machine that execs faster than the drain keeps
/// the most recent names rather than refusing new ones.
const BPF_MAP_TYPE_LRU_HASH: u32 = 9;

/// How many renamed tasks one drain interval can carry.
const MAX_RENAMES: u32 = 4096;

const LICENSE: &str = "Dual MIT/GPL";

/// `sched_process_exec(task_struct *, pid_t, linux_binprm *)`.
const SIGNATURE: [&str; 3] = ["task_struct *", "pid_t", "linux_binprm *"];

const COMM_BYTES: usize = 16;

/// Stack layout: the key is the thread id in an eight-byte slot, the thread
/// group beside it for the target comparison, then the value.
const KEY: i16 = -8;
const TGID: i16 = -16;
const VALUE: i16 = -32;
const FILTER_SCRATCH: i16 = -48;

fn instructions(btf: &Btf, whose: Whose, renames_fd: i32) -> Result<Vec<Insn>, String> {
    let signature = btf.tracepoint_signature("sched_process_exec")?;
    if signature != SIGNATURE {
        return Err(format!(
            "this kernel declares `sched_process_exec({})`, and this program is written for \
             `sched_process_exec({})`",
            signature.join(", "),
            SIGNATURE.join(", "),
        ));
    }
    let fields = TaskFields::locate(btf, whose)?;
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    // Every byte of a key is hashed, so the slot is cleared before the four
    // the thread id fills.
    program.push(Insn::st_imm(BPF_DW, R10, KEY, 0));
    read_field(&mut program, KEY, 4, 7, pid_offset);
    program.push(Insn::st_imm(BPF_DW, R10, TGID, 0));
    read_field(&mut program, TGID, 4, 7, tgid_offset);
    push_task_filter(
        &mut program,
        &fields,
        whose,
        Subject::with_ids(7, KEY, TGID),
        FILTER_SCRATCH,
        "done",
    );
    // The name `begin_new_exec` has just given the task.
    read_field(&mut program, VALUE, COMM_BYTES as i32, 7, comm_offset);
    program.extend(Insn::load_map_fd(1, renames_fd));
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

/// A loaded, attached exec program and the map of new names it writes.
pub struct Renames {
    map: OwnedFd,
    program: OwnedFd,
    _link: OwnedFd,
    /// What the last drain took, applied once more: see [`Renames::take`].
    previous: HashMap<u32, String>,
}

impl Renames {
    /// Runs the kernel skipped because the program was already running on
    /// that cpu: an exec whose new name this run did not record, so the rows
    /// of that task keep the name they were made with.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.program])
    }

    /// Loads and attaches it, narrowed to the same tasks as the program it
    /// corrects.
    pub fn load(whose: Whose) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let map = syscall::map_create(
            BPF_MAP_TYPE_LRU_HASH,
            4,
            COMM_BYTES as u32,
            MAX_RENAMES,
            "omn_renames",
        )
        .map_err(|error| super::map::map_failure("exec name map", error))?;
        let instructions = instructions(btf, whose, map.as_raw_fd())?;
        let program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &instructions,
            "omn_renames",
            LICENSE,
        )
        .map_err(|error| format!("the kernel refused the exec name program: {error}"))?;
        let link = syscall::raw_tracepoint_open("sched_process_exec", program.as_raw_fd())
            .map_err(|error| {
                format!("cannot attach to the `sched_process_exec` tracepoint: {error}")
            })?;
        Ok(Self {
            map,
            program,
            _link: link,
            previous: HashMap::new(),
        })
    }

    /// The names to give this drain's rows, by thread id.
    ///
    /// Every name recorded since the last drain, and the ones the last drain
    /// took as well. The second set is not a repeat: a drain retires the row
    /// map, waits a grace period and then reads it, and this is taken after
    /// that, so an exec in the grace period is taken by this drain while the
    /// rows its task made before it are in the map the *next* drain reads.
    /// Applying a name twice to a task that already has it changes nothing,
    /// and a task that exec'd again is given the newer name, which is applied
    /// last.
    pub fn take(&mut self) -> HashMap<u32, String> {
        let fd = self.map.as_raw_fd();
        let mut fresh = HashMap::new();
        let mut from: Option<Vec<u8>> = None;
        // Bounded by the map's size: an LRU map evicts rather than grows.
        for _ in 0..MAX_RENAMES {
            let Ok(Some(key)) = syscall::map_next_key(fd, from.as_deref(), 4) else {
                break;
            };
            if let Ok(Some(value)) = syscall::map_lookup(fd, &key, COMM_BYTES) {
                let tid = u32::from_ne_bytes(key[..4].try_into().expect("4"));
                fresh.insert(tid, super::sched_common::text_of(&value));
            }
            from = Some(key);
        }
        for tid in fresh.keys() {
            let _ = syscall::map_take(fd, &tid.to_ne_bytes(), COMM_BYTES);
        }
        let mut names = std::mem::replace(&mut self.previous, fresh.clone());
        names.extend(fresh);
        names
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

    /// The program reads the name out of the task and nothing out of the
    /// `linux_binprm`, and a target is compared before the name is read.
    #[test]
    fn the_name_is_read_after_the_target_is_compared() {
        let Some(btf) = kernel_btf() else { return };
        let program = instructions(&btf, Whose::Process(4242), 3).expect("a program");
        let compared = program
            .iter()
            .position(|insn| insn.imm == 4242)
            .expect("the comparison");
        let updated = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::MAP_UPDATE_ELEM)
            .expect("the update");
        assert!(compared < updated);
        let machine = instructions(&btf, Whose::Any, 3).expect("a program");
        assert!(!machine.iter().any(|insn| insn.imm == 4242));
    }
}
