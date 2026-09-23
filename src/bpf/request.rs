//! A request's identity, carried in the task that is serving it.
//!
//! Everything else in this layer is keyed by something a program can compute
//! where it runs: a task id, a socket cookie, a request pointer. A *request* --
//! one unit of work a server does for one client -- has no such key. The
//! kernel never sees one; it sees a thread receive some bytes, take a lock,
//! read a file and send some bytes, and nothing in any of those events says
//! they were the same piece of work. Joining them after the fact is four
//! inferences deep (`bio -> inode -> file -> fd -> task`) and every one of them
//! is fragile.
//!
//! So the identity is **propagated** instead of inferred. A
//! `BPF_MAP_TYPE_TASK_STORAGE` map holds one value per task, inside the task:
//! where a request is born, a program writes its id into the current task's
//! slot, and any program that later runs *in that task* -- on any hook --
//! reads it back with no key to agree on. A read is submitted in the reader's
//! context, so the block layer's issue is such a hook: `src/bpf/io.rs` reads
//! the slot there and the block request carries `req:<id>`. That is what makes
//! `block_io` IDENTITY rather than KEY.
//!
//! ## Where a request is born: the server says so
//!
//! The birth is a USDT probe the server fires, `request__start(id)`, and the
//! end is `request__done(id)`, any provider. The alternative was to derive a
//! request from the connection -- the socket cookie and a sequence number, at
//! the first receive after a send -- and it was rejected, because it is a guess
//! about exactly the thing this mechanism exists to make exact: a keep-alive
//! connection that pipelines two requests is one request to it, a server that
//! reads a request in one thread and serves it in another loses it at the
//! hand-off, and the end is only knowable after the fact, at the next receive.
//! A probe costs the program two `nop`s and gives an id the program itself
//! chose, which is the id its own logs carry. `docs/scenarios/vertical.md`
//! has the reasoning at length.
//!
//! ## What the kernel adds up while the id is in the task
//!
//! Between the birth and the end, every nanosecond of the serving task is in
//! exactly one of four states, and `sched_switch` is where it moves between
//! them:
//!
//! ```text
//!   on a cpu --switch out, still runnable--> runnable --switch in--> on a cpu
//!   on a cpu --switch out, sleeping--> blocked --sched_waking--> runnable --switch in-->
//! ```
//!
//! A blocked stretch is split at the wakeup, so the wait for a cpu after it is
//! runnable time rather than blocked time -- the double count
//! `sched.offcpu` carries as `queued_ns` for the same reason. And it is split by
//! the state the task slept in: `TASK_INTERRUPTIBLE` is a program waiting for
//! something to happen (a lock, a timer, a socket), `TASK_UNINTERRUPTIBLE` is
//! usually a disk. The waker of the longest interruptible sleep is kept: for a
//! lock, it is the thread that released it.
//!
//! **The switch-in is read lazily.** At `sched_switch` the program runs in the
//! task leaving the cpu, and the task arriving is a raw pointer whose storage a
//! raw tracepoint cannot reach. But the last switch on a cpu is always the
//! switch-in of whoever is on it now, so each cpu remembers when it last
//! switched, and a task settles its own off-cpu stretch the next time it is
//! current: at its next switch-out, or at the end of the request. The four
//! states then tile the request's life exactly, and a missed switch -- the
//! kernel skips a program re-entered on its own cpu -- shows up as an
//! `anomalies` count rather than as a number that looks right.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::OnceLock;

use super::btf::Btf;
use super::insn::{count_drop, helper, read_field, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash, TaskStorage};
use super::ring::{ring_bytes, RingBuffer};
use super::syscall::{self, BPF_MAP_TYPE_PERCPU_ARRAY, BPF_MAP_TYPE_RINGBUF};
use super::usdt::{Operand, Site};

// ------------------------------------------------------------- helpers

/// `bpf_task_storage_get(map, task, value, flags)`, `bpf_func_id` 156.
pub const TASK_STORAGE_GET: i32 = 156;
/// `bpf_task_storage_delete(map, task)`, 157.
pub const TASK_STORAGE_DELETE: i32 = 157;
/// `bpf_get_current_task_btf()`, 158: the current task as a pointer the
/// verifier knows the type of, which is what the storage helpers take.
pub const GET_CURRENT_TASK_BTF: i32 = 158;
/// `BPF_LOCAL_STORAGE_GET_F_CREATE`.
const F_CREATE: i32 = 1;
/// `bpf_probe_read_user`, 112, for an argument the probe left in memory.
const PROBE_READ_USER: i32 = 112;

/// Register-to-register operations `insn.rs` does not encode, in
/// `include/uapi/linux/bpf_common.h`'s encoding. The comparisons are unsigned:
/// every operand here is a timestamp or a duration.
fn add64_reg(dst: u8, src: u8) -> Insn {
    Insn {
        code: 0x07 | 0x08,
        regs: (dst & 0xf) | ((src & 0xf) << 4),
        off: 0,
        imm: 0,
    }
}

/// `if dst > src goto`
fn jgt_reg(dst: u8, src: u8) -> Insn {
    Insn {
        code: 0x05 | 0x20 | 0x08,
        regs: (dst & 0xf) | ((src & 0xf) << 4),
        off: 0,
        imm: 0,
    }
}

/// `if dst >= src goto`
fn jge_reg(dst: u8, src: u8) -> Insn {
    Insn {
        code: 0x05 | 0x30 | 0x08,
        regs: (dst & 0xf) | ((src & 0xf) << 4),
        off: 0,
        imm: 0,
    }
}

/// `if dst != src goto`
fn jne_reg(dst: u8, src: u8) -> Insn {
    Insn {
        code: 0x05 | 0x50 | 0x08,
        regs: (dst & 0xf) | ((src & 0xf) << 4),
        off: 0,
        imm: 0,
    }
}

// --------------------------------------------------- the storage value

