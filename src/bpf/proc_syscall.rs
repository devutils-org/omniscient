//! The syscall latency programs: how long each call into the kernel took.
//!
//! Two programs, because no single event is the measurement: `sys_enter` says a
//! task called into the kernel and `sys_exit` says it came back, and the gap
//! between them is the latency. They share a map -- the first writes the moment
//! and the syscall number, the second reads them back and closes the interval.
//!
//! **The histogram is built in the kernel, and here that is not an
//! optimisation.** `raw_syscalls:sys_enter` fires on every syscall of every
//! process on the machine; the catalog entry says so in its own caveat. A
//! streaming shape would publish a ring record per syscall, which on a busy
//! machine is millions a minute -- a measurement whose cost grows with the
//! thing being measured, producing numbers that describe a machine running
//! `omn`. What the kernel keeps instead is a count and an exact total per
//! `(task, syscall, log2 bucket)`, which turns those millions into a few dozen
//! rows per drain.
//!
//! **The filter runs in the program, and that is a promise rather than a
//! speed.** The resolver credits any `bpf` backend with pushing predicates into
//! the kernel and then leaves its events alone. For every other source in this
//! build that credit is about the numbers; here it is about the cost as well,
//! because filtering `syscall == 'read'` after the fact still pays for every
//! `write` on the machine. So a predicate that names one syscall is written
//! into the instruction stream, ahead of the map write, and the collector says
//! in its notes that it was -- a filter reported as pushed down and applied in
//! userspace is the shape `DECISIONS.md` calls the worst a bug takes here.
//!
//! The bucketing is `emit::BucketLayout::Log2`, the same definition userspace
//! uses, for the reason `src/bpf/runqlat.rs` records: it is what lets a
//! kernel-built histogram pass through the emit layer without being re-bucketed
//! into something else.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{
    self, count_drop, helper, push_current_filter, Insn, Log2Labels, Program, Whose, BPF_DW, BPF_W,
    R10,
};
use super::map::{map_failure, row_map, select_rows, Counters, Hash, Rows};
use super::syscall;

/// Helper numbers this family needs and the shared `insn::helper` module does
/// not name yet; the request to move them there is in `docs/open-findings.md`.
mod extra_helper {
    /// `bpf_get_current_comm(buf, size)`.
    ///
    /// `sys_exit` hands its program a `pt_regs *` and a return value and no
    /// task at all, so the name of whoever made the call cannot be read out of
    /// a `task_struct` the way every other program in this layer reads it. The
    /// alternative is a `/proc/<tid>/comm` read in userspace at drain time,
    /// which answers for whoever holds that tid *then* -- and a short-lived
    /// task is exactly the one a syscall histogram is interesting about.
    pub const GET_CURRENT_COMM: i32 = 16;
}

/// The prototypes these programs are written against.
///
/// `sys_exit`'s second argument is the *return value*, not the syscall number:
/// the number is only on the entry side, which is the whole reason the pending
/// map carries it rather than being keyed by it.
const ENTER_SIGNATURE: [&str; 2] = ["pt_regs *", "long int"];
const EXIT_SIGNATURE: [&str; 2] = ["pt_regs *", "long int"];

/// How many threads may be inside a syscall at once before the entry program
/// stops being able to record new ones.
///
/// Every runnable thread on the machine can be in a syscall at the same
/// instant, and on a machine doing I/O most of them are.
const MAX_PENDING: u32 = 16384;

/// How many distinct `(task, syscall, bucket)` rows one interval may hold.
///
/// The cardinality this buys is the point of aggregating: a thread uses a
/// handful of syscalls and each lands in a handful of buckets, whatever the
/// workload does.
const MAX_ROWS: u32 = 32768;

