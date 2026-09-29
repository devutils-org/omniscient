//! The stall programs: the kernel doing the allocator's work inside the
//! allocating thread.
//!
//! Two sources, one shape. **Direct reclaim** is what happens when a thread
//! asks for memory and there is none free: instead of being handed a page, it
//! is made to go and find one -- writing back dirty pages, dropping cache,
//! swapping -- and it does that on its own stack, in the middle of whatever it
//! was doing. **Compaction** is the same bargain for contiguity rather than
//! quantity: the allocator wants an order-9 block, the free pages are scattered,
//! and somebody has to move them. Both are microseconds to hundreds of
//! milliseconds of a thread's own latency that no cpu profile explains and no
//! footprint metric shows.
//!
//! Neither is one event. The kernel publishes a `begin` and an `end` and the
//! measurement is the gap, so two programs are loaded per source and they share
//! a map: the first writes the moment the thread entered, the second reads it
//! back and buckets the difference. `src/bpf/runqlat.rs` is the same
//! construction for a different pair of tracepoints, and the reasons it gives
//! for counting in the kernel rather than streaming apply here with less force
//! -- reclaim is rare -- but the histogram is what `--emit hist` needs, and a
//! distribution is the only honest shape for a latency that spans four orders
//! of magnitude.
//!
//! **Three things an end without a beginning could be, and only one is
//! interesting.** A thread already inside reclaim when the programs attached
//! ends without ever having begun; so does a thread whose beginning the map had
//! no room for; and so would a thread whose `begin` fired on one kernel path
//! and whose `end` fired on another, which would be a bug in this program. They
//! are indistinguishable from here, so they are counted together, surfaced as a
//! loss, and named as what they are: an episode that happened and has no
//! duration.

use std::os::fd::{AsRawFd, OwnedFd};

use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, row_map, select_rows, Counters, Hash, Rows};
use super::mem_program::{
    bucket_low, check_signature, comm, count_drop, load, long, push_log2_bucket, read_cgroup_id,
    read_comm, read_pid_tgid, word,
};
use super::syscall;
use super::DrainLosses;

/// How many threads may be inside reclaim or compaction at once before the
/// program stops being able to record new ones.
const MAX_IN_FLIGHT: u32 = 8192;

/// How many distinct `(task, cgroup, bucket)` rows one interval may hold.
const MAX_ROWS: u32 = 4096;

/// Slots of the drop counter, one per thing that can be lost.
const DROP_ROWS: u32 = 0;
const DROP_IN_FLIGHT: u32 = 1;
const DROP_UNPAIRED: u32 = 2;
const DROP_SLOTS: u32 = 3;

/// Where each map's key and value live on the program's stack.
const ROW_VALUE: i16 = -48;
const ROW_KEY: i16 = -80;
const PID_TGID: i16 = -88;
const START_KEY: i16 = -96;
const START_VALUE: i16 = -104;
const DROP_KEY: i16 = -108;
/// Where the program keeps the row map it looked up through the generation
/// slot, once per run; see `map::select_rows`.
const ROWS_SLOT: i16 = -120;

/// The layout of a row key.
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
const KEY_CGROUP: i16 = 8;
/// The log2 bucket of the episode's duration, in nanoseconds.
const KEY_BUCKET: i16 = 16;
/// Why the thread was reclaiming: the machine was out of memory, or its cgroup
/// was out of its allowance. In the key rather than the value because the two
/// are different diagnoses with the same shape, and a field that only ever holds
/// its last-seen value could not tell a row of one from a row of the other.
const KEY_SCOPE: i16 = 20;
const ROW_KEY_BYTES: usize = 24;