/// The value each serving task holds, as byte offsets into it.
///
/// Written by the birth, the switch and the end programs, read by the block
/// layer's issue. One set of numbers for all four programs and for the test
/// that holds them to the struct: a disagreement is a duration read out of
/// the slot holding a timestamp, which is a number.
const S_ID: i16 = 0;
const S_BORN: i16 = 8;
/// When the current on-cpu stretch began. Valid while the task is on a cpu.
const S_RUN_SINCE: i16 = 16;
/// When the task left the cpu, zero while it is on one.
const S_OFF_SINCE: i16 = 24;
/// `prev_state` at that switch-out: zero is preempted, anything else asleep.
const S_OFF_STATE: i16 = 32;
const S_ONCPU: i16 = 40;
const S_RUNQ: i16 = 48;
const S_SLEEP: i16 = 56;
const S_DISK: i16 = 64;
/// The longest interruptible sleep, and who ended it.
const S_HOLDER_NS: i16 = 72;
const S_HOLDER_TID: i16 = 80;
const S_BLOCKS: i16 = 84;
const S_PREEMPTED: i16 = 88;
const S_ANOMALIES: i16 = 92;
/// The size of the value, stated once for the map and for the programs.
pub(crate) const STORAGE_BYTES: usize = 96;

/// The one task-storage map a run's programs share.
///
/// One per process, because the point of it is that programs which never met
/// -- the request's birth in one collector, the block layer's issue in
/// another -- read the same slot. Created the first time either asks, and kept
/// for the life of the process: it holds nothing unless a birth program wrote
/// into a task, and each entry dies with its task.
///
/// A failure is kept too. A kernel that cannot make the map will not make it
/// on the second try, and the block programs load without the read when
/// there is no map, rather than failing.
pub fn storage() -> Result<&'static TaskStorage, String> {
    static STORAGE: OnceLock<Result<TaskStorage, String>> = OnceLock::new();
    STORAGE
        .get_or_init(|| {
            TaskStorage::create(STORAGE_BYTES, "omn_req_task")
                .map_err(|error| map_failure("request task-storage map", error))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Emits the read of the current task's request id into the eight bytes of
/// stack at `into`: the id, or zero when the task is serving none.
///
/// This is the whole of what another program needs to carry a request: two
/// helper calls and a load, clobbering `r0` through `r5`. It never creates an
/// entry -- only a birth does -- so a task that serves no request costs the
/// lookup and nothing else. `skip` must be a label unique in the caller's
/// program.
pub(crate) fn read_request_id(
    program: &mut Program,
    storage_fd: i32,
    into: i16,
    skip: &'static str,
) {
    program.push(Insn::st_imm(BPF_DW, R10, into, 0));
    program.push(Insn::call(GET_CURRENT_TASK_BTF));
    program.push(Insn::mov64_reg(2, 0));
    program.extend(Insn::load_map_fd(1, storage_fd));
    program.push(Insn::mov64_imm(3, 0));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(TASK_STORAGE_GET));
    program.jump_to(skip, Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, S_ID));
    program.push(Insn::stx(BPF_DW, R10, into, 1));
    program.label(skip);
}

/// How a request id is spelled in a `req:` key, by every collector that
/// carries one: lowercase hex, as the server's own id.
pub fn request_key(id: u64) -> String {
    format!("{id:x}")
}

// ------------------------------------------------------------- the record

/// Set on a record published at the birth rather than at the end.
pub const BORN: u32 = 1;
/// Set when the end named a different id from the one the task carried: the
/// server finished something it never started, or started a second request
/// on this thread without finishing the first.
pub const ID_MISMATCH: u32 = 2;

/// One request, as the end program writes it and userspace reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestRecord {
    pub id: u64,
    pub born_ns: u64,
    /// When the end probe fired; zero on a birth record.
    pub done_ns: u64,
    pub oncpu_ns: u64,
    pub runq_ns: u64,
    pub sleep_ns: u64,
    pub disk_ns: u64,
    /// The longest interruptible sleep of the request.
    pub holder_ns: u64,
    pub tid: u32,
    pub tgid: u32,
    /// The task whose wakeup ended that sleep: the lock's holder, when it was
    /// a lock. Zero when the sleep ended with no wakeup this run saw.
    pub holder_tid: u32,
    /// Stretches spent asleep, and stretches taken off the cpu while runnable.
    pub blocks: u32,
    pub preempted: u32,
    pub flags: u32,
    /// The id the end probe named, which is `id` unless `ID_MISMATCH`.
    pub declared_id: u64,
    comm: [u8; 16],
    /// Off-cpu stretches whose switch-in could not be placed, because the cpu
    /// switched without this program seeing it. Their time is in no state.
    pub anomalies: u32,
    pad: u32,
}

const RECORD_BYTES: usize = 120;

const R_ID: i16 = 0;
const R_BORN: i16 = 8;
const R_DONE: i16 = 16;
const R_ONCPU: i16 = 24;
const R_RUNQ: i16 = 32;
const R_SLEEP: i16 = 40;
const R_DISK: i16 = 48;
const R_HOLDER_NS: i16 = 56;
const R_TID: i16 = 64;
const R_TGID: i16 = 68;
const R_HOLDER_TID: i16 = 72;
const R_BLOCKS: i16 = 76;
const R_PREEMPTED: i16 = 80;
const R_FLAGS: i16 = 84;
const R_DECLARED: i16 = 88;
const R_COMM: i16 = 96;
const R_ANOMALIES: i16 = 112;

impl RequestRecord {
    pub fn comm(&self) -> String {
        let end = self
            .comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.comm.len());
        String::from_utf8_lossy(&self.comm[..end]).into_owned()
    }

    pub fn is_birth(&self) -> bool {
        self.flags & BORN != 0
    }

    /// Decodes one ring record. Reachable from the collector's tests, which
    /// build a record from the bytes the ring carries.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let long = |at: i16| u64::from_ne_bytes(bytes[at as usize..][..8].try_into().unwrap());
        let word = |at: i16| u32::from_ne_bytes(bytes[at as usize..][..4].try_into().unwrap());
        let mut comm = [0u8; 16];
        comm.copy_from_slice(&bytes[R_COMM as usize..][..16]);
        Some(Self {
            id: long(R_ID),
            born_ns: long(R_BORN),
            done_ns: long(R_DONE),
            oncpu_ns: long(R_ONCPU),
            runq_ns: long(R_RUNQ),
            sleep_ns: long(R_SLEEP),
            disk_ns: long(R_DISK),
            holder_ns: long(R_HOLDER_NS),
            tid: word(R_TID),
            tgid: word(R_TGID),
            holder_tid: word(R_HOLDER_TID),
            blocks: word(R_BLOCKS),
            preempted: word(R_PREEMPTED),
            flags: word(R_FLAGS),
            declared_id: long(R_DECLARED),
            comm,
            anomalies: word(R_ANOMALIES),
            pad: 0,
        })
    }
}

