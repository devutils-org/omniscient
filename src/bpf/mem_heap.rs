// SPDX-License-Identifier: MIT OR GPL-2.0
//! `mem.heap.alloc` through uprobes on the C library's allocator: the backend
//! for a program the preload shim cannot reach (a static binary, one already
//! running, one that clears its environment).
//!
//! Five programs, linked through `BPF_TRACE_UPROBE_MULTI` (Linux 6.6) for the
//! one process and every thread in it, with the program's own `CAP_BPF` and
//! `CAP_PERFMON`:
//!
//! - on the entry of `malloc`, `calloc` and `realloc`, the size asked (and
//!   `calloc`'s count, `realloc`'s old block) is stamped per thread;
//! - on their return, the stamp is closed with the address the call returned
//!   and the thread's user stack, and one record is published;
//! - on the entry of `free`, the address given back is published.
//!
//! The records are the shim's in all but name -- an address, a size, a thread
//! and a moment -- so the collector builds the same events from them, and
//! adds the stack the shim cannot take.
//!
//! **What a uprobe costs** is a trap per call, about a microsecond and a half:
//! an allocation-heavy program runs slower under it, which the catalog says.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;

use super::btf::Btf;
use super::insn::{self, helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash, StackTraces};
use super::ring::{ring_bytes, RingBuffer};
use super::sched_common::BPF_F_USER_STACK;
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};

const LICENSE: &str = "Dual MIT/GPL";

/// What a record is.
pub const OP_MALLOC: u32 = 1;
pub const OP_CALLOC: u32 = 2;
pub const OP_REALLOC: u32 = 3;
pub const OP_FREE: u32 = 4;

/// The allocator's functions, with the op each is and which of its
/// arguments are the size and the count or the old block.
const ALLOCATORS: [(&str, u32); 3] = [
    ("malloc", OP_MALLOC),
    ("calloc", OP_CALLOC),
    ("realloc", OP_REALLOC),
];

/// Threads inside an allocation at once.
const MAX_PENDING: u32 = 16384;
/// Distinct allocation stacks interned.
const MAX_STACKS: u32 = 16384;
const STACK_DEPTH: u32 = 64;

/// One allocation or free, as the programs write it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapCall {
    pub ts_ns: u64,
    /// The block: the address returned, or the one freed.
    pub ptr: u64,
    /// The bytes asked: `malloc`'s and `realloc`'s size, `calloc`'s element
    /// size (the count is `extra`).
    pub size: u64,
    /// `calloc`'s count, `realloc`'s old block; zero otherwise.
    pub extra: u64,
    pub tid: u32,
    pub tgid: u32,
    pub op: u32,
    /// The user stack's id, or negative where the kernel walked none.
    pub stack: i32,
}

const RECORD_BYTES: usize = 48;

impl HeapCall {
    /// The bytes the block holds: `calloc` multiplies, the others are their
    /// size; a free none.
    pub fn bytes(&self) -> u64 {
        match self.op {
            OP_CALLOC => self.size.saturating_mul(self.extra),
            OP_FREE => 0,
            _ => self.size,
        }
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        Some(Self {
            ts_ns: long(0),
            ptr: long(8),
            size: long(16),
            extra: long(24),
            tid: word(32),
            tgid: word(36),
            op: word(40),
            stack: word(44) as i32,
        })
    }
}

const REC: i16 = -48;
const REC_PTR: i16 = REC + 8;
const REC_SIZE: i16 = REC + 16;
const REC_EXTRA: i16 = REC + 24;
const REC_TID: i16 = REC + 32;
const REC_TGID: i16 = REC + 36;
const REC_OP: i16 = REC + 40;
const REC_STACK: i16 = REC + 44;
const PEND_KEY: i16 = -56;
/// The pending value: size, extra, op (as a word in eight bytes).
const PEND_VALUE: i16 = -80;
const PEND_VALUE_BYTES: usize = 24;
const DROP_KEY: i16 = -84;

/// Drop counter slots.
const DROP_RING: u32 = 0;
const DROP_PENDING: u32 = 1;

/// Where the return value is in `pt_regs`.
fn return_register(btf: &Btf) -> Result<i32, String> {
    if cfg!(target_arch = "x86_64") {
        btf.member_offset("pt_regs", "ax")
    } else if cfg!(target_arch = "aarch64") {
        btf.member_offset("pt_regs", "regs")
    } else {
        Err(format!(
            "this build knows where a function returns its value on x86_64 and aarch64, not on {}",
            std::env::consts::ARCH
        ))
    }
}