/// The layout of a row value.
const VALUE_COUNT: i16 = 0;
/// The exact sum of the durations in this bucket, kept beside the count because
/// the bucket itself is lossy: `[2^(k-1), 2^k)` is a factor of two wide, and a
/// total built from bucket bounds would inherit that error.
const VALUE_TOTAL_NS: i16 = 8;
/// Whatever the `end` tracepoint carries that is worth adding up. For direct
/// reclaim that is the number of pages it actually freed, which is the other
/// half of the question: a thread that stalled ten milliseconds and reclaimed
/// nothing is in a different situation from one that stalled ten milliseconds
/// and freed a hundred megabytes.
const VALUE_YIELD: i16 = 16;
const VALUE_COMM: i16 = 24;
/// When the first episode of this row's window *began*, on the same
/// `CLOCK_MONOTONIC` everything else here uses.
///
/// A bucketed source emits one event per row per window, and the natural
/// timestamp for it is the drain that read it -- the window's end. That is
/// where the seventh wave's machine crossing found it, and it is a problem for
/// exactly one thing: a join that *opens* on this source opens at the close, so
/// everything that happened in the same window as the first episode falls
/// outside the state. That crossing worked around it by sleeping 300 ms between
/// writing and reading, and said so in the session.
///
/// Written once, when the row is created, which is the first completed episode
/// of that key in that window: the rows map is drained and emptied every
/// window, so a row that exists is a row this window opened.
const VALUE_FIRST_NS: i16 = 40;
const ROW_VALUE_BYTES: usize = 48;

/// The in-flight map: thread id to the moment it entered.
const START_KEY_BYTES: usize = 4;
const START_VALUE_BYTES: usize = 8;

/// One tracepoint a program attaches to.
struct Site {
    name: &'static str,
    /// Every prototype the program reads the same slots out of, newest first,
    /// in the order the catalog's requirement lists them.
    signatures: &'static [&'static [&'static str]],
    /// What the row gets for [`StallRow::scope`] when this one fires.
    scope: u32,
    /// Whether a kernel without it cannot serve the source at all.
    ///
    /// The kernel this was written against publishes the global and the
    /// cgroup-limit halves of reclaim under two names with one prototype, and a
    /// kernel that has only the first still answers the question a `--system`
    /// run asks. One that has only the second does not exist.
    required: bool,
}

/// The prototypes both reclaim `begin` tracepoints share. They come from one
/// `DECLARE_EVENT_CLASS`, which is why one program can serve both.
///
/// 6.6 and 5.15 declare `(int order, gfp_t)` and no cgroup; the kernel this
/// was written against moved the flags first and added the cgroup. The `begin`
/// program reads no argument at all, so the two are the same to it.
const RECLAIM_BEGIN: &[&[&str]] = &[&["gfp_t", "int", "mem_cgroup *"], &["int", "gfp_t"]];
/// The `end` pair: pages reclaimed first in both, and a cgroup after it only in
/// the newer shape. The program reads slot zero and nothing else.
const RECLAIM_END: &[&[&str]] = &[
    &["long unsigned int", "mem_cgroup *"],
    &["long unsigned int"],
];

/// Compaction's pair. 5.15 passes the zone's bounds and the two scanners'
/// positions as four integers where later kernels pass the `compact_control`
/// that holds them; neither program reads any of it.
const COMPACTION_BEGIN: &[&[&str]] = &[
    &[
        "compact_control *",
        "long unsigned int",
        "long unsigned int",
        "bool",
    ],
    &[
        "long unsigned int",
        "long unsigned int",
        "long unsigned int",
        "long unsigned int",
        "bool",
    ],
];
const COMPACTION_END: &[&[&str]] = &[
    &[
        "compact_control *",
        "long unsigned int",
        "long unsigned int",
        "bool",
        "int",
    ],
    &[
        "long unsigned int",
        "long unsigned int",
        "long unsigned int",
        "long unsigned int",
        "bool",
        "int",
    ],
];

/// Which stall a program is watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stall {
    /// Reclaim done by the allocating thread itself, in both of the shapes the
    /// kernel publishes: `mm_vmscan_direct_reclaim_*` when the machine is out
    /// of free memory, and `mm_vmscan_memcg_reclaim_*` when a cgroup is out of
    /// its own allowance.
    ///
    /// **Both, because either alone is a wrong answer.** The catalog's chain
    /// names the first pair, and a run scoped to a cgroup that watched only
    /// those would report zero while the cgroup reclaimed continuously -- the
    /// kernel fires the memcg pair there and never the global one. The two are
    /// the same measurement, a thread doing the allocator's work in-line, and
    /// they differ in why, which the row carries as its scope.
    Reclaim,
    /// `mm_compaction_begin` and `mm_compaction_end`.
    Compaction,
}

/// The scope a row's episodes were measured in.
pub const SCOPE_GLOBAL: u32 = 0;
pub const SCOPE_CGROUP: u32 = 1;