// ------------------------------------------------------------- the stack

/// One layout for every program here; each uses the part it needs, and every
/// slot is written before it is read.
const REC: i16 = -(RECORD_BYTES as i16);
const NOW: i16 = REC - 8;
const TIN: i16 = NOW - 8;
const ARG0: i16 = TIN - 8;
const SCRATCH: i16 = ARG0 - 8;
const KEY0: i16 = SCRATCH - 4;
const TID: i16 = KEY0 - 4;
const WAKER: i16 = TID - 4;
const DROP_KEY: i16 = WAKER - 4;
const SLEEPER_VALUE: i16 = DROP_KEY - 16;
const _: () = assert!(SLEEPER_VALUE >= -512);

/// A sleeper entry: the wakeup that ended a blocked stretch, and who made it.
const SLEEPER_WAKE_TS: i16 = 0;
const SLEEPER_WAKER: i16 = 8;
const SLEEPER_BYTES: usize = 16;
/// Tasks asleep in the middle of a request at once. One per serving thread.
const MAX_SLEEPERS: u32 = 4096;

// ------------------------------------------------------------- counters

const DROP_RING: u32 = 0;
/// A birth whose task storage could not be created: a request never tracked.
const NO_STORAGE: u32 = 1;
/// An end in a task carrying no request: the birth was not seen.
const UNBORN: u32 = 2;
/// A birth in a task still carrying a request that never ended.
const RESTARTED: u32 = 3;
const SLEEPERS_FULL: u32 = 4;
/// Births the kernel saw, so that userspace can say how many never ended.
const BIRTHS: u32 = 5;
const COUNTER_SLOTS: u32 = 6;

/// What the programs could not do, cumulative.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestCounts {
    pub births: u64,
    pub ring_full: u64,
    pub no_storage: u64,
    pub unborn: u64,
    pub restarted: u64,
    pub sleepers_full: u64,
}

// ------------------------------------------------------------- the programs

const SWITCH_SIGNATURE: [&str; 4] = ["bool", "task_struct *", "task_struct *", "unsigned int"];
const WAKEUP_SIGNATURE: [&str; 1] = ["task_struct *"];
const ARG_PREV_STATE: i16 = 24;
const TASK_UNINTERRUPTIBLE: i32 = 2;
const LICENSE: &str = "GPL";

/// The file descriptors the programs are built against.
#[derive(Debug, Clone, Copy)]
struct Maps {
    storage: i32,
    last_switch: i32,
    sleepers: i32,
    ring: i32,
    counters: i32,
}

fn check_signature(btf: &Btf, tracepoint: &str, expected: &[&str]) -> Result<(), String> {
    let signature = btf.tracepoint_signature(tracepoint)?;
    if signature != expected {
        return Err(format!(
            "this kernel declares `{tracepoint}({})`, and the request programs are written for \
             `{tracepoint}({})`",
            signature.join(", "),
            expected.join(", ")
        ));
    }
    Ok(())
}

/// `field += r<reg>`, on the storage value in `r7`.
fn accumulate(program: &mut Program, field: i16, reg: u8) {
    program.push(Insn::ldx(BPF_DW, 1, 7, field));
    program.push(add64_reg(1, reg));
    program.push(Insn::stx(BPF_DW, 7, field, 1));
}

/// `field += 1`, a four-byte count on the storage value in `r7`.
fn bump(program: &mut Program, field: i16) {
    program.push(Insn::ldx(BPF_W, 1, 7, field));
    program.push(Insn::add64_imm(1, 1));
    program.push(Insn::stx(BPF_W, 7, field, 1));
}

/// Loads the current cpu's last switch into `TIN`, or `NOW` when there is
/// none, and writes `NOW` in its place when `update`.
fn last_switch(program: &mut Program, maps: Maps, update: bool, missing: &'static str) {
    program.push(Insn::st_imm(BPF_W, R10, KEY0, 0));
    program.extend(Insn::load_map_fd(1, maps.last_switch));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, KEY0 as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to(missing, Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, 0));
    program.push(Insn::stx(BPF_DW, R10, TIN, 1));
    if update {
        program.push(Insn::ldx(BPF_DW, 1, R10, NOW));
        program.push(Insn::stx(BPF_DW, 0, 0, 1));
    }
}