/// Keeps the calls of one process: the high half of `pid_tgid` against it.
fn push_tgid_filter(program: &mut Program, tgid: u32) {
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::mov64_reg(7, 0));
    program.push(Insn::mov64_reg(1, 0));
    program.push(Insn::rsh64_imm(1, 32));
    program.jump_to("done", Insn::jne_imm(1, tgid as i32, 0));
}

/// The entry of an allocating function: stamp the size per thread.
fn entry_instructions(
    btf: &Btf,
    op: u32,
    tgid: u32,
    pending_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let (size_arg, extra_arg) = match op {
        OP_MALLOC => (0, None),
        OP_CALLOC => (1, Some(0)),
        _ => (1, Some(0)),
    };
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    push_tgid_filter(&mut program, tgid);
    program.push(Insn::st_imm(BPF_DW, R10, PEND_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, PEND_KEY, 7));
    program.push(Insn::ldx(
        BPF_DW,
        1,
        6,
        btf.syscall_argument(size_arg)? as i16,
    ));
    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE, 1));
    match extra_arg {
        Some(index) => {
            program.push(Insn::ldx(BPF_DW, 1, 6, btf.syscall_argument(index)? as i16));
            program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + 8, 1));
        }
        None => {
            program.push(Insn::st_imm(BPF_DW, R10, PEND_VALUE + 8, 0));
        }
    }
    program.push(Insn::st_imm(BPF_DW, R10, PEND_VALUE + 16, op as i32));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, PEND_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    insn::count_drop(&mut program, drops_fd, DROP_KEY, DROP_PENDING, "done");
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    insn::audit(&instructions)?;
    Ok(instructions)
}

/// Publishes the record on the stack, counting it when the ring is full.
fn publish(program: &mut Program, ring_fd: i32, drops_fd: i32) {
    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, REC as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    insn::count_drop(program, drops_fd, DROP_KEY, DROP_RING, "done");
}

/// The return of an allocating function: close the stamp with the address
/// and the stack, and publish.
fn return_instructions(
    btf: &Btf,
    tgid: u32,
    pending_fd: i32,
    stacks_fd: i32,
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let ret = return_register(btf)? as i16;
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    push_tgid_filter(&mut program, tgid);
    program.push(Insn::st_imm(BPF_DW, R10, PEND_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, PEND_KEY, 7));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // A return whose entry this run did not see: the call was under way at
    // the attach.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, 0));
    program.push(Insn::stx(BPF_DW, R10, REC_SIZE, 1));
    program.push(Insn::ldx(BPF_DW, 1, 0, 8));
    program.push(Insn::stx(BPF_DW, R10, REC_EXTRA, 1));
    program.push(Insn::ldx(BPF_DW, 1, 0, 16));
    program.push(Insn::stx(BPF_W, R10, REC_OP, 1));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    // A null return is an allocation that failed: nothing was allocated.
    program.push(Insn::ldx(BPF_DW, 1, 6, ret));
    program.jump_to("done", Insn::jeq_imm(1, 0, 0));
    program.push(Insn::stx(BPF_DW, R10, REC_PTR, 1));
    program.push(Insn::stx(BPF_W, R10, REC_TID, 7));
    program.push(Insn::mov64_reg(1, 7));
    program.push(Insn::rsh64_imm(1, 32));
    program.push(Insn::stx(BPF_W, R10, REC_TGID, 1));
    program.push(Insn::mov64_reg(1, 6));
    program.extend(Insn::load_map_fd(2, stacks_fd));
    program.push(Insn::mov64_imm(3, BPF_F_USER_STACK));
    program.push(Insn::call(helper::GET_STACKID));
    program.push(Insn::stx(BPF_W, R10, REC_STACK, 0));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, REC, 0));
    publish(&mut program, ring_fd, drops_fd);
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    insn::audit(&instructions)?;
    Ok(instructions)
}

