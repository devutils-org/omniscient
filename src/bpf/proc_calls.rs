// SPDX-License-Identifier: MIT OR GPL-2.0
//! `proc.calls`: one record per syscall, on `sys_enter` and `sys_exit`.
//!
//! `proc.syscall` buckets its calls in the kernel, which is what makes it
//! cheap and what makes "which call was slow, and when" unanswerable: a row is
//! a count in a bucket. This is the other shape (the twenty-second wave's
//! perfsched gaps 3 and 4, bpftrace gap 6): each call that returns is one
//! record in a ring -- when it began, how long it took, what it returned, the
//! task and its name -- and `openat`'s path, read out of the caller's memory.
//! `strace -T`'s line, `perf trace`'s, `opensnoop`'s.
//!
//! Every call is a record, so the cost is a ring write per call where
//! `proc.syscall` pays a map update: a target making a million calls a second
//! fills any ring, and the program counts what it could not publish. A
//! predicate naming one syscall is pushed into the entry program, as it is for
//! `proc.syscall`.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{count_drop, helper, push_current_filter, read_field, Insn, Program, Whose};
use super::insn::{BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash};
use super::ring::{ring_bytes, RingBuffer};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// `bpf_probe_read_user_str(dst, size, unsafe_ptr)`, which `insn::helper`
/// does not name.
const PROBE_READ_USER_STR: i32 = 114;

const LICENSE: &str = "Dual MIT/GPL";

const ENTER_SIGNATURE: [&str; 2] = ["pt_regs *", "long int"];
const EXIT_SIGNATURE: [&str; 2] = ["pt_regs *", "long int"];

/// How many threads can be inside a syscall at once and still be timed.
const MAX_PENDING: u32 = 16384;

/// The bytes of a path a record carries, its terminator included.
const PATH_BYTES: usize = 128;
const COMM_BYTES: usize = 16;

/// The drop counter's slots.
const DROP_PENDING: u32 = 0;
const DROP_RING: u32 = 1;
const SKIPPED_BY_PREDICATE: u32 = 2;

/// One syscall that returned, as the exit program writes it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Call {
    /// When the call began, `CLOCK_MONOTONIC`.
    pub ts_ns: u64,
    pub dur_ns: u64,
    /// The return value, as the kernel returned it: an errno is `-errno`.
    pub ret: i64,
    pub tid: u32,
    pub tgid: u32,
    pub nr: u32,
    /// What reading the path returned: its length with the terminator, a
    /// negative errno when the caller's memory could not be read, zero when
    /// this call has no path.
    pub path_read: i32,
    pub(crate) comm: [u8; COMM_BYTES],
    pub(crate) path: [u8; PATH_BYTES],
}

const CALL_BYTES: usize = 184;

