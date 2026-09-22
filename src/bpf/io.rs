//! The block layer programs: what a request cost, and where it spent it.
//!
//! A block request passes three moments the kernel publishes by name.
//! `block_rq_insert` is the block layer putting it on a software queue,
//! `block_rq_issue` is the driver being handed it, and `block_rq_complete` is
//! the device answering. No single one of them is a measurement: every number
//! this family wants is the *gap* between two, and which two decides what the
//! number means.
//!
//! ```text
//!   insert ----------- issue ----------- complete
//!          `io.queue'         `io.latency'
//!          software queueing  what the device did
//! ```
//!
//! So one shape serves all three sources: an **open** tracepoint writes the
//! moment and the submitting task into a map keyed by the request, and a
//! **close** tracepoint reads it back, subtracts, publishes one record and
//! removes the entry. `io.size` is the degenerate case with no open at all --
//! a request's length is readable at the issue itself.
//!
//! **Why this streams rather than aggregating in the kernel.**
//! `src/bpf/runqlat.rs` builds its histogram in the kernel because a wakeup
//! happens hundreds of thousands of times a second and one record per wakeup
//! would make the measurement cost grow with the workload. Block completions
//! are three to four orders of magnitude rarer -- a saturated NVMe device
//! answers a few hundred thousand times a *second* only under a synthetic
//! queue-depth sweep, and an ordinary machine sees thousands. That is not the
//! reason these stream, though. The reason is that **`io.latency` and
//! `io.queue` are the only sources that produce `blk:rq@open_ts`**, and a
//! kernel-side histogram keyed by `(device, bucket)` throws that key away. A
//! join needs the requests one at a time or it has nothing to join.
//!
//! **The scope comes from the kernel, not from this program.** A request
//! pointer is freed and handed straight back out, often within a millisecond,
//! so `blk:<ptr>` alone attributes one request's latency to another; the
//! catalog writes `blk:rq@open_ts` for that reason. The obvious scope -- the
//! moment this program first saw the request -- would be wrong in a way that
//! is hard to see: `io.queue` and `io.latency` are two requests and therefore
//! two independent program sets, each calling `bpf_ktime_get_ns` at its own
//! instant, so the same request would carry two different scopes and the join
//! between them would match nothing and report it as evidence that nothing
//! matched. `request.start_time_ns` is the kernel's own stamp on the request,
//! taken once when it was created, in `CLOCK_MONOTONIC` like everything else
//! here -- so two programs that never met agree on it exactly.
//!
//! It is zero on a queue the kernel keeps no statistics for. Those requests
//! fall back to this run's own sighting and say so, per event, in
//! `SCOPE_IS_OURS`: a scope that will not join across collectors must not be
//! indistinguishable from one that will.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, read_field, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash};
use super::ring::{ring_bytes, RingBuffer};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// Which two moments a program set measures between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// `block_rq_insert` to `block_rq_issue`: time on a software queue.
    Queue,
    /// `block_rq_issue` to `block_rq_complete`: what the device did.
    Latency,
    /// `block_rq_issue` alone: the request's length, which needs no pairing.
    Size,
}

impl Stage {
    /// The tracepoint that starts the measurement, when there is one.
    pub fn opens_at(self) -> Option<&'static str> {
        match self {
            Stage::Queue => Some("block_rq_insert"),
            Stage::Latency => Some("block_rq_issue"),
            Stage::Size => None,
        }
    }

    /// The tracepoint that ends it and publishes the record.
    pub fn closes_at(self) -> &'static str {
        match self {
            Stage::Queue | Stage::Size => "block_rq_issue",
            Stage::Latency => "block_rq_complete",
        }
    }

    fn map_name(self) -> &'static str {
        match self {
            Stage::Queue => "omn_blk_queue",
            Stage::Latency => "omn_blk_lat",
            Stage::Size => "omn_blk_size",
        }
    }
}

/// Whose requests a program set keeps.
///
/// Both halves are written into the entry at the open, so which one the filter
/// compares is a choice of stack slot. It is a choice rather than a
/// simplification: answering `--tid` by keeping the whole thread group would be
/// a filter reported as applied and applied to something wider, which is the
/// worst shape a bug takes here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitter {
    /// Every request on the machine.
    Any,
    /// The requests submitted by any thread of one process.
    Process(u32),
    /// The requests submitted by one thread.
    Thread(u32),
}

impl Submitter {
    /// The stack slot to compare and the value to compare it with, within an
    /// entry laid out at `base`.
    fn comparison(self, base: i16) -> Option<(i16, u32)> {
        match self {
            Submitter::Any => None,
            Submitter::Process(tgid) => Some((base + VAL_TGID, tgid)),
            Submitter::Thread(tid) => Some((base + VAL_TID, tid)),
        }
    }
}

/// The prototypes these programs are written against.
///
/// Checked before an instruction is built. A raw tracepoint hands over an array
/// of `u64` and nothing in it says what any slot holds, so a program written
/// for one prototype and run against another reads a `struct request *` out of
/// a slot holding an integer and gets a plausible number -- which is worse than
/// an error, because a plausible latency is one nobody questions.
const REQUEST_SIGNATURE: [&str; 1] = ["request *"];
const COMPLETE_SIGNATURE: [&str; 3] = ["request *", "blk_status_t", "unsigned int"];

