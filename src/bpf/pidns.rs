// SPDX-License-Identifier: MIT OR GPL-2.0
//! The kernel's ids for the tasks `omn` names, when `omn` runs in a container.
//!
//! Every program compares the kernel's own ids: `task_struct.pid` and `.tgid`,
//! `bpf_get_current_pid_tgid`, a cgroup's level in the whole hierarchy. Those
//! are the initial namespaces' numbers. A target is named in the namespaces
//! `omn` runs in -- a pid from `/proc`, a launched child's pid, a directory
//! under `/sys/fs/cgroup` -- and inside a container the two disagree: an
//! OrbStack machine's pid 2000 is some other number to the kernel, and a
//! program comparing against 2000 keeps nothing and reports nothing lost.
//!
//! `/proc` cannot translate. `NSpid` lists a task's ids from the reader's own
//! namespace down, never up, so a process inside a container has no file that
//! says what the kernel calls it. The kernel can: a task iterator
//! (`iter/task`) visits every task and reads, from its `struct pid`, the
//! number it has at each level of nesting and which namespace each level is.
//! The level whose namespace is `omn`'s own is the id `omn` sees; the task's
//! `pid` is the kernel's. One pass gives both for every task.
//!
//! The same pass reads each task's cgroup level in the whole hierarchy, which a
//! cgroup namespace hides the same way: `omn`'s own cgroup's true level, less
//! the depth `/proc/self/cgroup` gives it, is how deep the namespace's root is.
//!
//! In the initial namespaces none of this runs: the ids agree, and a run there
//! loads no iterator.

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::OnceLock;

use crate::bpf::attach::fentry;
use crate::bpf::btf::Btf;
use crate::bpf::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};

/// `PROC_PID_INIT_INO` and `PROC_CGROUP_INIT_INO`, from
/// `include/linux/proc_ns.h`: the inode numbers of the initial namespaces,
/// fixed since 3.8 and 4.6.
const INITIAL_PID_NS: u64 = 0xEFFF_FFFC;
const INITIAL_CGROUP_NS: u64 = 0xEFFF_FFFB;

/// `BPF_TRACE_ITER`, in `enum bpf_attach_type`. Frozen UAPI.
const BPF_TRACE_ITER: u32 = 28;
/// `bpf()` commands.
const BPF_LINK_CREATE: i32 = 28;
const BPF_ITER_CREATE: i32 = 33;

/// Levels of pid namespace nesting read per task. A container in a VM is two
/// (OrbStack's), a container in a container three; the kernel allows 32, and
/// a task deeper than this is one `omn` cannot be above anyway unless it is
/// itself that deep, which the translation then refuses by name.
const LEVELS: usize = 8;

/// One task's record, as the program writes it: tid, tgid, the pid's level,
/// the task's cgroup level, then `(nr, namespace inode)` for each level.
const TID: usize = 0;
const TGID: usize = 4;
const LEVEL: usize = 8;
const CGROUP_LEVEL: usize = 12;
const NUMBERS: usize = 16;
const RECORD: usize = NUMBERS + LEVELS * 8;

/// Where the record and an eight-byte scratch slot sit on the program's stack.
const BASE: i16 = -(RECORD as i16) - 8;
const SCRATCH: i16 = -8;

/// One task, in both numberings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Task {
    /// The kernel's thread id, `task_struct.pid`.
    pub tid: u32,
    /// The kernel's thread group id.
    pub tgid: u32,
    /// The thread id in `omn`'s pid namespace, `None` for a task outside it.
    pub ns_tid: Option<u32>,
    /// The task's cgroup's level in the whole unified hierarchy.
    pub cgroup_level: u32,
}