/// Slots of the drop counter, one per thing that can be lost.
const DROP_PENDING: u32 = 0;
const DROP_ROWS: u32 = 1;
/// Not a loss: the calls of the target the pushed-down syscall comparison
/// skipped, counted where it skips them, so that a run can say how much of the
/// target's work its predicate declined to time -- which is the one thing that
/// tells a predicate that ran in the program from one that matched nothing.
const SKIPPED_BY_PREDICATE: u32 = 2;

/// Where each map's key and value live on the program's stack.
///
/// Stated once so the sizes below, the decode on the other side and the map
/// descriptors all come from the same place. Every byte of a key is written
/// before the key is used: the kernel hashes all of it, and a byte nobody wrote
/// puts two identical calls in two rows.
const ROW_VALUE: i16 = -64;
const ROW_KEY: i16 = -80;
const PEND_VALUE: i16 = -96;
const PEND_KEY: i16 = -104;
const DROP_KEY: i16 = -112;
/// Where the exit program keeps the row map it looked up through the
/// generation slot, once per run; see `map::select_rows`.
const ROWS_SLOT: i16 = -120;
/// Eight bytes for a cgroup or thread comparison, `insn::push_current_filter`.
const FILTER_SCRATCH: i16 = -128;

/// The layout of a row key, as byte offsets within it.
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
/// The syscall number, which is the series a reader actually wants: a process
/// whose `futex` calls take milliseconds and whose `write` calls take
/// microseconds is two rows here and one meaningless average anywhere else.
const KEY_NR: i16 = 8;
/// The log2 bucket the call fell into. Part of the key is what makes this a
/// histogram rather than a mean.
const KEY_BUCKET: i16 = 12;
const ROW_KEY_BYTES: usize = 16;

/// The layout of a row value.
const VALUE_COUNT: i16 = 0;
/// The exact sum of the durations in this bucket, kept beside the count because
/// the bucket is a factor of two wide and a total built from its bounds would
/// inherit that error.
const VALUE_TOTAL_NS: i16 = 8;
const VALUE_COMM: i16 = 16;
const COMM_BYTES: usize = 16;
/// When the first call of this row's window *began*.
///
/// A bucketed source emits one event per row per window, and stamping that
/// event at the drain puts it at the window's end -- so a join that *opens* on
/// this source opens after everything that happened in the same window as the
/// first call. Finding 44 has the argument; `mem.reclaim` and
/// `sched.runqlat` carried it first.
///
/// Written once, where the row is created, which is that key's first call of
/// the window: the rows map is drained and emptied every window.
const VALUE_FIRST_NS: i16 = 32;
const ROW_VALUE_BYTES: usize = 40;

/// The pending map: thread id to the moment it entered a syscall, and which.
const PEND_KEY_BYTES: usize = 4;
const PEND_TS: i16 = 0;
const PEND_NR: i16 = 8;
const PEND_VALUE_BYTES: usize = 16;

/// `bpf_get_current_comm` is not GPL-only, but `bpf_probe_read_kernel` is and
/// the rest of this layer declares GPL for it; a program that declared
/// otherwise would be the one exception nobody expects.
const LICENSE: &str = "GPL";

/// One `(task, syscall, bucket)` row's calls over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyscallRow {
    pub tid: u32,
    pub tgid: u32,
    /// The architecture's syscall number. The name for it is a userspace table
    /// and belongs to the collector, not here: the kernel does not publish one.
    pub nr: u32,
    /// The log2 bucket, in `emit::BucketLayout::Log2`'s numbering: bucket 0
    /// holds a duration of zero, and bucket *k* holds `[2^(k-1), 2^k)`.
    pub bucket: u32,
    pub count: u64,
    /// The exact total, which the bucket bounds cannot give back.
    pub total_ns: u64,
    /// When the first of this row's window began, which is what the event is
    /// timestamped with. Zero means unknown, and falls back to the drain.
    pub first_ns: u64,
    pub comm: String,
}