/// How many requests may be in flight at once before the program stops being
/// able to record new ones.
///
/// A queue depth of a few thousand is the most a machine's devices hold
/// together; this leaves room for a synthetic sweep and for the requests of
/// every other process when a run is machine-wide.
const MAX_INFLIGHT: u32 = 16384;

/// Slots of the drop counter, one per thing that can be lost.
const DROP_INFLIGHT: u32 = 0;
const DROP_RING: u32 = 1;
/// Not a loss: a close with no open this run recorded. Counted in the same
/// per-cpu array because it is the same kind of fact -- something the program
/// saw and could not turn into a measurement -- and reported apart from the
/// losses because nothing was dropped.
const UNPAIRED: u32 = 2;
const DROP_SLOTS: u32 = 3;

/// `SCOPE_IS_OURS`: the kernel put no `start_time_ns` on this request, so the
/// scope of its `blk:rq@open_ts` key is this program's own first sighting and
/// will not agree with another collector's.
pub const SCOPE_IS_OURS: u32 = 1;

/// `NO_OPEN`: the closing tracepoint fired for a request whose opening one
/// never did.
///
/// Only `Stage::Queue` publishes these, and the reason is that on a blk-mq
/// queue with no I/O scheduler -- `[none]`, which is the default for NVMe --
/// `block_rq_insert` does not fire at all: a request that is issued directly
/// never goes on a software queue. Counting those as "unmeasurable" would
/// report a device that never queues as a device this tool cannot see, and the
/// two are opposite answers. So the record is published with no interval and
/// this flag, and the collector decides which it is from whether the request
/// was created after the programs attached.
pub const NO_OPEN: u32 = 2;

// ---- the entry the open tracepoint writes and the close one reads ----

const ENTRY_KEY_BYTES: usize = 8;
const ENTRY_VALUE_BYTES: usize = 32;

const VAL_OPEN_TS: i16 = 0;
const VAL_TID: i16 = 8;
const VAL_TGID: i16 = 12;
const VAL_COMM: i16 = 16;

// ---- the record the close tracepoint publishes ----

/// One block request, as the program writes it and userspace reads it.
///
/// `repr(C)` on both sides of the ring is the contract, and
/// `the_record_struct_matches_what_the_program_writes` below is what holds the
/// two together: the program writes fields at stack offsets and this reads them
/// at struct offsets, and nothing else connects the two.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockEvent {
    /// When the closing tracepoint fired: `bpf_ktime_get_ns`, which is
    /// `CLOCK_MONOTONIC`, never `bpf_ktime_get_boot_ns`.
    pub ts_ns: u64,
    /// The request's address, which is the identity half of `blk:rq@open_ts`.
    pub rq: u64,
    /// Which life of that address this is: `request.start_time_ns`, or this
    /// run's own sighting when the kernel stamped none, which `flags` says.
    pub open_ts: u64,
    /// The interval the stage measures, in nanoseconds. Zero for `Size`, which
    /// measures no interval.
    pub delta_ns: u64,
    /// The request's first sector, in the 512-byte units the block layer uses
    /// whatever the device's logical block size is.
    pub sector: u64,
    /// `block_device.bd_dev`, the kernel's packed `dev_t`. Zero when the
    /// request names no partition, which passthrough requests do not.
    pub dev: u32,
    /// The request's length at the moment of the event, from `__data_len`.
    pub bytes: u32,
    /// `blk_opf_t`: the operation in the low bits and the flags above them,
    /// carried raw and decoded in userspace rather than masked in the kernel.
    pub cmd_flags: u32,
    /// `blk_status_t` at completion, zero everywhere else. Non-zero is an I/O
    /// that failed, and a failed I/O's latency is a different measurement from
    /// a successful one's.
    pub status: i32,
    /// The thread that submitted, captured at the *opening* tracepoint. At the
    /// completion there is no submitter to ask: the interrupt runs in whatever
    /// context it interrupted.
    pub tid: u32,
    pub tgid: u32,
    /// The cpu the closing tracepoint ran on. For a completion that is the cpu
    /// the interrupt landed on, which is not the submitter's.
    pub cpu: u32,
    /// The fourth argument of `block_rq_complete`: how many bytes *this*
    /// completion accounted for. Smaller than `bytes` means the request
    /// completed in parts, and only the first part is timed.
    pub done_bytes: u32,
    /// `SCOPE_IS_OURS` when the scope is this run's rather than the kernel's.
    pub flags: u32,
    pad: u32,
    comm: [u8; 16],
}

/// The size of the record the program writes. Stated once, used by the program
/// and checked against the struct.
const RECORD_BYTES: usize = 96;