/// Settles the off-cpu stretch the task in `r7` is coming back from, if it is
/// coming back from one, using the switch-in time at `TIN` and the thread id
/// at `TID`.
///
/// Clobbers `r0`-`r5`, `r8` and `r9`; `r6` and `r7` survive. The labels are
/// this function's own and must not appear anywhere else in the program.
fn settle(program: &mut Program, maps: Maps) {
    program.push(Insn::ldx(BPF_DW, 2, 7, S_OFF_SINCE));
    program.jump_to("settled", Insn::jeq_imm(2, 0, 0));
    program.push(Insn::ldx(BPF_DW, 3, R10, TIN));
    // A switch-in before the switch-out: this cpu switched without the
    // program running, so where the stretch ended is not known.
    program.jump_to("anomalous", jgt_reg(2, 3));
    // r8 = the whole stretch off the cpu.
    program.push(Insn::mov64_reg(8, 3));
    program.push(Insn::sub64_reg(8, 2));

    program.push(Insn::ldx(BPF_W, 1, 7, S_OFF_STATE));
    program.jump_to("asleep", Insn::jne_imm(1, 0, 0));
    // Preempted: all of it was a wait for a cpu.
    accumulate(program, S_RUNQ, 8);
    program.jump_to("cleared", Insn::ja());

    program.label("asleep");
    program.push(Insn::st_imm(BPF_W, R10, WAKER, 0));
    program.extend(Insn::load_map_fd(1, maps.sleepers));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, TID as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("split", Insn::jeq_imm(0, 0, 0));
    // Everything is copied out before the delete invalidates the pointer.
    program.push(Insn::ldx(BPF_DW, 9, 0, SLEEPER_WAKE_TS));
    program.push(Insn::ldx(BPF_W, 1, 0, SLEEPER_WAKER));
    program.push(Insn::stx(BPF_W, R10, WAKER, 1));
    program.extend(Insn::load_map_fd(1, maps.sleepers));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, TID as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    // A wakeup outside the stretch is not this stretch's: the whole of it is
    // then counted asleep, which is the state it was in when last seen.
    program.jump_to("split", Insn::jeq_imm(9, 0, 0));
    program.push(Insn::ldx(BPF_DW, 2, 7, S_OFF_SINCE));
    program.jump_to("split", jgt_reg(2, 9));
    program.push(Insn::ldx(BPF_DW, 3, R10, TIN));
    program.jump_to("split", jgt_reg(9, 3));
    // Runnable from the wakeup to the switch-in, asleep before it.
    program.push(Insn::sub64_reg(3, 9));
    accumulate(program, S_RUNQ, 3);
    program.push(Insn::mov64_reg(8, 9));
    program.push(Insn::sub64_reg(8, 2));

    program.label("split");
    program.push(Insn::ldx(BPF_W, 1, 7, S_OFF_STATE));
    program.jump_to(
        "uninterruptible",
        Insn::jset_imm(1, TASK_UNINTERRUPTIBLE, 0),
    );
    accumulate(program, S_SLEEP, 8);
    program.push(Insn::ldx(BPF_DW, 1, 7, S_HOLDER_NS));
    program.jump_to("cleared", jge_reg(1, 8));
    program.push(Insn::stx(BPF_DW, 7, S_HOLDER_NS, 8));
    program.push(Insn::ldx(BPF_W, 1, R10, WAKER));
    program.push(Insn::stx(BPF_W, 7, S_HOLDER_TID, 1));
    program.jump_to("cleared", Insn::ja());

    program.label("uninterruptible");
    accumulate(program, S_DISK, 8);
    program.jump_to("cleared", Insn::ja());

    program.label("anomalous");
    bump(program, S_ANOMALIES);
    // The stretch's end is unknown, so the on-cpu stretch is taken to start
    // now: its time goes to no state, and the count says it did.
    program.push(Insn::ldx(BPF_DW, 1, R10, NOW));
    program.push(Insn::stx(BPF_DW, R10, TIN, 1));

    program.label("cleared");
    program.push(Insn::ldx(BPF_DW, 1, R10, TIN));
    program.push(Insn::stx(BPF_DW, 7, S_RUN_SINCE, 1));
    program.push(Insn::st_imm(BPF_DW, 7, S_OFF_SINCE, 0));
    program.label("settled");
}

/// The current task's storage into `r7`, creating it when `create`; jumps to
/// `absent` when there is none.
fn current_storage(program: &mut Program, maps: Maps, create: bool, absent: &'static str) {
    program.push(Insn::call(GET_CURRENT_TASK_BTF));
    program.push(Insn::mov64_reg(2, 0));
    program.extend(Insn::load_map_fd(1, maps.storage));
    program.push(Insn::mov64_imm(3, 0));
    program.push(Insn::mov64_imm(4, if create { F_CREATE } else { 0 }));
    program.push(Insn::call(TASK_STORAGE_GET));
    program.jump_to(absent, Insn::jeq_imm(0, 0, 0));
    program.push(Insn::mov64_reg(7, 0));
}

/// Reads a probe's first argument into `ARG0`, sign- or zero-extended.
///
/// `r6` holds the probe's `pt_regs`. An argument the note describes in a form
/// this cannot read is refused before the program loads: a request id read as
/// zero would be every request's id.
fn read_argument(program: &mut Program, btf: &Btf, site: &Site) -> Result<(), String> {
    let Some(arg) = site.args.first() else {
        return Err(format!(
            "`{}` declares no argument, and the request's id is its first",
            site.qualified()
        ));
    };
    let register = |member: &str| -> Result<i16, String> {
        let offset = btf.member_offset("pt_regs", member)?;
        i16::try_from(offset).map_err(|_| format!("`pt_regs.{member}` is at {offset}"))
    };
    match &arg.operand {
        Operand::Register {
            member,
            width,
            shift,
        } => {
            program.push(Insn::ldx(BPF_DW, 1, 6, register(member)?));
            if *shift > 0 {
                program.push(Insn::rsh64_imm(1, 8 * i32::from(*shift)));
            }
            let size = arg.size.min(*width);
            if size < 8 {
                program.push(Insn::and64_imm(1, ((1u64 << (8 * size)) - 1) as i32));
            }
            program.push(Insn::stx(BPF_DW, R10, ARG0, 1));
        }
        Operand::Immediate(value) => {
            let value = i32::try_from(*value).map_err(|_| {
                format!(
                    "`{}` passes its id as the constant {value}, which is not a request id",
                    site.qualified()
                )
            })?;
            program.push(Insn::st_imm(BPF_DW, R10, ARG0, value));
        }
        Operand::Memory { base, offset } => {
            let offset = i32::try_from(*offset)
                .map_err(|_| format!("`{}`: an offset of {offset}", site.qualified()))?;
            program.push(Insn::st_imm(BPF_DW, R10, ARG0, 0));
            program.push(Insn::ldx(BPF_DW, 3, 6, register(base)?));
            program.push(Insn::add64_imm(3, offset));
            program.push(Insn::mov64_reg(1, R10));
            program.push(Insn::add64_imm(1, ARG0 as i32));
            program.push(Insn::mov64_imm(2, i32::from(arg.size.clamp(1, 8))));
            program.push(Insn::call(PROBE_READ_USER));
        }
        Operand::Unreadable(why) => {
            return Err(format!(
                "`{}` passes its id as `{}`, which this build cannot read: {why}",
                site.qualified(),
                arg.text
            ))
        }
    }
    Ok(())
}

