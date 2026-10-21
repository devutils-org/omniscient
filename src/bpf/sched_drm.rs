// SPDX-License-Identifier: MIT OR GPL-2.0
//! The DRM scheduler's `drm_run_job` / `drm_sched_process_job` pair: how long a
//! command buffer spent on the GPU.
//!
//! It lives under the `sched_` prefix because it is a scheduler -- the DRM
//! one -- and because that prefix is this wave's unit of ownership. What it
//! measures belongs to the `gpu` family, and `collect/gpu/job.rs` is the other
//! half.
//!
//! **This is the only program in this build on the classic tracepoint path**,
//! and it has no choice: `drm_run_job` and `drm_sched_process_job` are defined
//! in the `gpu_scheduler` module rather than in the kernel image, so they carry
//! no `btf_trace_` type in `vmlinux` BTF and `BPF_RAW_TRACEPOINT_OPEN` cannot
//! find them by name. The classic path reads a numeric id and a field layout
//! out of `/sys/kernel/tracing`, which is what the catalog entry's single
//! `bpf:tp` backend says.
//!
//! **The field layout is discovered, never assumed.** A classic tracepoint's
//! record is a structure the kernel lays out itself, and its fields move
//! between versions: `drm_sched_process_job` carried a `struct dma_fence *`
//! until the DRM scheduler was changed to publish the fence's context and
//! sequence number instead. The catalog entry's own caveat says to prefer the
//! pair over the pointer, and this refuses a kernel that offers only the
//! pointer rather than using it -- a fence pointer is recycled as soon as the
//! fence is freed, so a completion would be attributed to whichever later job
//! landed on the same address. It is the same argument the catalog makes for
//! scoping block requests by `(ptr, open_ts)`.
//!
//! **It is written here and exercised elsewhere.** The machine this repository
//! is developed on runs the NVIDIA proprietary driver, which has no DRM
//! scheduler and publishes neither tracepoint, and its tracefs is not readable
//! by the uid the tool runs as. So nothing below has ever attached. What is
//! checked here is everything that can be: the format parser against the text
//! real kernels write, the refusal against a layout that would be read wrong,
//! and the instruction stream's own shape. `docs/families/gpu.md` says which
//! is which, and says it in those words.

use super::attach::{self, PerfAttachment, Scope};
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash};
use super::sched_common::{count_update_failure, text_of, BPF_NOEXIST, E2BIG};

/// The tracepoints, by the category and name tracefs files them under.
pub const CATEGORY: &str = "gpu_scheduler";
pub const RUN_JOB: &str = "drm_run_job";
pub const PROCESS_JOB: &str = "drm_sched_process_job";

/// How many jobs may be on the GPU at once before the program stops being able
/// to record new ones.
const MAX_IN_FLIGHT: u32 = 16384;

/// How many finished jobs one drain interval may hold.
///
/// Every finished job is its own row here rather than a bucket, because the
/// catalog gives this source a join key of `gpu:fence_ctx+seqno` and an
/// aggregate has no sequence number to join on. A GPU submits thousands of
/// command buffers a second, not the hundreds of thousands a lock tracepoint
/// sees, so one row each is affordable where for `sched.lock` it would not be.
const MAX_DONE: u32 = 16384;

/// Slots of the counter.
const DROP_IN_FLIGHT_FULL: u32 = 0;
const DROP_IN_FLIGHT_BUSY: u32 = 1;
const DROP_DONE_FULL: u32 = 2;
const DROP_DONE_BUSY: u32 = 3;
const DROP_BACKWARDS: u32 = 4;
const COUNTER_SLOTS: u32 = 5;

/// Where the maps' keys and values live on the program's stack.
const VALUE: i16 = -64;
const KEY: i16 = -32;
const DROP_KEY: i16 = -16;