impl BlockEvent {
    /// The submitting thread's name, as the kernel held it at the open.
    pub fn comm(&self) -> String {
        let end = self
            .comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.comm.len());
        String::from_utf8_lossy(&self.comm[..end]).into_owned()
    }

    /// The device's major number, out of the kernel's packed `dev_t`.
    pub fn major(&self) -> u32 {
        (self.dev >> 20) & 0xfff
    }

    /// The device's minor number.
    pub fn minor(&self) -> u32 {
        self.dev & 0xf_ffff
    }

    /// Whether the scope of this event's key came from the kernel, and
    /// therefore agrees with what another collector would compute.
    pub fn scope_is_the_kernels(&self) -> bool {
        self.flags & SCOPE_IS_OURS == 0
    }

    /// Whether this record is a close whose open never fired.
    ///
    /// It carries no interval. What that *means* is not decidable here -- a
    /// request issued without ever being queued and one queued before the
    /// programs attached look identical at this tracepoint -- so the collector
    /// decides, from `open_ts` against the moment it attached.
    pub fn had_no_open(&self) -> bool {
        self.flags & NO_OPEN != 0
    }

    /// Decodes one ring record.
    ///
    /// Reachable from the family that reads these because a test there builds
    /// a record out of the same bytes the ring carries, rather than naming
    /// fields the program never writes by name.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let long = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().ok().unwrap());
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().ok().unwrap());
        let mut comm = [0u8; 16];
        comm.copy_from_slice(&bytes[80..96]);
        Some(Self {
            ts_ns: long(0),
            rq: long(8),
            open_ts: long(16),
            delta_ns: long(24),
            sector: long(32),
            dev: word(40),
            bytes: word(44),
            cmd_flags: word(48),
            status: word(52) as i32,
            tid: word(56),
            tgid: word(60),
            cpu: word(64),
            done_bytes: word(68),
            flags: word(72),
            pad: 0,
            comm,
        })
    }
}

/// Stack offsets of the record, negative from the frame pointer.
const EVENT: i16 = -96;
const EV_TS: i16 = EVENT;
const EV_RQ: i16 = EVENT + 8;
const EV_OPEN_TS: i16 = EVENT + 16;
const EV_DELTA: i16 = EVENT + 24;
const EV_SECTOR: i16 = EVENT + 32;
const EV_DEV: i16 = EVENT + 40;
const EV_BYTES: i16 = EVENT + 44;
const EV_CMD_FLAGS: i16 = EVENT + 48;
const EV_STATUS: i16 = EVENT + 52;
const EV_TID: i16 = EVENT + 56;
const EV_TGID: i16 = EVENT + 60;
const EV_CPU: i16 = EVENT + 64;
const EV_DONE_BYTES: i16 = EVENT + 68;
const EV_FLAGS: i16 = EVENT + 72;
const EV_COMM: i16 = EVENT + 80;

/// Everything below the record, on the close program's stack.
const LOOKUP_KEY: i16 = EVENT - 8;
const PART: i16 = EVENT - 16;
const CLOSE_DROP_KEY: i16 = EVENT - 24;

/// The open program's stack: the entry it builds, its key, and a drop slot.
const ENTRY_VALUE: i16 = -32;
const ENTRY_KEY: i16 = -40;
const OPEN_DROP_KEY: i16 = -48;

/// `bpf_probe_read_kernel` is GPL-only, and every field read out of a
/// `struct request` comes through it.
const LICENSE: &str = "GPL";