impl Stall {
    fn begins(self) -> &'static [Site] {
        match self {
            Stall::Reclaim => &[
                Site {
                    name: "mm_vmscan_direct_reclaim_begin",
                    signatures: RECLAIM_BEGIN,
                    scope: SCOPE_GLOBAL,
                    required: true,
                },
                Site {
                    name: "mm_vmscan_memcg_reclaim_begin",
                    signatures: RECLAIM_BEGIN,
                    scope: SCOPE_CGROUP,
                    required: false,
                },
            ],
            Stall::Compaction => &[Site {
                name: "mm_compaction_begin",
                signatures: COMPACTION_BEGIN,
                scope: SCOPE_GLOBAL,
                required: true,
            }],
        }
    }

    fn ends(self) -> &'static [Site] {
        match self {
            Stall::Reclaim => &[
                Site {
                    name: "mm_vmscan_direct_reclaim_end",
                    signatures: RECLAIM_END,
                    scope: SCOPE_GLOBAL,
                    required: true,
                },
                Site {
                    name: "mm_vmscan_memcg_reclaim_end",
                    signatures: RECLAIM_END,
                    scope: SCOPE_CGROUP,
                    required: false,
                },
            ],
            Stall::Compaction => &[Site {
                name: "mm_compaction_end",
                signatures: COMPACTION_END,
                scope: SCOPE_GLOBAL,
                required: true,
            }],
        }
    }

    /// The slot of the `end` tracepoint holding a number worth accumulating,
    /// and what to call it.
    ///
    /// Compaction's `end` carries a status code rather than a quantity, and a
    /// status summed over a bucket is not a number: what a reader would want
    /// from it is how often compaction failed, which needs the status in the
    /// key, which the entry declares no key for. That is written down in
    /// `docs/families/mem.md` rather than approximated here.
    fn yields(self) -> Option<(i16, &'static str)> {
        match self {
            Stall::Reclaim => Some((0, "pages_reclaimed")),
            Stall::Compaction => None,
        }
    }

    /// Whether the entry declares a cgroup key.
    fn with_cgroup(self) -> bool {
        match self {
            Stall::Reclaim => true,
            Stall::Compaction => false,
        }
    }

    /// What the maps and the programs are called, which is what
    /// `bpftool map list` and `bpftool prog list` show while a run is going on.
    fn names(self) -> [&'static str; 5] {
        match self {
            Stall::Reclaim => [
                "omn_rclm_live",
                "omn_rclm_rows",
                "omn_rclm_drop",
                "omn_rclm_beg",
                "omn_rclm_end",
            ],
            Stall::Compaction => [
                "omn_cmpt_live",
                "omn_cmpt_rows",
                "omn_cmpt_drop",
                "omn_cmpt_beg",
                "omn_cmpt_end",
            ],
        }
    }

    /// What this source calls one of its episodes, for the messages a reader
    /// sees.
    pub fn episode(self) -> &'static str {
        match self {
            Stall::Reclaim => "a reclaim",
            Stall::Compaction => "a compaction",
        }
    }
}

/// One `(task, cgroup, duration bucket)` row over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallRow {
    pub tid: u32,
    pub tgid: u32,
    pub cgroup_id: u64,
    /// The log2 bucket, in `emit::BucketLayout::Log2`'s numbering.
    pub bucket: u32,
    /// [`SCOPE_GLOBAL`] or [`SCOPE_CGROUP`]: whether the thread was reclaiming
    /// because the machine had no free memory or because its cgroup had none of
    /// its allowance left. Always [`SCOPE_GLOBAL`] for compaction, which the
    /// kernel publishes under one name.
    pub scope: u32,
    /// How many episodes landed in this bucket.
    pub count: u64,
    /// Their exact total, which the bucket bounds cannot give back.
    pub total_ns: u64,
    /// What the episodes yielded, where the `end` tracepoint says: pages
    /// reclaimed, for direct reclaim.
    pub yielded: u64,
    /// When the first episode of this row began, which is what the event is
    /// timestamped with. Zero on a kernel-side row this build did not create --
    /// there are none today, and it is read as "unknown, fall back to the
    /// drain" rather than as the beginning of time.
    pub first_ns: u64,
    pub comm: String,
}