/// The entry of `free`: the block given back.
fn free_instructions(
    btf: &Btf,
    tgid: u32,
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    push_tgid_filter(&mut program, tgid);
    // `free(NULL)` frees nothing.
    program.push(Insn::ldx(BPF_DW, 1, 6, btf.syscall_argument(0)? as i16));
    program.jump_to("done", Insn::jeq_imm(1, 0, 0));
    program.push(Insn::stx(BPF_DW, R10, REC_PTR, 1));
    for slot in [REC_SIZE, REC_EXTRA] {
        program.push(Insn::st_imm(BPF_DW, R10, slot, 0));
    }
    program.push(Insn::st_imm(BPF_W, R10, REC_OP, OP_FREE as i32));
    program.push(Insn::st_imm(BPF_W, R10, REC_STACK, -1));
    program.push(Insn::stx(BPF_W, R10, REC_TID, 7));
    program.push(Insn::mov64_reg(1, 7));
    program.push(Insn::rsh64_imm(1, 32));
    program.push(Insn::stx(BPF_W, R10, REC_TGID, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, REC, 0));
    publish(&mut program, ring_fd, drops_fd);
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    insn::audit(&instructions)?;
    Ok(instructions)
}

/// The C library a process has mapped, by its maps: the file whose name
/// starts `libc.so` or `libc-`, else the program itself (a static binary
/// carries its own allocator).
pub fn allocator_of(pid: i32) -> Result<PathBuf, String> {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .map_err(|error| format!("cannot read process {pid}'s maps: {error}"))?;
    let paths: Vec<PathBuf> = super::usdt::parse_maps(&maps)
        .into_iter()
        .map(|mapping| mapping.path)
        .collect();
    match the_c_library(&paths) {
        Some(path) => Ok(path),
        None => std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|error| {
            format!("process {pid} maps no C library and its binary cannot be read: {error}")
        }),
    }
}

/// The C library among the files a program will map, the executable first:
/// the file whose name starts `libc.so` or `libc-`, else the executable
/// itself, which is what a static binary's allocator is in. A launched
/// program is read this way because at the gate its maps are still this
/// tool's own, and a static program never maps the C library they name.
pub fn allocator_among(paths: &[PathBuf]) -> Option<PathBuf> {
    the_c_library(paths).or_else(|| paths.first().cloned())
}

fn the_c_library(paths: &[PathBuf]) -> Option<PathBuf> {
    paths
        .iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libc.so") || name.starts_with("libc-"))
        })
        .cloned()
}

fn load(instructions: &[Insn], name: &str) -> Result<OwnedFd, String> {
    syscall::prog_load_for(
        syscall::BPF_PROG_TYPE_KPROBE,
        syscall::BPF_TRACE_UPROBE_MULTI,
        instructions,
        name,
        LICENSE,
    )
    .map_err(|error| {
        if error.is_permission() {
            super::permission_advice("loading a heap uprobe program")
        } else {
            format!(
                "the kernel refused the `{name}` program ({}); the verifier said:\n{}",
                error.error, error.verifier_log
            )
        }
    })
}

/// The five programs, linked, their ring and their stacks.
pub struct HeapProgram {
    ring: RingBuffer,
    drops: Counters,
    stacks: StackTraces,
    _pending: Hash,
    _links: Vec<(OwnedFd, OwnedFd)>,
    /// The file the functions were found in.
    pub library: PathBuf,
}

impl HeapProgram {
    /// The programs, placed on `library`'s allocator and kept to `pid`.
    pub fn load(pid: i32, library: PathBuf, requested_ring_bytes: usize) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let tgid = super::pidns::kernel_tgid(pid as u32)?;
        let offset = |name: &str| {
            crate::stack::symbol_file_offset(&library, name).ok_or_else(|| {
                format!(
                    "{} defines no `{name}` this build can find",
                    library.display()
                )
            })
        };
        let bytes = ring_bytes(requested_ring_bytes);
        let ring_map = syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_heap")
            .map_err(|error| map_failure("heap ring buffer", error))?;
        let drops = Counters::create(2, "omn_heap_drop")
            .map_err(|error| map_failure("heap drop counter", error))?;
        let pending = Hash::create(4, PEND_VALUE_BYTES, MAX_PENDING, "omn_heap_pend")
            .map_err(|error| map_failure("heap pending map", error))?;
        let stacks = StackTraces::create(MAX_STACKS, STACK_DEPTH, "omn_heap_stk")
            .map_err(|error| map_failure("heap stack map", error))?;
        let (ring_fd, drops_fd, pending_fd) =
            (ring_map.as_raw_fd(), drops.as_raw_fd(), pending.as_raw_fd());