/// Adds one to a slot of the per-cpu counter array.
///
/// `resume` is where control goes both when the count has been made and when
/// the counter could not be found: failing to record a loss is not a reason to
/// stop recording measurements.
fn count(program: &mut Program, counters_fd: i32, key_at: i16, slot: u32, resume: &'static str) {
    program.push(Insn::st_imm(BPF_W, R10, key_at, slot as i32));
    program.extend(Insn::load_map_fd(1, counters_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, key_at as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to(resume, Insn::jeq_imm(0, 0, 0));
    // Per-cpu, so this slot belongs to this cpu alone and needs no atomic.
    program.push(Insn::ldx(BPF_DW, 1, 0, 0));
    program.push(Insn::add64_imm(1, 1));
    program.push(Insn::stx(BPF_DW, 0, 0, 1));
}

/// Checks a tracepoint's prototype against the shape a program assumes.
fn check_signature(btf: &Btf, tracepoint: &str, expected: &[&str]) -> Result<(), String> {
    let signature = btf.tracepoint_signature(tracepoint)?;
    if signature != expected {
        return Err(format!(
            "this kernel declares `{tracepoint}({})`, and this program is written for \
             `{tracepoint}({})`: it reads the request out of a fixed slot, and against another \
             prototype that slot holds something else -- which does not fail, it produces a \
             number",
            signature.join(", "),
            expected.join(", "),
        ));
    }
    Ok(())
}

/// The program on the opening tracepoint: record when, and by whom.
///
/// Nothing is measured here. What it writes is one map entry per in-flight
/// request, which the closing program reads back; a request whose close this
/// run never sees leaves its entry behind, and `still_in_flight` reports how
/// many there were rather than letting them look like requests that took no
/// time.
fn open_instructions(
    btf: &Btf,
    stage: Stage,
    submitter: Submitter,
    inflight_fd: i32,
    counters_fd: i32,
) -> Result<Vec<Insn>, String> {
    let tracepoint = stage
        .opens_at()
        .expect("an open program is only built for a stage that has one");
    check_signature(btf, tracepoint, &REQUEST_SIGNATURE)?;

    let mut program = Program::new();
    // r6 keeps the context and r7 the request; both survive helper calls,
    // which clobber r1 through r5 and r0.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, ENTRY_VALUE + VAL_OPEN_TS, 0));

    // The low half is the thread id and the high half the process id. This is
    // the one moment the submitting task is the current one: at the completion
    // the same question is answered by whatever the interrupt interrupted.
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, ENTRY_VALUE + VAL_TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, ENTRY_VALUE + VAL_TGID, 0));

    // A run observes one process. The thread group is known before the program
    // loads and never changes, so the comparison is written into the
    // instruction stream rather than looked up in a map.
    if let Some((slot, wanted)) = submitter.comparison(ENTRY_VALUE) {
        program.push(Insn::ldx(BPF_W, 1, R10, slot));
        program.jump_to("done", Insn::jne_imm(1, wanted as i32, 0));
    }

    // `bpf_get_current_comm` writes all sixteen bytes or zeroes them, so the
    // entry carries no stack left over from whatever ran before.
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (ENTRY_VALUE + VAL_COMM) as i32));
    program.push(Insn::mov64_imm(2, 16));
    program.push(Insn::call(helper::GET_CURRENT_COMM));

    program.push(Insn::stx(BPF_DW, R10, ENTRY_KEY, 7));
    program.extend(Insn::load_map_fd(1, inflight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ENTRY_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ENTRY_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    // The map is full: this request will never be timed, and nothing else
    // anywhere would record that it happened.
    count(
        &mut program,
        counters_fd,
        OPEN_DROP_KEY,
        DROP_INFLIGHT,
        "done",
    );

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on the closing tracepoint: subtract, describe, publish.
fn close_instructions(
    btf: &Btf,
    stage: Stage,
    submitter: Submitter,
    inflight_fd: i32,
    ring_fd: i32,
    counters_fd: i32,
) -> Result<Vec<Insn>, String> {
    let tracepoint = stage.closes_at();
    let expected: &[&str] = if tracepoint == "block_rq_complete" {
        &COMPLETE_SIGNATURE
    } else {
        &REQUEST_SIGNATURE
    };
    check_signature(btf, tracepoint, expected)?;

    let sector = btf.member_offset("request", "__sector")?;
    let data_len = btf.member_offset("request", "__data_len")?;
    let cmd_flags = btf.member_offset("request", "cmd_flags")?;
    let part = btf.member_offset("request", "part")?;
    let start_time = btf.member_offset("request", "start_time_ns")?;
    let bd_dev = btf.member_offset("block_device", "bd_dev")?;

    let paired = stage.opens_at().is_some();
    // Only the queueing stage: a completion with no issue has no latency to
    // report and no reading that would stand for one, whereas an issue with no
    // insert is a request that queued for no time.
    let publishes_unpaired = stage == Stage::Queue;

    /// Settles the scope and the flags, then jumps to `publish`.
    ///
    /// Written once because there are two ways into it and they must agree
    /// about the flags: the fallback scope is a fact about the event, so it is
    /// combined with whatever the path already knew rather than overwriting it.
    fn finish(program: &mut Program, fallback: u8, flags: u32, own: &'static str) {
        program.push(Insn::ldx(BPF_DW, 1, R10, EV_OPEN_TS));
        program.jump_to(own, Insn::jeq_imm(1, 0, 0));
        program.push(Insn::st_imm(BPF_W, R10, EV_FLAGS, flags as i32));
        program.jump_to("publish", Insn::ja());
        program.label(own);
        program.push(Insn::stx(BPF_DW, R10, EV_OPEN_TS, fallback));
        program.push(Insn::st_imm(
            BPF_W,
            R10,
            EV_FLAGS,
            (flags | SCOPE_IS_OURS) as i32,
        ));
        program.jump_to("publish", Insn::ja());
    }

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    // Every byte of the record is written before it is published, so the whole
    // thing is cleared first rather than each hole being tracked by hand. An
    // uninitialised byte here is not a wrong number, it is whatever the last
    // program to use this stack left behind, published as a measurement.
    for slot in 0..(RECORD_BYTES / 8) as i16 {
        program.push(Insn::st_imm(BPF_DW, R10, EVENT + slot * 8, 0));
    }

    program.push(Insn::stx(BPF_DW, R10, EV_TS, 8));
    program.push(Insn::stx(BPF_DW, R10, EV_RQ, 7));

    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, EV_CPU, 0));

    // The request describes itself, and all of it is readable at either close:
    // `__data_len` is decremented by `blk_update_request` *after* the
    // completion tracepoint fires, so at both the issue and the first
    // completion it is still the request's length.
    read_field(&mut program, EV_OPEN_TS, 8, 7, start_time);
    read_field(&mut program, EV_SECTOR, 8, 7, sector);
    read_field(&mut program, EV_BYTES, 4, 7, data_len);
    read_field(&mut program, EV_CMD_FLAGS, 4, 7, cmd_flags);

    // The device hangs off `request.part`, which a passthrough request does not
    // have. A null there is a request that belongs to no partition, not an
    // error: it is reported as device zero and the collector says so.
    read_field(&mut program, PART, 8, 7, part);
    program.push(Insn::ldx(BPF_DW, 9, R10, PART));
    program.jump_to("no_device", Insn::jeq_imm(9, 0, 0));
    read_field(&mut program, EV_DEV, 4, 9, bd_dev);
    program.label("no_device");

    if tracepoint == "block_rq_complete" {
        // The second argument is `blk_status_t` and the third the bytes this
        // completion accounted for.
        program.push(Insn::ldx(BPF_DW, 1, 6, 8));
        program.push(Insn::stx(BPF_W, R10, EV_STATUS, 1));
        program.push(Insn::ldx(BPF_DW, 1, 6, 16));
        program.push(Insn::stx(BPF_W, R10, EV_DONE_BYTES, 1));
    }

    if paired {
        program.push(Insn::stx(BPF_DW, R10, LOOKUP_KEY, 7));
        program.extend(Insn::load_map_fd(1, inflight_fd));
        program.push(Insn::mov64_reg(2, R10));
        program.push(Insn::add64_imm(2, LOOKUP_KEY as i32));
        program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
        // No entry: this run attached after the request opened, or the request
        // belongs to another process and the open program refused it. Either
        // way there is nothing to subtract from.
        program.jump_to("unpaired", Insn::jeq_imm(0, 0, 0));

        // Everything is copied out of the value before the entry is deleted:
        // the delete invalidates the pointer the lookup returned.
        program.push(Insn::ldx(BPF_DW, 9, 0, VAL_OPEN_TS));
        program.push(Insn::ldx(BPF_W, 1, 0, VAL_TID));
        program.push(Insn::stx(BPF_W, R10, EV_TID, 1));
        program.push(Insn::ldx(BPF_W, 1, 0, VAL_TGID));
        program.push(Insn::stx(BPF_W, R10, EV_TGID, 1));
        program.push(Insn::ldx(BPF_DW, 1, 0, VAL_COMM));
        program.push(Insn::stx(BPF_DW, R10, EV_COMM, 1));
        program.push(Insn::ldx(BPF_DW, 1, 0, VAL_COMM + 8));
        program.push(Insn::stx(BPF_DW, R10, EV_COMM + 8, 1));

        // The interval, through r1 because nothing between here and the next
        // call reads it.
        program.push(Insn::mov64_reg(1, 8));
        program.push(Insn::sub64_reg(1, 9));
        program.push(Insn::stx(BPF_DW, R10, EV_DELTA, 1));

        program.extend(Insn::load_map_fd(1, inflight_fd));
        program.push(Insn::mov64_reg(2, R10));
        program.push(Insn::add64_imm(2, LOOKUP_KEY as i32));
        program.push(Insn::call(helper::MAP_DELETE_ELEM));
    } else {
        // Nothing opened this one, so the submitter is the current task: a
        // `block_rq_issue` for a read runs in the context that asked for it.
        program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
        program.push(Insn::stx(BPF_W, R10, EV_TID, 0));
        program.push(Insn::rsh64_imm(0, 32));
        program.push(Insn::stx(BPF_W, R10, EV_TGID, 0));
        program.push(Insn::mov64_reg(1, R10));
        program.push(Insn::add64_imm(1, EV_COMM as i32));
        program.push(Insn::mov64_imm(2, 16));
        program.push(Insn::call(helper::GET_CURRENT_COMM));

        // The record's own tid and tgid sit where an entry's would, so the
        // comparison is the same one the open program makes.
        if let Some((slot, wanted)) = submitter.comparison(EV_TID - VAL_TID) {
            program.push(Insn::ldx(BPF_W, 1, R10, slot));
            program.jump_to("done", Insn::jne_imm(1, wanted as i32, 0));
        }
    }

    // A stage with no open has only this moment to fall back on; a paired one
    // has the sighting it took at the open, in r9.
    finish(
        &mut program,
        if paired { 9 } else { 8 },
        0,
        "paired_scope_is_ours",
    );

    program.label("publish");
    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EVENT as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count(&mut program, counters_fd, CLOSE_DROP_KEY, DROP_RING, "done");
    program.jump_to("done", Insn::ja());

    if paired {
        program.label("unpaired");
        if publishes_unpaired {
            // The issue runs in the context that submitted the request, so the
            // submitter is readable here exactly as it would have been at an
            // insert that never happened.
            program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
            program.push(Insn::stx(BPF_W, R10, EV_TID, 0));
            program.push(Insn::rsh64_imm(0, 32));
            program.push(Insn::stx(BPF_W, R10, EV_TGID, 0));
            program.push(Insn::mov64_reg(1, R10));
            program.push(Insn::add64_imm(1, EV_COMM as i32));
            program.push(Insn::mov64_imm(2, 16));
            program.push(Insn::call(helper::GET_CURRENT_COMM));
            if let Some((slot, wanted)) = submitter.comparison(EV_TID - VAL_TID) {
                program.push(Insn::ldx(BPF_W, 1, R10, slot));
                program.jump_to("done", Insn::jne_imm(1, wanted as i32, 0));
            }
            // No interval: the record is already cleared, so `delta_ns` is
            // zero and the flag says the zero is an absence rather than a
            // measurement of no time.
            finish(&mut program, 8, NO_OPEN, "direct_scope_is_ours");
        } else {
            count(&mut program, counters_fd, CLOSE_DROP_KEY, UNPAIRED, "done");
        }
    }

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// A loaded, attached pair of programs and the maps between them.
///
/// Dropping it detaches both: the links, the programs and the maps are file
/// descriptors, and the kernel unloads a program when the last one closes.
pub struct BlockProgram {
    ring: RingBuffer,
    counters: Counters,
    inflight: Option<Hash>,
    _open_program: Option<OwnedFd>,
    _open_link: Option<OwnedFd>,
    _close_program: OwnedFd,
    _close_link: OwnedFd,
}

impl BlockProgram {
    pub fn load(
        stage: Stage,
        submitter: Submitter,
        requested_ring_bytes: usize,
    ) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let bytes = ring_bytes(requested_ring_bytes);

        let ring_map =
            syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_blk_ring")
                .map_err(|error| map_failure("block ring buffer", error))?;
        let counters = Counters::create(DROP_SLOTS, "omn_blk_count")
            .map_err(|error| map_failure("block drop counter", error))?;

        let inflight = match stage.opens_at() {
            Some(_) => Some(
                Hash::create(
                    ENTRY_KEY_BYTES,
                    ENTRY_VALUE_BYTES,
                    MAX_INFLIGHT,
                    stage.map_name(),
                )
                .map_err(|error| map_failure("in-flight request map", error))?,
            ),
            None => None,
        };
        let inflight_fd = inflight.as_ref().map_or(-1, Hash::as_raw_fd);

        let close = close_instructions(
            btf,
            stage,
            submitter,
            inflight_fd,
            ring_map.as_raw_fd(),
            counters.as_raw_fd(),
        )?;
        let close_program = load(&close, "omn_blk_close")?;

        // The opening program is attached first, and that order is not
        // arbitrary. An entry written with no closing program attached ages in
        // the map and is reported as still in flight, which is harmless; a
        // close attached first would find entries only for requests that
        // happened to open after it, which is the same thing one tick later.
        // What neither order can produce is a measurement from an entry nobody
        // wrote, because the lookup that fails is the only path to it.
        let (open_program, open_link) = match stage.opens_at() {
            Some(tracepoint) => {
                let open =
                    open_instructions(btf, stage, submitter, inflight_fd, counters.as_raw_fd())?;
                let program = load(&open, "omn_blk_open")?;
                let link = syscall::raw_tracepoint_open(tracepoint, program.as_raw_fd()).map_err(
                    |error| format!("cannot attach to the `{tracepoint}` tracepoint: {error}"),
                )?;
                (Some(program), Some(link))
            }
            None => (None, None),
        };

        let closes_at = stage.closes_at();
        let close_link = syscall::raw_tracepoint_open(closes_at, close_program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `{closes_at}` tracepoint: {error}"))?;

        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the block ring buffer: {error}"))?;

        Ok(Self {
            ring,
            counters,
            inflight,
            _open_program: open_program,
            _open_link: open_link,
            _close_program: close_program,
            _close_link: close_link,
        })
    }

    /// Hands every complete record to `handle`, and counts the records that
    /// were not the size this reader expects rather than reinterpreting them.
    pub fn drain(&mut self, mut handle: impl FnMut(BlockEvent), undecoded: &mut u64) {
        self.ring
            .drain(|bytes| match BlockEvent::from_bytes(bytes) {
                Some(event) => handle(event),
                None => *undecoded += 1,
            });
    }

    /// Requests the program could not record because the in-flight map was
    /// full, and records the ring could not hold.
    pub fn dropped(&self) -> (u64, u64) {
        (
            self.counters.read(DROP_INFLIGHT),
            self.counters.read(DROP_RING),
        )
    }

    /// Closing events with no opening this run recorded.
    ///
    /// Not a loss: nothing was dropped. It is a completion for a request that
    /// was already in flight when the programs attached -- or, under a process
    /// target, one belonging to somebody else, which is most of them.
    pub fn unpaired(&self) -> u64 {
        self.counters.read(UNPAIRED)
    }

    /// Requests opened and not yet closed.
    ///
    /// Their interval is real and is in no event, because it has no end yet.
    /// That is a fact about the window rather than a loss, and it is reported
    /// as one.
    pub fn still_in_flight(&self) -> usize {
        self.inflight
            .as_ref()
            .and_then(|map| map.len().ok())
            .unwrap_or(0)
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

    fn kernel_btf() -> Option<&'static Btf> {
        match super::super::kernel_btf() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    /// The program writes fields at stack offsets and `from_bytes` reads them
    /// at struct offsets. Nothing but this test connects the two, and a
    /// mismatch would be a field quietly read from the wrong place -- a
    /// latency taken out of the slot holding a sector number is a number, and
    /// a large one.
    #[test]
    fn the_record_struct_matches_what_the_program_writes() {
        use std::mem::offset_of;
        assert_eq!(std::mem::size_of::<BlockEvent>(), RECORD_BYTES);
        for (stack, field, name) in [
            (EV_TS, offset_of!(BlockEvent, ts_ns), "ts_ns"),
            (EV_RQ, offset_of!(BlockEvent, rq), "rq"),
            (EV_OPEN_TS, offset_of!(BlockEvent, open_ts), "open_ts"),
            (EV_DELTA, offset_of!(BlockEvent, delta_ns), "delta_ns"),
            (EV_SECTOR, offset_of!(BlockEvent, sector), "sector"),
            (EV_DEV, offset_of!(BlockEvent, dev), "dev"),
            (EV_BYTES, offset_of!(BlockEvent, bytes), "bytes"),
            (EV_CMD_FLAGS, offset_of!(BlockEvent, cmd_flags), "cmd_flags"),
            (EV_STATUS, offset_of!(BlockEvent, status), "status"),
            (EV_TID, offset_of!(BlockEvent, tid), "tid"),
            (EV_TGID, offset_of!(BlockEvent, tgid), "tgid"),
            (EV_CPU, offset_of!(BlockEvent, cpu), "cpu"),
            (
                EV_DONE_BYTES,
                offset_of!(BlockEvent, done_bytes),
                "done_bytes",
            ),
            (EV_FLAGS, offset_of!(BlockEvent, flags), "flags"),
            (EV_COMM, offset_of!(BlockEvent, comm), "comm"),
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
        assert!(BlockEvent::from_bytes(&[0u8; 32]).is_none());
    }

    /// The kernel packs a `dev_t` as twelve bits of major and twenty of minor,
    /// and this is the only place that is unpacked. `259:0` is what an NVMe
    /// namespace looks like, and getting the split wrong reports every device
    /// on the machine as the same one.
    #[test]
    fn a_packed_device_number_splits_into_the_pair_the_kernel_writes() {
        let mut bytes = [0u8; RECORD_BYTES];
        bytes[40..44].copy_from_slice(&((259u32 << 20) | 3).to_ne_bytes());
        let event = BlockEvent::from_bytes(&bytes).expect("a full record");
        assert_eq!((event.major(), event.minor()), (259, 3));
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
    /// `bpf_ktime_get_boot_ns` includes suspend time, so a program that called
    /// it would put this stream in a different clock domain from every other
    /// collector, every join across them would be wrong, and nothing anywhere
    /// would report an error.
    #[test]
    fn every_program_reads_the_monotonic_clock_and_no_other() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        for stage in [Stage::Queue, Stage::Latency, Stage::Size] {
            let mut programs = vec![close_instructions(btf, stage, Submitter::Any, 3, 4, 5)
                .unwrap_or_else(|reason| panic!("{stage:?} close: {reason}"))];
            if stage.opens_at().is_some() {
                programs.push(
                    open_instructions(btf, stage, Submitter::Any, 3, 5)
                        .unwrap_or_else(|reason| panic!("{stage:?} open: {reason}")),
                );
            }
            for program in programs {
                let called = helpers_called(&program);
                assert!(
                    called.contains(&helper::KTIME_GET_NS),
                    "{stage:?} must timestamp"
                );
                assert!(
                    !called.contains(&KTIME_GET_BOOT_NS),
                    "boot time includes suspend, and mixing clock domains breaks every join \
                     silently"
                );
            }
        }
    }

    /// The size handed to `bpf_ringbuf_output` is the size of the record the
    /// rest of the program wrote. A smaller one publishes a truncated record
    /// and a larger one publishes whatever is on the stack behind it.
    #[test]
    fn the_published_record_is_exactly_the_size_of_the_struct() {
        let Some(btf) = kernel_btf() else { return };
        for stage in [Stage::Queue, Stage::Latency, Stage::Size] {
            let program =
                close_instructions(btf, stage, Submitter::Any, 3, 4, 5).expect("a program");
            let output_at = program
                .iter()
                .position(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT)
                .expect("the program publishes its record");
            let length = program[..output_at]
                .iter()
                .rev()
                .find(|insn| insn.regs & 0xf == 3 && insn.code == 0xb7)
                .expect("a length is moved into r3");
            assert_eq!(length.imm as usize, std::mem::size_of::<BlockEvent>());
        }
    }

    /// Every byte of the record is written before it is published. The program
    /// clears the whole thing rather than tracking the holes, and this is what
    /// says it still does: an uninitialised byte is not a wrong number, it is
    /// whatever the last program to use this stack left there.
    #[test]
    fn the_whole_record_is_cleared_before_any_of_it_is_filled() {
        let Some(btf) = kernel_btf() else { return };
        let program =
            close_instructions(btf, Stage::Latency, Submitter::Any, 3, 4, 5).expect("a program");
        let cleared: Vec<i16> = program
            .iter()
            .take_while(|insn| insn.code != 0x85 || insn.imm != helper::GET_SMP_PROCESSOR_ID)
            // `*(u64 *)(r10 + off) = 0`
            .filter(|insn| insn.code == 0x7a && insn.regs & 0xf == 10 && insn.imm == 0)
            .map(|insn| insn.off)
            .collect();
        for slot in 0..(RECORD_BYTES / 8) as i16 {
            assert!(
                cleared.contains(&(EVENT + slot * 8)),
                "byte {} of the record is never cleared",
                slot * 8
            );
        }
    }

    /// A machine-wide program keeps every request on the machine, so it must
    /// carry no comparison against a process; a targeted one must carry
    /// exactly one, at the moment the submitting task is the current task.
    #[test]
    fn the_filter_is_present_only_when_a_process_was_named() {
        let Some(btf) = kernel_btf() else { return };
        let machine_wide =
            open_instructions(btf, Stage::Latency, Submitter::Any, 3, 5).expect("a program");
        assert!(
            !machine_wide.iter().any(|insn| insn.imm == 4242),
            "a machine-wide program compares against no process"
        );
        let filtered = open_instructions(btf, Stage::Latency, Submitter::Process(4242), 3, 5)
            .expect("a program");
        assert_eq!(
            filtered.iter().filter(|insn| insn.imm == 4242).count(),
            1,
            "one comparison, against the thread group of the submitting task"
        );
    }

    /// The close program reuses the open program's comparison by pointing it
    /// at the record instead of at an entry, which only works while the two
    /// hold `tid` and `tgid` the same distance apart. Nothing else says so.
    #[test]
    fn the_record_and_the_entry_agree_on_where_the_submitter_is() {
        assert_eq!(EV_TID - VAL_TID + VAL_TGID, EV_TGID);
    }

    /// A thread target must compare the thread, not its process. Widening it
    /// would be a filter reported as applied and applied to something else.
    #[test]
    fn a_thread_target_compares_the_thread_and_a_process_target_the_process() {
        assert_eq!(
            Submitter::Thread(7).comparison(ENTRY_VALUE),
            Some((ENTRY_VALUE + VAL_TID, 7))
        );
        assert_eq!(
            Submitter::Process(7).comparison(ENTRY_VALUE),
            Some((ENTRY_VALUE + VAL_TGID, 7))
        );
        assert_eq!(Submitter::Any.comparison(ENTRY_VALUE), None);
    }

    /// `io.size` has no opening tracepoint, so its close reads the submitter
    /// from the current task and its filter lives there instead.
    #[test]
    fn the_unpaired_stage_filters_in_its_only_program() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(Stage::Size.opens_at(), None);
        let filtered = close_instructions(btf, Stage::Size, Submitter::Process(4242), -1, 4, 5)
            .expect("a program");
        assert_eq!(
            filtered.iter().filter(|insn| insn.imm == 4242).count(),
            1,
            "the only program there is has to carry the comparison"
        );
    }

    /// The paired stages subtract; the unpaired one has nothing to subtract
    /// from and must not pretend to.
    #[test]
    fn only_a_paired_stage_computes_an_interval() {
        let Some(btf) = kernel_btf() else { return };
        // `r1 -= r9`, the one subtraction in any of these programs.
        let subtracts = |program: &[Insn]| program.iter().any(|insn| insn.code == 0x1f);
        assert!(subtracts(
            &close_instructions(btf, Stage::Latency, Submitter::Any, 3, 4, 5).expect("a program")
        ));
        assert!(subtracts(
            &close_instructions(btf, Stage::Queue, Submitter::Any, 3, 4, 5).expect("a program")
        ));
        assert!(!subtracts(
            &close_instructions(btf, Stage::Size, Submitter::Any, -1, 4, 5).expect("a program")
        ));
    }

    /// The stages must not be wired to each other's tracepoints. The pairs are
    /// the whole definition of what each source measures, and swapping two of
    /// them produces a number rather than an error.
    #[test]
    fn each_stage_names_the_pair_that_defines_its_measurement() {
        assert_eq!(Stage::Queue.opens_at(), Some("block_rq_insert"));
        assert_eq!(Stage::Queue.closes_at(), "block_rq_issue");
        assert_eq!(Stage::Latency.opens_at(), Some("block_rq_issue"));
        assert_eq!(Stage::Latency.closes_at(), "block_rq_complete");
        assert_eq!(Stage::Size.opens_at(), None);
        assert_eq!(Stage::Size.closes_at(), "block_rq_issue");
    }

    /// The prototypes are checked against the running kernel rather than
    /// assumed. If this fails on a machine, the programs must not be loaded
    /// there: the slot they read holds something else.
    #[test]
    fn the_running_kernel_declares_the_prototypes_these_programs_assume() {
        let Some(btf) = kernel_btf() else { return };
        for (tracepoint, expected) in [
            ("block_rq_insert", &REQUEST_SIGNATURE[..]),
            ("block_rq_issue", &REQUEST_SIGNATURE[..]),
            ("block_rq_complete", &COMPLETE_SIGNATURE[..]),
        ] {
            let signature = btf
                .tracepoint_signature(tracepoint)
                .unwrap_or_else(|reason| panic!("{tracepoint}: {reason}"));
            assert_eq!(
                signature, expected,
                "this kernel declares a different `{tracepoint}`"
            );
        }
    }

    /// A prototype that does not match is refused by name, and the refusal
    /// says what the program would have read.
    #[test]
    fn a_tracepoint_of_another_shape_is_refused_with_both_prototypes() {
        let Some(btf) = kernel_btf() else { return };
        let error = check_signature(btf, "block_rq_complete", &REQUEST_SIGNATURE)
            .expect_err("the prototypes differ");
        assert!(error.contains("blk_status_t"), "{error}");
        assert!(error.contains("produces a number"), "{error}");
    }
}