/// Clears the record and fills the parts every record has.
fn begin_record(program: &mut Program) {
    for slot in 0..(RECORD_BYTES / 8) as i16 {
        program.push(Insn::st_imm(BPF_DW, R10, REC + slot * 8, 0));
    }
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, REC + R_TID, 0));
    program.push(Insn::stx(BPF_W, R10, TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, REC + R_TGID, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (REC + R_COMM) as i32));
    program.push(Insn::mov64_imm(2, 16));
    program.push(Insn::call(helper::GET_CURRENT_COMM));
}

fn publish(program: &mut Program, maps: Maps, resume: &'static str) {
    program.extend(Insn::load_map_fd(1, maps.ring));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, REC as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to(resume, Insn::jsge_imm(0, 0, 0));
    count_drop(program, maps.counters, DROP_KEY, DROP_RING, resume);
}

fn finish(program: Program) -> Result<Vec<Insn>, String> {
    let mut program = program;
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// `request__start(id)`: write the id into the current task and start its
/// clock.
fn birth_instructions(btf: &Btf, site: &Site, maps: Maps) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    read_argument(&mut program, btf, site)?;
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, NOW, 0));
    count_drop(&mut program, maps.counters, DROP_KEY, BIRTHS, "counted");
    program.label("counted");

    current_storage(&mut program, maps, true, "no_storage");
    program.push(Insn::ldx(BPF_DW, 1, 7, S_ID));
    program.jump_to("fresh", Insn::jeq_imm(1, 0, 0));
    count_drop(&mut program, maps.counters, DROP_KEY, RESTARTED, "fresh");
    program.label("fresh");
    for slot in 0..(STORAGE_BYTES / 8) as i16 {
        program.push(Insn::st_imm(BPF_DW, 7, slot * 8, 0));
    }
    program.push(Insn::ldx(BPF_DW, 1, R10, ARG0));
    program.push(Insn::stx(BPF_DW, 7, S_ID, 1));
    program.push(Insn::ldx(BPF_DW, 1, R10, NOW));
    program.push(Insn::stx(BPF_DW, 7, S_BORN, 1));
    program.push(Insn::stx(BPF_DW, 7, S_RUN_SINCE, 1));

    // A birth record, so that the request has an event at its start: a join
    // opening on the request needs one before the block requests it covers.
    begin_record(&mut program);
    program.push(Insn::ldx(BPF_DW, 1, R10, ARG0));
    program.push(Insn::stx(BPF_DW, R10, REC + R_ID, 1));
    program.push(Insn::stx(BPF_DW, R10, REC + R_DECLARED, 1));
    program.push(Insn::ldx(BPF_DW, 1, R10, NOW));
    program.push(Insn::stx(BPF_DW, R10, REC + R_BORN, 1));
    program.push(Insn::st_imm(BPF_W, R10, REC + R_FLAGS, BORN as i32));
    publish(&mut program, maps, "done");
    program.jump_to("done", Insn::ja());

    program.label("no_storage");
    count_drop(&mut program, maps.counters, DROP_KEY, NO_STORAGE, "done");
    finish(program)
}

/// `request__done(id)`: settle the task's last stretch, publish the request,
/// and take the id out of the task.
fn end_instructions(btf: &Btf, site: &Site, maps: Maps) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    read_argument(&mut program, btf, site)?;
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, NOW, 0));
    // The task is on its cpu, so that cpu's last switch is its switch-in.
    program.push(Insn::stx(BPF_DW, R10, TIN, 0));
    last_switch(&mut program, maps, false, "switch_read");
    program.label("switch_read");

    current_storage(&mut program, maps, false, "unborn");
    program.push(Insn::ldx(BPF_DW, 1, 7, S_ID));
    program.jump_to("unborn", Insn::jeq_imm(1, 0, 0));

    begin_record(&mut program);
    settle(&mut program, maps);
    // The last on-cpu stretch, from its switch-in or the birth to now.
    program.push(Insn::ldx(BPF_DW, 1, R10, NOW));
    program.push(Insn::ldx(BPF_DW, 2, 7, S_RUN_SINCE));
    program.push(Insn::sub64_reg(1, 2));
    program.push(Insn::mov64_reg(8, 1));
    accumulate(&mut program, S_ONCPU, 8);

    for (from, to) in [
        (S_ID, R_ID),
        (S_BORN, R_BORN),
        (S_ONCPU, R_ONCPU),
        (S_RUNQ, R_RUNQ),
        (S_SLEEP, R_SLEEP),
        (S_DISK, R_DISK),
        (S_HOLDER_NS, R_HOLDER_NS),
    ] {
        program.push(Insn::ldx(BPF_DW, 1, 7, from));
        program.push(Insn::stx(BPF_DW, R10, REC + to, 1));
    }
    for (from, to) in [
        (S_HOLDER_TID, R_HOLDER_TID),
        (S_BLOCKS, R_BLOCKS),
        (S_PREEMPTED, R_PREEMPTED),
        (S_ANOMALIES, R_ANOMALIES),
    ] {
        program.push(Insn::ldx(BPF_W, 1, 7, from));
        program.push(Insn::stx(BPF_W, R10, REC + to, 1));
    }
    program.push(Insn::ldx(BPF_DW, 1, R10, NOW));
    program.push(Insn::stx(BPF_DW, R10, REC + R_DONE, 1));
    program.push(Insn::ldx(BPF_DW, 1, R10, ARG0));
    program.push(Insn::stx(BPF_DW, R10, REC + R_DECLARED, 1));
    program.push(Insn::ldx(BPF_DW, 2, 7, S_ID));
    program.jump_to("same_id", Insn::jeq_imm(1, 0, 0));
    program.jump_to("mismatch", jne_reg(1, 2));
    program.jump_to("same_id", Insn::ja());
    program.label("mismatch");
    program.push(Insn::st_imm(BPF_W, R10, REC + R_FLAGS, ID_MISMATCH as i32));
    program.label("same_id");

    // The id leaves the task before the record is published, so nothing the
    // task does after its end is credited to it even if the ring is full.
    program.push(Insn::call(GET_CURRENT_TASK_BTF));
    program.push(Insn::mov64_reg(2, 0));
    program.extend(Insn::load_map_fd(1, maps.storage));
    program.push(Insn::call(TASK_STORAGE_DELETE));
    publish(&mut program, maps, "done");
    program.jump_to("done", Insn::ja());

    program.label("unborn");
    count_drop(&mut program, maps.counters, DROP_KEY, UNBORN, "done");
    finish(program)
}