/// A namespace link's inode, `pid:[4026531836]` read as the number.
fn namespace_inode(kind: &str) -> Option<u64> {
    let link = std::fs::read_link(format!("/proc/self/ns/{kind}")).ok()?;
    let text = link.to_str()?;
    text.strip_prefix(kind)?
        .strip_prefix(":[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// Whether `omn` runs in the initial pid namespace, where the ids it sees are
/// the kernel's. A namespace this cannot read is taken as the initial one,
/// which is what every run assumed before this file.
pub fn in_initial_pid_namespace() -> bool {
    static ANSWER: OnceLock<bool> = OnceLock::new();
    *ANSWER.get_or_init(|| namespace_inode("pid").is_none_or(|inode| inode == INITIAL_PID_NS))
}

/// Whether `omn` runs in the initial cgroup namespace, where a cgroup's depth
/// under `/sys/fs/cgroup` is its level.
pub fn in_initial_cgroup_namespace() -> bool {
    static ANSWER: OnceLock<bool> = OnceLock::new();
    *ANSWER.get_or_init(|| namespace_inode("cgroup").is_none_or(|inode| inode == INITIAL_CGROUP_NS))
}

/// Where the fields the program reads sit on this kernel.
struct Offsets {
    pid: i32,
    tgid: i32,
    thread_pid: i32,
    pid_level: i32,
    numbers: i32,
    upid_size: i32,
    upid_nr: i32,
    upid_ns: i32,
    ns_inum: i32,
    cgroups: i32,
    dfl_cgrp: i32,
    cgroup_level: i32,
}

impl Offsets {
    fn read(btf: &Btf) -> Result<Self, String> {
        let upid_ns = btf.member_offset("upid", "ns")?;
        Ok(Self {
            pid: btf.member_offset("task_struct", "pid")?,
            tgid: btf.member_offset("task_struct", "tgid")?,
            thread_pid: btf.member_offset("task_struct", "thread_pid")?,
            pid_level: btf.member_offset("pid", "level")?,
            numbers: btf.member_offset("pid", "numbers")?,
            // `struct upid` is an `int` and a pointer: the pointer is its last
            // member, so the size is where it starts plus eight.
            upid_size: upid_ns + 8,
            upid_nr: btf.member_offset("upid", "nr")?,
            upid_ns,
            ns_inum: btf.member_offset("pid_namespace", "ns")?
                + btf.member_offset("ns_common", "inum")?,
            cgroups: btf.member_offset("task_struct", "cgroups")?,
            dfl_cgrp: btf.member_offset("css_set", "dfl_cgrp")?,
            cgroup_level: btf.member_offset("cgroup", "level")?,
        })
    }
}

/// `bpf_probe_read_kernel(fp + into, size, r<base> + offset)`.
fn probe_read(program: &mut Program, into: i16, size: i32, base: u8, offset: i32) {
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, into as i32));
    program.push(Insn::mov64_imm(2, size));
    program.push(Insn::mov64_reg(3, base));
    program.push(Insn::add64_imm(3, offset));
    program.push(Insn::call(helper::PROBE_READ_KERNEL));
}