/// The layout of a key: the fence's identity, which is what the catalog's
/// `gpu:fence_ctx+seqno` join key names.
const KEY_FENCE_CONTEXT: i16 = 0;
const KEY_FENCE_SEQNO: i16 = 8;
const KEY_BYTES: usize = 16;

/// The layout of a value. It carries a timestamp while the job is in flight
/// and the job's duration once it is not, in the same slot: they are never
/// both wanted, and a second eight bytes per job would buy nothing.
const VALUE_TIME_NS: i16 = 0;
const VALUE_TID: i16 = 8;
const VALUE_TGID: i16 = 12;
const VALUE_COMM: i16 = 16;
const COMM_BYTES: usize = 16;
const VALUE_BYTES: usize = 32;

/// One finished job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuJob {
    /// The fence's context and sequence number, which together identify one
    /// submission across both tracepoints and are the catalog's join key.
    pub fence_context: u64,
    pub fence_seqno: u64,
    pub duration_ns: u64,
    /// The thread that ran the job through the scheduler. On the drivers that
    /// have a DRM scheduler this is a kernel thread named after the ring, not
    /// the process that submitted the work: `drm_run_job` fires in the
    /// scheduler's own context, and the submitter is not in it.
    pub tid: u32,
    pub tgid: u32,
    pub comm: String,
}

impl GpuJob {
    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < KEY_BYTES || value.len() < VALUE_BYTES {
            return None;
        }
        let word =
            |bytes: &[u8], at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long =
            |bytes: &[u8], at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        Some(Self {
            fence_context: long(key, KEY_FENCE_CONTEXT as usize),
            fence_seqno: long(key, KEY_FENCE_SEQNO as usize),
            duration_ns: long(value, VALUE_TIME_NS as usize),
            tid: word(value, VALUE_TID as usize),
            tgid: word(value, VALUE_TGID as usize),
            comm: text_of(&value[VALUE_COMM as usize..VALUE_COMM as usize + COMM_BYTES]),
        })
    }
}

// ------------------------------------------------------------ the format file

// Read by `attach::tracepoint_layout` since the seventh wave. This file carried
// the second copy of the two tracefs mount paths and of the `format` parser,
// and `gpu.job` is the only collector here on the classic tracepoint path --
// so the next one to take that path would have written a third.
pub use super::attach::{tracepoint_layout as layout_of, Layout};

/// The offset of a field that must be a plain eight-byte value.
///
/// Every failure here is a kernel whose record this program would read wrong,
/// and each says which of the three things it is: the field is absent, it is
/// the wrong width, or it is a `__data_loc` whose contents are somewhere else
/// in the record entirely.
fn fixed_u64(layout: &Layout, event: &str, field: &str) -> Result<i16, String> {
    match layout.get(field) {
        Some(found) if found.dynamic => Err(format!(
            "this kernel's `{event}` publishes `{field}` as a `__data_loc`, which holds a packed \
             offset and length rather than a value; reading it as a number would produce one"
        )),
        Some(found) if found.size != 8 => Err(format!(
            "this kernel's `{event}` publishes `{field}` as {} bytes, and this program reads \
             eight",
            found.size
        )),
        Some(found) => Ok(found.offset),
        None => Err(format!(
            "this kernel's `{event}` has no `{field}` field. Older kernels identify a job by its \
             `fence` pointer instead, and this build will not use one: a fence pointer is \
             recycled as soon as the fence is freed, so a completion would be attributed to \
             whichever later job landed on the same address. The DRM scheduler publishes the \
             fence's context and sequence number from Linux 6.11"
        )),
    }
}

/// Where a job's identity lives in each record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fences {
    pub run_context: i16,
    pub run_seqno: i16,
    pub done_context: i16,
    pub done_seqno: i16,
}

impl Fences {
    /// Reads both layouts and checks that this program can read them.
    pub fn discover() -> Result<Self, String> {
        Self::from_layouts(
            &layout_of(CATEGORY, RUN_JOB)?,
            &layout_of(CATEGORY, PROCESS_JOB)?,
        )
    }

