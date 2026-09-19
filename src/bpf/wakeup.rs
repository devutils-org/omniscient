//! The `sched_waking` program: who unblocks whom.
//!
//! It attaches to `sched_waking` rather than to `sched_wakeup`, and the
//! difference is the entire point of the source. `sched_waking` runs in the
//! context of the task doing the waking, so `bpf_get_current_pid_tgid` is the
//! waker. `sched_wakeup` can run on the woken task's cpu, after an inter
//! processor interrupt, where the current task is whatever happened to be
//! running there -- a waker attribution taken from it would be wrong exactly on
//! the cross-cpu wakeups that matter most.
//!
//! What comes out is one record per wakeup: when, who woke, who was woken, and
//! on which cpu the wakeup was issued. That is the raw material of the `WAKEUP`
//! join: the only way to carry latency across a block without instrumenting the
//! application.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::Counters;
use super::ring::{ring_bytes, RingBuffer};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// One wakeup, as the program writes it and userspace reads it.
///
/// `repr(C)` on both sides of the ring is the contract: the program writes
/// these fields at these offsets, and `from_bytes` below reads them back.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wakeup {
    /// `bpf_ktime_get_ns`, which is `CLOCK_MONOTONIC` -- never
    /// `bpf_ktime_get_boot_ns`, which includes suspend time and would put this
    /// stream in a different domain from every other collector.
    pub ts_ns: u64,
    pub waker_tid: u32,
    pub waker_pid: u32,
    pub wakee_tid: u32,
    pub wakee_pid: u32,
    /// The cpu the wakeup was issued on, which is the waker's.
    pub cpu: u32,
    pad: u32,
    comm: [u8; 16],
}

/// The size of the record the program writes. Stated once, used by the program
/// and checked against the struct.
const RECORD_BYTES: usize = 48;

impl Wakeup {
    /// The woken task's name, as the kernel holds it.
    pub fn wakee_comm(&self) -> String {
        let end = self
            .comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.comm.len());
        String::from_utf8_lossy(&self.comm[..end]).into_owned()
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().ok().unwrap());
        let mut comm = [0u8; 16];
        comm.copy_from_slice(&bytes[32..48]);
        Some(Self {
            ts_ns: u64::from_ne_bytes(bytes[0..8].try_into().ok()?),
            waker_tid: word(8),
            waker_pid: word(12),
            wakee_tid: word(16),
            wakee_pid: word(20),
            cpu: word(24),
            pad: 0,
            comm,
        })
    }
}

/// Stack offsets, negative from the frame pointer, matching `Wakeup`.
const EVENT: i16 = -48;
const WAKER_TID: i16 = EVENT + 8;
const WAKER_PID: i16 = EVENT + 12;
const WAKEE_TID: i16 = EVENT + 16;
const WAKEE_PID: i16 = EVENT + 20;
const CPU: i16 = EVENT + 24;
const PAD: i16 = EVENT + 28;
const COMM: i16 = EVENT + 32;
/// The one-word key of the drop counter, below the record.
const DROP_KEY: i16 = EVENT - 8;

/// Builds the program for a target, or for the whole machine.
///
/// The target's thread group id is written into the instruction stream rather
/// than kept in a map: it is known before the program is loaded and never
/// changes during a run, so a map lookup per wakeup would buy nothing and cost
/// something on a path that fires tens of thousands of times a second.
fn instructions(
    btf: &Btf,
    target_tgid: Option<u32>,
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;

    let mut program = Program::new();
    // r6 keeps the context and r7 the task being woken; both survive helper
    // calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    // The first argument of `sched_waking` is the task_struct being woken.
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, EVENT, 0));

    // The low half is the thread id, the high half the process id.
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, WAKER_TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, WAKER_PID, 0));

    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, CPU, 0));
    // Every byte of the record is written before it is published; a padding
    // word left uninitialised is stack garbage from whatever ran before.
    program.push(Insn::st_imm(BPF_W, R10, PAD, 0));

    for (into, size, field_offset) in [
        (WAKEE_TID, 4, pid_offset),
        (WAKEE_PID, 4, tgid_offset),
        (COMM, 16, comm_offset),
    ] {
        program.push(Insn::mov64_reg(1, R10));
        program.push(Insn::add64_imm(1, into as i32));
        program.push(Insn::mov64_imm(2, size));
        program.push(Insn::mov64_reg(3, 7));
        program.push(Insn::add64_imm(3, field_offset));
        program.push(Insn::call(helper::PROBE_READ_KERNEL));
    }

    // A run observes one process, so a wakeup is kept when either end of it
    // belongs to that process: the wakeups it receives are its blocked time
    // ending, the ones it sends are the blocked time it ends for others.
    if let Some(tgid) = target_tgid {
        program.push(Insn::ldx(BPF_W, 1, R10, WAKEE_PID));
        program.jump_to("emit", Insn::jeq_imm(1, tgid as i32, 0));
        program.push(Insn::ldx(BPF_W, 1, R10, WAKER_PID));
        program.jump_to("done", Insn::jne_imm(1, tgid as i32, 0));
    }

    program.label("emit");
    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EVENT as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));

    // A full ring returns an error and the wakeup is gone. Nothing else would
    // ever record that, so the program counts it: a correlation missing a third
    // of its evidence must not look like a correlation that found nothing.
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    program.push(Insn::st_imm(BPF_W, R10, DROP_KEY, 0));
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
    // Checked before the kernel sees it: a structural mistake here comes back
    // from the verifier as a register dump, and this is a program nobody
    // compiled, so nothing else would have caught it.
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// A loaded, attached wakeup program and the two maps it writes.
///
/// Dropping it detaches: the link, the program and the maps are file
/// descriptors, and the kernel unloads the program when the last one closes.
pub struct WakeupProgram {
    ring: RingBuffer,
    drops: Counters,
    _program: OwnedFd,
    _link: OwnedFd,
}