/// `sched_switch`: the task leaving the cpu closes its on-cpu stretch and
/// opens an off-cpu one; the cpu remembers when it switched.
fn switch_instructions(btf: &Btf, maps: Maps) -> Result<Vec<Insn>, String> {
    check_signature(btf, "sched_switch", &SWITCH_SIGNATURE)?;
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, NOW, 0));
    last_switch(&mut program, maps, true, "done");

    // At `sched_switch` the current task is still the one leaving.
    current_storage(&mut program, maps, false, "done");
    program.push(Insn::ldx(BPF_DW, 1, 7, S_ID));
    program.jump_to("done", Insn::jeq_imm(1, 0, 0));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, TID, 0));

    settle(&mut program, maps);
    program.push(Insn::ldx(BPF_DW, 1, R10, NOW));
    program.push(Insn::ldx(BPF_DW, 2, 7, S_RUN_SINCE));
    program.push(Insn::sub64_reg(1, 2));
    program.push(Insn::mov64_reg(8, 1));
    accumulate(&mut program, S_ONCPU, 8);

    program.push(Insn::ldx(BPF_DW, 1, R10, NOW));
    program.push(Insn::stx(BPF_DW, 7, S_OFF_SINCE, 1));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_PREV_STATE));
    program.push(Insn::stx(BPF_W, 7, S_OFF_STATE, 1));
    program.jump_to("sleeping", Insn::jne_imm(1, 0, 0));
    bump(&mut program, S_PREEMPTED);
    program.jump_to("done", Insn::ja());

    program.label("sleeping");
    bump(&mut program, S_BLOCKS);
    // Registered as asleep, so the wakeup that ends it can be told apart from
    // every other wakeup on the machine.
    program.push(Insn::st_imm(BPF_DW, R10, SLEEPER_VALUE, 0));
    program.push(Insn::st_imm(BPF_DW, R10, SLEEPER_VALUE + 8, 0));
    program.extend(Insn::load_map_fd(1, maps.sleepers));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, TID as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, SLEEPER_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, maps.counters, DROP_KEY, SLEEPERS_FULL, "done");
    finish(program)
}

/// `sched_waking`: the first wakeup of a sleeping request's task marks where
/// its sleep ended and who ended it.
///
/// `sched_waking` rather than `sched_wakeup`, and the difference is the whole
/// of the waker's identity. `sched_waking` fires at the top of
/// `try_to_wake_up`, in the waker's own context. `sched_wakeup` fires where the
/// task is put on a run queue, and since the kernel queues a remote wakeup onto
/// the wakee's cpu, that is often an interrupt on another cpu, in whatever was
/// running there: the first run of this program read the lock's holder as the
/// idle task, tid 0, on every request.
fn wakeup_instructions(btf: &Btf, maps: Maps) -> Result<Vec<Insn>, String> {
    check_signature(btf, "sched_waking", &WAKEUP_SIGNATURE)?;
    let pid = btf.member_offset("task_struct", "pid")?;
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    program.push(Insn::st_imm(BPF_W, R10, TID, 0));
    read_field(&mut program, TID, 4, 7, pid);
    program.extend(Insn::load_map_fd(1, maps.sleepers));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, TID as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::mov64_reg(7, 0));
    program.push(Insn::ldx(BPF_DW, 1, 7, SLEEPER_WAKE_TS));
    program.jump_to("done", Insn::jne_imm(1, 0, 0));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, 7, SLEEPER_WAKE_TS, 0));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, 7, SLEEPER_WAKER, 0));
    finish(program)
}

// ------------------------------------------------------------- loading

/// The birth and end probes, attached for one process, and the scheduler
/// programs that keep each request's clock.
pub struct RequestProgram {
    ring: RingBuffer,
    counters: Counters,
    _sleepers: Hash,
    _last_switch: OwnedFd,
    /// Links first, so they detach before the programs they hold close.
    _links: Vec<OwnedFd>,
    programs: Vec<OwnedFd>,
}

fn load(prog_type: u32, attach: u32, instructions: &[Insn], name: &str) -> Result<OwnedFd, String> {
    syscall::prog_load_for(prog_type, attach, instructions, name, LICENSE).map_err(|error| {
        if error.is_permission() {
            super::permission_advice(&format!("loading the `{name}` program"))
        } else {
            format!("the kernel refused the `{name}` program: {error}")
        }
    })
}

impl RequestProgram {
    /// `starts` and `ends` are the sites of `request__start` and
    /// `request__done`; `pid` the process whose hits count.
    pub fn load(
        starts: &[Site],
        ends: &[Site],
        pid: i32,
        requested_ring_bytes: usize,
    ) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let storage = storage()?;
        let bytes = ring_bytes(requested_ring_bytes);
        let ring_map =
            syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_req_ring")
                .map_err(|error| map_failure("request ring buffer", error))?;
        let counters = Counters::create(COUNTER_SLOTS, "omn_req_count")
            .map_err(|error| map_failure("request counters", error))?;
        let sleepers = Hash::create(4, SLEEPER_BYTES, MAX_SLEEPERS, "omn_req_sleep")
            .map_err(|error| map_failure("request sleeper map", error))?;
        let last_switch = syscall::map_create(BPF_MAP_TYPE_PERCPU_ARRAY, 4, 8, 1, "omn_req_switch")
            .map_err(|error| map_failure("per-cpu switch map", error))?;
        let maps = Maps {
            storage: storage.as_raw_fd(),
            last_switch: last_switch.as_raw_fd(),
            sleepers: sleepers.as_raw_fd(),
            ring: ring_map.as_raw_fd(),
            counters: counters.as_raw_fd(),
        };