    pub fn from_layouts(run: &Layout, done: &Layout) -> Result<Self, String> {
        Ok(Self {
            run_context: fixed_u64(run, RUN_JOB, "fence_context")?,
            run_seqno: fixed_u64(run, RUN_JOB, "fence_seqno")?,
            done_context: fixed_u64(done, PROCESS_JOB, "fence_context")?,
            done_seqno: fixed_u64(done, PROCESS_JOB, "fence_seqno")?,
        })
    }
}

// --------------------------------------------------------------- the programs

/// Writes the fence's identity from the record into the key on the stack.
///
/// The record *is* the context of a classic tracepoint program, so these are
/// plain loads at offsets the format file gave: no `bpf_probe_read`, because
/// there is no other structure to follow.
fn read_fence(program: &mut Program, context_at: i16, seqno_at: i16) {
    program.push(Insn::ldx(BPF_DW, 1, 6, context_at));
    program.push(Insn::stx(BPF_DW, R10, KEY + KEY_FENCE_CONTEXT, 1));
    program.push(Insn::ldx(BPF_DW, 1, 6, seqno_at));
    program.push(Insn::stx(BPF_DW, R10, KEY + KEY_FENCE_SEQNO, 1));
}

/// The program on `drm_run_job`: the scheduler has handed a command buffer to
/// the hardware.
fn run_instructions(fences: Fences, in_flight_fd: i32, drops_fd: i32) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    read_fence(&mut program, fences.run_context, fences.run_seqno);

    // Who ran the job. On a DRM scheduler this is a kernel thread named after
    // the ring, not the process that submitted the work: this tracepoint fires
    // in the scheduler's own context and the submitter is not in it. It is
    // recorded because it is the only name of a ring this build can reach --
    // the tracepoint's own `name` is a `__data_loc` string, which needs a
    // variable-offset read this layer does not have.
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, VALUE + VALUE_TID, 0));
    program.push(Insn::mov64_reg(1, 0));
    program.push(Insn::rsh64_imm(1, 32));
    program.push(Insn::stx(BPF_W, R10, VALUE + VALUE_TGID, 1));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (VALUE + VALUE_COMM) as i32));
    program.push(Insn::mov64_imm(2, COMM_BYTES as i32));
    program.push(Insn::call(helper::GET_CURRENT_COMM));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, VALUE + VALUE_TIME_NS, 0));

    program.extend(Insn::load_map_fd(1, in_flight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, VALUE as i32));
    // `BPF_NOEXIST`: a fence's `(context, seqno)` is issued once and never
    // reused, so an entry already there means the pair is not the identity
    // this program believes it is. Counting that beats overwriting.
    program.push(Insn::mov64_imm(4, BPF_NOEXIST));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_update_failure(
        &mut program,
        drops_fd,
        DROP_KEY,
        &[(E2BIG, DROP_IN_FLIGHT_FULL, "in_flight_not_full")],
        DROP_IN_FLIGHT_BUSY,
        "done",
    );

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `drm_sched_process_job`: the fence has signalled, so the
/// hardware is done with that command buffer.
fn done_instructions(
    fences: Fences,
    in_flight_fd: i32,
    done_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    // The clock first, so nothing this program does is counted as part of the
    // job it is closing.
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));
    read_fence(&mut program, fences.done_context, fences.done_seqno);

    program.extend(Insn::load_map_fd(1, in_flight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // A fence this run never saw submitted: the job was already on the GPU
    // when the programs attached.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    // The whole record is copied out before the entry is deleted, because
    // deleting invalidates the pointer the lookup returned.
    program.push(Insn::ldx(BPF_DW, 9, 0, VALUE_TIME_NS));
    program.push(Insn::ldx(BPF_W, 1, 0, VALUE_TID));
    program.push(Insn::stx(BPF_W, R10, VALUE + VALUE_TID, 1));
    program.push(Insn::ldx(BPF_W, 1, 0, VALUE_TGID));
    program.push(Insn::stx(BPF_W, R10, VALUE + VALUE_TGID, 1));
    for offset in (0..COMM_BYTES as i16).step_by(8) {
        program.push(Insn::ldx(BPF_DW, 1, 0, VALUE_COMM + offset));
        program.push(Insn::stx(BPF_DW, R10, VALUE + VALUE_COMM + offset, 1));
    }

    program.extend(Insn::load_map_fd(1, in_flight_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    // r9 = now - when the job was handed to the hardware.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::mov64_reg(9, 1));
    program.jump_to("forwards", Insn::jsge_imm(9, 0, 0));
    super::sched_common::count_drop(&mut program, drops_fd, DROP_KEY, DROP_BACKWARDS, "done");
    program.jump_to("done", Insn::ja());
    program.label("forwards");
    program.push(Insn::stx(BPF_DW, R10, VALUE + VALUE_TIME_NS, 9));

    program.extend(Insn::load_map_fd(1, done_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_update_failure(
        &mut program,
        drops_fd,
        DROP_KEY,
        &[(E2BIG, DROP_DONE_FULL, "done_not_full")],
        DROP_DONE_BUSY,
        "done",
    );

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Jobs neither map could take, under the kernel's own reason.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GpuJobDrops {
    pub in_flight_full: u64,
    pub in_flight_busy: u64,
    pub done_full: u64,
    pub done_busy: u64,
    pub backwards: u64,
}

/// Two loaded, attached programs and the maps they share.
pub struct GpuJobProgram {
    in_flight: Hash,
    done: Hash,
    drops: Counters,
    run: PerfAttachment,
    done_attachment: PerfAttachment,
}

impl GpuJobProgram {
    /// Runs the kernel skipped because one of the two programs was already
    /// running on that cpu; see `WakeupProgram::skipped`.
    pub fn skipped(&self) -> u64 {
        crate::bpf::program::skipped([self.run.program(), self.done_attachment.program()])
    }

    pub fn load() -> Result<Self, String> {
        let fences = Fences::discover()?;

        let in_flight = Hash::create(KEY_BYTES, VALUE_BYTES, MAX_IN_FLIGHT, "omn_drm_open")
            .map_err(|error| map_failure("GPU job in-flight map", error))?;
        let done = Hash::create(KEY_BYTES, VALUE_BYTES, MAX_DONE, "omn_drm_done")
            .map_err(|error| map_failure("GPU job map", error))?;
        let drops = Counters::create(COUNTER_SLOTS, "omn_drm_drop")
            .map_err(|error| map_failure("GPU job drop counter", error))?;

        let run_program = run_instructions(fences, in_flight.as_raw_fd(), drops.as_raw_fd())?;
        let done_program = done_instructions(
            fences,
            in_flight.as_raw_fd(),
            done.as_raw_fd(),
            drops.as_raw_fd(),
        )?;

        // Submissions before completions: a job recorded with no completion
        // ages out of the map, whereas a completion seen before any submission
        // was recorded would be a duration measured from nothing.
        let run = attach::tracepoint(
            CATEGORY,
            RUN_JOB,
            &run_program,
            "omn_drm_run",
            Scope::System,
        )?;
        let done_attachment = attach::tracepoint(
            CATEGORY,
            PROCESS_JOB,
            &done_program,
            "omn_drm_done",
            Scope::System,
        )?;

        Ok(Self {
            in_flight,
            done,
            drops,
            run,
            done_attachment,
        })
    }

    /// Takes every job that finished since the last drain.
    pub fn drain(&self) -> Result<(Vec<GpuJob>, super::DrainLosses), String> {
        let drained = self
            .done
            .drain()
            .map_err(|error| format!("cannot read the finished GPU jobs: {error}"))?;
        let mut losses = super::DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match GpuJob::decode(key, value) {
                Some(row) => rows.push(row),
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    /// How many command buffers were still on the GPU when the run ended.
    /// Their time is real and is in no total, because it has no end yet.
    pub fn still_running(&self) -> usize {
        self.in_flight.len().unwrap_or(0)
    }

    pub fn dropped(&self) -> GpuJobDrops {
        GpuJobDrops {
            in_flight_full: self.drops.read(DROP_IN_FLIGHT_FULL),
            in_flight_busy: self.drops.read(DROP_IN_FLIGHT_BUSY),
            done_full: self.drops.read(DROP_DONE_FULL),
            done_busy: self.drops.read(DROP_DONE_BUSY),
            backwards: self.drops.read(DROP_BACKWARDS),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::attach::{parse_layout, Field};
    use super::*;

    /// The `format` file of `drm_run_job` as Linux 6.12 writes it. Kept
    /// verbatim, because the point of the parser is to survive exactly this
    /// text and there is no machine here that can produce it.
    const RUN_JOB_FORMAT: &str = "\
name: drm_run_job
ID: 1189
format:
\tfield:unsigned short common_type;\toffset:0;\tsize:2;\tsigned:0;
\tfield:unsigned char common_flags;\toffset:2;\tsize:1;\tsigned:0;
\tfield:unsigned char common_preempt_count;\toffset:3;\tsize:1;\tsigned:0;
\tfield:int common_pid;\toffset:4;\tsize:4;\tsigned:1;

\tfield:struct drm_sched_entity * entity;\toffset:8;\tsize:8;\tsigned:0;
\tfield:u64 id;\toffset:16;\tsize:8;\tsigned:0;
\tfield:__data_loc char[] name;\toffset:24;\tsize:4;\tsigned:0;
\tfield:u32 job_count;\toffset:28;\tsize:4;\tsigned:0;
\tfield:int hw_job_count;\toffset:32;\tsize:4;\tsigned:1;
\tfield:__data_loc char[] dev;\toffset:36;\tsize:4;\tsigned:0;
\tfield:u64 fence_context;\toffset:40;\tsize:8;\tsigned:0;
\tfield:u64 fence_seqno;\toffset:48;\tsize:8;\tsigned:0;

print fmt: \"entity=%p, id=%llu, fence=%llu:%llu, ring=%s, job count:%u, hw job count:%d\"
";

    /// `drm_sched_process_job` on the same kernel.
    const PROCESS_JOB_FORMAT: &str = "\
name: drm_sched_process_job
ID: 1190
format:
\tfield:unsigned short common_type;\toffset:0;\tsize:2;\tsigned:0;
\tfield:unsigned char common_flags;\toffset:2;\tsize:1;\tsigned:0;
\tfield:unsigned char common_preempt_count;\toffset:3;\tsize:1;\tsigned:0;
\tfield:int common_pid;\toffset:4;\tsize:4;\tsigned:1;

\tfield:u64 fence_context;\toffset:8;\tsize:8;\tsigned:0;
\tfield:u64 fence_seqno;\toffset:16;\tsize:8;\tsigned:0;

print fmt: \"fence=%llu:%llu signaled\"
";

    /// The same tracepoint on a kernel before the DRM scheduler published the
    /// fence's identity: one pointer, which is recycled.
    const OLD_PROCESS_JOB_FORMAT: &str = "\
name: drm_sched_process_job
ID: 1190
format:
\tfield:int common_pid;\toffset:4;\tsize:4;\tsigned:1;

\tfield:struct dma_fence * fence;\toffset:8;\tsize:8;\tsigned:0;

print fmt: \"fence=%p signaled\"
";

    #[test]
    fn a_format_file_is_read_the_way_the_kernel_writes_it() {
        let layout = parse_layout(RUN_JOB_FORMAT);
        assert_eq!(
            layout.get("fence_context"),
            Some(&Field {
                offset: 40,
                size: 8,
                dynamic: false
            })
        );
        assert_eq!(
            layout.get("fence_seqno"),
            Some(&Field {
                offset: 48,
                size: 8,
                dynamic: false
            })
        );
        assert_eq!(
            layout.get("hw_job_count"),
            Some(&Field {
                offset: 32,
                size: 4,
                dynamic: false
            })
        );
        // The declaration is C, so the name is its last identifier and the
        // array brackets belong to the type.
        assert_eq!(
            layout.get("name"),
            Some(&Field {
                offset: 24,
                size: 4,
                dynamic: true
            })
        );
        assert!(layout.contains_key("entity"));
        assert!(layout.contains_key("common_pid"));
    }

    /// The offsets this program reads come out of the file rather than out of
    /// this file, which is the whole reason for the parser: the DRM scheduler
    /// has moved these fields twice.
    #[test]
    fn the_fence_offsets_come_from_the_kernels_own_layout() {
        let fences = Fences::from_layouts(
            &parse_layout(RUN_JOB_FORMAT),
            &parse_layout(PROCESS_JOB_FORMAT),
        )
        .expect("a layout this program can read");
        assert_eq!(fences.run_context, 40);
        assert_eq!(fences.run_seqno, 48);
        // The two records are laid out differently, which is exactly why they
        // are discovered separately rather than assumed to match.
        assert_eq!(fences.done_context, 8);
        assert_eq!(fences.done_seqno, 16);
    }

    /// A fence pointer is recycled as soon as the fence is freed, so a
    /// completion keyed by one would be attributed to whichever later job
    /// landed on the same address. The refusal has to name that rather than
    /// falling back to it.
    #[test]
    fn a_kernel_that_publishes_only_the_fence_pointer_is_refused_by_name() {
        let Err(error) = Fences::from_layouts(
            &parse_layout(RUN_JOB_FORMAT),
            &parse_layout(OLD_PROCESS_JOB_FORMAT),
        ) else {
            panic!("a layout without the fence's identity must be refused");
        };
        assert!(error.contains("fence_context"), "{error}");
        assert!(error.contains("recycled"), "{error}");
        assert!(error.contains("6.11"), "{error}");
    }

    /// A field that is there and is the wrong shape is a different failure
    /// from one that is absent, and reading it would produce a number rather
    /// than an error.
    #[test]
    fn a_field_of_the_wrong_shape_is_refused_with_which_shape_it_is() {
        let narrow = parse_layout(
            "\tfield:u32 fence_context;\toffset:8;\tsize:4;\tsigned:0;\n\
             \tfield:u64 fence_seqno;\toffset:16;\tsize:8;\tsigned:0;\n",
        );
        let error = Fences::from_layouts(&parse_layout(RUN_JOB_FORMAT), &narrow)
            .expect_err("four bytes is not eight");
        assert!(error.contains("4 bytes"), "{error}");

        let packed = parse_layout(
            "\tfield:__data_loc char[] fence_context;\toffset:8;\tsize:4;\tsigned:0;\n\
             \tfield:u64 fence_seqno;\toffset:16;\tsize:8;\tsigned:0;\n",
        );
        let error = Fences::from_layouts(&parse_layout(RUN_JOB_FORMAT), &packed)
            .expect_err("a packed offset is not a value");
        assert!(error.contains("__data_loc"), "{error}");
    }

    fn fences() -> Fences {
        Fences::from_layouts(
            &parse_layout(RUN_JOB_FORMAT),
            &parse_layout(PROCESS_JOB_FORMAT),
        )
        .expect("a layout")
    }

    /// The record is the context of a classic tracepoint program, so its
    /// fields are plain loads. A `bpf_probe_read_kernel` here would mean this
    /// program had confused the record with a structure it points at.
    #[test]
    fn the_record_is_read_directly_rather_than_probed() {
        for program in [
            run_instructions(fences(), 3, 4).expect("a program"),
            done_instructions(fences(), 3, 4, 5).expect("a program"),
        ] {
            assert!(
                !program
                    .iter()
                    .any(|insn| insn.code == 0x85 && insn.imm == helper::PROBE_READ_KERNEL),
                "a record field is read through a probe"
            );
        }
    }

    /// Both programs have to read the identity out of *their own* record's
    /// offsets. The two records are laid out differently, and a program using
    /// the other one's offsets would key completions by whatever is at 40.
    #[test]
    fn each_program_uses_its_own_records_offsets() {
        let fences = fences();
        let run = run_instructions(fences, 3, 4).expect("a program");
        let done = done_instructions(fences, 3, 4, 5).expect("a program");
        let reads_from_context = |program: &[Insn], offset: i16| {
            program
                .iter()
                .any(|insn| insn.code == 0x79 && (insn.regs >> 4) == 6 && insn.off == offset)
        };
        assert!(reads_from_context(&run, fences.run_context));
        assert!(reads_from_context(&run, fences.run_seqno));
        assert!(!reads_from_context(&run, fences.done_context));
        assert!(reads_from_context(&done, fences.done_context));
        assert!(reads_from_context(&done, fences.done_seqno));
        assert!(!reads_from_context(&done, fences.run_context));
    }

    #[test]
    fn both_programs_read_clock_monotonic_and_nothing_else() {
        const KTIME_GET_BOOT_NS: i32 = 125;
        for program in [
            run_instructions(fences(), 3, 4).expect("a program"),
            done_instructions(fences(), 3, 4, 5).expect("a program"),
        ] {
            let called: Vec<i32> = program
                .iter()
                .filter(|insn| insn.code == 0x85)
                .map(|insn| insn.imm)
                .collect();
            assert!(called.contains(&helper::KTIME_GET_NS));
            assert!(!called.contains(&KTIME_GET_BOOT_NS));
        }
    }

    #[test]
    fn every_byte_of_the_key_and_value_is_accounted_for() {
        let key = [(KEY_FENCE_CONTEXT, 8), (KEY_FENCE_SEQNO, 8)];
        let value = [
            (VALUE_TIME_NS, 8),
            (VALUE_TID, 4),
            (VALUE_TGID, 4),
            (VALUE_COMM, COMM_BYTES as i16),
        ];
        for (fields, bytes, what) in [
            (&key[..], KEY_BYTES, "the key"),
            (&value[..], VALUE_BYTES, "the value"),
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
            (VALUE, VALUE_BYTES as i16, "value"),
            (KEY, KEY_BYTES as i16, "key"),
            (DROP_KEY, 4, "drop key"),
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
        assert!(GpuJob::decode(&[0u8; 8], &[0u8; VALUE_BYTES]).is_none());
        assert!(GpuJob::decode(&[0u8; KEY_BYTES], &[0u8; 8]).is_none());
    }

    #[test]
    fn a_jobs_identity_survives_the_round_trip() {
        let mut key = [0u8; KEY_BYTES];
        key[..8].copy_from_slice(&77u64.to_ne_bytes());
        key[8..16].copy_from_slice(&123_456u64.to_ne_bytes());
        let mut value = [0u8; VALUE_BYTES];
        value[..8].copy_from_slice(&2_500_000u64.to_ne_bytes());
        value[VALUE_TID as usize..VALUE_TID as usize + 4].copy_from_slice(&931u32.to_ne_bytes());
        value[VALUE_COMM as usize..VALUE_COMM as usize + 5].copy_from_slice(b"gfx_0");
        let job = GpuJob::decode(&key, &value).expect("a full record");
        assert_eq!(
            (
                job.fence_context,
                job.fence_seqno,
                job.duration_ns,
                job.tid,
                job.comm.as_str()
            ),
            (77, 123_456, 2_500_000, 931, "gfx_0")
        );
    }
}