/// The license the program declares.
///
/// `bpf_probe_read_kernel` is one of the helpers the kernel marks GPL-only, and
/// reading a field out of a `task_struct` is not possible without it. The
/// declaration covers the instruction stream in this file, which is what the
/// kernel is asking about; `omn` itself stays under its own license.
const LICENSE: &str = "GPL";

impl WakeupProgram {
    /// Loads and attaches the program, or says why the kernel refused.
    pub fn load(target_tgid: Option<u32>, requested_ring_bytes: usize) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let bytes = ring_bytes(requested_ring_bytes);

        let ring_map = syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_wakeups")
            .map_err(|error| format!("cannot create the wakeup ring buffer: {error}"))?;
        let drops = Counters::create(1, "omn_wake_drop")
            .map_err(|error| format!("cannot create the drop counter: {error}"))?;

        let instructions = instructions(btf, target_tgid, ring_map.as_raw_fd(), drops.as_raw_fd())?;
        let program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &instructions,
            "omn_waking",
            LICENSE,
        )
        .map_err(|error| {
            if error.is_permission() {
                super::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the wakeup program: {error}")
            }
        })?;

        let link = syscall::raw_tracepoint_open("sched_waking", program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sched_waking` tracepoint: {error}"))?;

        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the wakeup ring buffer: {error}"))?;

        Ok(Self {
            ring,
            drops,
            _program: program,
            _link: link,
        })
    }

    /// Hands every complete wakeup to `handle`, and counts the records that
    /// were not the size this reader expects rather than reinterpreting them.
    pub fn drain(&mut self, mut handle: impl FnMut(Wakeup), undecoded: &mut u64) {
        self.ring.drain(|bytes| match Wakeup::from_bytes(bytes) {
            Some(wakeup) => handle(wakeup),
            None => *undecoded += 1,
        });
    }

    /// Wakeups the program could not publish because the ring was full.
    ///
    /// Summed across cpus, because the counter is per-cpu to keep the fast path
    /// free of atomics.
    pub fn dropped(&self) -> u64 {
        self.drops.read(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program writes fields at stack offsets and `from_bytes` reads them
    /// at struct offsets. Nothing but this test connects the two, and a
    /// mismatch would be a field quietly read from the wrong place.
    #[test]
    fn the_record_struct_matches_what_the_program_writes() {
        use std::mem::offset_of;
        assert_eq!(std::mem::size_of::<Wakeup>(), RECORD_BYTES);
        for (stack, field, name) in [
            (WAKER_TID, offset_of!(Wakeup, waker_tid), "waker_tid"),
            (WAKER_PID, offset_of!(Wakeup, waker_pid), "waker_pid"),
            (WAKEE_TID, offset_of!(Wakeup, wakee_tid), "wakee_tid"),
            (WAKEE_PID, offset_of!(Wakeup, wakee_pid), "wakee_pid"),
            (CPU, offset_of!(Wakeup, cpu), "cpu"),
            (COMM, offset_of!(Wakeup, comm), "comm"),
        ] {
            assert_eq!(
                (stack - EVENT) as usize,
                field,
                "the program writes `{name}` where the struct does not read it"
            );
        }
    }

    #[test]
    fn a_record_shorter_than_the_struct_is_refused_rather_than_padded() {
        assert!(Wakeup::from_bytes(&[0u8; 16]).is_none());
    }

    #[test]
    fn a_name_stops_at_its_terminator() {
        let mut bytes = [0u8; RECORD_BYTES];
        bytes[32..36].copy_from_slice(b"true");
        let wakeup = Wakeup::from_bytes(&bytes).expect("a full record");
        assert_eq!(wakeup.wakee_comm(), "true");
    }

    /// Every helper the program calls, in the order it calls them.
    fn helpers_called(instructions: &[Insn]) -> Vec<i32> {
        instructions
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect()
    }

    /// Invariant 6, checked in the instruction stream rather than trusted.
    ///
    /// `bpf_ktime_get_boot_ns` includes suspend time. A program that called it
    /// would produce timestamps in a different domain from every other
    /// collector, every join across them would be wrong, and nothing anywhere
    /// would report an error -- which is exactly why this is asserted against
    /// the built program and not left to a reviewer noticing the helper number.
    #[test]
    fn the_program_reads_the_monotonic_clock_and_no_other() {
        let Ok(btf) = Btf::from_kernel() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let program = instructions(&btf, Some(1), 3, 4).expect("a program");
        let called = helpers_called(&program);
        assert!(
            called.contains(&helper::KTIME_GET_NS),
            "the program must timestamp its records"
        );
        const KTIME_GET_BOOT_NS: i32 = 125;
        assert!(
            !called.contains(&KTIME_GET_BOOT_NS),
            "boot time includes suspend, and mixing clock domains breaks every join silently"
        );
    }

    /// The program calls these and nothing else. A helper appearing here that
    /// nobody intended is either dead weight on a path that runs tens of
    /// thousands of times a second, or a fact being read that the record does
    /// not carry.
    #[test]
    fn the_program_calls_only_the_helpers_it_needs() {
        let Ok(btf) = Btf::from_kernel() else { return };
        let program = instructions(&btf, Some(1), 3, 4).expect("a program");
        let called = helpers_called(&program);
        let expected = [
            helper::KTIME_GET_NS,
            helper::GET_CURRENT_PID_TGID,
            helper::GET_SMP_PROCESSOR_ID,
            helper::PROBE_READ_KERNEL,
            helper::RINGBUF_OUTPUT,
            helper::MAP_LOOKUP_ELEM,
        ];
        for helper in &called {
            assert!(
                expected.contains(helper),
                "the program calls helper {helper}, which it was never meant to"
            );
        }
        assert_eq!(
            called
                .iter()
                .filter(|id| **id == helper::PROBE_READ_KERNEL)
                .count(),
            3,
            "one read per field taken out of the task: tid, pid and comm"
        );
    }

    /// The machine-wide program keeps every wakeup on the machine, so it must
    /// carry no comparison against a process -- and the filtered one must carry
    /// two, because a wakeup counts at either end.
    #[test]
    fn the_filter_is_present_only_when_a_process_was_named() {
        let Ok(btf) = Btf::from_kernel() else { return };
        let machine_wide = instructions(&btf, None, 3, 4).expect("a program");
        assert!(
            !machine_wide.iter().any(|insn| insn.imm == 4242),
            "a machine-wide program compares against no process"
        );
        let filtered = instructions(&btf, Some(4242), 3, 4).expect("a program");
        assert_eq!(
            filtered.iter().filter(|insn| insn.imm == 4242).count(),
            2,
            "a wakeup is kept when either end belongs to the target"
        );
    }

    /// The size handed to `bpf_ringbuf_output` is the size of the record the
    /// rest of the program wrote. A smaller one publishes a truncated record
    /// and a larger one publishes whatever is on the stack behind it.
    #[test]
    fn the_published_record_is_exactly_the_size_of_the_struct() {
        let Ok(btf) = Btf::from_kernel() else { return };
        let program = instructions(&btf, None, 3, 4).expect("a program");
        let output_at = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT)
            .expect("the program publishes its record");
        // r3 is the length argument, set just before the call.
        let length = program[..output_at]
            .iter()
            .rev()
            .find(|insn| insn.regs & 0xf == 3 && insn.code == 0xb7)
            .expect("a length is moved into r3");
        assert_eq!(length.imm as usize, std::mem::size_of::<Wakeup>());
    }

    /// The program is built without any privilege, so this checks the shape
    /// even where the kernel would refuse to load it.
    #[test]
    fn the_program_is_built_against_the_running_kernels_offsets() {
        let Ok(btf) = Btf::from_kernel() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let filtered = instructions(&btf, Some(4242), 3, 4).expect("a program");
        let machine_wide = instructions(&btf, None, 3, 4).expect("a program");
        assert!(
            filtered.len() > machine_wide.len(),
            "the filtered program carries the comparisons the machine-wide one does not"
        );
        assert_eq!(
            filtered.last().copied(),
            Some(Insn::exit()),
            "a program must end in exit"
        );
        assert!(
            filtered.iter().any(|insn| insn.imm == 4242),
            "the target pid is written into the instruction stream"
        );
    }
}
