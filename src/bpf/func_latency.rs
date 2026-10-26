// SPDX-License-Identifier: MIT OR GPL-2.0
//! `runtime.func`: how long each call of a named function took, through a
//! uprobe on its entry and one on its return (bcc's `funclatency`).
//!
//! A profile says how often a function was on a cpu; it says nothing of how
//! long one call lasted, and a call that waited off the cpu is invisible to
//! it. Timing the call is another signal, with costs and losses of its own,
//! and this is it.
//!
//! **Per thread and per depth.** A call is stamped on its entry under `(thread,
//! function, depth)` and closed on its return at the same depth, so a recursive
//! function's calls each get their own time -- a nested call's time is inside
//! its caller's, and the record says its depth so the two are not added. The
//! depth is the thread's own count of open calls of that function: a return
//! the kernel never ran (a `longjmp`, an exception unwound past it) leaves the
//! count one high, which shifts the next calls one level and keeps them
//! paired, and leaves a stamp nothing closes, which the run counts.
//!
//! **What a call costs** is two traps into the kernel, one at the entry and one
//! at the return: a few microseconds, which a call shorter than that is mostly
//! made of. The run says how many calls it timed.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;

use super::attach::{self, PerfAttachment, Scope};
use super::btf::Btf;
use super::insn::{count_drop, helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash};
use super::syscall;
use super::transport::{Output, Transport};
use crate::bpf::program::LICENSE;

/// How deep a thread's calls of one function may nest before a call is
/// counted rather than timed.
pub const MAX_DEPTH: u64 = 64;

/// Calls open at once, across every thread and depth.
const MAX_OPEN: u32 = 65536;

/// The drop counter's slots.
const DROP_RING: u32 = 0;
const DROP_OPEN: u32 = 1;
const UNPAIRED: u32 = 2;
const STAMP_LOST: u32 = 3;
const TOO_DEEP: u32 = 4;
const DROP_SLOTS: u32 = 5;

/// The stack: the depth map's key and value, the stamp's, the drop key, and
/// the record.
const DEPTH_KEY: i16 = -8;
const DEPTH_VALUE: i16 = -16;
const STAMP_KEY: i16 = -32;
const STAMP_KEY_BYTES: usize = 16;
const STAMP_VALUE: i16 = -40;
const DROP_KEY: i16 = -48;
const RECORD: i16 = -112;

/// The record.
const R_START: i16 = 0;
const R_DURATION: i16 = 8;
const R_RET: i16 = 16;
const R_TID: i16 = 24;
const R_TGID: i16 = 28;
const R_FUNCTION: i16 = 32;
const R_DEPTH: i16 = 36;
const R_COMM: i16 = 40;
const RECORD_BYTES: usize = 56;

/// One call, timed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// When it began, on `CLOCK_MONOTONIC`.
    pub start_ns: u64,
    pub duration_ns: u64,
    /// The return register as the call left it.
    pub ret: u64,
    pub tid: u32,
    pub tgid: u32,
    /// Its index among the functions the run placed probes on.
    pub function: u32,
    /// 1 for an outermost call, 2 for a call inside one of the same function.
    pub depth: u32,
    pub comm: String,
}

impl Call {
    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let long = |at: i16| u64::from_ne_bytes(bytes[at as usize..][..8].try_into().expect("8"));
        let word = |at: i16| u32::from_ne_bytes(bytes[at as usize..][..4].try_into().expect("4"));
        let comm = &bytes[R_COMM as usize..][..16];
        let end = comm.iter().position(|byte| *byte == 0).unwrap_or(16);
        Some(Self {
            start_ns: long(R_START),
            duration_ns: long(R_DURATION),
            ret: long(R_RET),
            tid: word(R_TID),
            tgid: word(R_TGID),
            function: word(R_FUNCTION),
            depth: word(R_DEPTH),
            comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
        })
    }
}

/// The calls of one process this program keeps, and the function it is on.
#[derive(Clone, Copy)]
struct Placed {
    tgid: u32,
    function: u32,
}

