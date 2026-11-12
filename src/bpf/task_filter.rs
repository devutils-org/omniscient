//! Which tasks' samples a per-cpu sampled event lets into its ring.
//!
//! A profile of one process follows each of its threads with an event and a
//! ring of its own (`perf::TaskSampler`). A process already running a hundred
//! and fifty threads when the profile begins -- the late window of the ZML
//! campaign's model server -- needs a hundred and fifty rings at once, and the
//! kernel charges them all to one budget, `perf_event_mlock_kb` per online
//! cpu and then `RLIMIT_MEMLOCK`: the open is refused, and the only way
//! forward used to be a system setting.
//!
//! An event per cpu needs one ring per cpu whatever the thread count, and sees
//! every task that runs there. This program, attached to each one
//! (`PERF_EVENT_IOC_SET_BPF`), answers for every sample: zero drops it before
//! it is written, non-zero lets it through. It keeps a sample when the task
//! it was taken in is in a map userspace fills -- the process's thread group,
//! and those of the processes it started, by the kernel's numbering, or one
//! thread alone.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use perf_event_open_sys as sys;

use super::insn::{helper, Insn, Program, BPF_W, R10};
use super::map::{map_failure, Hash};
use super::syscall;

/// The thread groups (or threads) one profile may follow.
const MAX_TASKS: u32 = 4096;

const KEY: i16 = -8;

/// What the filter compares a sample's task by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    /// The thread group: every thread of each process in the map.
    Process,
    /// The thread itself.
    Thread,
}

fn instructions(by: By, tasks_fd: RawFd) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    if by == By::Process {
        program.push(Insn::rsh64_imm(0, 32));
    }
    // The low 32 bits: the thread group after the shift, the thread without.
    program.push(Insn::stx(BPF_W, R10, KEY, 0));
    program.extend(Insn::load_map_fd(1, tasks_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("drop", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::mov64_imm(0, 1));
    program.push(Insn::exit());
    program.label("drop");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program and the map of tasks it keeps.
pub struct TaskFilter {
    tasks: Hash,
    program: OwnedFd,
    by: By,
}

impl TaskFilter {
    pub fn load(by: By) -> Result<Self, String> {
        let tasks = Hash::create(4, 1, MAX_TASKS, "omn_prof_tasks")
            .map_err(|error| map_failure("profile task filter", error))?;
        let instructions = instructions(by, tasks.as_raw_fd())?;
        let program = super::program::load(
            syscall::BPF_PROG_TYPE_PERF_EVENT,
            &instructions,
            "omn_prof_filter",
        )?;
        Ok(Self { tasks, program, by })
    }

    pub fn by(&self) -> By {
        self.by
    }

    /// Lets the samples of `kernel_id` through: a thread group or a thread,
    /// as [`By`] says, by the kernel's numbering (`bpf::pidns`).
    pub fn allow(&self, kernel_id: u32) -> io::Result<()> {
        syscall::map_update(self.tasks.as_raw_fd(), &kernel_id.to_ne_bytes(), &[1])
    }

    /// Hangs the program off one sampled event, before it is enabled.
    pub fn attach(&self, event_fd: RawFd) -> Result<(), String> {
        // SAFETY: two descriptors this process holds; the kernel reads them.
        if unsafe { sys::ioctls::SET_BPF(event_fd, self.program.as_raw_fd() as u32) } < 0 {
            return Err(format!(
                "the kernel refused to attach the profile's task filter to a sampled event: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}