        let link = |program: &OwnedFd, at: u64, returning: bool, what: &str| {
            let linked = if returning {
                syscall::link_uretprobe_multi(program.as_raw_fd(), &library, at, pid)
            } else {
                syscall::link_uprobe_multi(program.as_raw_fd(), &library, at, None, pid)
            };
            linked.map_err(|error| {
                format!(
                    "cannot link the heap program to `{what}` in {}: {error}",
                    library.display()
                )
            })
        };
        let mut links = Vec::new();
        // The returns and the frees first: an entry stamped with nothing to
        // close it would wait in the pending map until the run ends.
        let returning = load(
            &return_instructions(btf, tgid, pending_fd, stacks.as_raw_fd(), ring_fd, drops_fd)?,
            "omn_heap_ret",
        )?;
        for (name, _) in ALLOCATORS {
            let linked = link(&returning, offset(name)?, true, name)?;
            links.push((
                linked,
                returning.try_clone().map_err(|error| error.to_string())?,
            ));
        }
        let free = load(
            &free_instructions(btf, tgid, ring_fd, drops_fd)?,
            "omn_heap_free",
        )?;
        let linked = link(&free, offset("free")?, false, "free")?;
        links.push((linked, free));
        for (name, op) in ALLOCATORS {
            let entry = load(
                &entry_instructions(btf, op, tgid, pending_fd, drops_fd)?,
                "omn_heap_entry",
            )?;
            let linked = link(&entry, offset(name)?, false, name)?;
            links.push((linked, entry));
        }
        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the heap ring buffer: {error}"))?;
        Ok(Self {
            ring,
            drops,
            stacks,
            _pending: pending,
            _links: links,
            library,
        })
    }

    pub fn drain(&mut self, mut handle: impl FnMut(HeapCall), undecoded: &mut u64) {
        self.ring.drain(|bytes| match HeapCall::from_bytes(bytes) {
            Some(call) => handle(call),
            None => *undecoded += 1,
        });
    }

    /// `(ring full, pending map full)`, cumulative.
    pub fn dropped(&self) -> (u64, u64) {
        (self.drops.read(DROP_RING), self.drops.read(DROP_PENDING))
    }

    /// The addresses behind a stack id, leaf first.
    pub fn stack(&self, id: u32) -> Option<Vec<u64>> {
        self.stacks.get(id).ok().flatten()
    }

    pub fn stack_depth(&self) -> usize {
        self.stacks.depth() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_decodes_and_calloc_multiplies() {
        let mut bytes = vec![0u8; RECORD_BYTES];
        bytes[8..16].copy_from_slice(&0x5555_0000u64.to_ne_bytes());
        bytes[16..24].copy_from_slice(&24u64.to_ne_bytes());
        bytes[24..32].copy_from_slice(&10u64.to_ne_bytes());
        bytes[40..44].copy_from_slice(&OP_CALLOC.to_ne_bytes());
        bytes[44..48].copy_from_slice(&(-14i32).to_ne_bytes());
        let call = HeapCall::from_bytes(&bytes).expect("a record");
        assert_eq!(call.bytes(), 240);
        assert_eq!(call.stack, -14);
        assert!(HeapCall::from_bytes(&bytes[..40]).is_none());
    }

    /// A launched program's allocator is its C library when it will map one,
    /// and its own executable when it maps none, as a static binary does.
    #[test]
    fn a_static_program_s_allocator_is_its_own_executable() {
        let dynamic = [
            PathBuf::from("/usr/bin/python3.14"),
            PathBuf::from("/usr/lib/libpython3.14.so.1.0"),
            PathBuf::from("/usr/lib/libc.so.6"),
        ];
        assert_eq!(
            allocator_among(&dynamic),
            Some(PathBuf::from("/usr/lib/libc.so.6"))
        );
        let static_binary = [PathBuf::from("/tmp/alloc-static")];
        assert_eq!(
            allocator_among(&static_binary),
            Some(PathBuf::from("/tmp/alloc-static"))
        );
        assert_eq!(allocator_among(&[]), None);
    }

    /// The five programs build against this kernel, and this test process's
    /// own C library defines the four functions they are placed on.
    #[test]
    fn the_programs_build_and_the_allocator_is_found() {
        let Ok(btf) = super::super::kernel_btf() else {
            eprintln!("skipped: no kernel BTF");
            return;
        };
        for (_, op) in ALLOCATORS {
            entry_instructions(btf, op, 4242, 3, 4).expect("an entry program");
        }
        return_instructions(btf, 4242, 3, 5, 6, 4).expect("the return program");
        free_instructions(btf, 4242, 6, 4).expect("the free program");
        let library = allocator_of(std::process::id() as i32).expect("our allocator");
        for name in ["malloc", "calloc", "realloc", "free"] {
            assert!(
                crate::stack::symbol_file_offset(&library, name).is_some(),
                "{name} in {}",
                library.display()
            );
        }
    }
}
