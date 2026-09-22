//! The run queue latency programs: how long a task was runnable and not running.
//!
//! This is the one source in the family whose whole value is the *difference*
//! between two events. Neither `sched_wakeup` nor `sched_switch` says anything
//! about waiting on its own: the first says a task became runnable, the second
//! says a task got the cpu, and the gap between them is the scheduler's queue.
//! So two programs are loaded here, not one, and they share a map -- the first
//! writes the moment a task joined the queue, the second reads it back when the
//! task finally runs.
//!
//! **A wakeup is not the only way onto a run queue.** A task the scheduler
//! preempts is still runnable: it goes straight back on the queue without any
//! wakeup, and its wait until it runs again is run queue latency exactly like
//! anyone else's. Measuring only woken tasks would report a machine under
//! heavy cpu contention -- where almost every wait is a preemption -- as a
//! machine with no queueing at all. So `sched_switch` enqueues the outgoing
//! task itself when it left in `TASK_RUNNING`, which is the same test
//! `offcpu.rs` uses to decide the opposite question.
//!
//! **The histogram is built in the kernel.** A wakeup happens tens to hundreds
//! of thousands of times a second on a busy machine, and one ring record per
//! wakeup is a measurement whose cost grows with the workload -- the numbers
//! would describe a machine running `omn`. What the kernel keeps instead is a
//! count per `(task, cpu, priority, log2 bucket)`, which turns those hundreds
//! of thousands of events into a few dozen map entries per drain.
//!
//! The bucketing is `emit::BucketLayout::Log2`, computed with the same
//! definition userspace uses: bucket *k* holds `[2^(k-1), 2^k)`, and bucket 0
//! holds only zero. That is not a coincidence to be maintained by hand -- it is
//! what lets a kernel-side histogram pass through the emit layer's own
//! bucketing without being re-bucketed into something else, and there is a test
//! below that crosses the two definitions.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{
    self, count_drop, helper, read_field, Insn, Log2Labels, Program, BPF_DW, BPF_W, R10,
};
use super::map::{map_failure, Counters, Hash};
use super::syscall;

/// Arguments of `sched_switch`, in the raw tracepoint's context.
const ARG_PREV: i16 = 8;
const ARG_NEXT: i16 = 16;
const ARG_PREV_STATE: i16 = 24;

/// The prototypes these programs are written against.
///
/// `sched_switch` gained its fourth argument in 5.18 and `sched_wakeup` has
/// carried one since it existed; both are checked before an instruction is
/// built, because reading a `task_struct *` out of a slot that holds an integer
/// does not fail, it produces a number.
const SWITCH_SIGNATURE: [&str; 4] = ["bool", "task_struct *", "task_struct *", "unsigned int"];
const WAKEUP_SIGNATURE: [&str; 1] = ["task_struct *"];

/// `TASK_RUNNING`: the task left the cpu still runnable, which is preemption.
/// Here that is not a case to drop -- it is a second way onto the run queue.
const TASK_RUNNING: i32 = 0;

/// How many tasks may be waiting for a cpu at once before the program stops
/// being able to record new ones. Every runnable task on the machine can be in
/// here at the same instant, which on a loaded machine is every task there is.
const MAX_QUEUED: u32 = 16384;

/// How many distinct `(task, cpu, priority, bucket)` rows one interval may
/// hold. The cardinality this buys is the point of the aggregation: a task
/// waits in a handful of buckets on a handful of cpus, whatever the workload
/// does.
const MAX_ROWS: u32 = 32768;

/// Slots of the drop counter, one per thing that can be lost.
const DROP_QUEUED: u32 = 0;
const DROP_ROWS: u32 = 1;

/// Where each map's key and value live on the program's stack.
///
/// Stated once so the sizes below, the decode on the other side and the map
/// descriptors all come from the same place. Every byte of a key is written
/// before the key is used: the kernel hashes all of it, and a byte nobody wrote
/// puts two identical waits in two buckets.
const ROW_VALUE: i16 = -112;
const ROW_KEY: i16 = -80;
const QUEUED_VALUE: i16 = -56;
const QUEUED_KEY: i16 = -48;
const DROP_KEY: i16 = -40;
const FILTER_TGID: i16 = -36;

