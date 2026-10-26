// SPDX-License-Identifier: MIT OR GPL-2.0
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
//! **Except when a histogram is all that was asked for.** `io.latency
//! --emit hist`, alone, with no filter and no key but the device, is answered by
//! a closing program that counts each interval into a `(device, log2 bucket)`
//! row of a hash ([`BlockProgram::load_histogram`]) and publishes nothing:
//! phase 6's criterion is that load at two million requests a second, where a
//! record per request is the cost that grows with the workload. The rows are
//! read once, at the end, after the closing program is detached.
//!
//! Those two programs are their own pair, not the ring pair with a different
//! ending ([`histogram_open_instructions`], [`histogram_close_instructions`]).
//! The ring pair, counting into a row, cost 23.6 % of a 3.16 M IOPS load: per
//! request it read the submitter's name and request identity nobody would see,
//! six fields of the request where one row needs the device, inserted and
//! deleted a hash entry under two bucket locks, and added to a row every cpu
//! shared with two atomic adds. The histogram pair reads the clock, the entry
//! and the device, writes the entry in place, and adds to its own cpu's copy of
//! the row (`docs/scenarios/io.md`, defect 19).
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
use super::insn::{self, helper, read_field, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash};
use super::request::read_request_id;
use super::ring::{ring_bytes, RingBuffer};
use super::sched_common::Descendants;
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};
use crate::bpf::program::load_raw_tracepoint as load;

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

/// What a program keeps: the submitter, and for a process the map of the
/// processes it started since (or had under it at) the attach.
///
/// A launched `fio` forks its job, and the job submits every read: compared
/// with the launched process alone, `io.latency` reported 0 requests for
/// 28 807 reads (`docs/scenarios/io.md`, defect 25). The root is compared as a
/// constant first, so a program that starts nothing costs its own requests no
/// lookup; any other thread group is looked up in the map
/// [`super::sched_common::Descendants`] keeps from `sched_process_fork`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Kept {
    submitter: Submitter,
    descendants: Option<i32>,
}

impl Kept {
    #[cfg(test)]
    fn only(submitter: Submitter) -> Self {
        Self {
            submitter,
            descendants: None,
        }
    }

    /// Falls through when the submitter whose ids sit in the entry laid out at
    /// `base` is one this run keeps, and jumps to `skip` when it is not.
    /// Clobbers `r0` to `r5`. `kept` is the label the root's comparison jumps
    /// to past the lookup, one per call site.
    fn push(self, program: &mut Program, base: i16, skip: &'static str, kept: &'static str) {
        let Some((slot, wanted)) = self.submitter.comparison(base) else {
            return;
        };
        program.push(Insn::ldx(BPF_W, 1, R10, slot));
        match (self.submitter, self.descendants) {
            (Submitter::Process(_), Some(map)) => {
                program.jump_to(kept, Insn::jeq_imm(1, wanted as i32, 0));
                program.extend(Insn::load_map_fd(1, map));
                program.push(Insn::mov64_reg(2, R10));
                program.push(Insn::add64_imm(2, slot as i32));
                program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
                program.jump_to(skip, Insn::jeq_imm(0, 0, 0));
                program.label(kept);
            }
            _ => {
                program.jump_to(skip, Insn::jne_imm(1, wanted as i32, 0));
            }
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
pub(super) const REQUEST_SIGNATURE: [&str; 1] = ["request *"];
pub(super) const COMPLETE_SIGNATURE: [&str; 3] = ["request *", "blk_status_t", "unsigned int"];
/// `block_rq_complete` on 5.15: the same three slots, with the status already
/// turned into a negative errno by `blk_status_to_errno`. The slot is read the
/// same way and means something else, so the collector is told which it is
/// ([`BlockProgram::status_is_errno`]) and names the attribute accordingly.
pub(super) const ERRNO_COMPLETE_SIGNATURE: [&str; 3] = ["request *", "int", "unsigned int"];
/// Every shape of the completion the program reads, in the order the catalog's
/// requirement lists them.
const COMPLETE_SIGNATURES: [&[&str]; 2] = [&COMPLETE_SIGNATURE, &ERRNO_COMPLETE_SIGNATURE];

/// How many requests may be in flight at once before the program stops being
/// able to record new ones.
///
/// A queue depth of a few thousand is the most a machine's devices hold
/// together; this leaves room for a synthetic sweep and for the requests of
/// every other process when a run is machine-wide.
const MAX_INFLIGHT: u32 = 16384;

/// How many request pointers a histogram run may see.
///
/// Larger than `MAX_INFLIGHT` because a histogram's entries are not deleted:
/// they count the request structures the kernel has handed this run's
/// requests in, which is every hardware queue's depth on every device used --
/// an NVMe drive with 24 queues of 1023 is 24552 by itself, if every tag is
/// used. At about 72 bytes per preallocated element this is under 5 MiB.
const MAX_REQUESTS_SEEN: u32 = 65536;

/// A histogram's in-flight entry: the open time, and nothing else.
const HIST_ENTRY_VALUE_BYTES: usize = 8;

/// Slots of the drop counter, one per thing that can be lost.
const DROP_INFLIGHT: u32 = 0;
const DROP_RING: u32 = 1;
/// Not a loss: a close with no open this run recorded. Counted in the same
/// per-cpu array because it is the same kind of fact -- something the program
/// saw and could not turn into a measurement -- and reported apart from the
/// losses because nothing was dropped.
const UNPAIRED: u32 = 2;
/// A completion the kernel-side histogram had no row for and could not make
/// one: the map was full. Lost from the histogram, and counted.
const DROP_HIST: u32 = 3;
/// A completion whose request the kernel never stamped at its issue, when
/// the histogram reads the kernel's stamp instead of making its own: a
/// passthrough request, or a queue that stopped keeping statistics. Not
/// measured, and counted as lost.
const UNSTAMPED: u32 = 4;
const DROP_SLOTS: u32 = 5;

/// Rows the kernel-side histogram may hold: one per `(device, log2 bucket)`,
/// so 64 devices each with a completion in every one of 64 buckets. A machine
/// that uses a tenth of that has an unusual number of disks.
const MAX_HIST_ROWS: u32 = 4096;

/// A histogram row, as the program writes it: the key is `(dev, bucket)` and
/// the value `(count, total_ns)`, each word where [`HistRow::decode`] reads it.
const HIST_KEY_BYTES: usize = 8;
const HIST_VALUE_BYTES: usize = 16;
const HIST_VAL_COUNT: i16 = 0;
const HIST_VAL_TOTAL: i16 = 8;

/// `BPF_NOEXIST`, and the errno an insert gets when another cpu won the race.
const BPF_NOEXIST: i32 = 1;
const EEXIST: i32 = 17;

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
const ENTRY_VALUE_BYTES: usize = 40;

/// Who inserted a request into its queue, read at `block_rq_insert` in that
/// task's context: thread id, process id and name, 24 bytes.
///
/// `io.latency` opens at `block_rq_issue`, and the task current there is the
/// submitter only when it issued its own request. A request held in a
/// scheduler queue or a plug and dispatched later is issued by kblockd's
/// `kworker`, and charging it to the kworker loses it from the program's
/// rows. The insert stamp is what the issue reads first.
const SUBMITTER_VALUE_BYTES: usize = 24;
/// Requests inserted and not yet issued at once; an LRU, so one never issued
/// is evicted rather than held.
const MAX_SUBMITTERS: u32 = 16384;
const BPF_MAP_TYPE_LRU_HASH: u32 = 9;

const VAL_OPEN_TS: i16 = 0;
const VAL_TID: i16 = 8;
const VAL_TGID: i16 = 12;
const VAL_COMM: i16 = 16;
/// The request the submitting task was serving, read from its task storage at
/// the open, because at a completion the current task is whatever the
/// interrupt interrupted.
const VAL_REQ: i16 = 32;

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
    /// The request the submitting task was serving, as the id its birth wrote
    /// into the task's storage; zero when it was serving none, or when this
    /// kernel could not give the programs a task-storage map. See
    /// `src/bpf/request.rs`: this field is what makes a block request's
    /// attribution to a request IDENTITY rather than an inference.
    pub req: u64,
}

/// The size of the record the program writes. Stated once, used by the program
/// and checked against the struct.
const RECORD_BYTES: usize = 104;

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
        let req = long(96);
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
            req,
        })
    }
}

/// Stack offsets of the record, negative from the frame pointer.
const EVENT: i16 = -104;
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
const EV_REQ: i16 = EVENT + 96;