/// The iterator program: one record per task, written to the seq file.
///
/// The task's own two ids are typed loads the verifier checks against
/// `task_struct`; everything past `thread_pid` is read with
/// `bpf_probe_read_kernel`, because `numbers[]` is a flexible array whose
/// length is the task's level and the verifier knows no bound for it.
fn instructions(at: &Offsets) -> Vec<Insn> {
    const NEXT: [&str; LEVELS] = ["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7"];
    let field = |offset: usize| BASE + offset as i16;
    let mut program = Program::new();
    // r7 the task, null on the iteration's last call; r8 the seq file.
    program.push(Insn::ldx(BPF_DW, 7, 1, 8));
    program.jump_to("done", Insn::jeq_imm(7, 0, 0));
    program.push(Insn::ldx(BPF_DW, 8, 1, 0));
    program.push(Insn::ldx(BPF_DW, 8, 8, 0));
    for word in 0..(RECORD / 8) as i16 {
        program.push(Insn::st_imm(BPF_DW, R10, BASE + word * 8, 0));
    }
    program.push(Insn::ldx(BPF_W, 1, 7, at.pid as i16));
    program.push(Insn::stx(BPF_W, R10, field(TID), 1));
    program.push(Insn::ldx(BPF_W, 1, 7, at.tgid as i16));
    program.push(Insn::stx(BPF_W, R10, field(TGID), 1));

    // r9 the task's `struct pid`, r6 its level.
    probe_read(&mut program, SCRATCH, 8, 7, at.thread_pid);
    program.push(Insn::ldx(BPF_DW, 9, R10, SCRATCH));
    program.jump_to("cgroup", Insn::jeq_imm(9, 0, 0));
    probe_read(&mut program, field(LEVEL), 4, 9, at.pid_level);
    program.push(Insn::ldx(BPF_W, 6, R10, field(LEVEL)));
    for (level, next) in NEXT.iter().enumerate() {
        // Past the task's own level the array has no entry.
        program.push(Insn::mov64_imm(1, level as i32));
        program.jump_to("cgroup", Insn::jgt_reg(1, 6, 0));
        let upid = at.numbers + level as i32 * at.upid_size;
        let slot = field(NUMBERS + level * 8);
        probe_read(&mut program, slot, 4, 9, upid + at.upid_nr);
        probe_read(&mut program, SCRATCH, 8, 9, upid + at.upid_ns);
        program.push(Insn::ldx(BPF_DW, 4, R10, SCRATCH));
        program.jump_to(next, Insn::jeq_imm(4, 0, 0));
        probe_read(&mut program, slot + 4, 4, 4, at.ns_inum);
        program.label(next);
    }

    program.label("cgroup");
    probe_read(&mut program, SCRATCH, 8, 7, at.cgroups);
    program.push(Insn::ldx(BPF_DW, 4, R10, SCRATCH));
    program.jump_to("write", Insn::jeq_imm(4, 0, 0));
    probe_read(&mut program, SCRATCH, 8, 4, at.dfl_cgrp);
    program.push(Insn::ldx(BPF_DW, 4, R10, SCRATCH));
    program.jump_to("write", Insn::jeq_imm(4, 0, 0));
    probe_read(&mut program, field(CGROUP_LEVEL), 4, 4, at.cgroup_level);

    program.label("write");
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, BASE as i32));
    program.push(Insn::mov64_imm(3, RECORD as i32));
    program.push(Insn::call(helper::SEQ_WRITE));
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    program.build()
}