/// `r7 = pid_tgid`, and on to `done` unless it is the process's.
fn kept(program: &mut Program, tgid: u32) {
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::mov64_reg(7, 0));
    program.push(Insn::mov64_reg(1, 0));
    program.push(Insn::rsh64_imm(1, 32));
    program.jump_to("done", Insn::jne_imm(1, tgid as i32, 0));
}

/// The depth map's key, `{tid, function}`, from `r7`.
fn depth_key(program: &mut Program, function: u32) {
    program.push(Insn::stx(BPF_W, R10, DEPTH_KEY, 7));
    program.push(Insn::st_imm(BPF_W, R10, DEPTH_KEY + 4, function as i32));
}

/// The stamp's key, `{tid, function, depth}`, from `r7` and `r8`.
fn stamp_key(program: &mut Program, function: u32) {
    program.push(Insn::st_imm(BPF_DW, R10, STAMP_KEY, 0));
    program.push(Insn::st_imm(BPF_DW, R10, STAMP_KEY + 8, 0));
    program.push(Insn::stx(BPF_W, R10, STAMP_KEY, 7));
    program.push(Insn::st_imm(BPF_W, R10, STAMP_KEY + 4, function as i32));
    program.push(Insn::stx(BPF_W, R10, STAMP_KEY + 8, 8));
}

fn lookup(program: &mut Program, map_fd: i32, key: i16) {
    program.extend(Insn::load_map_fd(1, map_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, key as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
}

fn update(program: &mut Program, map_fd: i32, key: i16, value: i16) {
    program.extend(Insn::load_map_fd(1, map_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, key as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, value as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
}

fn delete(program: &mut Program, map_fd: i32, key: i16) {
    program.extend(Insn::load_map_fd(1, map_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, key as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
}

fn done(mut program: Program) -> Result<Vec<Insn>, String> {
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on the function's entry: one more open call on this thread,
/// stamped at its depth.
fn entry_instructions(
    placed: Placed,
    depths_fd: i32,
    stamps_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    kept(&mut program, placed.tgid);
    program.push(Insn::st_imm(BPF_DW, R10, DEPTH_KEY, 0));
    depth_key(&mut program, placed.function);
    // r8 = the depth this call opens at.
    program.push(Insn::mov64_imm(8, 0));
    lookup(&mut program, depths_fd, DEPTH_KEY);
    program.jump_to("first", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 8, 0, 0));
    program.label("first");
    program.push(Insn::add64_imm(8, 1));
    program.push(Insn::stx(BPF_DW, R10, DEPTH_VALUE, 8));
    update(&mut program, depths_fd, DEPTH_KEY, DEPTH_VALUE);
    program.jump_to("counted", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_OPEN, "done");
    program.jump_to("done", Insn::ja());
    program.label("counted");
    // Deeper than this, the call is counted and not timed; its depth is
    // still kept, so its return finds the count it left.
    program.push(Insn::mov64_imm(1, MAX_DEPTH as i32));
    program.jump_to("deep", Insn::jgt_reg(8, 1, 0));
    stamp_key(&mut program, placed.function);
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, STAMP_VALUE, 0));
    update(&mut program, stamps_fd, STAMP_KEY, STAMP_VALUE);
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_OPEN, "done");
    program.jump_to("done", Insn::ja());
    program.label("deep");
    count_drop(&mut program, drops_fd, DROP_KEY, TOO_DEEP, "done");
    done(program)
}

/// The program on the function's return: the call open at this thread's
/// depth is closed, timed and published.
fn return_instructions(
    placed: Placed,
    ret_at: i16,
    depths_fd: i32,
    stamps_fd: i32,
    output: Output,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    // The clock first: what the call cost is up to here, not the program's
    // own work after it.
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(9, 0));
    kept(&mut program, placed.tgid);
    program.push(Insn::st_imm(BPF_DW, R10, DEPTH_KEY, 0));
    depth_key(&mut program, placed.function);
    lookup(&mut program, depths_fd, DEPTH_KEY);
    program.jump_to("unpaired", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 8, 0, 0));
    program.jump_to("unpaired", Insn::jeq_imm(8, 0, 0));
    // One open call fewer, through the value the lookup handed back.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::add64_imm(1, -1));
    program.push(Insn::stx(BPF_DW, 0, 0, 1));
    program.jump_to("still_open", Insn::jne_imm(1, 0, 0));
    delete(&mut program, depths_fd, DEPTH_KEY);
    program.label("still_open");
    program.push(Insn::mov64_imm(1, MAX_DEPTH as i32));
    program.jump_to("done", Insn::jgt_reg(8, 1, 0));
    stamp_key(&mut program, placed.function);
    lookup(&mut program, stamps_fd, STAMP_KEY);
    program.jump_to("lost", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, 0));
    program.push(Insn::stx(BPF_DW, R10, RECORD + R_START, 1));
    program.push(Insn::mov64_reg(2, 9));
    program.push(Insn::sub64_reg(2, 1));
    program.push(Insn::stx(BPF_DW, R10, RECORD + R_DURATION, 2));
    delete(&mut program, stamps_fd, STAMP_KEY);
    program.push(Insn::ldx(BPF_DW, 1, 6, ret_at));
    program.push(Insn::stx(BPF_DW, R10, RECORD + R_RET, 1));
    program.push(Insn::stx(BPF_W, R10, RECORD + R_TID, 7));
    program.push(Insn::mov64_reg(1, 7));
    program.push(Insn::rsh64_imm(1, 32));
    program.push(Insn::stx(BPF_W, R10, RECORD + R_TGID, 1));
    program.push(Insn::st_imm(
        BPF_W,
        R10,
        RECORD + R_FUNCTION,
        placed.function as i32,
    ));
    program.push(Insn::stx(BPF_W, R10, RECORD + R_DEPTH, 8));
    program.push(Insn::st_imm(BPF_DW, R10, RECORD + R_COMM, 0));
    program.push(Insn::st_imm(BPF_DW, R10, RECORD + R_COMM + 8, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (RECORD + R_COMM) as i32));
    program.push(Insn::mov64_imm(2, 16));
    program.push(Insn::call(helper::GET_CURRENT_COMM));
    output.push(&mut program, 6, RECORD, RECORD_BYTES as i32);
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_RING, "done");
    program.jump_to("done", Insn::ja());
    // A return whose entry this run never saw: the call was under way when
    // the probes attached.
    program.label("unpaired");
    count_drop(&mut program, drops_fd, DROP_KEY, UNPAIRED, "done");
    program.jump_to("done", Insn::ja());
    // A depth with no stamp: the entry could not keep it (the map full).
    program.label("lost");
    count_drop(&mut program, drops_fd, DROP_KEY, STAMP_LOST, "done");
    done(program)
}