impl StallRow {
    /// The low bound of this row's bucket, in nanoseconds. This is the value
    /// that travels on the event, because it is the one the emit layer's own
    /// log2 bucketing maps back to exactly this bucket.
    pub fn duration_ns(&self) -> u64 {
        bucket_low(self.bucket)
    }

    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        Some(Self {
            tid: word(key, KEY_TID as usize),
            tgid: word(key, KEY_TGID as usize),
            cgroup_id: long(key, KEY_CGROUP as usize),
            bucket: word(key, KEY_BUCKET as usize),
            scope: word(key, KEY_SCOPE as usize),
            count: long(value, VALUE_COUNT as usize),
            total_ns: long(value, VALUE_TOTAL_NS as usize),
            yielded: long(value, VALUE_YIELD as usize),
            first_ns: long(value, VALUE_FIRST_NS as usize),
            comm: comm(value, VALUE_COMM as usize),
        })
    }
}

/// The program on the `begin` tracepoint: this thread has just entered.
/// One program serves every `begin` this source has, because it reads no
/// argument at all: a beginning is a thread id and a timestamp, and both come
/// from helpers rather than from the tracepoint's own slots. The prototypes are
/// still checked, at the attach site, so a kernel that moved one is refused
/// there rather than read wrong here.
fn begin_instructions(
    target_tgid: Option<u32>,
    starts_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    read_pid_tgid(&mut program, PID_TGID);

    // A run observes one process. The thread group is known before the program
    // loads and never changes, so the filter is written into the instruction
    // stream rather than looked up in a map.
    if let Some(tgid) = target_tgid {
        program.push(Insn::ldx(BPF_W, 1, R10, PID_TGID + 4));
        program.jump_to("done", Insn::jne_imm(1, tgid as i32, 0));
    }

    // The whole slot is cleared first: the key is four bytes in an eight-byte
    // slot, and the four the program does not write are still hashed.
    program.push(Insn::st_imm(BPF_DW, R10, START_KEY, 0));
    program.push(Insn::ldx(BPF_W, 1, R10, PID_TGID));
    program.push(Insn::stx(BPF_W, R10, START_KEY, 1));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, START_VALUE, 0));

    program.extend(Insn::load_map_fd(1, starts_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, START_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, START_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    // The map is full: this episode will never be timed, and nothing else
    // anywhere would record that it happened.
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_IN_FLIGHT, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let built = program.build();
    super::insn::audit(&built)?;
    Ok(built)
}

/// The program on the `end` tracepoint: this thread is out, and the difference
/// is the measurement.
fn end_instructions(
    stall: Stall,
    scope: u32,
    starts_fd: i32,
    rows_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    // Read before any helper call can clobber the low registers, and kept in a
    // callee-saved one.
    if let Some((at, _)) = stall.yields() {
        program.push(Insn::ldx(BPF_DW, 9, 6, at));
    }
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    read_pid_tgid(&mut program, PID_TGID);
    program.push(Insn::st_imm(BPF_DW, R10, START_KEY, 0));
    program.push(Insn::ldx(BPF_W, 1, R10, PID_TGID));
    program.push(Insn::stx(BPF_W, R10, START_KEY, 1));

    program.extend(Insn::load_map_fd(1, starts_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, START_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // An end this run never saw begin: the thread was already inside when the
    // programs attached, or its beginning found the map full, or it belongs to
    // another process. An episode with no duration, counted as one.
    program.jump_to("unpaired", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 7, 0, 0));

    program.extend(Insn::load_map_fd(1, starts_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, START_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    // r7 = now - when it entered, computed through r1, which nothing between
    // here and the next call reads.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 7));
    program.push(Insn::mov64_reg(7, 1));

    push_log2_bucket(&mut program, 7);
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_BUCKET, 3));
    program.push(Insn::st_imm(BPF_W, R10, ROW_KEY + KEY_SCOPE, scope as i32));

    program.push(Insn::ldx(BPF_W, 1, R10, PID_TGID));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TID, 1));
    program.push(Insn::ldx(BPF_W, 1, R10, PID_TGID + 4));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TGID, 1));
    read_cgroup_id(&mut program, ROW_KEY + KEY_CGROUP, stall.with_cgroup());

    // The row map of this interval, taken once: a drain may swap it at any
    // instruction, and a lookup and an insert that named two maps would put
    // one occurrence in each. See `map::Rows`.
    select_rows(&mut program, rows_fd, ROWS_SLOT, "no_rows");
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu, so every increment has to be one the
    // other cpus cannot interleave with.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_TOTAL_NS, 7));
    if stall.yields().is_some() {
        program.push(Insn::atomic_add(BPF_DW, 0, VALUE_YIELD, 9));
    }
    program.jump_to("done", Insn::ja());

    program.label("first");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_TOTAL_NS, 7));
    // When this episode began: now, less how long it lasted. r8 is the reading
    // taken at the top of the program and r7 is the duration, so the beginning
    // is recovered rather than stored a second time in the in-flight map.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 7));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_FIRST_NS, 1));
    if stall.yields().is_some() {
        program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_YIELD, 9));
    } else {
        program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_YIELD, 0));
    }
    read_comm(&mut program, ROW_VALUE + VALUE_COMM);
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_ROWS, "done");
    program.jump_to("done", Insn::ja());

    program.label("unpaired");
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_UNPAIRED, "done");

    program.jump_to("done", Insn::ja());
    // The slot held no map, which it always does; counted rather than assumed.
    program.label("no_rows");
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_ROWS, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let built = program.build();
    super::insn::audit(&built)?;
    Ok(built)
}