#[repr(C)]
#[derive(Default)]
struct LinkCreateAttr {
    prog_fd: u32,
    target_fd: u32,
    attach_type: u32,
    flags: u32,
    iter_info: u64,
    iter_info_len: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct IterCreateAttr {
    link_fd: u32,
    flags: u32,
}

fn bpf<T>(command: i32, attr: &mut T) -> Result<OwnedFd, std::io::Error> {
    let fd = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            command,
            attr as *mut T as *mut libc::c_void,
            std::mem::size_of::<T>() as u32,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Every task on the machine, in both numberings: one pass of the iterator.
pub fn tasks() -> Result<Vec<Task>, String> {
    iterate()?.ok_or_else(|| {
        "`omn` runs in a pid namespace other than the initial one, and the task iterator \
         that translates its ids needs CAP_BPF and CAP_PERFMON"
            .to_owned()
    })
}

/// `BPF_MAP_TYPE_LRU_HASH`, in `enum bpf_map_type`. Frozen UAPI.
const BPF_MAP_TYPE_LRU_HASH: u32 = 9;

/// Births remembered at once: the oldest is forgotten first, so a long run on
/// a busy machine keeps the recent ones, which are the ones still to drain.
const BIRTHS: u32 = 65_536;

/// A program on `sched_process_fork` writing every new task's id in `omn`'s
/// namespace under the kernel's, for the tasks no iteration lives to see.
///
/// The iterator names the tasks alive when it runs, and a task that execs and
/// exits in a few milliseconds -- a build's compiler, a test's thread -- is
/// gone by the next pass: its events kept the kernel's id. The fork
/// tracepoint fires in `kernel_clone` once the child's `struct pid` is
/// attached and before it runs, for a thread as for a process, so every task
/// born after the first pass is in this map whatever it lived. The map is
/// only read for an id the iterator missed.
struct Births {
    map: OwnedFd,
    program: OwnedFd,
    _link: OwnedFd,
}

/// The births program, loaded with the iterator's link; the reason when it
/// could not be, which the run's note gives ([`untranslated_note`]).
static BORN: OnceLock<Result<Births, String>> = OnceLock::new();

/// Stack slots of the births program.
const BORN_KEY: i16 = -8;
const BORN_VALUE: i16 = -16;
const BORN_SCRATCH: i16 = -24;
const BORN_LEVEL: i16 = -32;

/// `sched_process_fork(parent, child)`: the child's `nr` at the level whose
/// namespace is `own`, under the child's `pid`. A child outside `omn`'s
/// namespace has no such level and writes nothing.
fn births_instructions(at: &Offsets, own: u64, map_fd: i32) -> Vec<Insn> {
    const NEXT: [&str; LEVELS] = ["b0", "b1", "b2", "b3", "b4", "b5", "b6", "b7"];
    let mut program = Program::new();
    // A raw tracepoint's context is its arguments: r7 the child.
    program.push(Insn::ldx(BPF_DW, 7, 1, 8));
    program.push(Insn::st_imm(BPF_DW, R10, BORN_KEY, 0));
    program.push(Insn::st_imm(BPF_DW, R10, BORN_VALUE, 0));
    program.push(Insn::st_imm(BPF_DW, R10, BORN_LEVEL, 0));
    probe_read(&mut program, BORN_KEY, 4, 7, at.pid);
    // r9 the child's `struct pid`, r8 its level.
    probe_read(&mut program, BORN_SCRATCH, 8, 7, at.thread_pid);
    program.push(Insn::ldx(BPF_DW, 9, R10, BORN_SCRATCH));
    program.jump_to("done", Insn::jeq_imm(9, 0, 0));
    probe_read(&mut program, BORN_LEVEL, 4, 9, at.pid_level);
    program.push(Insn::ldx(BPF_W, 8, R10, BORN_LEVEL));
    for (level, next) in NEXT.iter().enumerate() {
        program.push(Insn::mov64_imm(1, level as i32));
        program.jump_to("done", Insn::jgt_reg(1, 8, 0));
        let upid = at.numbers + level as i32 * at.upid_size;
        probe_read(&mut program, BORN_SCRATCH, 8, 9, upid + at.upid_ns);
        program.push(Insn::ldx(BPF_DW, 4, R10, BORN_SCRATCH));
        program.jump_to(next, Insn::jeq_imm(4, 0, 0));
        program.push(Insn::st_imm(BPF_DW, R10, BORN_SCRATCH, 0));
        probe_read(&mut program, BORN_SCRATCH, 4, 4, at.ns_inum);
        program.push(Insn::ldx(BPF_W, 1, R10, BORN_SCRATCH));
        // An inode above `i32::MAX` is not an immediate a 64-bit compare
        // takes as it is: it sign-extends.
        program.extend(Insn::ld_imm64(2, own as i64));
        program.jump_to(next, Insn::jne_reg(1, 2, 0));
        probe_read(&mut program, BORN_VALUE, 4, 9, upid + at.upid_nr);
        program.jump_to("store", Insn::ja());
        program.label(next);
    }
    program.jump_to("done", Insn::ja());
    program.label("store");
    program.extend(Insn::load_map_fd(1, map_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, BORN_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, BORN_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    program.build()
}

/// Loads and attaches the births program.
fn watch_births(btf: &Btf, at: &Offsets, own: u64) -> Result<Births, String> {
    let signature = btf.tracepoint_signature("sched_process_fork")?;
    if signature != ["task_struct *", "task_struct *"] {
        return Err(format!(
            "this kernel declares `sched_process_fork({})`, and the program reads the child \
             from the second of two task pointers",
            signature.join(", ")
        ));
    }
    let map =
        crate::bpf::syscall::map_create(BPF_MAP_TYPE_LRU_HASH, 4, 4, BIRTHS, "omn_pidns_born")
            .map_err(|error| format!("cannot create the births map: {error}"))?;
    let instructions = births_instructions(at, own, map.as_raw_fd());
    crate::bpf::insn::audit(&instructions)?;
    let program = crate::bpf::syscall::prog_load(
        crate::bpf::syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
        &instructions,
        "omn_pidns_born",
        "Dual MIT/GPL",
    )
    .map_err(|error| format!("the kernel refused the births program: {error}"))?;
    let link = crate::bpf::syscall::raw_tracepoint_open("sched_process_fork", program.as_raw_fd())
        .map_err(|error| format!("cannot attach to `sched_process_fork`: {error}"))?;
    Ok(Births {
        map,
        program,
        _link: link,
    })
}

/// Runs of the births program the kernel skipped, as every program here
/// reports them: a fork it skipped is a task whose id only an iteration that
/// saw it alive can translate. Zero where the program is not running.
pub fn skipped() -> u64 {
    match BORN.get() {
        Some(Ok(births)) => crate::bpf::attach::fentry::skipped([&births.program]),
        _ => 0,
    }
}

/// The id in `omn`'s namespace the births program wrote for a kernel id.
fn born(kernel: u32) -> Option<u32> {
    let births = BORN.get()?.as_ref().ok()?;
    let value = crate::bpf::syscall::map_lookup(births.map.as_raw_fd(), &kernel.to_ne_bytes(), 4)
        .ok()??;
    Some(u32::from_ne_bytes(value.get(..4)?.try_into().ok()?))
}

/// The iterator's link, loaded on the first pass and kept for the run: a
/// pass is then one `BPF_ITER_CREATE` and one read, and a run that refreshes
/// its table on every tick ([`remember`]) does not verify a program each time.
static LINK: std::sync::Mutex<Option<OwnedFd>> = std::sync::Mutex::new(None);

/// One pass of the iterator; `None` when this process may not load one.
///
/// Refused for permission, the ids are left as they were named: a process
/// that may not load an iterator loads no other tracing program either, so
/// every eBPF collector it starts is refused at its own load with the
/// capability it lacks, which is the reason worth reading -- not this one.
fn iterate() -> Result<Option<Vec<Task>>, String> {
    let why = "`omn` runs in a pid namespace other than the initial one, so the ids it names \
               a target by are not the kernel's, and translating them needs a task iterator";
    let mut link = LINK.lock().expect("the iterator's link");
    if link.is_none() {
        let btf = crate::bpf::kernel_btf()?;
        let offsets = Offsets::read(btf).map_err(|reason| format!("{why}: {reason}"))?;
        let target = fentry::index()
            .and_then(|index| index.function("bpf_iter_task"))
            .map_err(|reason| format!("{why}, and this kernel has none: {reason}"))?;
        let program = match fentry::load_tracing(
            &target,
            BPF_TRACE_ITER,
            &instructions(&offsets),
            "omn_pidns",
        ) {
            Ok(program) => program,
            Err(error) if error.is_permission() => return Ok(None),
            Err(error) => return Err(format!("{why}, which did not load: {error}")),
        };
        let mut attr = LinkCreateAttr {
            prog_fd: program.as_raw_fd() as u32,
            attach_type: BPF_TRACE_ITER,
            ..Default::default()
        };
        // Before the link, so that the births are watched from the first
        // pass on: a task born after it is either alive at the next one or
        // in the map.
        BORN.get_or_init(|| {
            watch_births(
                btf,
                &offsets,
                namespace_inode("pid").unwrap_or(INITIAL_PID_NS),
            )
        });
        // The link holds the program: its descriptor can close.
        *link = Some(
            bpf(BPF_LINK_CREATE, &mut attr)
                .map_err(|error| format!("{why}, which did not attach: {error}"))?,
        );
    }
    let mut attr = IterCreateAttr {
        link_fd: link.as_ref().expect("loaded above").as_raw_fd() as u32,
        flags: 0,
    };
    let iterator = bpf(BPF_ITER_CREATE, &mut attr)
        .map_err(|error| format!("{why}, which did not open: {error}"))?;
    drop(link);
    let mut bytes = Vec::new();
    std::fs::File::from(iterator)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{why}, which could not be read: {error}"))?;
    let own = namespace_inode("pid").unwrap_or(INITIAL_PID_NS);
    Ok(Some(
        bytes
            .chunks_exact(RECORD)
            .map(|record| decode(record, own))
            .collect(),
    ))
}

fn decode(record: &[u8], own_namespace: u64) -> Task {
    let word = |at: usize| u32::from_ne_bytes(record[at..at + 4].try_into().expect("four bytes"));
    let level = word(LEVEL) as usize;
    let ns_tid = (0..=level.min(LEVELS - 1))
        .map(|k| (word(NUMBERS + k * 8), word(NUMBERS + k * 8 + 4)))
        .find(|(_, inode)| u64::from(*inode) == own_namespace)
        .map(|(nr, _)| nr);
    Task {
        tid: word(TID),
        tgid: word(TGID),
        ns_tid,
        cgroup_level: word(CGROUP_LEVEL),
    }
}

/// The kernel's ids for a thread `omn` names `tid`: `(tid, tgid)`. The same
/// numbers in the initial namespace, where nothing is loaded.
pub fn kernel_ids(tid: u32) -> Result<(u32, u32), String> {
    if in_initial_pid_namespace() {
        return Ok((tid, tid));
    }
    let Some(tasks) = iterate()? else {
        return Ok((tid, tid));
    };
    tasks
        .into_iter()
        .find(|task| task.ns_tid == Some(tid))
        .map(|task| (task.tid, task.tgid))
        .ok_or_else(|| {
            format!(
                "`omn` runs in a pid namespace other than the initial one, and no task in it \
                 is {tid}: it has exited, or it is deeper than {LEVELS} levels of nesting"
            )
        })
}

/// The kernel's thread group id for the process `omn` names `pid`.
pub fn kernel_tgid(pid: u32) -> Result<u32, String> {
    if in_initial_pid_namespace() {
        return Ok(pid);
    }
    kernel_ids(pid).map(|(_, tgid)| tgid)
}

/// The kernel's thread id for the thread `omn` names `tid`.
pub fn kernel_tid(tid: u32) -> Result<u32, String> {
    if in_initial_pid_namespace() {
        return Ok(tid);
    }
    kernel_ids(tid).map(|(tid, _)| tid)
}

/// Whether an attribute of this name carries a task id: `pid`, `tid`,
/// `ppid`, and the other party's (`waker_tid`, `holder_tid`, `old_pid`).
fn names_a_task(name: &str) -> bool {
    matches!(name, "pid" | "tid" | "ppid") || name.ends_with("_tid") || name.ends_with("_pid")
}

/// The kernel's ids mapped back to `omn`'s, kept for the whole run: a task
/// seen once keeps its translation after it exits.
static BACK: std::sync::Mutex<Option<std::collections::HashMap<u32, u32>>> =
    std::sync::Mutex::new(None);

/// Rewrites the task ids a program wrote into the ones `omn` names tasks by,
/// so that an eBPF source's `task:<tid>` is the same task as a procfs or perf
/// source's. Returns how many ids it could not translate: a task that exited
/// before an iteration saw it, or one outside `omn`'s namespace, which keeps
/// the kernel's id. Nothing to do, and nothing loaded, in the initial
/// namespace.
pub fn translate(events: &mut [crate::event::Event]) -> usize {
    use crate::event::{AttrValue, KeyValue};
    if in_initial_pid_namespace() || events.is_empty() {
        return 0;
    }
    let mut back = BACK.lock().expect("the translation table");
    let table = back.get_or_insert_with(Default::default);
    let mut refreshed = false;
    let mut missed = 0;
    let mut map = |id: u32, table: &mut std::collections::HashMap<u32, u32>| -> Option<u32> {
        // Zero is "no task" in every record that carries one: the idle task,
        // or a field the program left empty.
        if id == 0 {
            return Some(0);
        }
        if let Some(ours) = table.get(&id) {
            return Some(*ours);
        }
        if !refreshed {
            refreshed = true;
            if let Ok(Some(tasks)) = iterate() {
                for task in tasks {
                    if let Some(ours) = task.ns_tid {
                        table.insert(task.tid, ours);
                    }
                }
            }
        }
        let ours = table.get(&id).copied().or_else(|| born(id))?;
        table.insert(id, ours);
        Some(ours)
    };
    for event in events.iter_mut() {
        for key in &mut event.keys {
            if let KeyValue::Task { tid } = key {
                match map(*tid as u32, table) {
                    Some(ours) => *tid = ours as i32,
                    None => missed += 1,
                }
            }
        }
        for attr in &mut event.attrs {
            if !names_a_task(&attr.name) {
                continue;
            }
            let id = match attr.value {
                AttrValue::Uint(id) => id as u32,
                AttrValue::Int(id) if id > 0 => id as u32,
                _ => continue,
            };
            match map(id, table) {
                Some(ours) => {
                    attr.value = match attr.value {
                        AttrValue::Int(_) => AttrValue::Int(ours.into()),
                        _ => AttrValue::Uint(ours.into()),
                    }
                }
                None => missed += 1,
            }
        }
    }
    missed
}

/// One kernel id in `omn`'s namespace, for a collector that reads `/proc` by
/// an id its program wrote: the same table [`translate`] keeps, refreshed
/// once when the id is not in it. `None` for a task that exited before an
/// iteration saw it, or one outside the namespace. The id itself in the
/// initial namespace, and where this process may not iterate.
pub fn ours(kernel: u32) -> Option<u32> {
    if in_initial_pid_namespace() || kernel == 0 {
        return Some(kernel);
    }
    let mut back = BACK.lock().expect("the translation table");
    let table = back.get_or_insert_with(Default::default);
    if let Some(id) = table.get(&kernel) {
        return Some(*id);
    }
    match iterate() {
        Ok(Some(tasks)) => {
            for task in tasks {
                if let Some(id) = task.ns_tid {
                    table.insert(task.tid, id);
                }
            }
        }
        // Refused for permission: the ids are as named, as `kernel_ids`
        // leaves them (`iterate`).
        Ok(None) => return Some(kernel),
        Err(_) => {}
    }
    let id = table.get(&kernel).copied().or_else(|| born(kernel))?;
    table.insert(kernel, id);
    Some(id)
}

/// When [`remember`] last iterated, in `CLOCK_MONOTONIC` nanoseconds.
static REMEMBERED_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The live tasks added to the table [`translate`] reads, at most once per
/// 100 ms, for a collector that keeps its rows in the kernel until its stop:
/// by the time it hands them over, a target's children have exited and no
/// iteration can name them. The runner calls this before it attaches an eBPF
/// collector, which also starts the births program ([`Births`]), and on every
/// read of one that handed nothing over. A task born before the first call
/// and gone before an iteration saw it still keeps the kernel's id, and is
/// counted ([`untranslated_note`]).
pub fn remember() {
    use std::sync::atomic::Ordering;
    if in_initial_pid_namespace() {
        return;
    }
    let now = crate::clock::monotonic_ns();
    let last = REMEMBERED_NS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 100_000_000
        || REMEMBERED_NS
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    let Ok(Some(tasks)) = iterate() else {
        return;
    };
    let mut back = BACK.lock().expect("the translation table");
    let table = back.get_or_insert_with(Default::default);
    for task in tasks {
        if let Some(ours) = task.ns_tid {
            table.insert(task.tid, ours);
        }
    }
}

/// The note a run carries for the ids [`translate`] left as the kernel's.
pub fn untranslated_note(missed: usize) -> String {
    match BORN.get() {
        Some(Err(reason)) => format!(
            "{missed} task id(s) are the kernel's rather than this pid namespace's: the task \
             exited before the iterator that translates them saw it, and the program that \
             remembers each task's birth is not running ({reason}), or it lives outside the \
             namespace `omn` runs in"
        ),
        _ => {
            let skipped = match skipped() {
                0 => String::new(),
                runs => format!(
                    ", or the kernel skipped the run of the program that remembers its birth \
                     ({runs} skipped)"
                ),
            };
            format!(
                "{missed} task id(s) are the kernel's rather than this pid namespace's: the task \
                 was born before `omn` began translating and exited before the iterator saw \
                 it{skipped}, or it lives outside the namespace `omn` runs in"
            )
        }
    }
}

/// How many levels of the whole hierarchy lie above the root of `omn`'s
/// cgroup namespace: zero in the initial one.
///
/// `omn`'s own cgroup's level, read by the kernel, less its depth in
/// `/proc/self/cgroup`, which is written from the namespace's root.
pub fn cgroup_namespace_depth() -> Result<u32, String> {
    if in_initial_cgroup_namespace() {
        return Ok(0);
    }
    static DEPTH: OnceLock<Result<u32, String>> = OnceLock::new();
    DEPTH
        .get_or_init(|| {
            let why = "`omn` runs in a cgroup namespace, so a cgroup's depth under \
                       /sys/fs/cgroup is not its level in the hierarchy";
            let own = std::fs::read_to_string("/proc/self/cgroup")
                .map_err(|error| format!("{why}, and /proc/self/cgroup is unreadable: {error}"))?;
            let path = own
                .lines()
                .find_map(|line| line.strip_prefix("0::"))
                .ok_or_else(|| format!("{why}, and /proc/self/cgroup names no unified cgroup"))?;
            let depth = path.split('/').filter(|name| !name.is_empty()).count() as u32;
            let me = std::process::id();
            // Refused for permission: see `iterate`.
            let Some(tasks) = iterate()? else {
                return Ok(0);
            };
            let level = tasks
                .into_iter()
                .find(|task| task.ns_tid == Some(me))
                .map(|task| task.cgroup_level)
                .ok_or_else(|| format!("{why}, and the task iterator did not see `omn` itself"))?;
            level.checked_sub(depth).ok_or_else(|| {
                format!(
                    "{why}, and the kernel puts `omn` at level {level}, above the {depth} its \
                     own namespace counts"
                )
            })
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tid: u32, tgid: u32, levels: &[(u32, u32)], cgroup_level: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; RECORD];
        let mut put =
            |at: usize, value: u32| bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
        put(TID, tid);
        put(TGID, tgid);
        put(LEVEL, levels.len() as u32 - 1);
        put(CGROUP_LEVEL, cgroup_level);
        for (k, (nr, inode)) in levels.iter().enumerate() {
            put(NUMBERS + k * 8, *nr);
            put(NUMBERS + k * 8 + 4, *inode);
        }
        bytes
    }

    /// A task two levels down: the kernel calls it 9001, the container 42.
    #[test]
    fn the_id_omn_sees_is_the_one_at_the_level_of_its_own_namespace() {
        let task = decode(&record(9001, 9000, &[(9001, 1), (42, 77)], 5), 77);
        assert_eq!(task.ns_tid, Some(42));
        assert_eq!((task.tid, task.tgid, task.cgroup_level), (9001, 9000, 5));
    }

    /// Every attribute the collectors name a task in is rewritten, and none
    /// that only looks like one: a syscall named `getpid` is not a task.
    #[test]
    fn the_attributes_that_carry_a_task_id_are_the_ones_rewritten() {
        for name in [
            "pid",
            "tid",
            "ppid",
            "waker_tid",
            "wakee_pid",
            "holder_tid",
            "old_pid",
        ] {
            assert!(names_a_task(name), "{name}");
        }
        for name in ["syscall", "pidfd", "comm", "tids", "cpu"] {
            assert!(!names_a_task(name), "{name}");
        }
    }

    #[test]
    fn a_task_outside_omn_s_namespace_has_no_id_there() {
        assert_eq!(decode(&record(12, 12, &[(12, 1)], 1), 77).ns_tid, None);
    }

    /// Ground truth from the running machine: `omn`'s own process, through
    /// the iterator, is the pid it reads from `/proc` -- wherever it runs.
    #[test]
    fn the_iterator_finds_this_process_under_the_pid_it_has_here() {
        let me = std::process::id();
        match tasks() {
            Ok(tasks) => {
                let mine = tasks
                    .iter()
                    .find(|task| task.ns_tid == Some(me))
                    .expect("this process in the iteration");
                if in_initial_pid_namespace() {
                    assert_eq!(mine.tgid, me);
                }
            }
            Err(reason) => eprintln!("skipped: {reason}"),
        }
    }
}