        let mut programs = Vec::new();
        let mut links = Vec::new();
        // The scheduler programs first: a request born before they attach
        // would run its first stretch with nobody watching it switch.
        for (tracepoint, instructions, name) in [
            (
                "sched_switch",
                switch_instructions(btf, maps)?,
                "omn_req_switch",
            ),
            (
                "sched_waking",
                wakeup_instructions(btf, maps)?,
                "omn_req_wake",
            ),
        ] {
            let program = load(
                syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
                0,
                &instructions,
                name,
            )?;
            links.push(
                syscall::raw_tracepoint_open(tracepoint, program.as_raw_fd()).map_err(|error| {
                    format!("cannot attach to the `{tracepoint}` tracepoint: {error}")
                })?,
            );
            programs.push(program);
        }
        for (sites, build, name) in [
            (
                ends,
                end_instructions as fn(&Btf, &Site, Maps) -> Result<Vec<Insn>, String>,
                "omn_req_done",
            ),
            (starts, birth_instructions, "omn_req_start"),
        ] {
            for site in sites {
                if site.semaphore.is_some() {
                    return Err(format!(
                        "`{}` in {} is guarded by a semaphore, and this build attaches request \
                         probes without raising one: the probe would never fire",
                        site.qualified(),
                        site.object.display()
                    ));
                }
                let instructions = build(btf, site, maps)?;
                let program = load(
                    syscall::BPF_PROG_TYPE_KPROBE,
                    syscall::BPF_TRACE_UPROBE_MULTI,
                    &instructions,
                    name,
                )?;
                let link =
                    syscall::link_uprobe_multi(program.as_raw_fd(), &site.object, site.offset, pid)
                        .map_err(|error| match error.raw_os_error() {
                            Some(libc::EPERM) | Some(libc::EACCES) => super::permission_advice(
                                &format!("attaching to `{}`", site.qualified()),
                            ),
                            _ => format!(
                        "`{}` in {}: the kernel refused to link the program to {:#x}: {error}",
                        site.qualified(),
                        site.object.display(),
                        site.offset
                    ),
                        })?;
                links.push(link);
                programs.push(program);
            }
        }

        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the request ring buffer: {error}"))?;
        Ok(Self {
            ring,
            counters,
            _sleepers: sleepers,
            _last_switch: last_switch,
            _links: links,
            programs,
        })
    }

    pub fn drain(&mut self, mut handle: impl FnMut(RequestRecord), undecoded: &mut u64) {
        self.ring
            .drain(|bytes| match RequestRecord::from_bytes(bytes) {
                Some(record) => handle(record),
                None => *undecoded += 1,
            });
    }

    pub fn counts(&self) -> RequestCounts {
        RequestCounts {
            births: self.counters.read(BIRTHS),
            ring_full: self.counters.read(DROP_RING),
            no_storage: self.counters.read(NO_STORAGE),
            unborn: self.counters.read(UNBORN),
            restarted: self.counters.read(RESTARTED),
            sleepers_full: self.counters.read(SLEEPERS_FULL),
        }
    }

    /// Runs the kernel skipped, across all the programs: a switch this set
    /// never saw is a stretch that cannot be placed.
    pub fn skipped(&self) -> u64 {
        super::attach::fentry::skipped(self.programs.iter())
    }
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

    const MAPS: Maps = Maps {
        storage: 3,
        last_switch: 4,
        sleepers: 5,
        ring: 6,
        counters: 7,
    };

    fn site(arguments: &str) -> Site {
        Site {
            object: "/bin/true".into(),
            provider: "omn".to_owned(),
            name: "request__start".to_owned(),
            offset: 0x1000,
            semaphore: None,
            args: super::super::usdt::parse_arguments(arguments).expect("descriptors"),
        }
    }

    /// The program writes fields at record offsets and `decode` reads them at
    /// struct offsets; this is the only thing holding the two together.
    #[test]
    fn the_record_struct_matches_what_the_programs_write() {
        use std::mem::offset_of;
        assert_eq!(std::mem::size_of::<RequestRecord>(), RECORD_BYTES);
        for (written, field, name) in [
            (R_ID, offset_of!(RequestRecord, id), "id"),
            (R_BORN, offset_of!(RequestRecord, born_ns), "born_ns"),
            (R_DONE, offset_of!(RequestRecord, done_ns), "done_ns"),
            (R_ONCPU, offset_of!(RequestRecord, oncpu_ns), "oncpu_ns"),
            (R_RUNQ, offset_of!(RequestRecord, runq_ns), "runq_ns"),
            (R_SLEEP, offset_of!(RequestRecord, sleep_ns), "sleep_ns"),
            (R_DISK, offset_of!(RequestRecord, disk_ns), "disk_ns"),
            (
                R_HOLDER_NS,
                offset_of!(RequestRecord, holder_ns),
                "holder_ns",
            ),
            (R_TID, offset_of!(RequestRecord, tid), "tid"),
            (R_TGID, offset_of!(RequestRecord, tgid), "tgid"),
            (
                R_HOLDER_TID,
                offset_of!(RequestRecord, holder_tid),
                "holder_tid",
            ),
            (R_BLOCKS, offset_of!(RequestRecord, blocks), "blocks"),
            (
                R_PREEMPTED,
                offset_of!(RequestRecord, preempted),
                "preempted",
            ),
            (R_FLAGS, offset_of!(RequestRecord, flags), "flags"),
            (
                R_DECLARED,
                offset_of!(RequestRecord, declared_id),
                "declared_id",
            ),
            (R_COMM, offset_of!(RequestRecord, comm), "comm"),
            (
                R_ANOMALIES,
                offset_of!(RequestRecord, anomalies),
                "anomalies",
            ),
        ] {
            assert_eq!(
                written as usize, field,
                "`{name}` is written where it is not read"
            );
        }
    }

    /// Every stack region fits in the frame and none overlaps another. A
    /// region written over another is a timestamp read back as a key.
    #[test]
    fn the_stack_regions_fit_the_frame_and_do_not_overlap() {
        let mut regions = [
            (REC, RECORD_BYTES as i16),
            (NOW, 8),
            (TIN, 8),
            (ARG0, 8),
            (SCRATCH, 8),
            (KEY0, 4),
            (TID, 4),
            (WAKER, 4),
            (DROP_KEY, 4),
            (SLEEPER_VALUE, SLEEPER_BYTES as i16),
        ];
        regions.sort();
        assert!(regions[0].0 >= -512);
        for pair in regions.windows(2) {
            assert!(
                pair[0].0 + pair[0].1 <= pair[1].0,
                "{:?} overlaps {:?}",
                pair[0],
                pair[1]
            );
        }
        let last = regions.last().unwrap();
        assert!(last.0 + last.1 <= 0);
    }

    /// Every slot of the storage value is inside it, and the four-byte ones do
    /// not straddle an eight-byte one.
    #[test]
    fn the_storage_fields_are_inside_the_value_and_apart() {
        let mut fields = [
            (S_ID, 8),
            (S_BORN, 8),
            (S_RUN_SINCE, 8),
            (S_OFF_SINCE, 8),
            (S_OFF_STATE, 4),
            (S_ONCPU, 8),
            (S_RUNQ, 8),
            (S_SLEEP, 8),
            (S_DISK, 8),
            (S_HOLDER_NS, 8),
            (S_HOLDER_TID, 4),
            (S_BLOCKS, 4),
            (S_PREEMPTED, 4),
            (S_ANOMALIES, 4),
        ];
        fields.sort();
        for pair in fields.windows(2) {
            assert!(
                pair[0].0 + pair[0].1 <= pair[1].0,
                "{:?} and {:?}",
                pair[0],
                pair[1]
            );
        }
        let last = fields.last().unwrap();
        assert!((last.0 + last.1) as usize <= STORAGE_BYTES);
        assert_eq!(
            S_ID, 0,
            "`read_request_id` reads the id at the start of the value"
        );
    }

    fn helpers(instructions: &[Insn]) -> Vec<i32> {
        instructions
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect()
    }

    /// Only a birth creates a task's storage. Anything else that created it
    /// would give every task it ran in an entry, and every one of them an id
    /// of zero to be read back.
    #[test]
    fn only_the_birth_creates_storage() {
        let Some(btf) = kernel_btf() else { return };
        let creates = |instructions: &[Insn]| {
            instructions.windows(2).any(|pair| {
                pair[0].code == 0xb7
                    && pair[0].regs & 0xf == 4
                    && pair[0].imm == F_CREATE
                    && pair[1].code == 0x85
                    && pair[1].imm == TASK_STORAGE_GET
            })
        };
        let birth = birth_instructions(btf, &site("8@%rdi"), MAPS).expect("a birth");
        assert!(creates(&birth));
        let end = end_instructions(btf, &site("8@%rdi"), MAPS).expect("an end");
        assert!(!creates(&end));
        assert!(!creates(&switch_instructions(btf, MAPS).expect("a switch")));
        let mut program = Program::new();
        read_request_id(&mut program, 3, -8, "skip");
        program.push(Insn::mov64_imm(0, 0));
        program.push(Insn::exit());
        assert!(!creates(&program.build()));
    }

    /// The end takes the id out of the task, or everything the thread does
    /// after the request -- the next accept, the next idle wait -- is credited
    /// to it.
    #[test]
    fn the_end_removes_the_id_from_the_task() {
        let Some(btf) = kernel_btf() else { return };
        let end = end_instructions(btf, &site("-8@-24(%rbp)"), MAPS).expect("an end");
        assert!(helpers(&end).contains(&TASK_STORAGE_DELETE));
    }

    /// Invariant: `CLOCK_MONOTONIC` is the only clock domain.
    #[test]
    fn every_program_reads_the_monotonic_clock_and_no_other() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        for program in [
            birth_instructions(btf, &site("8@%rdi"), MAPS).unwrap(),
            end_instructions(btf, &site("8@%rdi"), MAPS).unwrap(),
            switch_instructions(btf, MAPS).unwrap(),
            wakeup_instructions(btf, MAPS).unwrap(),
        ] {
            let called = helpers(&program);
            assert!(called.contains(&helper::KTIME_GET_NS));
            assert!(!called.contains(&KTIME_GET_BOOT_NS));
        }
    }

    /// A request id the probe passes in a way this cannot read is refused
    /// before anything loads: read as zero, it would be every request's id.
    #[test]
    fn an_id_this_build_cannot_read_is_refused_by_name() {
        let Some(btf) = kernel_btf() else { return };
        let error = birth_instructions(btf, &site("8@%xmm0"), MAPS).expect_err("unreadable");
        assert!(error.contains("request__start"), "{error}");
        let error = birth_instructions(btf, &site(""), MAPS).expect_err("no argument");
        assert!(error.contains("declares no argument"), "{error}");
    }

    /// The record is cleared before any of it is filled, in both programs
    /// that publish one.
    #[test]
    fn every_record_is_cleared_before_it_is_filled() {
        let Some(btf) = kernel_btf() else { return };
        for program in [
            birth_instructions(btf, &site("8@%rdi"), MAPS).unwrap(),
            end_instructions(btf, &site("8@%rdi"), MAPS).unwrap(),
        ] {
            let cleared: Vec<i16> = program
                .iter()
                .filter(|insn| insn.code == 0x7a && insn.regs & 0xf == 10 && insn.imm == 0)
                .map(|insn| insn.off)
                .collect();
            for slot in 0..(RECORD_BYTES / 8) as i16 {
                assert!(
                    cleared.contains(&(REC + slot * 8)),
                    "byte {} never cleared",
                    slot * 8
                );
            }
        }
    }

    /// The prototypes the scheduler programs read are this kernel's.
    #[test]
    fn the_running_kernel_declares_the_prototypes_these_programs_assume() {
        let Some(btf) = kernel_btf() else { return };
        check_signature(btf, "sched_switch", &SWITCH_SIGNATURE).unwrap();
        check_signature(btf, "sched_waking", &WAKEUP_SIGNATURE).unwrap();
    }
}