/// Everything below the record, on the close program's stack.
const LOOKUP_KEY: i16 = EVENT - 8;
const PART: i16 = EVENT - 16;
const CLOSE_DROP_KEY: i16 = EVENT - 24;
/// The histogram row's key and the value an insert writes, below the rest.
const HIST_KEY: i16 = EVENT - 32;
const HIST_VALUE: i16 = EVENT - 48;

/// The open program's stack: the entry it builds, its key, and a drop slot.
const ENTRY_VALUE: i16 = -40;
const ENTRY_KEY: i16 = -48;
const OPEN_DROP_KEY: i16 = -56;

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

/// The program on the opening tracepoint: record when, and by whom.
///
/// Nothing is measured here. What it writes is one map entry per in-flight
/// request, which the closing program reads back; a request whose close this
/// run never sees leaves its entry behind, and `still_in_flight` reports how
/// many there were rather than letting them look like requests that took no
/// time.
/// The program on `block_rq_insert`: who put the request in the queue.
fn insert_instructions(btf: &Btf, submitters_fd: i32) -> Result<Vec<Insn>, String> {
    crate::bpf::program::prototype(btf, "block_rq_insert", &[&REQUEST_SIGNATURE], "the request")?;
    const KEY: i16 = -8;
    const VALUE: i16 = -32;
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 1, 6, 0));
    program.push(Insn::stx(BPF_DW, R10, KEY, 1));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_DW, R10, VALUE, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (VALUE + 8) as i32));
    program.push(Insn::mov64_imm(2, 16));
    program.push(Insn::call(helper::GET_CURRENT_COMM));
    program.extend(Insn::load_map_fd(1, submitters_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

fn open_instructions(
    btf: &Btf,
    stage: Stage,
    kept: Kept,
    inflight_fd: i32,
    counters_fd: i32,
    storage_fd: Option<i32>,
    submitters_fd: Option<i32>,
) -> Result<Vec<Insn>, String> {
    let tracepoint = stage
        .opens_at()
        .expect("an open program is only built for a stage that has one");
    crate::bpf::program::prototype(btf, tracepoint, &[&REQUEST_SIGNATURE], "the request")?;

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

    // `bpf_get_current_comm` writes all sixteen bytes or zeroes them, so the
    // entry carries no stack left over from whatever ran before.
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (ENTRY_VALUE + VAL_COMM) as i32));
    program.push(Insn::mov64_imm(2, 16));
    program.push(Insn::call(helper::GET_CURRENT_COMM));

    // Who inserted it, where `block_rq_insert` saw it: the submitter of a
    // request a kworker dispatched, in place of the kworker. A request that
    // skipped the queue was never inserted, and its issuer is its submitter.
    if let Some(submitters) = submitters_fd {
        program.push(Insn::stx(BPF_DW, R10, ENTRY_KEY, 7));
        program.extend(Insn::load_map_fd(1, submitters));
        program.push(Insn::mov64_reg(2, R10));
        program.push(Insn::add64_imm(2, ENTRY_KEY as i32));
        program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
        program.jump_to("submitter_known", Insn::jeq_imm(0, 0, 0));
        program.push(Insn::ldx(BPF_DW, 1, 0, 0));
        program.push(Insn::stx(BPF_DW, R10, ENTRY_VALUE + VAL_TID, 1));
        program.push(Insn::ldx(BPF_DW, 1, 0, 8));
        program.push(Insn::stx(BPF_DW, R10, ENTRY_VALUE + VAL_COMM, 1));
        program.push(Insn::ldx(BPF_DW, 1, 0, 16));
        program.push(Insn::stx(BPF_DW, R10, ENTRY_VALUE + VAL_COMM + 8, 1));
        program.extend(Insn::load_map_fd(1, submitters));
        program.push(Insn::mov64_reg(2, R10));
        program.push(Insn::add64_imm(2, ENTRY_KEY as i32));
        program.push(Insn::call(helper::MAP_DELETE_ELEM));
        program.label("submitter_known");
    }

    // A run observes one process. The thread group is known before the program
    // loads and never changes, so the comparison is written into the
    // instruction stream rather than looked up in a map.
    kept.push(&mut program, ENTRY_VALUE, "done", "open_kept");

    // The request, from the same task at the same moment. Zero when there is
    // no map to read, so the entry never carries stack left over.
    match storage_fd {
        Some(fd) => read_request_id(&mut program, fd, ENTRY_VALUE + VAL_REQ, "open_req"),
        None => {
            program.push(Insn::st_imm(BPF_DW, R10, ENTRY_VALUE + VAL_REQ, 0));
        }
    }

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
    kept: Kept,
    inflight_fd: i32,
    ring_fd: i32,
    counters_fd: i32,
    storage_fd: Option<i32>,
) -> Result<Vec<Insn>, String> {
    let tracepoint = stage.closes_at();
    let accepted: &[&[&str]] = if tracepoint == "block_rq_complete" {
        &COMPLETE_SIGNATURES
    } else {
        &[&REQUEST_SIGNATURE]
    };
    crate::bpf::program::prototype(btf, tracepoint, accepted, "the request")?;

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
        // The second argument is `blk_status_t` -- or, on 5.15, the errno it
        // was turned into -- and the third the bytes this completion
        // accounted for.
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
        program.push(Insn::ldx(BPF_DW, 1, 0, VAL_REQ));
        program.push(Insn::stx(BPF_DW, R10, EV_REQ, 1));

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
        if let Some(fd) = storage_fd {
            read_request_id(&mut program, fd, EV_REQ, "close_req");
        }

        // The record's own tid and tgid sit where an entry's would, so the
        // comparison is the same one the open program makes.
        kept.push(&mut program, EV_TID - VAL_TID, "done", "close_kept");
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
            if let Some(fd) = storage_fd {
                read_request_id(&mut program, fd, EV_REQ, "direct_req");
            }
            kept.push(&mut program, EV_TID - VAL_TID, "done", "direct_kept");
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

/// The opening program of a kernel-side histogram: when, and nothing else.
///
/// A histogram row is `(device, bucket)`, so the submitter's name, its thread
/// and the request it was serving -- what the ring record carries, and what
/// [`open_instructions`] spends a `bpf_get_current_comm`, a task-storage
/// lookup and a forty-byte entry on -- are read by nobody. What is left is the
/// filter, the clock and one store.
///
/// **The entry outlives its request.** A request pointer is one slot of a
/// pool the kernel allocated once per queue and hands out again as soon as the
/// request completes, so the keys this map sees are a few thousand pointers
/// used over and over. The ring path inserts an entry at every open and
/// deletes it at every close, which at two million requests a second is two
/// bucket locks and a free list each way per request. Here the entry is
/// inserted the first time a pointer is seen and written in place every time
/// after, and the close marks it consumed by zeroing the time instead of
/// deleting it: an inlined hash lookup on each side, and no lock. A zero time
/// is "not in flight", which `bpf_ktime_get_ns` never returns.
fn histogram_open_instructions(
    btf: &Btf,
    stage: Stage,
    kept: Kept,
    inflight_fd: i32,
    counters_fd: i32,
) -> Result<Vec<Insn>, String> {
    let tracepoint = stage
        .opens_at()
        .expect("an open program is only built for a stage that has one");
    crate::bpf::program::prototype(btf, tracepoint, &[&REQUEST_SIGNATURE], "the request")?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));

    // The filter reads the task where the ring path's entry would hold it, so
    // the comparison is the same one, against the same slot.
    if kept.submitter != Submitter::Any {
        program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
        program.push(Insn::stx(BPF_W, R10, ENTRY_VALUE + VAL_TID, 0));
        program.push(Insn::rsh64_imm(0, 32));
        program.push(Insn::stx(BPF_W, R10, ENTRY_VALUE + VAL_TGID, 0));
        kept.push(&mut program, ENTRY_VALUE, "done", "hist_kept");
    }

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    program.push(Insn::stx(BPF_DW, R10, ENTRY_KEY, 7));
    program.extend(Insn::load_map_fd(1, inflight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ENTRY_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first_sighting", Insn::jeq_imm(0, 0, 0));
    // The pointer has been seen before: its slot is written in place.
    program.push(Insn::stx(BPF_DW, 0, VAL_OPEN_TS, 8));
    program.jump_to("done", Insn::ja());

    // The first time this run sees the pointer. `BPF_ANY` rather than
    // `BPF_NOEXIST`: nothing else inserts this key, and if something had, the
    // newer time is the right one.
    program.label("first_sighting");
    program.push(Insn::stx(BPF_DW, R10, ENTRY_VALUE + VAL_OPEN_TS, 8));
    program.extend(Insn::load_map_fd(1, inflight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ENTRY_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ENTRY_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    // The map is full of pointers already seen: this request is never timed.
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

/// Where a kernel-side histogram's interval starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistStart {
    /// The opening program's stamp, in the in-flight map under this fd.
    Paired(i32),
    /// The kernel's own `request.io_start_time_ns`, at this offset: stamped
    /// in `blk_mq_start_request`, where `block_rq_issue` fires, on a queue
    /// that keeps statistics. One program instead of two, and no map.
    Kernel(i32),
}

/// The closing program of a kernel-side histogram: subtract, bucket, count.
///
/// What it reads out of the request is the device, because that is half the
/// row's key, and nothing else: the ring path's sector, length, flags, status,
/// cpu and scope are fields of a record this program does not write.
///
/// **The row is per-cpu, and the additions plain.** The first version counted
/// into one shared hash with two atomic adds per request, and at two million
/// completions a second on eight cpus those adds all land on the same cache
/// line -- one device, and almost every request in one or two buckets -- which
/// then moves between cpus on every request. A `BPF_MAP_TYPE_PERCPU_HASH`
/// gives each cpu its own copy of the row, which nothing else writes: a raw
/// tracepoint program runs with preemption off and is never re-entered on its
/// own cpu (the kernel skips and counts the nested run, [`BlockProgram::skipped`]),
/// so a load, an add and a store cannot be interleaved. The reader sums the
/// copies, once, at the end.
///
/// **`direct`** builds it for a `tp_btf` attach, where the request is a
/// typed kernel pointer the verifier lets the program load through: the two
/// reads of the device are then two loads, where a raw tracepoint program
/// pays a `bpf_probe_read_kernel` call for each. They were most of the
/// program's 149 ns a run at two million requests a second (`docs/ROADMAP.md`,
/// phase 6).
///
/// **`HistStart::Kernel`** takes the start from the request itself, which only
/// a `direct` program can load.
fn histogram_close_instructions(
    btf: &Btf,
    stage: Stage,
    start: HistStart,
    hist_fd: i32,
    counters_fd: i32,
    direct: bool,
) -> Result<Vec<Insn>, String> {
    if matches!(start, HistStart::Kernel(_)) && !direct {
        return Err("the kernel's issue stamp is read through `tp_btf` alone".to_owned());
    }
    if stage.opens_at().is_none() {
        return Err(format!(
            "a kernel-side histogram is of an interval, and `{}` alone measures none",
            stage.closes_at()
        ));
    }
    let tracepoint = stage.closes_at();
    let accepted: &[&[&str]] = if tracepoint == "block_rq_complete" {
        &COMPLETE_SIGNATURES
    } else {
        &[&REQUEST_SIGNATURE]
    };
    crate::bpf::program::prototype(btf, tracepoint, accepted, "the request")?;
    let part = btf.member_offset("request", "part")?;
    let bd_dev = btf.member_offset("block_device", "bd_dev")?;

    const LABELS: insn::Log2Labels = [
        "hist_b32",
        "hist_b16",
        "hist_b8",
        "hist_b4",
        "hist_b2",
        "hist_b1",
        "hist_bucket",
    ];

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    match start {
        HistStart::Paired(inflight_fd) => {
            program.push(Insn::stx(BPF_DW, R10, LOOKUP_KEY, 7));
            program.extend(Insn::load_map_fd(1, inflight_fd));
            program.push(Insn::mov64_reg(2, R10));
            program.push(Insn::add64_imm(2, LOOKUP_KEY as i32));
            program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
            // No entry, or one whose time was consumed: this run never saw
            // the request open, because it opened before the attach or
            // belongs to a task the open program refused.
            program.jump_to("unpaired", Insn::jeq_imm(0, 0, 0));
            program.push(Insn::ldx(BPF_DW, 9, 0, VAL_OPEN_TS));
            program.jump_to("unpaired", Insn::jeq_imm(9, 0, 0));
            program.push(Insn::st_imm(BPF_DW, 0, VAL_OPEN_TS, 0));
        }
        HistStart::Kernel(offset) => {
            // Zeroed when the request is set up, written at the issue only
            // where the queue keeps statistics.
            program.push(Insn::ldx(BPF_DW, 9, 7, offset as i16));
            program.jump_to("unstamped", Insn::jeq_imm(9, 0, 0));
        }
    }
    // The interval, kept in r8 across every helper call below.
    program.push(Insn::sub64_reg(8, 9));
    // A stamp the kernel took from a plug's cached clock is never later than
    // this read; a negative interval would be a stamp from somewhere else.
    if matches!(start, HistStart::Kernel(_)) {
        program.jump_to("stamp_ok", Insn::jsge_imm(8, 0, 0));
        program.push(Insn::mov64_imm(8, 0));
        program.label("stamp_ok");
    }

    // The device, or zero for a request that belongs to no partition -- the
    // same reading, and the same zero, as the ring path's.
    program.push(Insn::st_imm(BPF_DW, R10, HIST_KEY, 0));
    if direct {
        // A load through a typed pointer that faults reads zero, which is
        // the same zero the helper leaves on a failed read.
        program.push(Insn::ldx(BPF_DW, 9, 7, part as i16));
        program.jump_to("no_device", Insn::jeq_imm(9, 0, 0));
        program.push(Insn::ldx(BPF_W, 1, 9, bd_dev as i16));
        program.push(Insn::stx(BPF_W, R10, HIST_KEY, 1));
    } else {
        read_field(&mut program, PART, 8, 7, part);
        program.push(Insn::ldx(BPF_DW, 9, R10, PART));
        program.jump_to("no_device", Insn::jeq_imm(9, 0, 0));
        read_field(&mut program, HIST_KEY, 4, 9, bd_dev);
    }
    program.label("no_device");

    insn::push_log2_bucket(&mut program, 8, &LABELS);
    program.push(Insn::stx(BPF_W, R10, HIST_KEY + 4, 3));

    let lookup = |program: &mut Program| {
        program.extend(Insn::load_map_fd(1, hist_fd));
        program.push(Insn::mov64_reg(2, R10));
        program.push(Insn::add64_imm(2, HIST_KEY as i32));
        program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    };
    // This cpu's copy of the row: plain loads and stores, no `lock` prefix.
    let add = |program: &mut Program| {
        program.push(Insn::ldx(BPF_DW, 1, 0, HIST_VAL_COUNT));
        program.push(Insn::add64_imm(1, 1));
        program.push(Insn::stx(BPF_DW, 0, HIST_VAL_COUNT, 1));
        program.push(Insn::ldx(BPF_DW, 1, 0, HIST_VAL_TOTAL));
        program.push(Insn::add64_reg(1, 8));
        program.push(Insn::stx(BPF_DW, 0, HIST_VAL_TOTAL, 1));
        program.jump_to("done", Insn::ja());
    };

    lookup(&mut program);
    program.jump_to("hist_new", Insn::jeq_imm(0, 0, 0));
    add(&mut program);

    // A new row: the kernel writes this value into this cpu's copy and zeroes
    // every other cpu's.
    program.label("hist_new");
    program.push(Insn::st_imm(BPF_DW, R10, HIST_VALUE + HIST_VAL_COUNT, 1));
    program.push(Insn::stx(BPF_DW, R10, HIST_VALUE + HIST_VAL_TOTAL, 8));
    program.extend(Insn::load_map_fd(1, hist_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, HIST_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, HIST_VALUE as i32));
    program.push(Insn::mov64_imm(4, BPF_NOEXIST));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.jump_to("hist_raced", Insn::jeq_imm(0, -EEXIST, 0));
    count(&mut program, counters_fd, CLOSE_DROP_KEY, DROP_HIST, "done");
    program.jump_to("done", Insn::ja());

    // Another cpu made the row between the lookup and the insert: add to this
    // cpu's copy of it, once, straight-line -- a jump back is a loop the
    // verifier refuses.
    program.label("hist_raced");
    lookup(&mut program);
    program.jump_to("hist_gone", Insn::jeq_imm(0, 0, 0));
    add(&mut program);
    program.label("hist_gone");
    count(&mut program, counters_fd, CLOSE_DROP_KEY, DROP_HIST, "done");
    program.jump_to("done", Insn::ja());

    // Only the start in use has a path here: a block nothing jumps to is code
    // the verifier refuses as unreachable.
    match start {
        HistStart::Paired(_) => {
            program.label("unpaired");
            count(&mut program, counters_fd, CLOSE_DROP_KEY, UNPAIRED, "done");
        }
        HistStart::Kernel(_) => {
            program.label("unstamped");
            count(&mut program, counters_fd, CLOSE_DROP_KEY, UNSTAMPED, "done");
        }
    }

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Whether every request-based block device keeps queue statistics, which
/// is what has the kernel stamp `io_start_time_ns` at each issue: writeback
/// throttling keeps them, and says so with a non-zero `wbt_lat_usec`. A
/// bio-based device (no `mq` directory: zram) issues no request and does not
/// count. A device that answers nothing here is taken for one without.
fn queues_keep_statistics() -> bool {
    let Ok(devices) = std::fs::read_dir("/sys/block") else {
        return false;
    };
    let mut any = false;
    for device in devices.flatten() {
        let path = device.path();
        let request_based =
            std::fs::read_dir(path.join("mq")).is_ok_and(|mut queues| queues.next().is_some());
        if !request_based {
            continue;
        }
        any = true;
        let keeps = std::fs::read_to_string(path.join("queue/wbt_lat_usec"))
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .is_some_and(|usec| usec > 0);
        if !keeps {
            return false;
        }
    }
    any
}

/// One `(device, log2 bucket)` row of the kernel-side latency histogram, over
/// the whole run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistRow {
    /// The device, packed the way `block_device.bd_dev` is.
    pub dev: u32,
    /// `emit::BucketLayout::Log2`'s numbering.
    pub bucket: u32,
    pub count: u64,
    /// The exact sum of the intervals, which the bucket cannot give back.
    pub total_ns: u64,
}

impl HistRow {
    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < HIST_KEY_BYTES || value.len() < HIST_VALUE_BYTES {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(key[at..at + 4].try_into().expect("4"));
        let long = |at: i16| {
            let at = at as usize;
            u64::from_ne_bytes(value[at..at + 8].try_into().expect("8"))
        };
        Some(Self {
            dev: word(0),
            bucket: word(4),
            count: long(HIST_VAL_COUNT),
            total_ns: long(HIST_VAL_TOTAL),
        })
    }

    pub fn major(&self) -> u32 {
        self.dev >> 20
    }

    pub fn minor(&self) -> u32 {
        self.dev & ((1 << 20) - 1)
    }

    pub fn bucket_low_ns(&self) -> u64 {
        insn::bucket_low_ns(self.bucket)
    }
}

/// A loaded, attached pair of programs and the maps between them.
///
/// Dropping it detaches both: the links, the programs and the maps are file
/// descriptors, and the kernel unloads a program when the last one closes.
pub struct BlockProgram {
    ring: RingBuffer,
    counters: Counters,
    inflight: Option<Hash>,
    open_program: Option<OwnedFd>,
    /// Taken by [`BlockProgram::detach`], with `close_link`.
    open_link: Option<OwnedFd>,
    close_program: OwnedFd,
    /// Taken when the histogram is read, so the read is of a map nothing
    /// writes any more.
    close_link: Option<OwnedFd>,
    /// The kernel-side histogram, when the closing program counts into one
    /// rather than publishing a record per request.
    histogram: Option<PerCpuRows>,
    /// Whether the closing program went through `tp_btf` rather than the raw
    /// tracepoint: the same rows, with two helper calls fewer a request.
    close_is_tp_btf: bool,
    /// Whether it reads the kernel's issue stamp, with no opening program.
    close_reads_kernel_stamp: bool,
    /// Why the programs read no request identity, when they do not: this
    /// kernel would not give them a task-storage map. The measurement is the
    /// same; only `req` is missing from every event, and the run says so.
    pub no_request_identity: Option<String>,
    /// Whether this kernel's completion carries a negative errno rather than a
    /// `blk_status_t`: 5.15's `block_rq_complete(request *, int, unsigned
    /// int)`. Both are zero on success and neither is the other's number.
    pub status_is_errno: bool,
    /// The processes a process target started, followed from the fork.
    descendants: Option<Descendants>,
    /// `io.latency`'s insert stamp: the map, the program and its link.
    _submitters: Option<OwnedFd>,
    insert_program: Option<OwnedFd>,
    insert_link: Option<OwnedFd>,
}

impl BlockProgram {
    pub fn load(
        stage: Stage,
        submitter: Submitter,
        requested_ring_bytes: usize,
    ) -> Result<Self, String> {
        Self::load_into(stage, submitter, requested_ring_bytes, false)
    }

    /// The programs with their measurements counted into a kernel-side log2
    /// histogram, one row per `(device, bucket)`, instead of a ring record per
    /// request: what `--emit hist` alone needs, at a cost that no longer grows
    /// with the request rate. Only a paired stage has an interval to bucket.
    pub fn load_histogram(stage: Stage, submitter: Submitter) -> Result<Self, String> {
        Self::load_into(stage, submitter, 0, true)
    }

    fn load_into(
        stage: Stage,
        submitter: Submitter,
        requested_ring_bytes: usize,
        histogram: bool,
    ) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        // A process is followed into the processes it starts: attached before
        // the block programs, so a fork between the two is in the map.
        let descendants = match submitter {
            Submitter::Process(root) => Some(Descendants::follow(root)?),
            _ => None,
        };
        let kept = Kept {
            submitter,
            descendants: descendants.as_ref().map(Descendants::map_fd),
        };
        // A histogram run writes no record, and keeps the smallest ring the
        // kernel makes so the drain has one to find empty.
        let bytes = ring_bytes(if histogram { 0 } else { requested_ring_bytes });

        let ring_map =
            syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_blk_ring")
                .map_err(|error| map_failure("block ring buffer", error))?;
        let counters = Counters::create(DROP_SLOTS, "omn_blk_count")
            .map_err(|error| map_failure("block drop counter", error))?;

        let inflight = match stage.opens_at() {
            // A histogram's entry is the open time alone, and stays for the
            // run: see `histogram_open_instructions`.
            Some(_) if histogram => Some(
                Hash::create(
                    ENTRY_KEY_BYTES,
                    HIST_ENTRY_VALUE_BYTES,
                    MAX_REQUESTS_SEEN,
                    stage.map_name(),
                )
                .map_err(|error| map_failure("in-flight request map", error))?,
            ),
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

        // The request identity is read wherever the submitting task is the
        // current one. A kernel that cannot make the map still gets the
        // measurement, without the identity, and the collector says which.
        // A histogram's rows carry no request, so its programs read none.
        let (storage_fd, no_request_identity) = match super::request::storage() {
            _ if histogram => (None, None),
            Ok(storage) => (Some(storage.as_raw_fd()), None),
            Err(reason) => (None, Some(reason)),
        };

        let histogram = if histogram {
            Some(
                PerCpuRows::create(MAX_HIST_ROWS, "omn_blk_hist")
                    .map_err(|error| map_failure("block latency histogram", error))?,
            )
        } else {
            None
        };
        // A histogram's close goes through `tp_btf` where the kernel loads
        // it, and otherwise through the raw tracepoint: the same rows, read
        // with two helper calls more.
        // One program, reading the kernel's issue stamp, where every queue
        // keeps it and no submitter is to be told apart at the issue.
        let stamped = match &histogram {
            Some(rows)
                if stage == Stage::Latency
                    && submitter == Submitter::Any
                    && queues_keep_statistics() =>
            {
                btf.member_offset("request", "io_start_time_ns")
                    .and_then(|offset| {
                        histogram_close_instructions(
                            btf,
                            stage,
                            HistStart::Kernel(offset),
                            rows.as_raw_fd(),
                            counters.as_raw_fd(),
                            true,
                        )
                    })
                    .and_then(|close| load_tp_btf(btf, stage.closes_at(), &close, "omn_blk_close"))
                    .ok()
            }
            _ => None,
        };
        let close_reads_kernel_stamp = stamped.is_some();
        let direct = match &histogram {
            _ if stamped.is_some() => stamped,
            Some(rows) => histogram_close_instructions(
                btf,
                stage,
                HistStart::Paired(inflight_fd),
                rows.as_raw_fd(),
                counters.as_raw_fd(),
                true,
            )
            .ok()
            .and_then(|close| load_tp_btf(btf, stage.closes_at(), &close, "omn_blk_close").ok()),
            None => None,
        };
        let close_is_tp_btf = direct.is_some();
        let close_program = match direct {
            Some(program) => program,
            None => {
                let close = match &histogram {
                    Some(rows) => histogram_close_instructions(
                        btf,
                        stage,
                        HistStart::Paired(inflight_fd),
                        rows.as_raw_fd(),
                        counters.as_raw_fd(),
                        false,
                    )?,
                    None => close_instructions(
                        btf,
                        stage,
                        kept,
                        inflight_fd,
                        ring_map.as_raw_fd(),
                        counters.as_raw_fd(),
                        storage_fd,
                    )?,
                };
                load(&close, "omn_blk_close")?
            }
        };
        let status_is_errno = stage.closes_at() == "block_rq_complete"
            && crate::bpf::program::prototype(
                btf,
                "block_rq_complete",
                &COMPLETE_SIGNATURES,
                "the request",
            )? == 1;

        // The opening program is attached first, and that order is not
        // arbitrary. An entry written with no closing program attached ages in
        // the map and is reported as still in flight, which is harmless; a
        // close attached first would find entries only for requests that
        // happened to open after it, which is the same thing one tick later.
        // What neither order can produce is a measurement from an entry nobody
        // wrote, because the lookup that fails is the only path to it.
        // `io.latency`'s submitters, stamped at the insert, attached before
        // the issue that reads them.
        let (submitters, insert_program, insert_link) = if stage == Stage::Latency
            && histogram.is_none()
            && btf.has_type("btf_trace_block_rq_insert")
        {
            let map = syscall::map_create(
                BPF_MAP_TYPE_LRU_HASH,
                ENTRY_KEY_BYTES as u32,
                SUBMITTER_VALUE_BYTES as u32,
                MAX_SUBMITTERS,
                "omn_blk_subm",
            )
            .map_err(|error| map_failure("block submitter map", error))?;
            let program = load(
                &insert_instructions(btf, map.as_raw_fd())?,
                "omn_blk_insert",
            )?;
            let link = crate::bpf::program::attach_raw("block_rq_insert", &program)?;
            (Some(map), Some(program), Some(link))
        } else {
            (None, None, None)
        };
        let (open_program, open_link) = match stage.opens_at() {
            // The kernel stamps the issue itself: nothing to open.
            Some(_) if close_reads_kernel_stamp => (None, None),
            Some(tracepoint) => {
                let open = if histogram.is_some() {
                    histogram_open_instructions(
                        btf,
                        stage,
                        kept,
                        inflight_fd,
                        counters.as_raw_fd(),
                    )?
                } else {
                    open_instructions(
                        btf,
                        stage,
                        kept,
                        inflight_fd,
                        counters.as_raw_fd(),
                        storage_fd,
                        submitters.as_ref().map(AsRawFd::as_raw_fd),
                    )?
                };
                let program = load(&open, "omn_blk_open")?;
                let link = crate::bpf::program::attach_raw(tracepoint, &program)?;
                (Some(program), Some(link))
            }
            None => (None, None),
        };

        let closes_at = stage.closes_at();
        let close_link = if close_is_tp_btf {
            syscall::tracing_open(close_program.as_raw_fd())
        } else {
            syscall::raw_tracepoint_open(closes_at, close_program.as_raw_fd())
        }
        .map_err(|error| format!("cannot attach to the `{closes_at}` tracepoint: {error}"))?;

        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the block ring buffer: {error}"))?;

        Ok(Self {
            ring,
            counters,
            inflight,
            open_program,
            open_link,
            close_program,
            close_link: Some(close_link),
            histogram,
            close_is_tp_btf,
            close_reads_kernel_stamp,
            no_request_identity,
            status_is_errno,
            descendants,
            _submitters: submitters,
            insert_program,
            insert_link,
        })
    }

    /// Processes the target started that no program here followed, because
    /// the map of them was full.
    pub fn unfollowed(&self) -> u64 {
        self.descendants.as_ref().map_or(0, Descendants::missed)
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
    /// full, and completions it could not keep: records the ring could not
    /// hold, or, counting into a histogram, completions with no row to go in.
    pub fn dropped(&self) -> (u64, u64) {
        (
            self.counters.read(DROP_INFLIGHT),
            self.counters.read(DROP_RING) + self.counters.read(DROP_HIST),
        )
    }

    /// Whether the closing program counts into a kernel-side histogram.
    pub fn is_histogram(&self) -> bool {
        self.histogram.is_some()
    }

    /// Whether the closing program is attached through `tp_btf`.
    pub fn close_is_tp_btf(&self) -> bool {
        self.close_is_tp_btf
    }

    /// Whether the histogram's interval starts at the kernel's own issue
    /// stamp, read by the closing program alone.
    pub fn close_reads_kernel_stamp(&self) -> bool {
        self.close_reads_kernel_stamp
    }

    /// Completions the stamped close could not measure: their request
    /// carried no issue stamp.
    pub fn unstamped(&self) -> u64 {
        self.counters.read(UNSTAMPED)
    }

    /// Detaches both programs and keeps them loaded.
    ///
    /// What the kernel has counted of a program's runs (`run_time_ns` and
    /// `run_cnt`, under `kernel.bpf_stats_enabled`) is read from its
    /// descriptor, and the run reads it when it closes its accounts, after
    /// every collector has stopped. A stop that closed the programs left that
    /// read nothing to find: the coordinator's `null_blk` run printed *"the
    /// programs ran 0 times for 0 ns"* beside a histogram of 26 838 511
    /// requests. Detached, they run no more; loaded, their account stays
    /// readable until the collector is dropped.
    pub fn detach(&mut self) {
        self.insert_link = None;
        self.open_link = None;
        self.close_link = None;
    }

    /// The kernel-side histogram, read once, at the end.
    ///
    /// The closing program is detached first and a grace period waited for,
    /// so nothing is adding to a row while it is read: a completion either
    /// landed before the detach, and is in a row, or happened after the run's
    /// window, and is in none. Empty when the programs publish records.
    ///
    /// A kernel that withdraws the grace-period wait (`nohz_full`) is read
    /// anyway, after the detach, and the second value says so: a run of the
    /// closing program that began before the detach and was preempted could
    /// still add to a row after it was read.
    pub fn read_histogram(&mut self) -> Result<(Vec<HistRow>, Option<String>), String> {
        let Some(map) = &self.histogram else {
            return Ok((Vec::new(), None));
        };
        self.close_link = None;
        let unwaited = super::map::grace_period().err().map(|error| {
            format!(
                "the histogram was read after its program was detached without waiting for \
                 the runs already under way ({error}); a completion one of them was counting \
                 may be in no row"
            )
        });
        let read = map
            .read()
            .map_err(|error| format!("cannot read the block latency histogram: {error}"))?;
        if read.vanished > 0 || read.truncated {
            return Err(format!(
                "the block latency histogram's walk lost its place ({} rows vanished{}), so \
                 its counts are not the requests'",
                read.vanished,
                if read.truncated {
                    ", walk truncated"
                } else {
                    ""
                }
            ));
        }
        let rows = read
            .entries
            .iter()
            .filter_map(|(key, value)| HistRow::decode(key, value))
            .collect();
        Ok((rows, unwaited))
    }

    /// Runs of these programs the kernel skipped, and counted only for itself.
    ///
    /// A raw tracepoint runs its program behind a per-cpu guard. Probes run
    /// with preemption off, so the guard is found taken only when the program
    /// is re-entered on its own cpu: an interrupt arriving while it runs issues
    /// or completes another request, and that request's run is skipped whole.
    /// Neither the drop counters nor the ring can say so, because the program
    /// never ran. The kernel counts it in the program's `recursion_misses`, and
    /// this is that count, for both halves.
    ///
    /// **Measured, not theoretical.** Under the rest of `tests/scenario_io.rs`
    /// the count reached 766 and 1237 in one run each, and a launched program
    /// making 4096 direct reads was short by 2 to 23 requests in every one of
    /// six runs, with nothing reported lost before this was read. It is a count
    /// of the machine's requests, not the target's: the program runs on every
    /// request and filters afterwards, so a skipped run cannot say whose it
    /// was, and this is an upper bound on the target's share.
    pub fn skipped(&self) -> u64 {
        crate::bpf::program::skipped(
            self.open_program
                .iter()
                .chain(self.insert_program.iter())
                .chain(std::iter::once(&self.close_program)),
        ) + self.descendants.as_ref().map_or(0, Descendants::skipped)
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
    ///
    /// A histogram's entries outlive their requests, so there it is the
    /// entries whose open time was not consumed, not every entry.
    ///
    /// The count, and whether it is a lower bound because the key walk was cut
    /// short; or why it could not be read, which is not the same as none.
    pub fn still_in_flight(&self) -> Result<(usize, bool), String> {
        let Some(map) = self.inflight.as_ref() else {
            return Ok((0, false));
        };
        let (keys, truncated) = map
            .keys()
            .map_err(|error| format!("walking the in-flight map failed: {error}"))?;
        if self.histogram.is_none() {
            return Ok((keys.len(), truncated));
        }
        let mut open = 0;
        for key in &keys {
            let value = syscall::map_lookup(map.as_raw_fd(), key, HIST_ENTRY_VALUE_BYTES)
                .map_err(|error| format!("reading an in-flight entry failed: {error}"))?;
            // An entry removed between the walk and the read is not in flight.
            if value.is_some_and(|value| value.iter().any(|byte| *byte != 0)) {
                open += 1;
            }
        }
        Ok((open, truncated))
    }
}

/// A `BPF_MAP_TYPE_PERCPU_HASH` of histogram rows, read once and summed.
///
/// Each cpu writes its own copy of a row and the reader adds the copies up.
/// The map layer's `Hash` is a shared hash, and `Rows` swaps two of them for a
/// drain on a tick; this map is read once, after its program is detached, so
/// it needs neither, and lives here rather than in a shared file for one user.
struct PerCpuRows {
    fd: OwnedFd,
    max_entries: u32,
    cpus: usize,
}

/// `bpf_map_type::BPF_MAP_TYPE_PERCPU_HASH`.
const BPF_MAP_TYPE_PERCPU_HASH: u32 = 5;

/// What one read of the rows found: every row, its copies summed.
#[derive(Debug, Default)]
struct RowsRead {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    vanished: u64,
    truncated: bool,
}

impl PerCpuRows {
    fn create(max_entries: u32, name: &str) -> std::io::Result<Self> {
        let fd = syscall::map_create(
            BPF_MAP_TYPE_PERCPU_HASH,
            HIST_KEY_BYTES as u32,
            HIST_VALUE_BYTES as u32,
            max_entries,
            name,
        )?;
        Ok(Self {
            fd,
            max_entries,
            cpus: syscall::possible_cpus(),
        })
    }

    fn as_raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    /// Every row, with each word of its value summed over the cpus.
    ///
    /// The kernel hands back one value per *possible* cpu, each padded to
    /// eight bytes; `HIST_VALUE_BYTES` is already a multiple of eight.
    fn read(&self) -> std::io::Result<RowsRead> {
        let fd = self.as_raw_fd();
        let mut read = RowsRead::default();
        let mut from: Option<Vec<u8>> = None;
        while let Some(key) = syscall::map_next_key(fd, from.as_deref(), HIST_KEY_BYTES)? {
            if read.entries.len() + read.vanished as usize >= self.max_entries as usize {
                read.truncated = true;
                break;
            }
            match syscall::map_lookup(fd, &key, HIST_VALUE_BYTES * self.cpus)? {
                Some(copies) => read.entries.push((key.clone(), sum_copies(&copies))),
                None => read.vanished += 1,
            }
            from = Some(key);
        }
        Ok(read)
    }
}

/// One row's per-cpu copies, added word by word into one value.
fn sum_copies(copies: &[u8]) -> Vec<u8> {
    let mut words = [0u64; HIST_VALUE_BYTES / 8];
    for copy in copies.chunks_exact(HIST_VALUE_BYTES) {
        for (word, bytes) in words.iter_mut().zip(copy.chunks_exact(8)) {
            *word = word.wrapping_add(u64::from_ne_bytes(bytes.try_into().expect("8")));
        }
    }
    words.iter().flat_map(|word| word.to_ne_bytes()).collect()
}

/// `BPF_TRACE_RAW_TP`, from `include/uapi/linux/bpf.h`. Frozen UAPI.
const BPF_TRACE_RAW_TP: u32 = 23;

/// Loads `instructions` as a `tp_btf` program on `tracepoint`, unattached:
/// the caller opens the link ([`syscall::tracing_open`]) after the opening
/// program's, so the attach order is the same either way. A failure -- a
/// kernel without the attach type, a verifier that refuses the loads -- is
/// the caller's cue to fall back on the raw tracepoint.
fn load_tp_btf(
    btf: &Btf,
    tracepoint: &str,
    instructions: &[Insn],
    name: &str,
) -> Result<OwnedFd, String> {
    let id = btf
        .tracepoint_id(tracepoint)
        .ok_or_else(|| format!("no `btf_trace_{tracepoint}` in this kernel's BTF"))?;
    let function = super::fentry::Function {
        name: tracepoint.to_owned(),
        id,
        params: Vec::new(),
        returns: "void".to_owned(),
    };
    super::fentry::load_tracing(&function, BPF_TRACE_RAW_TP, instructions, name)
        .map_err(|error| error.to_string())
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
            (EV_REQ, offset_of!(BlockEvent, req), "req"),
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
            let mut programs =
                vec![
                    close_instructions(btf, stage, Kept::only(Submitter::Any), 3, 4, 5, Some(6))
                        .unwrap_or_else(|reason| panic!("{stage:?} close: {reason}")),
                ];
            if stage.opens_at().is_some() {
                programs.push(
                    open_instructions(btf, stage, Kept::only(Submitter::Any), 3, 5, Some(6), None)
                        .unwrap_or_else(|reason| panic!("{stage:?} open: {reason}")),
                );
                programs.push(
                    histogram_open_instructions(btf, stage, Kept::only(Submitter::Any), 3, 5)
                        .unwrap_or_else(|reason| panic!("{stage:?} histogram open: {reason}")),
                );
                programs.push(
                    histogram_close_instructions(btf, stage, HistStart::Paired(3), 7, 5, false)
                        .unwrap_or_else(|reason| panic!("{stage:?} histogram close: {reason}")),
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
                close_instructions(btf, stage, Kept::only(Submitter::Any), 3, 4, 5, Some(6))
                    .expect("a program");
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
        let program = close_instructions(
            btf,
            Stage::Latency,
            Kept::only(Submitter::Any),
            3,
            4,
            5,
            Some(6),
        )
        .expect("a program");
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

    /// The issue reads who inserted the request before it compares the
    /// target: the submitter of a request a kworker dispatched is the one
    /// compared and recorded, and the insert program builds against this
    /// kernel.
    #[test]
    fn the_issue_reads_the_insert_s_submitter_before_the_target_is_compared() {
        let Some(btf) = kernel_btf() else { return };
        insert_instructions(btf, 7).expect("the insert program");
        let open = open_instructions(
            btf,
            Stage::Latency,
            Kept::only(Submitter::Process(4242)),
            3,
            5,
            Some(6),
            Some(7),
        )
        .expect("a program");
        let deleted = open
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::MAP_DELETE_ELEM)
            .expect("the stamp taken");
        let compared = open
            .iter()
            .position(|insn| insn.imm == 4242)
            .expect("the comparison");
        assert!(deleted < compared);
    }

    /// A machine-wide program keeps every request on the machine, so it must
    /// carry no comparison against a process; a targeted one must carry
    /// exactly one, at the moment the submitting task is the current task.
    #[test]
    fn the_filter_is_present_only_when_a_process_was_named() {
        let Some(btf) = kernel_btf() else { return };
        let machine_wide = open_instructions(
            btf,
            Stage::Latency,
            Kept::only(Submitter::Any),
            3,
            5,
            Some(6),
            None,
        )
        .expect("a program");
        assert!(
            !machine_wide.iter().any(|insn| insn.imm == 4242),
            "a machine-wide program compares against no process"
        );
        let filtered = open_instructions(
            btf,
            Stage::Latency,
            Kept::only(Submitter::Process(4242)),
            3,
            5,
            Some(6),
            None,
        )
        .expect("a program");
        assert_eq!(
            filtered.iter().filter(|insn| insn.imm == 4242).count(),
            1,
            "one comparison, against the thread group of the submitting task"
        );
    }

    /// A process with the processes it started compares the root as a
    /// constant and looks every other thread group up in the map, at every
    /// site that filters: the open, the unpaired close, and the histogram's
    /// open. A launched `fio` forks its job, and only the job submits.
    #[test]
    fn a_followed_process_keeps_its_descendants_through_the_map() {
        let Some(btf) = kernel_btf() else { return };
        let kept = Kept {
            submitter: Submitter::Process(4242),
            descendants: Some(9),
        };
        let lookups = |program: &[Insn]| {
            helpers_called(program)
                .iter()
                .filter(|helper| **helper == helper::MAP_LOOKUP_ELEM)
                .count()
        };
        let open = open_instructions(btf, Stage::Latency, kept, 3, 5, Some(6), None).expect("open");
        let open_alone = open_instructions(
            btf,
            Stage::Latency,
            Kept::only(kept.submitter),
            3,
            5,
            Some(6),
            None,
        )
        .expect("open");
        assert_eq!(open.iter().filter(|insn| insn.imm == 4242).count(), 1);
        assert_eq!(lookups(&open), lookups(&open_alone) + 1, "one lookup more");
        let size = close_instructions(btf, Stage::Size, kept, -1, 4, 5, Some(6)).expect("size");
        assert_eq!(size.iter().filter(|insn| insn.imm == 4242).count(), 1);
        let hist = histogram_open_instructions(btf, Stage::Latency, kept, 3, 5).expect("hist");
        let hist_alone =
            histogram_open_instructions(btf, Stage::Latency, Kept::only(kept.submitter), 3, 5)
                .expect("hist");
        assert_eq!(lookups(&hist), lookups(&hist_alone) + 1);
        // A thread is never widened to its process's children.
        let thread = Kept {
            submitter: Submitter::Thread(4242),
            descendants: Some(9),
        };
        let open_thread =
            open_instructions(btf, Stage::Latency, thread, 3, 5, Some(6), None).expect("open");
        assert_eq!(lookups(&open_thread), lookups(&open_alone));
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
        let filtered = close_instructions(
            btf,
            Stage::Size,
            Kept::only(Submitter::Process(4242)),
            -1,
            4,
            5,
            Some(6),
        )
        .expect("a program");
        assert_eq!(
            filtered.iter().filter(|insn| insn.imm == 4242).count(),
            1,
            "the only program there is has to carry the comparison"
        );
    }

    /// A close into the histogram adds to this cpu's copy of a `(device,
    /// bucket)` row and writes no ring record: the per-request stream is
    /// exactly what `--emit hist` alone is meant not to pay for, and an atomic
    /// add on a row every cpu shares is what the 23.6 % at three million
    /// requests a second was made of (`docs/scenarios/io.md`, defect 19).
    #[test]
    fn a_histogram_close_writes_no_record_and_counts_its_row_without_an_atomic() {
        let Some(btf) = kernel_btf() else { return };
        let program =
            histogram_close_instructions(btf, Stage::Latency, HistStart::Paired(3), 7, 5, false)
                .expect("a program");
        let helpers = helpers_called(&program);
        assert!(
            !helpers.contains(&helper::RINGBUF_OUTPUT),
            "a histogram run publishes no record per request"
        );
        assert!(
            helpers.contains(&helper::MAP_UPDATE_ELEM),
            "a new row is inserted"
        );
        assert!(
            !helpers.contains(&helper::MAP_DELETE_ELEM),
            "the entry is consumed in place, not deleted"
        );
        const ATOMIC_DW: u8 = 0xdb;
        assert_eq!(
            program.iter().filter(|insn| insn.code == ATOMIC_DW).count(),
            0,
            "each cpu adds to its own copy of the row"
        );
        // A stage with no interval has nothing to bucket.
        let error =
            histogram_close_instructions(btf, Stage::Size, HistStart::Paired(-1), 7, 5, false)
                .expect_err("io.size has no interval");
        assert!(error.contains("measures none"), "{error}");
    }

    /// The hot path of each histogram program, as helper calls: what a
    /// request costs, counted where it can be counted without a device fast
    /// enough to measure it. A first sighting and a full map are the rare
    /// paths; the paths every request takes are these.
    ///
    /// - open: the clock, and one lookup -- plus the task, when a process was
    ///   named. No comm, no task storage, no insert.
    /// - close: the clock, the lookup, the two reads that find the device, and
    ///   the row's lookup. No delete, no record, no cpu id.
    #[test]
    fn the_histogram_programs_hot_paths_call_only_what_a_row_needs() {
        let Some(btf) = kernel_btf() else { return };
        let open =
            histogram_open_instructions(btf, Stage::Latency, Kept::only(Submitter::Any), 3, 5)
                .expect("a program");
        let open_helpers = helpers_called(&open);
        for absent in [
            helper::GET_CURRENT_COMM,
            helper::GET_CURRENT_PID_TGID,
            helper::TASK_STORAGE_GET,
        ] {
            assert!(
                !open_helpers.contains(&absent),
                "the machine-wide histogram open calls helper {absent}"
            );
        }
        // The whole program: clock, lookup, and the first-sighting insert and
        // its counter's lookup.
        assert_eq!(
            open_helpers,
            [
                helper::KTIME_GET_NS,
                helper::MAP_LOOKUP_ELEM,
                helper::MAP_UPDATE_ELEM,
                helper::MAP_LOOKUP_ELEM,
            ]
        );
        let filtered = histogram_open_instructions(
            btf,
            Stage::Latency,
            Kept::only(Submitter::Process(4242)),
            3,
            5,
        )
        .expect("a program");
        assert_eq!(
            filtered.iter().filter(|insn| insn.imm == 4242).count(),
            1,
            "one comparison, against the thread group of the submitting task"
        );
        assert_eq!(
            helpers_called(&filtered)[..2],
            [helper::GET_CURRENT_PID_TGID, helper::KTIME_GET_NS],
            "the filter runs before the clock, so a refused request costs one helper"
        );

        let close =
            histogram_close_instructions(btf, Stage::Latency, HistStart::Paired(3), 7, 5, false)
                .expect("a program");
        let close_helpers = helpers_called(&close);
        // Up to the row's first lookup, in order: what every paired
        // completion pays.
        assert_eq!(
            close_helpers[..5],
            [
                helper::KTIME_GET_NS,
                helper::MAP_LOOKUP_ELEM,
                helper::PROBE_READ_KERNEL,
                helper::PROBE_READ_KERNEL,
                helper::MAP_LOOKUP_ELEM,
            ]
        );
        for absent in [
            helper::GET_CURRENT_COMM,
            helper::GET_SMP_PROCESSOR_ID,
            helper::MAP_DELETE_ELEM,
            helper::RINGBUF_OUTPUT,
        ] {
            assert!(
                !close_helpers.contains(&absent),
                "the histogram close calls helper {absent}"
            );
        }
    }

    /// Built for `tp_btf`, the close reads the device with two loads and
    /// no helper; and this kernel's verifier takes it, where the machine
    /// lets the test load programs.
    #[test]
    fn the_tp_btf_close_reads_the_device_without_a_helper() {
        let Some(btf) = kernel_btf() else {
            return;
        };
        let close =
            histogram_close_instructions(btf, Stage::Latency, HistStart::Paired(3), 7, 5, true)
                .expect("a program");
        let helpers = helpers_called(&close);
        assert!(!helpers.contains(&helper::PROBE_READ_KERNEL), "{helpers:?}");
        assert_eq!(
            helpers[..3],
            [
                helper::KTIME_GET_NS,
                helper::MAP_LOOKUP_ELEM,
                helper::MAP_LOOKUP_ELEM
            ]
        );
        assert!(btf.tracepoint_id("block_rq_complete").is_some());
        assert_eq!(btf.tracepoint_id("no_such_tracepoint"), None);
    }

    /// On a kernel with BTF and the capabilities, a histogram's close is
    /// attached through `tp_btf`; the ring path keeps its raw tracepoint.
    #[test]
    fn a_histogram_closes_through_tp_btf_where_the_kernel_loads_it() {
        let program = match BlockProgram::load_histogram(Stage::Latency, Submitter::Any) {
            Ok(program) => program,
            Err(error) => {
                eprintln!("skipped: the block programs did not load here: {error}");
                return;
            }
        };
        assert!(program.close_is_tp_btf());
        let ring = BlockProgram::load(Stage::Latency, Submitter::Any, 0).expect("the ring path");
        assert!(!ring.close_is_tp_btf());
    }

    /// Started at the kernel's issue stamp, the close reads the clock and
    /// looks up the row and nothing else: no in-flight map, no helper read;
    /// and only a `tp_btf` program can load the stamp.
    #[test]
    fn a_close_on_the_kernel_s_stamp_looks_up_its_row_alone() {
        let Some(btf) = kernel_btf() else {
            return;
        };
        let offset = btf
            .member_offset("request", "io_start_time_ns")
            .expect("this kernel's request carries an issue stamp");
        let close = histogram_close_instructions(
            btf,
            Stage::Latency,
            HistStart::Kernel(offset),
            7,
            5,
            true,
        )
        .expect("a program");
        let helpers = helpers_called(&close);
        assert_eq!(
            helpers[..2],
            [helper::KTIME_GET_NS, helper::MAP_LOOKUP_ELEM],
            "{helpers:?}"
        );
        assert!(!helpers.contains(&helper::PROBE_READ_KERNEL), "{helpers:?}");
        assert!(histogram_close_instructions(
            btf,
            Stage::Latency,
            HistStart::Kernel(offset),
            7,
            5,
            false
        )
        .is_err());
    }

    /// A row's per-cpu copies are added word by word: the count with the
    /// count, the total with the total, and nothing carried across.
    #[test]
    fn a_rows_per_cpu_copies_sum_into_one_value() {
        let mut copies = Vec::new();
        for (count, total) in [(3u64, 300u64), (0, 0), (2, 1_000)] {
            copies.extend(count.to_ne_bytes());
            copies.extend(total.to_ne_bytes());
        }
        let summed = sum_copies(&copies);
        assert_eq!(summed.len(), HIST_VALUE_BYTES);
        let row = HistRow::decode(&[0u8; HIST_KEY_BYTES], &summed).expect("a row");
        assert_eq!((row.count, row.total_ns), (5, 1_300));
    }

    /// The row the program writes is the row the reader decodes: the device
    /// then the bucket in the key, the count then the total in the value.
    #[test]
    fn a_histogram_row_decodes_where_the_program_writes_it() {
        let mut key = Vec::new();
        key.extend(((8u32 << 20) | 1).to_ne_bytes());
        key.extend(13u32.to_ne_bytes());
        let mut value = Vec::new();
        value.extend(3u64.to_ne_bytes());
        value.extend(12_345u64.to_ne_bytes());
        assert_eq!(key.len(), HIST_KEY_BYTES);
        assert_eq!(value.len(), HIST_VALUE_BYTES);
        let row = HistRow::decode(&key, &value).expect("a row");
        assert_eq!((row.major(), row.minor(), row.bucket), (8, 1, 13));
        assert_eq!((row.count, row.total_ns), (3, 12_345));
        assert_eq!(row.bucket_low_ns(), 1 << 12);
        // And the program's stack slots are laid out the same way.
        assert_eq!(HIST_VAL_COUNT, 0);
        assert_eq!(HIST_VAL_TOTAL, 8);
        assert!(HIST_VALUE + HIST_VALUE_BYTES as i16 <= HIST_KEY);
        assert!(HIST_KEY + HIST_KEY_BYTES as i16 <= CLOSE_DROP_KEY);
    }

    /// The paired stages subtract; the unpaired one has nothing to subtract
    /// from and must not pretend to.
    #[test]
    fn only_a_paired_stage_computes_an_interval() {
        let Some(btf) = kernel_btf() else { return };
        // `r1 -= r9`, the one subtraction in any of these programs.
        let subtracts = |program: &[Insn]| program.iter().any(|insn| insn.code == 0x1f);
        assert!(subtracts(
            &close_instructions(
                btf,
                Stage::Latency,
                Kept::only(Submitter::Any),
                3,
                4,
                5,
                Some(6)
            )
            .expect("a program")
        ));
        assert!(subtracts(
            &close_instructions(
                btf,
                Stage::Queue,
                Kept::only(Submitter::Any),
                3,
                4,
                5,
                Some(6)
            )
            .expect("a program")
        ));
        assert!(!subtracts(
            &close_instructions(
                btf,
                Stage::Size,
                Kept::only(Submitter::Any),
                -1,
                4,
                5,
                Some(6)
            )
            .expect("a program")
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

    /// The request identity is read only where the submitting task is the
    /// current one. At `block_rq_complete` the current task is whatever the
    /// interrupt landed on, and a read of *its* storage would credit the
    /// device's time to a request that task happened to be serving -- a
    /// propagated key, and the wrong one, which is worse than none.
    #[test]
    fn the_request_is_read_where_the_submitter_is_current_and_nowhere_else() {
        use crate::bpf::insn::helper::GET_CURRENT_TASK_BTF;
        let Some(btf) = kernel_btf() else { return };
        let reads = |program: &[Insn]| helpers_called(program).contains(&GET_CURRENT_TASK_BTF);

        let issue = open_instructions(
            btf,
            Stage::Latency,
            Kept::only(Submitter::Any),
            3,
            5,
            Some(6),
            None,
        )
        .unwrap();
        assert!(reads(&issue), "the issue runs in the submitter's context");
        let complete = close_instructions(
            btf,
            Stage::Latency,
            Kept::only(Submitter::Any),
            3,
            4,
            5,
            Some(6),
        )
        .unwrap();
        assert!(
            !reads(&complete),
            "the completion runs in whoever's context it interrupted"
        );

        // Where the issue itself closes, it reads it directly.
        let size = close_instructions(
            btf,
            Stage::Size,
            Kept::only(Submitter::Any),
            -1,
            4,
            5,
            Some(6),
        )
        .unwrap();
        assert!(reads(&size));

        // No map, no read, and the entry's slot is still written.
        let bare = open_instructions(
            btf,
            Stage::Latency,
            Kept::only(Submitter::Any),
            3,
            5,
            None,
            None,
        )
        .unwrap();
        assert!(!reads(&bare));
        assert!(bare
            .iter()
            .any(|insn| insn.code == 0x7a && insn.off == ENTRY_VALUE + VAL_REQ && insn.imm == 0));
    }

    /// The prototypes are checked against the running kernel rather than
    /// assumed. If this fails on a machine, the programs must not be loaded
    /// there: the slot they read holds something else.
    #[test]
    fn the_running_kernel_declares_the_prototypes_these_programs_assume() {
        let Some(btf) = kernel_btf() else { return };
        for (tracepoint, accepted) in [
            ("block_rq_insert", &[&REQUEST_SIGNATURE[..]][..]),
            ("block_rq_issue", &[&REQUEST_SIGNATURE[..]][..]),
            ("block_rq_complete", &COMPLETE_SIGNATURES[..]),
        ] {
            let signature = btf
                .tracepoint_signature(tracepoint)
                .unwrap_or_else(|reason| panic!("{tracepoint}: {reason}"));
            assert!(
                accepted.iter().any(|shape| signature == *shape),
                "this kernel declares a different `{tracepoint}({})`",
                signature.join(", ")
            );
        }
    }

    /// The probe answers from the catalog's line and the attach from the
    /// program: the completion's shapes are listed in both, in one order.
    #[test]
    fn the_catalog_lists_every_completion_shape_the_program_reads() {
        crate::bpf::mem_program::assert_catalog_lists(
            "io.latency",
            "block_rq_complete",
            &COMPLETE_SIGNATURES,
        );
        for source in ["io.latency", "io.queue"] {
            crate::bpf::mem_program::assert_catalog_lists(
                source,
                "block_rq_issue",
                &[&REQUEST_SIGNATURE],
            );
        }
    }

    /// The two completion shapes differ in what the status slot means and in
    /// nothing the program reads: the request first, the byte count third.
    #[test]
    fn both_completion_shapes_keep_the_request_and_the_bytes_where_the_program_reads_them() {
        for shape in COMPLETE_SIGNATURES {
            assert_eq!(shape.len(), 3, "{shape:?}");
            assert_eq!(shape[0], "request *", "{shape:?}");
            assert_eq!(shape[2], "unsigned int", "{shape:?}");
        }
    }

    /// A prototype that does not match is refused by name, and the refusal
    /// says what the program would have read.
    #[test]
    fn a_tracepoint_of_another_shape_is_refused_with_both_prototypes() {
        let Some(btf) = kernel_btf() else { return };
        let error = crate::bpf::program::prototype(
            btf,
            "block_rq_complete",
            &[&REQUEST_SIGNATURE],
            "the request",
        )
        .expect_err("the prototypes differ");
        assert!(error.contains("blk_status_t"), "{error}");
        assert!(error.contains("produces a number"), "{error}");
    }
}