/// Where a probe goes: a function in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub object: PathBuf,
    pub offset: u64,
}

/// What keeps one probe attached, by whichever route attached it.
enum Attachment {
    Link { _link: OwnedFd, _program: OwnedFd },
    Perf(#[allow(dead_code)] PerfAttachment),
}

/// What the run could not time, cumulative.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Lost {
    /// Calls timed and never published: the ring was full.
    pub ring: u64,
    /// Calls never timed: the maps of open calls were full.
    pub open: u64,
    /// Returns whose entry the run did not see.
    pub unpaired: u64,
    /// Returns whose stamp was gone.
    pub stamp: u64,
    /// Calls nested deeper than [`MAX_DEPTH`], counted and not timed.
    pub deep: u64,
}

/// The programs on every function's entry and return, for one process.
pub struct FuncProgram {
    ring: Transport,
    drops: Counters,
    stamps: Hash,
    _depths: Hash,
    _attachments: Vec<Attachment>,
    /// Why the probes went through the `uprobe` PMU, when they did.
    pub fallback: Option<String>,
}

fn load_for_multi(instructions: &[Insn], name: &str) -> Result<OwnedFd, String> {
    syscall::prog_load_for(
        syscall::BPF_PROG_TYPE_KPROBE,
        syscall::BPF_TRACE_UPROBE_MULTI,
        instructions,
        name,
        LICENSE,
    )
    .map_err(|error| {
        if error.is_permission() {
            super::permission_advice("loading a function's uprobe program")
        } else {
            format!(
                "the kernel refused the `{name}` program ({}); the verifier said:\n{}",
                error.error, error.verifier_log
            )
        }
    })
}