/// Two loaded, attached programs and the maps they share.
pub struct StallProgram {
    stall: Stall,
    in_flight: Hash,
    rows: Rows,
    drops: Counters,
    absent: Vec<String>,
    programs: Vec<OwnedFd>,
    _links: Vec<OwnedFd>,
}

/// What the two programs could not record, read off their own counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StallDrops {
    /// Rows the histogram map had no room for.
    pub rows: u64,
    /// Beginnings the in-flight map had no room for.
    pub in_flight: u64,
    /// Endings whose beginning this run never saw.
    pub unpaired: u64,
}

impl StallProgram {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped(self.programs.iter())
    }

    pub fn load(stall: Stall, target_tgid: Option<u32>) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let [live_name, rows_name, drops_name, begin_name, end_name] = stall.names();

        let in_flight = Hash::create(START_KEY_BYTES, START_VALUE_BYTES, MAX_IN_FLIGHT, live_name)
            .map_err(|error| map_failure("in-flight map", error))?;
        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, rows_name)
            .map_err(|error| map_failure("stall histogram map", error))?;
        let drops = Counters::create(DROP_SLOTS, drops_name)
            .map_err(|error| map_failure("stall drop counter", error))?;

        let begin = begin_instructions(target_tgid, in_flight.as_raw_fd(), drops.as_raw_fd())?;
        let begin_program = load(&begin, begin_name)?;

        let mut programs = vec![];
        let mut links = Vec::new();
        let mut absent = Vec::new();

        // Attached beginnings first, and every beginning before any ending. An
        // episode recorded by a program whose partner is not attached yet is one
        // that will never be closed -- harmless, it ages out of the map --
        // whereas an ending seen before any beginning has been recorded would
        // close an episode nobody opened, and the lookup that fails is the only
        // thing standing between that and a duration measured from an
        // uninitialised timestamp.
        for site in stall.begins() {
            match check_signature(
                btf,
                site.name,
                site.signatures,
                "nothing but the moment it fired",
            ) {
                Ok(_) => links.push(
                    syscall::raw_tracepoint_open(site.name, begin_program.as_raw_fd()).map_err(
                        |error| format!("cannot attach to the `{}` tracepoint: {error}", site.name),
                    )?,
                ),
                Err(reason) if site.required => return Err(reason),
                Err(reason) => absent.push(reason),
            }
        }
        programs.push(begin_program);

        let reads = match stall.yields() {
            Some((_, what)) => what,
            None => "nothing but the moment it fired",
        };
        for site in stall.ends() {
            if let Err(reason) = check_signature(btf, site.name, site.signatures, reads) {
                if site.required {
                    return Err(reason);
                }
                absent.push(reason);
                continue;
            }
            // One program per ending, because each writes its own scope into the
            // rows it creates: the instructions are otherwise identical, and the
            // scope is the one thing userspace cannot recover afterwards.
            let end = end_instructions(
                stall,
                site.scope,
                in_flight.as_raw_fd(),
                rows.as_raw_fd(),
                drops.as_raw_fd(),
            )?;
            let program = load(&end, end_name)?;
            links.push(
                syscall::raw_tracepoint_open(site.name, program.as_raw_fd()).map_err(|error| {
                    format!("cannot attach to the `{}` tracepoint: {error}", site.name)
                })?,
            );
            programs.push(program);
        }

        Ok(Self {
            stall,
            in_flight,
            rows,
            drops,
            absent,
            programs,
            _links: links,
        })
    }

    /// The tracepoints this kernel does not publish in the shape the programs
    /// were written for, and which were therefore not attached.
    ///
    /// Not an error, and not silence either: a run whose kernel has no
    /// `mm_vmscan_memcg_reclaim_*` cannot see a cgroup reclaiming under its own
    /// limit, and a reader comparing two machines has to be told which one that
    /// was.
    pub fn absent(&self) -> &[String] {
        &self.absent
    }

    pub fn stall(&self) -> Stall {
        self.stall
    }

    /// Takes every row the kernel has accumulated since the last drain.
    pub fn drain(&self) -> Result<(Vec<StallRow>, DrainLosses), String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the stall histogram: {error}"))?;
        let mut losses = DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match StallRow::decode(key, value) {
                Some(row) => rows.push(row),
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    /// How many threads were inside and had not come out.
    ///
    /// Their episode is real and is in no bucket, because it has no end yet.
    /// That is a fact about the window rather than a loss.
    pub fn still_inside(&self) -> usize {
        self.in_flight.len().unwrap_or(0)
    }

    pub fn dropped(&self) -> StallDrops {
        StallDrops {
            rows: self.drops.read(DROP_ROWS),
            in_flight: self.drops.read(DROP_IN_FLIGHT),
            unpaired: self.drops.read(DROP_UNPAIRED),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bpf::btf::Btf;
    use crate::bpf::mem_program::COMM_BYTES;

    fn kernel_btf() -> Option<Btf> {
        match Btf::from_kernel() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    /// Ground truth from the kernel that is running: the four prototypes these
    /// programs index by, compared with what the kernel says they are.
    #[test]
    fn the_running_kernel_declares_the_tracepoints_these_programs_attach_to() {
        let Some(btf) = kernel_btf() else { return };
        for stall in [Stall::Reclaim, Stall::Compaction] {
            for site in stall.begins().iter().chain(stall.ends()) {
                let found = btf.tracepoint_signature(site.name);
                if !site.required && found.is_err() {
                    eprintln!(
                        "this kernel publishes no `{}`, so a run here cannot see that half",
                        site.name
                    );
                    continue;
                }
                let found = found.unwrap_or_else(|error| panic!("{}: {error}", site.name));
                assert!(
                    site.signatures.iter().any(|shape| found == *shape),
                    "this kernel's `{}({})` is none of the prototypes these programs read",
                    site.name,
                    found.join(", ")
                );
            }
        }
    }

    /// The probe answers from the catalog's line and the attach from the
    /// program. A required site's line lists exactly the program's shapes; an
    /// optional one may ask for existence alone, because its absence is
    /// reported by the run rather than refused.
    #[test]
    fn the_catalog_lists_every_shape_the_programs_read() {
        for (stall, source) in [
            (Stall::Reclaim, "mem.reclaim"),
            (Stall::Compaction, "mem.compaction"),
        ] {
            for site in stall.begins().iter().chain(stall.ends()) {
                if !site.required
                    && crate::bpf::mem_program::catalog_prototypes(source, site.name).is_empty()
                {
                    continue;
                }
                crate::bpf::mem_program::assert_catalog_lists(source, site.name, site.signatures);
            }
        }
    }

    /// The one argument either program reads is reclaim's pages reclaimed, in
    /// slot zero of `end`, and it is that in every shape the program accepts.
    #[test]
    fn what_the_end_program_reads_is_in_the_same_slot_in_every_shape() {
        let Some((at, _)) = Stall::Reclaim.yields() else {
            panic!("reclaim yields its pages");
        };
        for site in Stall::Reclaim.ends() {
            for shape in site.signatures {
                assert_eq!(shape[(at / 8) as usize], "long unsigned int", "{shape:?}");
            }
        }
        assert!(
            Stall::Compaction.yields().is_none(),
            "compaction reads no argument"
        );
    }

    /// A kernel that publishes the beginning and not the ending cannot serve
    /// this source at all, and the refusal has to say which half is missing.
    ///
    /// It is the case the catalog entry does not cover: `mem.compaction`'s
    /// chain requires `mm_compaction_begin` and nothing else, so the resolver
    /// would call the backend available on a kernel where the duration cannot
    /// be measured. The request for that line is in `docs/families/mem.md`.
    #[test]
    fn a_missing_end_tracepoint_is_refused_by_name() {
        let Some(btf) = kernel_btf() else { return };
        let error = check_signature(&btf, "definitely_not_a_tracepoint", &[], "nothing")
            .expect_err("a refusal");
        assert!(
            error.contains("definitely_not_a_tracepoint"),
            "the refusal names the tracepoint that is missing: {error}"
        );
    }

    /// Invariant 6, checked in the instruction stream rather than trusted:
    /// `bpf_ktime_get_boot_ns` includes suspend time, and a duration computed
    /// from one reading of each clock is not a duration at all.
    ///
    /// This one never skips. These programs read no field of any kernel
    /// structure -- a thread id and a timestamp both come from helpers -- so
    /// they are built without BTF and can be inspected on any machine.
    #[test]
    fn both_programs_read_the_monotonic_clock_and_no_other() {
        const KTIME_GET_BOOT_NS: i32 = 125;
        for stall in [Stall::Reclaim, Stall::Compaction] {
            for (what, instructions) in [
                (
                    "the begin program",
                    begin_instructions(None, 3, 4).expect("a program"),
                ),
                (
                    "the end program",
                    end_instructions(stall, SCOPE_GLOBAL, 3, 4, 5).expect("a program"),
                ),
            ] {
                let called: Vec<i32> = instructions
                    .iter()
                    .filter(|insn| insn.code == 0x85)
                    .map(|insn| insn.imm)
                    .collect();
                assert!(
                    called.contains(&helper::KTIME_GET_NS),
                    "{what} of {stall:?} takes no timestamp"
                );
                assert!(!called.contains(&KTIME_GET_BOOT_NS), "{what} of {stall:?}");
                assert_eq!(
                    called
                        .iter()
                        .filter(|id| **id == helper::KTIME_GET_NS)
                        .count(),
                    1,
                    "{what} of {stall:?} must stamp everything it does with one reading"
                );
            }
        }
    }

    /// The measurement is a difference between two timestamps. A row built from
    /// the absolute second one is the machine's uptime, and it looks like a
    /// number.
    #[test]
    fn the_duration_is_a_difference_and_not_a_timestamp() {
        let program = end_instructions(Stall::Reclaim, SCOPE_GLOBAL, 3, 4, 5).expect("a program");
        assert!(
            program.iter().any(|insn| *insn == Insn::sub64_reg(1, 7)),
            "the program must subtract the moment the thread entered from the moment it left"
        );
    }

    /// A machine-wide run has no filter in it at all; a targeted one filters
    /// where an episode begins, which is the only place it can.
    #[test]
    fn the_filter_is_present_only_where_an_episode_begins() {
        let plain = begin_instructions(None, 3, 4).expect("a program");
        assert!(!plain.iter().any(|insn| insn.imm == 4242));

        let filtered = begin_instructions(Some(4242), 3, 4).expect("a program");
        assert_eq!(
            filtered.iter().filter(|insn| insn.imm == 4242).count(),
            1,
            "where an episode ends the thread is already known, because only one this run \
             recorded can be found in the map"
        );
        // And the end program never carries it.
        let end = end_instructions(Stall::Reclaim, SCOPE_GLOBAL, 3, 4, 5).expect("a program");
        assert!(!end.iter().any(|insn| insn.imm == 4242));
    }

    /// Only the source whose `end` carries a quantity accumulates one, and it
    /// does so atomically like everything else in a shared row.
    #[test]
    fn the_yield_is_accumulated_only_where_the_tracepoint_carries_one() {
        let atomics = |stall: Stall| {
            end_instructions(stall, SCOPE_GLOBAL, 3, 4, 5)
                .expect("a program")
                .iter()
                .filter(|insn| insn.code == 0xdb)
                .count()
        };
        assert_eq!(
            atomics(Stall::Reclaim),
            3,
            "the count, the total duration and the pages reclaimed"
        );
        assert_eq!(
            atomics(Stall::Compaction),
            2,
            "the count and the total duration; compaction's end carries a status, not a quantity"
        );
    }

    /// An end with no beginning must be counted rather than turned into a
    /// duration measured from whatever was in the map slot.
    #[test]
    fn an_unpaired_end_is_counted_and_not_measured() {
        let program = end_instructions(Stall::Reclaim, SCOPE_GLOBAL, 3, 4, 5).expect("a program");
        let slots: Vec<i32> = program
            .iter()
            .filter(|insn| insn.code == 0x62 && insn.off == DROP_KEY)
            .map(|insn| insn.imm)
            .collect();
        assert!(
            slots.contains(&(DROP_UNPAIRED as i32)),
            "the end program must have a path that counts an unpaired ending: {slots:?}"
        );
        assert!(
            slots.contains(&(DROP_ROWS as i32)),
            "and one that counts a row the map had no room for: {slots:?}"
        );
    }

    #[test]
    fn every_byte_of_a_key_and_a_value_is_accounted_for() {
        let key = [
            (KEY_TID, 4),
            (KEY_TGID, 4),
            (KEY_CGROUP, 8),
            (KEY_BUCKET, 4),
            (KEY_SCOPE, 4),
        ];
        let value = [
            (VALUE_COUNT, 8),
            (VALUE_TOTAL_NS, 8),
            (VALUE_FIRST_NS, 8),
            (VALUE_YIELD, 8),
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

    #[test]
    fn the_stack_regions_do_not_overlap_and_stay_inside_the_frame() {
        let regions = [
            (ROW_VALUE, ROW_VALUE_BYTES as i16, "row value"),
            (ROW_KEY, ROW_KEY_BYTES as i16, "row key"),
            (PID_TGID, 8, "pid and tgid"),
            (START_KEY, 8, "in-flight key slot"),
            (START_VALUE, START_VALUE_BYTES as i16, "in-flight value"),
            (DROP_KEY, 4, "drop key"),
            (ROWS_SLOT, 8, "row map slot"),
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
        assert!(StallRow::decode(&[0u8; 8], &[0u8; ROW_VALUE_BYTES]).is_none());
        assert!(StallRow::decode(&[0u8; ROW_KEY_BYTES], &[0u8; 8]).is_none());
    }

    #[test]
    fn a_rows_fields_survive_the_round_trip() {
        let mut key = [0u8; ROW_KEY_BYTES];
        key[KEY_TID as usize..KEY_TID as usize + 4].copy_from_slice(&77u32.to_ne_bytes());
        key[KEY_BUCKET as usize..KEY_BUCKET as usize + 4].copy_from_slice(&21u32.to_ne_bytes());
        key[KEY_SCOPE as usize..KEY_SCOPE as usize + 4]
            .copy_from_slice(&SCOPE_CGROUP.to_ne_bytes());
        let mut value = [0u8; ROW_VALUE_BYTES];
        value[VALUE_COUNT as usize..VALUE_COUNT as usize + 8].copy_from_slice(&5u64.to_ne_bytes());
        value[VALUE_TOTAL_NS as usize..VALUE_TOTAL_NS as usize + 8]
            .copy_from_slice(&7_000_000u64.to_ne_bytes());
        value[VALUE_YIELD as usize..VALUE_YIELD as usize + 8]
            .copy_from_slice(&1234u64.to_ne_bytes());
        value[VALUE_COMM as usize..VALUE_COMM as usize + 4].copy_from_slice(b"hog\0");
        let row = StallRow::decode(&key, &value).expect("a full record");
        assert_eq!(row.tid, 77);
        assert_eq!(row.scope, SCOPE_CGROUP);
        assert_eq!(row.count, 5);
        assert_eq!(row.total_ns, 7_000_000);
        assert_eq!(row.yielded, 1234);
        assert_eq!(row.comm, "hog");
        // Bucket 21 starts at 2^20 nanoseconds, just over a millisecond.
        assert_eq!(row.duration_ns(), 1 << 20);
    }

    /// The row map is found through the generation slot once per run, and the
    /// lookup and the insert both use what it found -- the property that makes
    /// the drain's swap safe; see `map::Rows`.
    #[test]
    fn the_row_map_is_taken_from_the_slot_once_per_run() {
        let program = end_instructions(Stall::Reclaim, SCOPE_GLOBAL, 3, 4, 5).expect("a program");
        crate::bpf::map::assert_rows_taken_once(&program, 4);
    }
}