/// The layout of a row key, as byte offsets within it.
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
/// The cpu the task was given, read where it is given -- `sched_switch` runs on
/// it. Not the cpu it was woken from: what a reader wants to know is which
/// queue was long, and the queue belongs to the cpu that eventually served it.
const KEY_CPU: i16 = 8;
/// `task_struct.prio`, the scheduler's own effective priority. In the key
/// rather than the value because it is a filter field the catalog offers, and
/// a field that only ever holds its last-seen value is not one a predicate can
/// mean anything about.
const KEY_PRIO: i16 = 12;
/// The log2 bucket the wait fell into. Part of the key is what makes this a
/// histogram rather than a mean: a task that mostly waits 2 us and occasionally
/// waits 40 ms is two rows here and one indistinguishable average anywhere else.
const KEY_BUCKET: i16 = 16;
const ROW_KEY_BYTES: usize = 20;

/// The layout of a row value.
const VALUE_COUNT: i16 = 0;
/// The exact sum of the waits in this bucket, kept beside the count because the
/// bucket itself is lossy: `[2^(k-1), 2^k)` is a factor of two wide, and a
/// total built from bucket bounds would inherit that error. The sum costs one
/// more atomic add and makes the mean exact.
const VALUE_TOTAL_NS: i16 = 8;
const VALUE_COMM: i16 = 16;
const COMM_BYTES: usize = 16;
const ROW_VALUE_BYTES: usize = 32;

/// The queued map: thread id to the moment it joined a run queue.
const QUEUED_KEY_BYTES: usize = 4;
const QUEUED_VALUE_BYTES: usize = 8;

/// `bpf_probe_read_kernel` is GPL-only, and every field these programs read
/// comes out of a `task_struct` through it.
const LICENSE: &str = "GPL";

/// One `(task, cpu, priority, bucket)` row's waits over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunqLat {
    pub tid: u32,
    pub tgid: u32,
    pub cpu: u32,
    /// The scheduler's effective priority, signed: a negative value is a
    /// real-time task, and reading it unsigned would report one as four
    /// billion.
    pub prio: i32,
    /// The log2 bucket, in `emit::BucketLayout::Log2`'s numbering: bucket 0
    /// holds a wait of zero, and bucket *k* holds `[2^(k-1), 2^k)`.
    pub bucket: u32,
    /// How many waits landed in this bucket.
    pub count: u64,
    /// Their exact total, which the bucket bounds cannot give back.
    pub total_ns: u64,
    pub comm: String,
}

impl RunqLat {
    /// The low bound of this row's bucket, in nanoseconds.
    ///
    /// This is the value that travels on the event, because it is the one the
    /// emit layer's own log2 bucketing maps back to exactly this bucket: a
    /// histogram built in the kernel and a histogram built in userspace from
    /// these events put the waits in the same places.
    pub fn bucket_low_ns(&self) -> u64 {
        match self.bucket {
            0 => 0,
            // A wait is a nanosecond count and `bucket` is derived from one, so
            // this cannot reach 64 -- but a shift that would overflow is a
            // panic rather than a wrong answer, so the case is written down.
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
            cpu: word(key, KEY_CPU as usize),
            prio: word(key, KEY_PRIO as usize) as i32,
            bucket: word(key, KEY_BUCKET as usize),
            count: long(value, VALUE_COUNT as usize),
            total_ns: long(value, VALUE_TOTAL_NS as usize),
            comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
        })
    }
}

/// Everything both enqueue sites need: the maps to write into, where the two
/// fields of a `task_struct` are on this kernel, and whose tasks to keep.
///
/// Grouped rather than passed one by one because the two sites must agree on
/// all five, and a list of five bare integers at two call sites is a place for
/// two of them to be swapped without the compiler minding.
struct Enqueue {
    queued_fd: i32,
    drops_fd: i32,
    pid_offset: i32,
    tgid_offset: i32,
    target_tgid: Option<u32>,
}