fn link_multi(
    program: &OwnedFd,
    target: &Target,
    returning: bool,
    pid: i32,
) -> io::Result<OwnedFd> {
    if returning {
        syscall::link_uretprobe_multi(program.as_raw_fd(), &target.object, target.offset, pid)
    } else {
        syscall::link_uprobe_multi(
            program.as_raw_fd(),
            &target.object,
            target.offset,
            None,
            pid,
        )
    }
}

impl FuncProgram {
    /// Places an entry and a return probe on each of `targets`, kept to the
    /// process `pid`; the returns first, so no entry is stamped that nothing
    /// will close.
    pub fn load(targets: &[Target], pid: i32, requested_ring_bytes: usize) -> Result<Self, String> {
        let btf: &Btf = super::kernel_btf()?;
        let ret_at = btf.return_register()? as i16;
        let tgid = super::pidns::kernel_tgid(pid as u32)?;
        let ring = Transport::create(requested_ring_bytes, Some(R_START as usize), "omn_func")?;
        let drops = Counters::create(DROP_SLOTS, "omn_func_drop")
            .map_err(|error| map_failure("function drop counter", error))?;
        let depths = Hash::create(8, 8, MAX_OPEN, "omn_func_depth")
            .map_err(|error| map_failure("function depth map", error))?;
        let stamps = Hash::create(STAMP_KEY_BYTES, 8, MAX_OPEN, "omn_func_open")
            .map_err(|error| map_failure("function open-call map", error))?;

        let mut attachments = Vec::with_capacity(targets.len() * 2);
        let mut fallback: Option<String> = None;
        for returning in [true, false] {
            for (index, target) in targets.iter().enumerate() {
                let placed = Placed {
                    tgid,
                    function: index as u32,
                };
                let instructions = if returning {
                    return_instructions(
                        placed,
                        ret_at,
                        depths.as_raw_fd(),
                        stamps.as_raw_fd(),
                        ring.output(),
                        drops.as_raw_fd(),
                    )?
                } else {
                    entry_instructions(
                        placed,
                        depths.as_raw_fd(),
                        stamps.as_raw_fd(),
                        drops.as_raw_fd(),
                    )?
                };
                let name = if returning {
                    "omn_func_ret"
                } else {
                    "omn_func_entry"
                };
                let where_ = || format!("{}+{:#x}", target.object.display(), target.offset);
                if fallback.is_none() {
                    let program = load_for_multi(&instructions, name)
                        .map_err(|error| format!("{}: {error}", where_()))?;
                    match link_multi(&program, target, returning, pid) {
                        Ok(link) => {
                            attachments.push(Attachment::Link {
                                _link: link,
                                _program: program,
                            });
                            continue;
                        }
                        // Before 6.6 the kernel does not know the link, and
                        // says so on the first one.
                        Err(error)
                            if attachments.is_empty()
                                && error.raw_os_error() == Some(libc::EINVAL) =>
                        {
                            fallback = Some(format!(
                                "this kernel refused a uprobe-multi link ({error}), which Linux \
                                 6.6 introduced; the probes went through the `uprobe` PMU instead"
                            ));
                        }
                        Err(error) => {
                            return Err(format!(
                                "{}: the kernel refused to link the program ({error})",
                                where_()
                            ))
                        }
                    }
                }
                if let Err(pmu) = attach::uprobe_pmu_permitted() {
                    return Err(format!(
                        "{}: this kernel has no uprobe-multi link (Linux 6.6), and the uprobe PMU \
                         it falls back to {pmu}",
                        where_()
                    ));
                }
                let attachment = attach::uprobe(
                    &target.object,
                    target.offset,
                    returning,
                    &instructions,
                    name,
                    Scope::Task(pid),
                )
                .map_err(|error| format!("{}: {error}", where_()))?;
                attachments.push(Attachment::Perf(attachment));
            }
        }
        Ok(Self {
            ring,
            drops,
            stamps,
            _depths: depths,
            _attachments: attachments,
            fallback,
        })
    }