fn text(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

impl Call {
    /// The bytes of a path a record carries.
    pub const PATH_BYTES: usize = PATH_BYTES;

    pub fn comm(&self) -> String {
        text(&self.comm)
    }

    /// The path, when this call has one and it was read.
    pub fn path(&self) -> Option<String> {
        (self.path_read > 0).then(|| text(&self.path))
    }

    /// Whether the path filled the record.
    pub fn path_truncated(&self) -> bool {
        self.path_read as usize >= PATH_BYTES
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < CALL_BYTES {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        let mut comm = [0u8; COMM_BYTES];
        comm.copy_from_slice(&bytes[40..56]);
        let mut path = [0u8; PATH_BYTES];
        path.copy_from_slice(&bytes[56..56 + PATH_BYTES]);
        Some(Self {
            ts_ns: long(0),
            dur_ns: long(8),
            ret: long(16) as i64,
            tid: word(24),
            tgid: word(28),
            nr: word(32),
            path_read: word(36) as i32,
            comm,
            path,
        })
    }
}

// ---- stack layout ----
const REC: i16 = -184;
const REC_TS: i16 = REC;
const REC_DUR: i16 = REC + 8;
const REC_RET: i16 = REC + 16;
const REC_TID: i16 = REC + 24;
const REC_TGID: i16 = REC + 28;
const REC_NR: i16 = REC + 32;
const REC_PATH_READ: i16 = REC + 36;
const REC_COMM: i16 = REC + 40;
const REC_PATH: i16 = REC + 56;
const PEND_KEY: i16 = -192;
const ARG_SLOT: i16 = -200;
const DROP_KEY: i16 = -208;
const FILTER_SCRATCH: i16 = -216;
const PEND_VALUE: i16 = -240;
const PEND_TS: i16 = 0;
const PEND_NR: i16 = 8;
const PEND_ARG: i16 = 16;
const PEND_VALUE_BYTES: usize = 24;
const PEND_KEY_BYTES: usize = 4;

/// What the two programs are narrowed to, and which call's path they read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallsNarrowing {
    pub whose: Whose,
    /// The one syscall kept, or every one.
    pub syscall_nr: Option<u32>,
    /// The syscall whose second argument is a path to read (`openat`), on an
    /// architecture this layer knows the registers of.
    pub path_nr: Option<u32>,
}

/// Where the second syscall argument is in `pt_regs`.
fn second_argument(btf: &Btf) -> Option<i32> {
    if cfg!(target_arch = "x86_64") {
        btf.member_offset("pt_regs", "si").ok()
    } else if cfg!(target_arch = "aarch64") {
        // `user_pt_regs.regs[1]`, at the head of `pt_regs`.
        Some(8)
    } else {
        None
    }
}

fn check_signature(btf: &Btf, tracepoint: &str, expected: &[&str]) -> Result<(), String> {
    let signature = btf.tracepoint_signature(tracepoint)?;
    if signature != expected {
        return Err(format!(
            "this kernel declares `{tracepoint}({})`, and this program is written for \
             `{tracepoint}({})`",
            signature.join(", "),
            expected.join(", "),
        ));
    }
    Ok(())
}

fn enter_instructions(
    btf: &Btf,
    narrowing: CallsNarrowing,
    pending_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    check_signature(btf, "sys_enter", &ENTER_SIGNATURE)?;
    let argument = second_argument(btf);

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    push_current_filter(&mut program, narrowing.whose, FILTER_SCRATCH, "done");

    // The syscall number, the tracepoint's second argument.
    program.push(Insn::ldx(BPF_DW, 9, 6, 8));
    if let Some(nr) = narrowing.syscall_nr {
        program.jump_to("the_syscall", Insn::jeq_imm(9, nr as i32, 0));
        count_drop(
            &mut program,
            drops_fd,
            DROP_KEY,
            SKIPPED_BY_PREDICATE,
            "done",
        );
        program.jump_to("done", Insn::ja());
        program.label("the_syscall");
    }

    program.push(Insn::st_imm(BPF_DW, R10, PEND_KEY, 0));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, PEND_KEY, 0));

    // The path's address, for the one call that has one; zero otherwise.
    program.push(Insn::st_imm(BPF_DW, R10, PEND_VALUE + PEND_ARG, 0));
    if let (Some(path_nr), Some(offset)) = (narrowing.path_nr, argument) {
        program.jump_to("no_path", Insn::jne_imm(9, path_nr as i32, 0));
        program.push(Insn::ldx(BPF_DW, 7, 6, 0));
        read_field(&mut program, PEND_VALUE + PEND_ARG, 8, 7, offset);
        program.label("no_path");
    }

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + PEND_TS, 0));
    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + PEND_NR, 9));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, PEND_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_PENDING, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