/// Writes the moment this task joined a run queue, from a `task_struct` in
/// `task_reg` and a timestamp in `ts_reg`.
///
/// Shared by both programs because both are the same event seen from two sides:
/// a wakeup puts a sleeping task on a queue, and a preemption puts a running
/// one straight back on it. Anything that distinguished them here would be
/// recording where the task came from, which is not what is being measured.
fn enqueue(program: &mut Program, at: &Enqueue, task_reg: u8, ts_reg: u8, resume: &'static str) {
    let &Enqueue {
        queued_fd,
        drops_fd,
        pid_offset,
        tgid_offset,
        target_tgid,
    } = at;
    // The whole slot is cleared first: the key is four bytes in an eight-byte
    // slot, and the four the program does not write are still hashed.
    program.push(Insn::st_imm(BPF_DW, R10, QUEUED_KEY, 0));
    read_field(program, QUEUED_KEY, 4, task_reg, pid_offset);

    // Thread zero is the per-cpu idle task, and it is not waiting for a cpu in
    // any sense a reader means. It is `TASK_RUNNING` whenever it leaves the cpu
    // -- that is what idle *is* -- so it would be enqueued at every switch and
    // its "wait" would be the time the cpu spent doing real work. Measured
    // system-wide before this line, the idle task was the largest queueing
    // series on the machine, which is the opposite of what happened.
    program.push(Insn::ldx(BPF_W, 1, R10, QUEUED_KEY));
    program.jump_to(resume, Insn::jeq_imm(1, 0, 0));

    // A run observes one process. The thread group is known before the program
    // loads and never changes, so the filter is written into the instruction
    // stream rather than looked up in a map on a path this hot.
    if let Some(tgid) = target_tgid {
        read_field(program, FILTER_TGID, 4, task_reg, tgid_offset);
        program.push(Insn::ldx(BPF_W, 1, R10, FILTER_TGID));
        program.jump_to(resume, Insn::jne_imm(1, tgid as i32, 0));
    }

    program.push(Insn::stx(BPF_DW, R10, QUEUED_VALUE, ts_reg));
    program.extend(Insn::load_map_fd(1, queued_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, QUEUED_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, QUEUED_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to(resume, Insn::jsge_imm(0, 0, 0));
    // The map is full: this wait will never be timed, and nothing else
    // anywhere would record that it happened.
    count_drop(program, drops_fd, DROP_KEY, DROP_QUEUED, resume);
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

/// The program on `sched_wakeup`: a task has just become runnable.
fn wakeup_instructions(
    btf: &Btf,
    target_tgid: Option<u32>,
    queued_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let signature = btf.tracepoint_signature("sched_wakeup")?;
    if signature != WAKEUP_SIGNATURE {
        return Err(format!(
            "this kernel declares `sched_wakeup({})`, and this program is written for \
             `sched_wakeup({})`: it reads the task being woken out of the first slot, and \
             against another prototype that slot holds something else",
            signature.join(", "),
            WAKEUP_SIGNATURE.join(", "),
        ));
    }
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;

    let mut program = Program::new();
    // r6 keeps the context, r7 the task being woken and r8 the timestamp. All
    // three survive the helper calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    enqueue(
        &mut program,
        &Enqueue {
            queued_fd,
            drops_fd,
            pid_offset,
            tgid_offset,
            target_tgid,
        },
        7,
        8,
        "done",
    );

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `sched_switch`: one task leaves the cpu and another takes it.
///
/// Both halves matter here, and for opposite reasons. The task leaving is
/// enqueued when it leaves *runnable*, because preemption is a way onto the run
/// queue. The task arriving is the one whose wait just ended, and it is the
/// only place that wait can be closed.
fn switch_instructions(
    btf: &Btf,
    target_tgid: Option<u32>,
    queued_fd: i32,
    rows_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let signature = btf.tracepoint_signature("sched_switch")?;
    if signature != SWITCH_SIGNATURE {
        return Err(format!(
            "this kernel declares `sched_switch({})`, and this program is written for \
             `sched_switch({})`: it reads the task that left the cpu, the one that took it and \
             the state of the first out of fixed slots, and against another prototype those \
             slots hold something else",
            signature.join(", "),
            SWITCH_SIGNATURE.join(", "),
        ));
    }
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let prio_offset = btf.member_offset("task_struct", "prio")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    // ---- the task leaving the cpu ----
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_PREV));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_PREV_STATE));
    // It left for something: it is blocked, not queued, and its time belongs to
    // `sched.offcpu`. Only a task that left still runnable is back in the queue.
    program.jump_to("arriving", Insn::jne_imm(1, TASK_RUNNING, 0));
    enqueue(
        &mut program,
        &Enqueue {
            queued_fd,
            drops_fd,
            pid_offset,
            tgid_offset,
            target_tgid,
        },
        7,
        8,
        "arriving",
    );
    program.label("arriving");

    // ---- the task taking the cpu ----
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_NEXT));
    program.push(Insn::st_imm(BPF_DW, R10, QUEUED_KEY, 0));
    read_field(&mut program, QUEUED_KEY, 4, 7, pid_offset);
    program.extend(Insn::load_map_fd(1, queued_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, QUEUED_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // A task this run never saw join a queue: it was already running when the
    // programs were attached, or it belongs to another process.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 9, 0, 0));

    program.extend(Insn::load_map_fd(1, queued_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, QUEUED_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    // r9 = now - when it joined the queue. Computed through r1, which nothing
    // between here and the next call reads.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::mov64_reg(9, 1));

    // The cpu goes in the key before the bucket does: this call clobbers r1
    // through r5, and the bucket is computed in them.
    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_CPU, 0));

    insn::push_log2_bucket(&mut program, 9, &LOG2);
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_BUCKET, 3));

    // The thread id is already on the stack from the lookup just done.
    program.push(Insn::ldx(BPF_W, 1, R10, QUEUED_KEY));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TID, 1));
    read_field(&mut program, ROW_KEY + KEY_TGID, 4, 7, tgid_offset);
    read_field(&mut program, ROW_KEY + KEY_PRIO, 4, 7, prio_offset);

    program.extend(Insn::load_map_fd(1, rows_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first_wait", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu, so both increments have to be ones
    // the other cpus cannot interleave with.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_TOTAL_NS, 9));
    program.jump_to("done", Insn::ja());

    program.label("first_wait");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_TOTAL_NS, 9));
    // The name is read once, when the row is created, rather than on every
    // wait: it does not change, and this is the hot path.
    read_field(
        &mut program,
        ROW_VALUE + VALUE_COMM,
        COMM_BYTES as i32,
        7,
        comm_offset,
    );
    program.extend(Insn::load_map_fd(1, rows_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
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
pub struct RunqLatProgram {
    queued: Hash,
    rows: Hash,
    drops: Counters,
    _wakeup_program: OwnedFd,
    _wakeup_link: OwnedFd,
    _switch_program: OwnedFd,
    _switch_link: OwnedFd,
}

/// What one drain found, beside the measurements.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainLosses {
    /// Rows the walk returned and the read no longer found.
    pub vanished: u64,
    /// The walk stopped at the map's capacity rather than at its end.
    pub truncated: bool,
}

impl RunqLatProgram {
    pub fn load(target_tgid: Option<u32>) -> Result<Self, String> {
        let btf = super::kernel_btf()?;

        let queued = Hash::create(
            QUEUED_KEY_BYTES,
            QUEUED_VALUE_BYTES,
            MAX_QUEUED,
            "omn_runq_wait",
        )
        .map_err(|error| map_failure("run queue map", error))?;
        let rows = Hash::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_runq_hist")
            .map_err(|error| map_failure("run queue histogram map", error))?;
        let drops = Counters::create(2, "omn_runq_drop")
            .map_err(|error| map_failure("run queue drop counter", error))?;

        let wakeup = wakeup_instructions(btf, target_tgid, queued.as_raw_fd(), drops.as_raw_fd())?;
        let switch = switch_instructions(
            btf,
            target_tgid,
            queued.as_raw_fd(),
            rows.as_raw_fd(),
            drops.as_raw_fd(),
        )?;

        let wakeup_program = load(&wakeup, "omn_runq_wake")?;
        let switch_program = load(&switch, "omn_runq_swtch")?;

        // Attached in this order on purpose. A wait recorded by a program whose
        // partner is not attached yet is a wait that will never be closed --
        // harmless, it ages out of the map -- whereas a switch seen before any
        // wakeup has been recorded would close a wait nobody opened, and the
        // lookup that fails is the only thing standing between that and a
        // duration measured from an uninitialised timestamp.
        let wakeup_link = syscall::raw_tracepoint_open("sched_wakeup", wakeup_program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sched_wakeup` tracepoint: {error}"))?;
        let switch_link = syscall::raw_tracepoint_open("sched_switch", switch_program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `sched_switch` tracepoint: {error}"))?;

        Ok(Self {
            queued,
            rows,
            drops,
            _wakeup_program: wakeup_program,
            _wakeup_link: wakeup_link,
            _switch_program: switch_program,
            _switch_link: switch_link,
        })
    }

    /// Takes every row the kernel has accumulated since the last drain.
    ///
    /// Each row is a delta over the interval between two drains: the rows are
    /// removed as they are read, so what comes back next time is what happened
    /// in between.
    pub fn drain(&self) -> Result<(Vec<RunqLat>, DrainLosses), String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the run queue histogram: {error}"))?;
        let mut losses = DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match RunqLat::decode(key, value) {
                Some(row) => rows.push(row),
                // A record the wrong size means this reader and the program
                // disagree about the layout, which is a bug rather than a
                // machine condition -- but counting it beats reinterpreting it.
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    /// How many tasks were on a run queue and had not been given a cpu.
    ///
    /// Their wait is real and is in no bucket, because it has no end yet. On a
    /// machine with more runnable tasks than cpus that is the normal state, and
    /// it is a fact about the window rather than a loss.
    pub fn still_queued(&self) -> usize {
        self.queued.len().unwrap_or(0)
    }

    /// Waits the programs could not record because a map was full, as
    /// `(queued, rows)`.
    pub fn dropped(&self) -> (u64, u64) {
        (self.drops.read(DROP_QUEUED), self.drops.read(DROP_ROWS))
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

    /// The bucket index the emit layer computes, written here as the machine's
    /// own count-leading-zeros rather than as the six-step search the program
    /// performs. `emit::BucketLayout::Log2` documents exactly this: bucket 0
    /// holds what is below one, and bucket *k* holds `[2^(k-1), 2^k)`.
    ///
    /// Deliberately not a call into `emit`, and deliberately not the same
    /// arithmetic as the instruction stream: an independent expression of the
    /// definition is the only thing that can catch the search being off by one.
    /// The end-to-end check that the emit layer really agrees is in
    /// `tests/family_sched.rs`, where a known wait goes through the real one.
    fn reference_index(value: u64) -> u64 {
        u64::from(u64::BITS - value.leading_zeros())
    }

    fn kernel_btf() -> Option<Btf> {
        match Btf::from_kernel() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    fn switch_program(btf: &Btf) -> Vec<Insn> {
        switch_instructions(btf, None, 3, 4, 5).expect("a program")
    }

    /// The kernel's bucketing and the emit layer's must be the same function.
    ///
    /// This is the whole reason the histogram can be built in the kernel at
    /// all: the rows come back as `(bucket, count)` and are rendered by a layer
    /// that computes bucket indices itself. If the two disagree by one, every
    /// wait is reported in the neighbouring bucket -- a factor of two -- and
    /// nothing anywhere says so.
    ///
    /// `bucket_low_ns` is the bridge: the value that travels on the event is
    /// the low bound of the kernel's bucket, and putting it back through the
    /// emit layer has to land in that same bucket.
    #[test]
    fn the_kernels_buckets_and_the_emit_layers_are_the_same_buckets() {
        for bucket in 0..48u32 {
            let row = RunqLat {
                tid: 1,
                tgid: 1,
                cpu: 0,
                prio: 120,
                bucket,
                count: 1,
                total_ns: 0,
                comm: String::new(),
            };
            let low = row.bucket_low_ns();
            assert_eq!(
                reference_index(low),
                u64::from(bucket),
                "bucket {bucket} starts at {low} ns, which the layout puts elsewhere"
            );
            if bucket > 0 {
                // And the bucket must not be wider than the layout says: the
                // value one below its low bound belongs to the bucket before.
                assert_eq!(
                    reference_index(low - 1),
                    u64::from(bucket - 1),
                    "bucket {bucket} and the one below it overlap"
                );
            }
        }
    }

    /// The unrolled binary search has to compute `floor(log2(v)) + 1`, which is
    /// what the emit layer's index is. Checked here against the same shift
    /// arithmetic the instruction stream performs, because the instructions
    /// themselves cannot be run outside the kernel.
    /// Invariant 6, checked in the instruction stream rather than trusted.
    /// `bpf_ktime_get_boot_ns` includes suspend time, and a wait computed from
    /// one reading of each clock is not a wait at all.
    #[test]
    fn both_programs_read_the_monotonic_clock_and_no_other() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        for (what, instructions) in [
            (
                "the wakeup program",
                wakeup_instructions(&btf, None, 3, 4).expect("a program"),
            ),
            ("the switch program", switch_program(&btf)),
        ] {
            let called: Vec<i32> = instructions
                .iter()
                .filter(|insn| insn.code == 0x85)
                .map(|insn| insn.imm)
                .collect();
            assert!(
                called.contains(&helper::KTIME_GET_NS),
                "{what} takes no timestamp"
            );
            assert!(!called.contains(&KTIME_GET_BOOT_NS), "{what}");
            assert_eq!(
                called
                    .iter()
                    .filter(|id| **id == helper::KTIME_GET_NS)
                    .count(),
                1,
                "{what} must stamp everything it does with one reading, or a wait measured \
                 across its two halves is short by whatever happened between them"
            );
        }
    }

    /// A preempted task is still runnable and is back on the run queue, so its
    /// wait counts here. Dropping it would report a machine under cpu
    /// contention -- where nearly every wait is a preemption -- as a machine
    /// with no queueing at all.
    #[test]
    fn a_preempted_task_is_put_back_on_the_queue_rather_than_dropped() {
        let Some(btf) = kernel_btf() else { return };
        let program = switch_program(&btf);
        // `jne TASK_RUNNING` skips the enqueue: the task is only enqueued when
        // it left *still runnable*. The opposite test -- `jeq` -- would enqueue
        // exactly the tasks that blocked, which is the other source entirely.
        assert!(
            program
                .iter()
                .any(|insn| insn.code == 0x55 && insn.imm == TASK_RUNNING),
            "the switch program must test the outgoing task's state, and enqueue it only when \
             it is still runnable"
        );
    }

    /// The wait is a difference between two timestamps. A row built from the
    /// absolute second one is the machine's uptime, and it looks like a number.
    #[test]
    fn the_wait_is_a_difference_and_not_a_timestamp() {
        let Some(btf) = kernel_btf() else { return };
        let program = switch_program(&btf);
        assert!(
            program.iter().any(|insn| *insn == Insn::sub64_reg(1, 9)),
            "the program must subtract the moment the task was queued from the moment it ran"
        );
    }

    /// Both accumulations are into one allocation shared by every cpu, so both
    /// have to be atomic. A plain load-add-store loses whichever increment
    /// lands between the two halves of another cpu's, and loses most of them on
    /// exactly the busy machines the number is read on.
    #[test]
    fn the_count_and_the_total_are_accumulated_atomically() {
        let Some(btf) = kernel_btf() else { return };
        let atomics = switch_program(&btf)
            .iter()
            .filter(|insn| insn.code == 0xdb)
            .count();
        assert_eq!(atomics, 2, "the count of waits and their total, both");
    }

    /// The per-cpu idle task leaves the cpu `TASK_RUNNING` every time anything
    /// else runs, so without this it is enqueued at every switch and its
    /// "wait" is the time the machine spent working. Both programs have to
    /// refuse it, because a wakeup can name it too.
    #[test]
    fn the_idle_task_is_never_put_on_a_run_queue() {
        let Some(btf) = kernel_btf() else { return };
        for (what, instructions) in [
            (
                "the wakeup program",
                wakeup_instructions(&btf, None, 3, 4).expect("a program"),
            ),
            ("the switch program", switch_program(&btf)),
        ] {
            // A word load of the thread id off the stack, followed by a jump
            // taken when it is zero: the enqueue is skipped for thread zero.
            let pairs = instructions.windows(2).filter(|pair| {
                pair[0] == Insn::ldx(BPF_W, 1, R10, QUEUED_KEY)
                    && pair[1].code == 0x15
                    && pair[1].imm == 0
            });
            assert_eq!(
                pairs.count(),
                1,
                "{what} must skip the idle task exactly where a task is enqueued"
            );
        }
    }

    /// A machine-wide run has no filter in it at all; a targeted one filters
    /// where a task joins the queue, which is both places a task can join one.
    #[test]
    fn the_filter_is_present_only_where_a_task_joins_the_queue() {
        let Some(btf) = kernel_btf() else { return };
        assert!(!switch_program(&btf).iter().any(|insn| insn.imm == 4242));
        assert!(!wakeup_instructions(&btf, None, 3, 4)
            .expect("a program")
            .iter()
            .any(|insn| insn.imm == 4242));

        let filtered = switch_instructions(&btf, Some(4242), 3, 4, 5).expect("a program");
        assert_eq!(
            filtered.iter().filter(|insn| insn.imm == 4242).count(),
            1,
            "a preempted task is filtered where it is enqueued; where it arrives it is already \
             known, because only a task this run enqueued can be found in the map"
        );
        let filtered = wakeup_instructions(&btf, Some(4242), 3, 4).expect("a program");
        assert_eq!(filtered.iter().filter(|insn| insn.imm == 4242).count(), 1);
    }

    /// A kernel whose prototypes are not the ones these programs read must be
    /// refused by name, on both tracepoints.
    #[test]
    fn a_kernel_with_another_prototype_is_refused_rather_than_read_wrong() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            btf.tracepoint_signature("sched_switch")
                .expect("a prototype"),
            SWITCH_SIGNATURE
        );
        assert_eq!(
            btf.tracepoint_signature("sched_wakeup")
                .expect("a prototype"),
            WAKEUP_SIGNATURE
        );
    }

    /// A key or a value the program leaves a hole in is not a smaller record,
    /// it is a record with stack garbage in it: the kernel hashes every byte of
    /// a key, so one byte nobody wrote puts two identical waits in two buckets.
    #[test]
    fn every_byte_of_a_key_and_a_value_is_accounted_for() {
        let key = [
            (KEY_TID, 4),
            (KEY_TGID, 4),
            (KEY_CPU, 4),
            (KEY_PRIO, 4),
            (KEY_BUCKET, 4),
        ];
        let value = [
            (VALUE_COUNT, 8),
            (VALUE_TOTAL_NS, 8),
            (VALUE_COMM, COMM_BYTES as i16),
        ];
        for (fields, bytes, what) in [
            (&key[..], ROW_KEY_BYTES, "the row key"),
            (&value[..], ROW_VALUE_BYTES, "the row value"),
        ] {
            let mut covered = vec![false; bytes];
            for (at, size) in fields {
                for byte in *at..*at + *size {
                    let byte = byte as usize;
                    assert!(byte < bytes, "{what} is written past its own end");
                    assert!(!covered[byte], "{what} writes byte {byte} twice");
                    covered[byte] = true;
                }
            }
            assert!(
                covered.iter().all(|byte| *byte),
                "{what} has a byte the program never writes"
            );
        }
    }

    /// The stack regions must not overlap. They are addressed by offset
    /// arithmetic, and two that overlap build a key partly out of a value --
    /// which hashes to a bucket nobody looks in, so the numbers are simply
    /// short.
    #[test]
    fn the_stack_regions_do_not_overlap_and_stay_inside_the_frame() {
        let regions = [
            (ROW_VALUE, ROW_VALUE_BYTES as i16, "row value"),
            (ROW_KEY, ROW_KEY_BYTES as i16, "row key"),
            (QUEUED_VALUE, QUEUED_VALUE_BYTES as i16, "queued value"),
            (QUEUED_KEY, 8, "queued key slot"),
            (DROP_KEY, 4, "drop key"),
            (FILTER_TGID, 4, "filter tgid"),
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

    #[test]
    fn a_record_the_wrong_size_is_refused_rather_than_padded() {
        assert!(RunqLat::decode(&[0u8; 8], &[0u8; ROW_VALUE_BYTES]).is_none());
        assert!(RunqLat::decode(&[0u8; ROW_KEY_BYTES], &[0u8; 8]).is_none());
    }

    /// A real-time task has a negative `prio`, and reading it unsigned would
    /// report the most urgent thing on the machine as the least.
    #[test]
    fn a_negative_priority_survives_the_round_trip() {
        let mut key = [0u8; ROW_KEY_BYTES];
        key[KEY_PRIO as usize..KEY_PRIO as usize + 4].copy_from_slice(&(-1i32).to_ne_bytes());
        let mut value = [0u8; ROW_VALUE_BYTES];
        value[VALUE_COUNT as usize..VALUE_COUNT as usize + 8].copy_from_slice(&3u64.to_ne_bytes());
        value[VALUE_COMM as usize..VALUE_COMM as usize + 4].copy_from_slice(b"idle");
        let row = RunqLat::decode(&key, &value).expect("a full record");
        assert_eq!((row.prio, row.count, row.comm.as_str()), (-1, 3, "idle"));
    }
}