    /// Hands every timed call to `handle`, counting records this reader does
    /// not recognise rather than reinterpreting them.
    pub fn drain(&mut self, mut handle: impl FnMut(Call), undecoded: &mut u64) {
        self.ring.drain(|bytes| match Call::decode(bytes) {
            Some(call) => handle(call),
            None => *undecoded += 1,
        });
    }

    pub fn lost(&self) -> Lost {
        Lost {
            ring: self.drops.read(DROP_RING),
            open: self.drops.read(DROP_OPEN),
            unpaired: self.drops.read(UNPAIRED),
            stamp: self.drops.read(STAMP_LOST),
            deep: self.drops.read(TOO_DEEP),
        }
    }

    /// Calls entered and not yet returned: in flight now, or abandoned by a
    /// `longjmp` or an exception the return probe never saw.
    pub fn open(&self) -> usize {
        self.stamps.len().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed() -> Placed {
        Placed {
            tgid: 4242,
            function: 3,
        }
    }

    /// Both programs build, keep one process, and the return reads the
    /// return register at the offset it is given.
    #[test]
    fn the_programs_keep_one_process_and_read_the_return_register() {
        let entry = entry_instructions(placed(), 3, 4, 5).expect("an entry");
        let ret =
            return_instructions(placed(), 80, 3, 4, Output::PerfArray(6), 5).expect("a return");
        for program in [&entry, &ret] {
            assert!(
                program
                    .iter()
                    .any(|insn| insn.code == 0x55 && insn.imm == 4242),
                "the process is compared"
            );
        }
        assert!(
            ret.contains(&Insn::ldx(BPF_DW, 1, 6, 80)),
            "the return register"
        );
        assert!(ret
            .iter()
            .any(crate::bpf::transport::Output::is_output_call));
    }

    /// The record the program writes is the record the reader decodes.
    #[test]
    fn a_record_decodes() {
        let mut bytes = vec![0u8; RECORD_BYTES];
        bytes[R_START as usize..][..8].copy_from_slice(&100u64.to_ne_bytes());
        bytes[R_DURATION as usize..][..8].copy_from_slice(&2_000u64.to_ne_bytes());
        bytes[R_RET as usize..][..8].copy_from_slice(&7u64.to_ne_bytes());
        bytes[R_TID as usize..][..4].copy_from_slice(&11u32.to_ne_bytes());
        bytes[R_TGID as usize..][..4].copy_from_slice(&10u32.to_ne_bytes());
        bytes[R_FUNCTION as usize..][..4].copy_from_slice(&2u32.to_ne_bytes());
        bytes[R_DEPTH as usize..][..4].copy_from_slice(&3u32.to_ne_bytes());
        bytes[R_COMM as usize..][..4].copy_from_slice(b"work");
        let call = Call::decode(&bytes).expect("a call");
        assert_eq!(
            (
                call.start_ns,
                call.duration_ns,
                call.ret,
                call.tid,
                call.tgid,
                call.function,
                call.depth
            ),
            (100, 2_000, 7, 11, 10, 2, 3)
        );
        assert_eq!(call.comm, "work");
        assert!(Call::decode(&bytes[..RECORD_BYTES - 1]).is_none());
    }

    #[test]
    fn the_stack_regions_do_not_overlap_and_stay_inside_the_frame() {
        let regions = [
            (DEPTH_KEY, 8, "depth key"),
            (DEPTH_VALUE, 8, "depth value"),
            (STAMP_KEY, STAMP_KEY_BYTES as i16, "stamp key"),
            (STAMP_VALUE, 8, "stamp value"),
            (DROP_KEY, 4, "drop key"),
            (RECORD, RECORD_BYTES as i16, "record"),
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
}