fn exit_instructions(
    btf: &Btf,
    pending_fd: i32,
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    check_signature(btf, "sys_exit", &EXIT_SIGNATURE)?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    program.push(Insn::st_imm(BPF_DW, R10, PEND_KEY, 0));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, PEND_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, REC_TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, REC_TGID, 0));

    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // A call this run never saw begin, or one the entry program declined.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 9, 0, PEND_TS));
    program.push(Insn::ldx(BPF_DW, 7, 0, PEND_NR));
    program.push(Insn::ldx(BPF_DW, 1, 0, PEND_ARG));
    program.push(Insn::stx(BPF_DW, R10, ARG_SLOT, 1));

    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    program.push(Insn::stx(BPF_DW, R10, REC_TS, 9));
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::stx(BPF_DW, R10, REC_DUR, 1));
    // The return value, the tracepoint's second argument.
    program.push(Insn::ldx(BPF_DW, 1, 6, 8));
    program.push(Insn::stx(BPF_DW, R10, REC_RET, 1));
    program.push(Insn::stx(BPF_W, R10, REC_NR, 7));
    program.push(Insn::st_imm(BPF_W, R10, REC_PATH_READ, 0));

    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, REC_COMM as i32));
    program.push(Insn::mov64_imm(2, COMM_BYTES as i32));
    program.push(Insn::call(helper::GET_CURRENT_COMM));

    // The path is zeroed first: the ring copies every byte of the record, and
    // a byte nobody wrote is whatever the stack held.
    for at in (0..PATH_BYTES as i16).step_by(8) {
        program.push(Insn::st_imm(BPF_DW, R10, REC_PATH + at, 0));
    }
    program.push(Insn::ldx(BPF_DW, 3, R10, ARG_SLOT));
    program.jump_to("publish", Insn::jeq_imm(3, 0, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, REC_PATH as i32));
    program.push(Insn::mov64_imm(2, PATH_BYTES as i32));
    program.push(Insn::call(PROBE_READ_USER_STR));
    program.push(Insn::stx(BPF_W, R10, REC_PATH_READ, 0));

    program.label("publish");
    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, REC as i32));
    program.push(Insn::mov64_imm(3, CALL_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_RING, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The two attached programs, their pending map, their ring and their drops.
pub struct CallsProgram {
    ring: RingBuffer,
    pending: Hash,
    drops: Counters,
    enter_program: OwnedFd,
    _enter_link: OwnedFd,
    exit_program: OwnedFd,
    _exit_link: OwnedFd,
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

impl CallsProgram {
    pub fn load(narrowing: CallsNarrowing, requested_ring_bytes: usize) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let bytes = ring_bytes(requested_ring_bytes);
        let pending = Hash::create(
            PEND_KEY_BYTES,
            PEND_VALUE_BYTES,
            MAX_PENDING,
            "omn_call_pend",
        )
        .map_err(|error| map_failure("pending call map", error))?;
        let ring_map = syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_calls")
            .map_err(|error| map_failure("call ring buffer", error))?;
        let drops = Counters::create(3, "omn_call_drop")
            .map_err(|error| map_failure("call drop counter", error))?;

        let enter = enter_instructions(btf, narrowing, pending.as_raw_fd(), drops.as_raw_fd())?;
        let exit = exit_instructions(
            btf,
            pending.as_raw_fd(),
            ring_map.as_raw_fd(),
            drops.as_raw_fd(),
        )?;
        let enter_program = load(&enter, "omn_call_enter")?;
        let exit_program = load(&exit, "omn_call_exit")?;
        // Entry first, for the reason `proc_syscall.rs` gives.
        let enter_link = syscall::raw_tracepoint_open("sys_enter", enter_program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sys_enter` tracepoint: {error}"))?;
        let exit_link = syscall::raw_tracepoint_open("sys_exit", exit_program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sys_exit` tracepoint: {error}"))?;
        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the call ring buffer: {error}"))?;
        Ok(Self {
            ring,
            pending,
            drops,
            enter_program,
            _enter_link: enter_link,
            exit_program,
            _exit_link: exit_link,
        })
    }

    /// Hands every record to `handle`, counting the ones it cannot read.
    pub fn drain(&mut self, mut handle: impl FnMut(Call), undecoded: &mut u64) {
        self.ring.drain(|bytes| match Call::from_bytes(bytes) {
            Some(call) => handle(call),
            None => *undecoded += 1,
        });
    }

    /// `(pending map full, ring full)`, cumulative.
    pub fn dropped(&self) -> (u64, u64) {
        (self.drops.read(DROP_PENDING), self.drops.read(DROP_RING))
    }

    /// Calls of the target the pushed-down syscall comparison skipped.
    pub fn skipped_by_predicate(&self) -> u64 {
        self.drops.read(SKIPPED_BY_PREDICATE)
    }

    /// Runs the kernel skipped because one was already running on that cpu.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.enter_program, &self.exit_program])
    }

    /// Threads inside a call when asked, whose call has no end yet.
    pub fn still_pending(&self) -> usize {
        self.pending.len().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_is_the_size_the_program_writes() {
        assert_eq!(std::mem::size_of::<Call>(), CALL_BYTES);
        assert_eq!((REC_PATH - REC) as usize + PATH_BYTES, CALL_BYTES);
        assert_eq!((REC_COMM - REC) as usize, std::mem::offset_of!(Call, comm));
        assert_eq!((REC_PATH - REC) as usize, std::mem::offset_of!(Call, path));
        assert_eq!(
            (REC_PATH_READ - REC) as usize,
            std::mem::offset_of!(Call, path_read)
        );
        assert_eq!((REC_NR - REC) as usize, std::mem::offset_of!(Call, nr));
        assert_eq!((REC_RET - REC) as usize, std::mem::offset_of!(Call, ret));
    }

    #[test]
    fn the_stack_regions_do_not_overlap_and_stay_inside_the_frame() {
        let regions = [
            (REC, CALL_BYTES as i16, "record"),
            (PEND_KEY, 8, "pending key"),
            (ARG_SLOT, 8, "argument"),
            (DROP_KEY, 4, "drop key"),
            (FILTER_SCRATCH, 8, "filter scratch"),
            (PEND_VALUE, PEND_VALUE_BYTES as i16, "pending value"),
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

    #[test]
    fn a_record_decodes_its_fields_and_its_path() {
        let mut bytes = vec![0u8; CALL_BYTES];
        bytes[16..24].copy_from_slice(&(-2i64).to_ne_bytes());
        bytes[36..40].copy_from_slice(&5i32.to_ne_bytes());
        bytes[40..44].copy_from_slice(b"cat\0");
        bytes[56..60].copy_from_slice(b"/etc");
        let call = Call::from_bytes(&bytes).unwrap();
        assert_eq!(call.ret, -2);
        assert_eq!(call.comm(), "cat");
        assert_eq!(call.path().as_deref(), Some("/etc"));
        assert!(Call::from_bytes(&bytes[..100]).is_none());
    }

    #[test]
    fn both_programs_build_and_pass_the_audit() {
        let btf = match Btf::from_kernel() {
            Ok(btf) => btf,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                return;
            }
        };
        let narrowing = CallsNarrowing {
            whose: Whose::Process(1),
            syscall_nr: Some(257),
            path_nr: Some(257),
        };
        enter_instructions(&btf, narrowing, 3, 4).expect("the entry program");
        exit_instructions(&btf, 3, 4, 5).expect("the exit program");
    }
}