impl SyscallRow {
    /// The low bound of this row's bucket, in nanoseconds.
    ///
    /// This is the value that travels on the event, because it is the one the
    /// emit layer's own log2 bucketing maps back to exactly this bucket.
    pub fn bucket_low_ns(&self) -> u64 {
        match self.bucket {
            0 => 0,
            // A duration is a nanosecond count and `bucket` is derived from
            // one, so this cannot be reached -- but a shift that would overflow
            // is a panic rather than a wrong answer, so the case is written.
            k if k >= 64 => 1 << 63,
            k => 1 << (k - 1),
        }
    }

    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        let word =
            |bytes: &[u8], at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long =
            |bytes: &[u8], at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        let comm = &value[VALUE_COMM as usize..VALUE_COMM as usize + COMM_BYTES];
        let end = comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(comm.len());
        Some(Self {
            tid: word(key, KEY_TID as usize),
            tgid: word(key, KEY_TGID as usize),
            nr: word(key, KEY_NR as usize),
            bucket: word(key, KEY_BUCKET as usize),
            count: long(value, VALUE_COUNT as usize),
            total_ns: long(value, VALUE_TOTAL_NS as usize),
            first_ns: long(value, VALUE_FIRST_NS as usize),
            comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
        })
    }
}

/// This program's own labels for the shared unrolled log2, which is
/// `insn::push_log2_bucket` -- one definition of `emit::BucketLayout::Log2`'s
/// numbering, where there were five.
const LOG2: Log2Labels = [
    "log2_32",
    "log2_16",
    "log2_8",
    "log2_4",
    "log2_2",
    "log2_1",
    "log2_done",
];

/// Refuses a kernel whose tracepoint is not the shape the program reads.
fn check_signature(btf: &Btf, tracepoint: &str, expected: &[&str]) -> Result<(), String> {
    let signature = btf.tracepoint_signature(tracepoint)?;
    if signature != expected {
        return Err(format!(
            "this kernel declares `{tracepoint}({})`, and this program is written for \
             `{tracepoint}({})`: it reads the syscall number out of a fixed slot, and against \
             another prototype that slot holds something else",
            signature.join(", "),
            expected.join(", "),
        ));
    }
    Ok(())
}

/// What the two programs are narrowed to before they do any work.
///
/// Both are written into the instruction stream rather than looked up in a map:
/// they are known before the program loads, they never change during a run, and
/// this path runs on every syscall of every process on the machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Narrowing {
    /// Whose calls are kept.
    pub whose: Whose,
    /// The one syscall number kept, or none for all of them. This is the
    /// predicate the catalog's caveat says to push down.
    pub syscall_nr: Option<u32>,
}

/// The program on `sys_enter`: a task has called into the kernel.
fn enter_instructions(
    btf: &Btf,
    narrowing: Narrowing,
    pending_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    check_signature(btf, "sys_enter", &ENTER_SIGNATURE)?;

    let mut program = Program::new();
    // r6 keeps the context, r8 the timestamp and r9 the syscall number; all
    // three survive helper calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    // The key is four bytes in an eight-byte slot, and the four the program
    // does not write are still on the stack.
    program.push(Insn::st_imm(BPF_DW, R10, PEND_KEY, 0));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, PEND_KEY, 0));
    match narrowing.whose {
        Whose::Any => {}
        // The thread group is the high half of what is still in r0.
        Whose::Process(tgid) => {
            program.push(Insn::rsh64_imm(0, 32));
            program.jump_to("done", Insn::jne_imm(0, tgid as i32, 0));
        }
        // The thread id is the low half, already on the stack as the key.
        Whose::Thread(tid) => {
            program.push(Insn::ldx(BPF_W, 1, R10, PEND_KEY));
            program.jump_to("done", Insn::jne_imm(1, tid as i32, 0));
        }
        Whose::Cgroup(_) => {
            push_current_filter(&mut program, narrowing.whose, FILTER_SCRATCH, "done")
        }
    }

    // The second argument is the syscall number.
    program.push(Insn::ldx(BPF_DW, 9, 6, 8));
    if let Some(nr) = narrowing.syscall_nr {
        program.jump_to("the_syscall", Insn::jeq_imm(9, nr as i32, 0));
        // A call of the target's that the predicate declines: counted, so the
        // run can say what it did not time and why.
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

    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + PEND_TS, 8));
    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + PEND_NR, 9));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, PEND_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    // The map is full: this call will never be timed, and nothing else anywhere
    // would record that it happened.
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_PENDING, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `sys_exit`: the call has returned, and its duration is known.
///
/// There is no filtering here at all, and that is deliberate rather than an
/// omission: a call the entry program declined to record has no entry in the
/// pending map, so the lookup that opens this program is the same filter,
/// applied where it cannot disagree with itself.
fn exit_instructions(
    btf: &Btf,
    pending_fd: i32,
    rows_fd: i32,
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
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TGID, 0));

    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // A call this run never saw begin: the thread was already inside it when
    // the programs attached, or it belongs to another process, or the entry
    // program's filter declined it.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    // Both values are taken out of the map *before* the entry is removed: the
    // pointer a lookup returns is only good until then.
    program.push(Insn::ldx(BPF_DW, 9, 0, PEND_TS));
    program.push(Insn::ldx(BPF_DW, 7, 0, PEND_NR));

    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    // r9 = now - when the call began. Computed through r1, which nothing
    // between here and the next call reads.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::mov64_reg(9, 1));

    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_NR, 7));
    insn::push_log2_bucket(&mut program, 9, &LOG2);
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_BUCKET, 3));

    // The row map of this interval, taken once: a drain may swap it at any
    // instruction, and a lookup and an insert that named two maps would put
    // one call in each.
    select_rows(&mut program, rows_fd, ROWS_SLOT, "no_rows");
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first_call", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu, so both increments have to be ones
    // the other cpus cannot interleave with.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_TOTAL_NS, 9));
    program.jump_to("done", Insn::ja());

    program.label("first_call");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_TOTAL_NS, 9));
    // When this one began: now, less how long it lasted. r8 is the reading
    // taken at the top of the program and r9 is the duration, so the beginning
    // is recovered rather than carried through a second map field.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_FIRST_NS, 1));
    // The name is read once, when the row is created, rather than on every
    // call: it does not change, and this is the hot path.
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (ROW_VALUE + VALUE_COMM) as i32));
    program.push(Insn::mov64_imm(2, COMM_BYTES as i32));
    program.push(Insn::call(extra_helper::GET_CURRENT_COMM));
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    program.label("no_rows");
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_ROWS, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Two loaded, attached programs and the maps they share.
///
/// Dropping it detaches both: the links, the programs and the maps are file
/// descriptors, and the kernel unloads a program when the last one closes.
pub struct SyscallProgram {
    pending: Hash,
    rows: Rows,
    drops: Counters,
    enter_program: OwnedFd,
    _enter_link: OwnedFd,
    exit_program: OwnedFd,
    _exit_link: OwnedFd,
}

/// What one drain found, beside the measurements.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyscallDrainLosses {
    /// Rows the walk returned and the read no longer found.
    pub vanished: u64,
    /// The walk stopped at the map's capacity rather than at its end.
    pub truncated: bool,
}

impl SyscallProgram {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.enter_program, &self.exit_program])
    }

    pub fn load(narrowing: Narrowing) -> Result<Self, String> {
        let btf = super::kernel_btf()?;

        let pending = Hash::create(
            PEND_KEY_BYTES,
            PEND_VALUE_BYTES,
            MAX_PENDING,
            "omn_sys_pend",
        )
        .map_err(|error| map_failure("pending syscall map", error))?;
        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_sys_hist")
            .map_err(|error| map_failure("syscall histogram map", error))?;
        let drops = Counters::create(3, "omn_sys_drop")
            .map_err(|error| map_failure("syscall drop counter", error))?;

        let enter = enter_instructions(btf, narrowing, pending.as_raw_fd(), drops.as_raw_fd())?;
        let exit = exit_instructions(
            btf,
            pending.as_raw_fd(),
            rows.as_raw_fd(),
            drops.as_raw_fd(),
        )?;

        let enter_program = load(&enter, "omn_sys_enter")?;
        let exit_program = load(&exit, "omn_sys_exit")?;

        // Attached in this order on purpose, for the reason `runqlat.rs`
        // records: a call recorded by a program whose partner is not attached
        // yet is a call that is never closed -- harmless, it is taken by the
        // next thread to reuse the tid or ages out with the map -- whereas an
        // exit seen before any entry was recorded would close a call nobody
        // opened, and the lookup that fails is the only thing standing between
        // that and a duration measured from an uninitialised timestamp.
        let enter_link = syscall::raw_tracepoint_open("sys_enter", enter_program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sys_enter` tracepoint: {error}"))?;
        let exit_link = syscall::raw_tracepoint_open("sys_exit", exit_program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sys_exit` tracepoint: {error}"))?;

        Ok(Self {
            pending,
            rows,
            drops,
            enter_program,
            _enter_link: enter_link,
            exit_program,
            _exit_link: exit_link,
        })
    }

    /// Takes every row the kernel has accumulated since the last drain.
    ///
    /// Each row is a delta over the interval between two drains: the programs
    /// are pointed at an empty map first, and the one they were writing is
    /// read and emptied once nothing can still be adding to it -- see
    /// `map::Rows` for why that is the whole fix to a race that lost calls.
    pub fn drain(&self) -> Result<(Vec<SyscallRow>, SyscallDrainLosses), String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the syscall histogram: {error}"))?;
        let mut losses = SyscallDrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match SyscallRow::decode(key, value) {
                Some(row) => rows.push(row),
                // A record of the wrong size means this reader and the program
                // disagree about the layout, which is a bug rather than a
                // machine condition -- but counting it beats reinterpreting it.
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    /// How many threads were inside a syscall and had not come back.
    ///
    /// Their call is real and is in no bucket, because it has no end yet. A
    /// machine with threads blocked in `epoll_wait` always has some, and that
    /// is a fact about the window rather than a loss.
    pub fn still_pending(&self) -> usize {
        self.pending.len().unwrap_or(0)
    }

    /// Calls the programs could not record because a map was full, as
    /// `(pending, rows)`.
    pub fn dropped(&self) -> (u64, u64) {
        (self.drops.read(DROP_PENDING), self.drops.read(DROP_ROWS))
    }

    /// Calls of the target the pushed-down syscall comparison skipped without
    /// timing them, summed across cpus. Zero when no syscall was pushed down.
    pub fn skipped_by_predicate(&self) -> u64 {
        self.drops.read(SKIPPED_BY_PREDICATE)
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

    /// The bucket index the emit layer computes, written here as the machine's
    /// own count-leading-zeros rather than as the six-step search the program
    /// performs. An independent expression of the definition is the only thing
    /// that can catch the search being off by one.
    fn reference_index(value: u64) -> u64 {
        u64::from(u64::BITS - value.leading_zeros())
    }

    /// The kernel's bucketing and the emit layer's must be the same function.
    ///
    /// If the two disagree by one, every call is reported in the neighbouring
    /// bucket -- a factor of two -- and nothing anywhere says so.
    #[test]
    fn the_kernels_buckets_and_the_emit_layers_are_the_same_buckets() {
        for bucket in 0..48u32 {
            let row = SyscallRow {
                tid: 1,
                tgid: 1,
                nr: 1,
                bucket,
                count: 1,
                total_ns: 0,
                first_ns: 0,
                comm: String::new(),
            };
            let low = row.bucket_low_ns();
            assert_eq!(
                reference_index(low),
                u64::from(bucket),
                "bucket {bucket} has low bound {low}, which the emit layer would put in \
                 bucket {}",
                reference_index(low)
            );
        }
    }

    #[test]
    fn a_row_of_the_wrong_size_is_refused_rather_than_reinterpreted() {
        assert!(SyscallRow::decode(&[0u8; 8], &[0u8; ROW_VALUE_BYTES]).is_none());
        assert!(SyscallRow::decode(&[0u8; ROW_KEY_BYTES], &[0u8; 8]).is_none());
        assert!(SyscallRow::decode(&[0u8; ROW_KEY_BYTES], &[0u8; ROW_VALUE_BYTES]).is_some());
    }

    fn helpers_called(instructions: &[Insn]) -> Vec<i32> {
        instructions
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect()
    }

    /// Invariant 6: `bpf_ktime_get_boot_ns` includes suspend time, and a
    /// duration measured with one of each would be wrong by however long the
    /// machine had been asleep.
    #[test]
    fn both_programs_read_the_monotonic_clock_and_no_other() {
        const KTIME_GET_BOOT_NS: i32 = 125;
        let Some(btf) = kernel_btf() else { return };
        for program in [
            enter_instructions(&btf, Narrowing::default(), 3, 4).expect("a program"),
            exit_instructions(&btf, 3, 4, 5).expect("a program"),
        ] {
            let called = helpers_called(&program);
            assert!(called.contains(&helper::KTIME_GET_NS));
            assert!(!called.contains(&KTIME_GET_BOOT_NS));
        }
    }

    /// The whole promise of this source's caveat. A predicate naming one
    /// syscall has to appear in the instruction stream ahead of the map write,
    /// and a run that names none must carry no comparison at all -- otherwise
    /// the resolved file says the filter was pushed down and it was not.
    #[test]
    fn a_syscall_predicate_is_written_into_the_instruction_stream() {
        let Some(btf) = kernel_btf() else { return };
        let everything = enter_instructions(&btf, Narrowing::default(), 3, 4).expect("a program");
        assert!(
            !everything.iter().any(|insn| insn.imm == 257),
            "a run that named no syscall compares against none"
        );
        let narrowed = enter_instructions(
            &btf,
            Narrowing {
                whose: Whose::Any,
                syscall_nr: Some(257),
            },
            3,
            4,
        )
        .expect("a program");
        assert_eq!(
            narrowed.iter().filter(|insn| insn.imm == 257).count(),
            1,
            "the syscall number is compared once, in the program"
        );
        assert!(
            narrowed.len() > everything.len(),
            "the narrowed program carries a comparison the other does not"
        );
    }

    /// A process filter and a syscall filter are independent, and both belong
    /// in the entry program: the exit program has neither, because a call the
    /// entry program declined has no pending entry for it to find.
    #[test]
    fn the_process_filter_and_the_syscall_filter_are_both_in_the_entry_program() {
        let Some(btf) = kernel_btf() else { return };
        let both = enter_instructions(
            &btf,
            Narrowing {
                whose: Whose::Process(4242),
                syscall_nr: Some(257),
            },
            3,
            4,
        )
        .expect("a program");
        assert_eq!(both.iter().filter(|insn| insn.imm == 4242).count(), 1);
        assert_eq!(both.iter().filter(|insn| insn.imm == 257).count(), 1);

        let exit = exit_instructions(&btf, 3, 4, 5).expect("a program");
        assert!(
            !exit.iter().any(|insn| insn.imm == 4242 || insn.imm == 257),
            "the exit program filters through the pending map and nothing else"
        );
    }

    /// The exit program reads both halves of the pending value out of the map
    /// before it deletes the entry. A lookup's pointer is good until then and
    /// not after, and reading it afterwards is a duration computed from freed
    /// memory -- which is a number rather than a fault.
    #[test]
    fn the_pending_entry_is_read_before_it_is_deleted() {
        let Some(btf) = kernel_btf() else { return };
        let program = exit_instructions(&btf, 3, 4, 5).expect("a program");
        let delete_at = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::MAP_DELETE_ELEM)
            .expect("the exit program removes the pending entry");
        // The two loads out of r0, the map value pointer.
        let loads = program[..delete_at]
            .iter()
            .filter(|insn| insn.code == 0x79 && (insn.regs >> 4) == 0)
            .count();
        assert_eq!(
            loads, 2,
            "the timestamp and the syscall number are both read before the delete"
        );
    }

    /// Both programs end in `exit`, and the whole stream passes the structural
    /// audit -- which is what stands in for a compiler on a program nobody
    /// compiled.
    #[test]
    fn both_programs_are_built_against_the_running_kernel() {
        let Some(btf) = kernel_btf() else { return };
        for program in [
            enter_instructions(&btf, Narrowing::default(), 3, 4).expect("a program"),
            exit_instructions(&btf, 3, 4, 5).expect("a program"),
        ] {
            assert_eq!(program.last().copied(), Some(Insn::exit()));
            super::super::insn::audit(&program).expect("the audit passes");
        }
    }

    /// The stack regions this program writes must not overlap, and must stay
    /// inside the frame.
    ///
    /// They are addressed by offset arithmetic, and two that overlap produce a
    /// key built partly out of a value -- which hashes to a bucket nobody looks
    /// in, so the numbers are simply short, or comes back as a field with a
    /// plausible-looking number in it that was never written there.
    ///
    /// This file had no such test until the eighth wave, and that is exactly
    /// where the defect landed: a field added to the row value grew it into the
    /// row key, and what caught it was another stream's scenario reporting
    /// `vector 1760` for a softirq vector that only goes up to nine. Every
    /// program that keeps a row now has this test.
    #[test]
    fn the_stack_regions_do_not_overlap_and_stay_inside_the_frame() {
        let regions = [
            (ROW_VALUE, ROW_VALUE_BYTES as i16, "row value"),
            (ROW_KEY, ROW_KEY_BYTES as i16, "row key"),
            (PEND_VALUE, PEND_VALUE_BYTES as i16, "pending value"),
            (PEND_KEY, PEND_KEY_BYTES as i16, "pending key"),
            (DROP_KEY, 4, "drop key"),
            (ROWS_SLOT, 8, "row map slot"),
            (FILTER_SCRATCH, 8, "filter scratch"),
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

    /// Every byte of the row value is written by the program before the value
    /// is handed to a helper. A byte nobody wrote is whatever the stack held.
    #[test]
    fn every_byte_of_the_row_value_is_accounted_for() {
        let fields = [
            (VALUE_COUNT, 8),
            (VALUE_TOTAL_NS, 8),
            (VALUE_COMM, COMM_BYTES as i16),
            (VALUE_FIRST_NS, 8),
        ];
        let mut covered = [false; ROW_VALUE_BYTES];
        for (at, size) in fields {
            for byte in at..at + size {
                let byte = byte as usize;
                assert!(
                    byte < ROW_VALUE_BYTES,
                    "the row value is written past its end"
                );
                assert!(!covered[byte], "the row value writes byte {byte} twice");
                covered[byte] = true;
            }
        }
        assert!(
            covered.iter().all(|byte| *byte),
            "the row value has a byte the program never writes"
        );
    }

    /// The row map is found through the generation slot once per run, and the
    /// lookup and the insert both use what it found -- the property that makes
    /// the drain's swap safe; see `map::Rows`.
    #[test]
    fn the_row_map_is_taken_from_the_slot_once_per_run() {
        let Some(btf) = kernel_btf() else { return };
        let program = exit_instructions(&btf, 3, 4, 5).expect("a program");
        crate::bpf::map::assert_rows_taken_once(&program, 4);
    }
}
