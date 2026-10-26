// SPDX-License-Identifier: MIT OR GPL-2.0
//! The `bpf()` syscall, as a syscall.
//!
//! Each command of `bpf()` reads a different prefix of one big union. Rather
//! than model the union, each command here has a `#[repr(C)]` struct holding
//! exactly the fields it uses, and passes its own size: the kernel accepts a
//! short attribute as long as everything past it would have been zero, which is
//! what `bpf_check_uarg_tail_zero` checks on the other side.
//!
//! Nothing here interprets a failure. An `EPERM` from `BPF_PROG_LOAD` is not
//! the same answer as an `EPERM` from `mmap`, and the caller is what knows
//! which question was being asked.

use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// Commands, from `include/uapi/linux/bpf.h`.
const BPF_MAP_CREATE: i32 = 0;
const BPF_MAP_LOOKUP_ELEM: i32 = 1;
const BPF_MAP_UPDATE_ELEM: i32 = 2;
const BPF_MAP_DELETE_ELEM: i32 = 3;
const BPF_MAP_GET_NEXT_KEY: i32 = 4;
const BPF_PROG_LOAD: i32 = 5;
const BPF_RAW_TRACEPOINT_OPEN: i32 = 17;
const BPF_BTF_LOAD: i32 = 18;
const BPF_MAP_LOOKUP_AND_DELETE_ELEM: i32 = 21;
const BPF_LINK_CREATE: i32 = 28;

/// `bpf_attach_type::BPF_TRACE_UPROBE_MULTI`, Linux 6.6.
pub const BPF_TRACE_UPROBE_MULTI: u32 = 48;

/// Map types, same header.
pub const BPF_MAP_TYPE_HASH: u32 = 1;
pub const BPF_MAP_TYPE_PERCPU_ARRAY: u32 = 6;
pub const BPF_MAP_TYPE_STACK_TRACE: u32 = 7;
pub const BPF_MAP_TYPE_RINGBUF: u32 = 27;
pub const BPF_MAP_TYPE_TASK_STORAGE: u32 = 29;

/// `BPF_F_NO_PREALLOC`, which a local-storage map is refused without: its
/// entries live in the object they belong to, so there is nothing to allocate
/// up front.
pub const BPF_F_NO_PREALLOC: u32 = 1;

/// Program types.
///
/// A program's type is fixed at load time and has to match what it is attached
/// to: the kernel reports a mismatch as `EINVAL` from the attach, not from the
/// load, which is a long way from the line that chose the wrong one.
pub const BPF_PROG_TYPE_KPROBE: u32 = 2;
pub const BPF_PROG_TYPE_TRACEPOINT: u32 = 5;
pub const BPF_PROG_TYPE_PERF_EVENT: u32 = 7;
pub const BPF_PROG_TYPE_RAW_TRACEPOINT: u32 = 17;

/// How much verifier log to keep. The interesting part of a rejection is the
/// last few lines -- the instruction that failed and the state it failed in --
/// so a buffer that is too small loses exactly what is needed.
const LOG_BYTES: usize = 64 * 1024;

#[repr(C)]
#[derive(Default)]
struct MapCreateAttr {
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
    inner_map_fd: u32,
    numa_node: u32,
    map_name: [u8; 16],
    map_ifindex: u32,
    btf_fd: u32,
    btf_key_type_id: u32,
    btf_value_type_id: u32,
    btf_vmlinux_value_type_id: u32,
    map_extra: u64,
}

#[repr(C)]
#[derive(Default)]
struct BtfLoadAttr {
    btf: u64,
    btf_log_buf: u64,
    btf_size: u32,
    btf_log_size: u32,
    btf_log_level: u32,
}

#[repr(C)]
#[derive(Default)]
struct ProgLoadAttr {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
    prog_name: [u8; 16],
    prog_ifindex: u32,
    expected_attach_type: u32,
}

#[repr(C)]
#[derive(Default)]
struct RawTracepointOpenAttr {
    name: u64,
    prog_fd: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct MapElemAttr {
    map_fd: u32,
    pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

/// SAFETY: `attr` must point at `size` readable bytes shaped like the union
/// prefix that `cmd` reads.
unsafe fn bpf(cmd: i32, attr: *mut libc::c_void, size: u32) -> i64 {
    libc::syscall(libc::SYS_bpf, cmd, attr, size)
}

/// A name, truncated to the 15 characters plus NUL the kernel stores.
fn kernel_name(name: &str) -> [u8; 16] {
    let mut buffer = [0u8; 16];
    let bytes = name.as_bytes();
    let len = bytes.len().min(15);
    buffer[..len].copy_from_slice(&bytes[..len]);
    buffer
}

pub fn map_create(
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    name: &str,
) -> io::Result<OwnedFd> {
    let mut attr = MapCreateAttr {
        map_type,
        key_size,
        value_size,
        max_entries,
        map_name: kernel_name(name),
        ..Default::default()
    };
    let fd = unsafe {
        bpf(
            BPF_MAP_CREATE,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<MapCreateAttr>() as u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// A map whose key and value the kernel is told the types of.
///
/// Most maps here are byte strings to the kernel. A local-storage map is not
/// allowed to be: `bpf_local_storage_map_alloc_check` refuses one without a
/// BTF key and value, because the kernel lays the value out inside the object
/// it belongs to and wants to know what it is holding.
pub struct TypedMap<'a> {
    pub map_type: u32,
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
    pub map_flags: u32,
    pub btf_fd: RawFd,
    pub btf_key_type_id: u32,
    pub btf_value_type_id: u32,
    pub name: &'a str,
}

pub fn map_create_typed(map: &TypedMap<'_>) -> io::Result<OwnedFd> {
    let mut attr = MapCreateAttr {
        map_type: map.map_type,
        key_size: map.key_size,
        value_size: map.value_size,
        max_entries: map.max_entries,
        map_flags: map.map_flags,
        map_name: kernel_name(map.name),
        btf_fd: map.btf_fd as u32,
        btf_key_type_id: map.btf_key_type_id,
        btf_value_type_id: map.btf_value_type_id,
        ..Default::default()
    };
    let fd = unsafe {
        bpf(
            BPF_MAP_CREATE,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<MapCreateAttr>() as u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Hands the kernel a BTF blob this process built, keeping its log.
///
/// The blob describes the types of one map's key and value and nothing else;
/// the kernel checks it as strictly as it checks its own, and its complaint is
/// the only diagnosis there is, so it travels with the error.
pub fn btf_load(blob: &[u8]) -> Result<OwnedFd, LoadError> {
    let mut log = vec![0u8; LOG_BYTES];
    let mut attr = BtfLoadAttr {
        btf: blob.as_ptr() as u64,
        btf_log_buf: log.as_mut_ptr() as u64,
        btf_size: blob.len() as u32,
        btf_log_size: log.len() as u32,
        btf_log_level: 1,
    };
    let fd = unsafe {
        bpf(
            BPF_BTF_LOAD,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<BtfLoadAttr>() as u32,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        let end = log.iter().position(|byte| *byte == 0).unwrap_or(log.len());
        return Err(LoadError {
            error,
            verifier_log: String::from_utf8_lossy(&log[..end]).trim().to_owned(),
        });
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Why a program did not load.
///
/// The verifier's log is the whole diagnosis, so it travels with the error
/// rather than being printed and forgotten.
#[derive(Debug)]
pub struct LoadError {
    pub error: io::Error,
    pub verifier_log: String,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.verifier_log.is_empty() {
            write!(f, "{}", self.error)
        } else {
            write!(
                f,
                "{}; the verifier said:\n{}",
                self.error, self.verifier_log
            )
        }
    }
}

impl LoadError {
    /// Whether the kernel refused for lack of privileges rather than for
    /// anything about the program. The two call for completely different
    /// things from the reader, so they are never reported the same way.
    pub fn is_permission(&self) -> bool {
        matches!(
            self.error.raw_os_error(),
            Some(libc::EPERM) | Some(libc::EACCES)
        )
    }
}

/// Loads a program, keeping the verifier's log whether or not it loaded.
pub fn prog_load(
    prog_type: u32,
    instructions: &[crate::bpf::insn::Insn],
    name: &str,
    license: &str,
) -> Result<OwnedFd, LoadError> {
    prog_load_for(prog_type, 0, instructions, name, license)
}

/// Loads a program declared for one attach type, which some attachments check
/// at load time rather than at the attach: a uprobe-multi link refuses a
/// program that was not loaded for it.
pub fn prog_load_for(
    prog_type: u32,
    expected_attach_type: u32,
    instructions: &[crate::bpf::insn::Insn],
    name: &str,
    license: &str,
) -> Result<OwnedFd, LoadError> {
    let portable = crate::bpf::portable(instructions);
    prog_load_exactly(prog_type, expected_attach_type, &portable, name, license)
}

/// Loads `instructions` as they are, with no helper rewritten for an older
/// kernel: what the probes that decide the rewrite load.
pub fn prog_load_exactly(
    prog_type: u32,
    expected_attach_type: u32,
    instructions: &[crate::bpf::insn::Insn],
    name: &str,
    license: &str,
) -> Result<OwnedFd, LoadError> {
    let license = CString::new(license).expect("a license has no interior NUL");
    let mut log = vec![0u8; LOG_BYTES];
    // Level 1 is the rejection and the state around it. Level 2 is every
    // instruction the verifier walked, which is megabytes for anything real.
    let loaded = load_logging(
        prog_type,
        expected_attach_type,
        instructions,
        name,
        &license,
        &mut log,
        1,
    );
    match loaded {
        // Before 5.2 a level-1 log records the state at every branch, and a
        // log that fills fails the load with `ENOSPC` however valid the
        // program: Debian 10 refused `sched.runqlat`'s switch program so.
        // Loaded again without one, a valid program loads, and a refused one
        // keeps the first attempt's log, which is what it was refused with.
        Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => {
            match load_logging(
                prog_type,
                expected_attach_type,
                instructions,
                name,
                &license,
                &mut [],
                0,
            ) {
                Ok(fd) => Ok(fd),
                Err(_) => Err(load_error(error, &log)),
            }
        }
        Err(error) => Err(load_error(error, &log)),
        Ok(fd) => Ok(fd),
    }
}

fn load_error(error: io::Error, log: &[u8]) -> LoadError {
    let end = log.iter().position(|byte| *byte == 0).unwrap_or(log.len());
    LoadError {
        error,
        verifier_log: String::from_utf8_lossy(&log[..end]).trim().to_owned(),
    }
}

/// One `BPF_PROG_LOAD`, with the verifier writing into `log` at `log_level`.
fn load_logging(
    prog_type: u32,
    expected_attach_type: u32,
    instructions: &[crate::bpf::insn::Insn],
    name: &str,
    license: &CString,
    log: &mut [u8],
    log_level: u32,
) -> io::Result<OwnedFd> {
    let mut attr = ProgLoadAttr {
        prog_type,
        insn_cnt: instructions.len() as u32,
        insns: instructions.as_ptr() as u64,
        license: license.as_ptr() as u64,
        log_level,
        log_size: log.len() as u32,
        log_buf: if log.is_empty() {
            0
        } else {
            log.as_mut_ptr() as u64
        },
        prog_name: kernel_name(name),
        expected_attach_type,
        ..Default::default()
    };
    let fd = unsafe {
        bpf(
            BPF_PROG_LOAD,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<ProgLoadAttr>() as u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Attaches a loaded program to a raw tracepoint, by name.
///
/// This is the attachment that needs no tracefs: the kernel resolves the name
/// against its own table instead of the caller reading an id out of
/// `/sys/kernel/tracing`, which is mounted and unreadable on a normal machine.
pub fn raw_tracepoint_open(name: &str, prog_fd: RawFd) -> io::Result<OwnedFd> {
    let name = CString::new(name).map_err(|_| io::Error::other("a tracepoint name has no NUL"))?;
    let mut attr = RawTracepointOpenAttr {
        name: name.as_ptr() as u64,
        prog_fd: prog_fd as u32,
        pad: 0,
    };
    let fd = unsafe {
        bpf(
            BPF_RAW_TRACEPOINT_OPEN,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<RawTracepointOpenAttr>() as u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Attaches a loaded tracing program where its load named: a `tp_btf`
/// program to its tracepoint, an fentry to its function. The kernel takes
/// the place from the program's `attach_btf_id`, and refuses a name here.
pub fn tracing_open(prog_fd: RawFd) -> io::Result<OwnedFd> {
    let mut attr = RawTracepointOpenAttr {
        name: 0,
        prog_fd: prog_fd as u32,
        pad: 0,
    };
    let fd = unsafe {
        bpf(
            BPF_RAW_TRACEPOINT_OPEN,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<RawTracepointOpenAttr>() as u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

#[repr(C)]
#[derive(Default)]
struct UprobeMultiAttr {
    prog_fd: u32,
    target_fd: u32,
    attach_type: u32,
    flags: u32,
    path: u64,
    offsets: u64,
    ref_ctr_offsets: u64,
    cookies: u64,
    cnt: u32,
    multi_flags: u32,
    pid: u32,
    pad: u32,
}

/// Links a program loaded for `BPF_TRACE_UPROBE_MULTI` to one offset of a
/// file, for one process and every thread in it.
///
/// This is the uprobe route that needs only `CAP_BPF` and `CAP_PERFMON`: the
/// `uprobe` PMU behind `attach::uprobe` asks for `CAP_SYS_ADMIN`, which this
/// project does not request.
///
/// `semaphore` is the USDT semaphore's file offset, when the probe site has
/// one. The kernel then increments it while the link exists, which is what
/// makes a guarded probe fire at all: a program checks the semaphore before
/// evaluating a probe's arguments, and a probe nobody is watching costs it
/// nothing. `None` for a site with no semaphore.
pub fn link_uprobe_multi(
    prog_fd: RawFd,
    path: &std::path::Path,
    offset: u64,
    semaphore: Option<u64>,
    pid: i32,
) -> io::Result<OwnedFd> {
    link_uprobe_multi_flags(prog_fd, path, offset, semaphore, pid, 0)
}

/// `BPF_F_UPROBE_MULTI_RETURN`: the link fires when the function returns.
const BPF_F_UPROBE_MULTI_RETURN: u32 = 1;

/// [`link_uprobe_multi`] on the function's return: a uretprobe, where the
/// return value is in the context.
pub fn link_uretprobe_multi(
    prog_fd: RawFd,
    path: &std::path::Path,
    offset: u64,
    pid: i32,
) -> io::Result<OwnedFd> {
    link_uprobe_multi_flags(prog_fd, path, offset, None, pid, BPF_F_UPROBE_MULTI_RETURN)
}

fn link_uprobe_multi_flags(
    prog_fd: RawFd,
    path: &std::path::Path,
    offset: u64,
    semaphore: Option<u64>,
    pid: i32,
    multi_flags: u32,
) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::other("a path has no interior NUL"))?;
    let offsets = [offset];
    let ref_ctr_offsets = [semaphore.unwrap_or(0)];
    let mut attr = UprobeMultiAttr {
        prog_fd: prog_fd as u32,
        attach_type: BPF_TRACE_UPROBE_MULTI,
        path: path.as_ptr() as u64,
        offsets: offsets.as_ptr() as u64,
        ref_ctr_offsets: if semaphore.is_some() {
            ref_ctr_offsets.as_ptr() as u64
        } else {
            0
        },
        cnt: 1,
        multi_flags,
        pid: pid as u32,
        ..Default::default()
    };
    let fd = unsafe {
        bpf(
            BPF_LINK_CREATE,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<UprobeMultiAttr>() as u32,
        )
    };
    drop(path);
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Reads one element of a per-cpu map: one value per possible cpu.
pub fn map_lookup_percpu_u64(map_fd: RawFd, key: u32, cpus: usize) -> io::Result<Vec<u64>> {
    // The kernel writes one 8-byte value per cpu, each padded to 8 bytes, which
    // for a u64 value means exactly `cpus` slots.
    let mut values = vec![0u64; cpus];
    let mut key = key;
    let mut attr = MapElemAttr {
        map_fd: map_fd as u32,
        pad: 0,
        key: &mut key as *mut u32 as u64,
        value: values.as_mut_ptr() as u64,
        flags: 0,
    };
    let result = unsafe {
        bpf(
            BPF_MAP_LOOKUP_ELEM,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<MapElemAttr>() as u32,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(values)
}

/// Writes one element, creating it or replacing it.
///
/// `key` and `value` must be exactly the sizes the map was created with; the
/// kernel reads that many bytes through the pointers and has no way to be told
/// otherwise.
///
/// A map the programs fill is only drained from here. The writes are a perf
/// event array's slots, which hold the events the programs write into
/// (`transport.rs`), and the tests' round trip through `map_create`,
/// `map_next_key` and `map_take`, checked against the kernel instead of
/// against itself.
pub fn map_update(map_fd: RawFd, key: &[u8], value: &[u8]) -> io::Result<()> {
    let mut attr = MapElemAttr {
        map_fd: map_fd as u32,
        pad: 0,
        key: key.as_ptr() as u64,
        value: value.as_ptr() as u64,
        // BPF_ANY: create it if it is absent, overwrite it if it is there.
        flags: 0,
    };
    let result = unsafe {
        bpf(
            BPF_MAP_UPDATE_ELEM,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<MapElemAttr>() as u32,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads one element and removes it in the same operation.
///
/// This is what a drain must use rather than a lookup followed by a delete.
/// The program keeps adding to a value while userspace walks the map, so
/// anything that lands between the two halves of a read-then-delete is gone
/// with nothing recording that it existed -- a total quietly short by whatever
/// happened in that window, which is the shape of error this layer exists to
/// refuse. Hash maps have served this command since 5.14.
///
/// **Before 5.14** the kernel refuses it for a hash, and a lookup followed by
/// a delete is all there is. That is exact for the maps the drains here empty
/// once nothing writes them -- a [`crate::bpf::map::Rows`] map retired a
/// grace period ago -- and opens the window above only on a map read while
/// its programs run, which on these kernels is a map of tasks that left, one
/// entry each, rather than a count still being added to.
pub fn map_take(map_fd: RawFd, key: &[u8], value_bytes: usize) -> io::Result<Option<Vec<u8>>> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REFUSED: AtomicBool = AtomicBool::new(false);
    if !REFUSED.load(Ordering::Relaxed) {
        match elem_or_absent(BPF_MAP_LOOKUP_AND_DELETE_ELEM, map_fd, key, value_bytes) {
            Err(error) if refuses_the_command(&error) => REFUSED.store(true, Ordering::Relaxed),
            other => return other,
        }
    }
    let Some(value) = elem_or_absent(BPF_MAP_LOOKUP_ELEM, map_fd, key, value_bytes)? else {
        return Ok(None);
    };
    map_delete(map_fd, key)?;
    Ok(Some(value))
}

/// Whether an error is a kernel refusing a command or a map type it has not
/// been taught: `EINVAL`, `ENOTSUPP` (524, the kernel's own) or `EOPNOTSUPP`.
fn refuses_the_command(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL) | Some(524) | Some(libc::EOPNOTSUPP)
    )
}

/// Removes one element; an element already gone is not an error.
pub fn map_delete(map_fd: RawFd, key: &[u8]) -> io::Result<()> {
    let mut attr = MapElemAttr {
        map_fd: map_fd as u32,
        pad: 0,
        key: key.as_ptr() as u64,
        value: 0,
        flags: 0,
    };
    let result = unsafe {
        bpf(
            BPF_MAP_DELETE_ELEM,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<MapElemAttr>() as u32,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

/// Reads a value without removing it.
///
/// Needed for a stack-trace map, whose entries are shared: many blocks resolve
/// to the same stack, and taking one out on the first read would leave every
/// later sighting of that stack unresolvable.
pub fn map_lookup(map_fd: RawFd, key: &[u8], value_bytes: usize) -> io::Result<Option<Vec<u8>>> {
    elem_or_absent(BPF_MAP_LOOKUP_ELEM, map_fd, key, value_bytes)
}

fn elem_or_absent(
    cmd: i32,
    map_fd: RawFd,
    key: &[u8],
    value_bytes: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut value = vec![0u8; value_bytes];
    let mut attr = MapElemAttr {
        map_fd: map_fd as u32,
        pad: 0,
        key: key.as_ptr() as u64,
        value: value.as_mut_ptr() as u64,
        flags: 0,
    };
    let result = unsafe {
        bpf(
            cmd,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<MapElemAttr>() as u32,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some(value))
}

/// The key after `from`, or the first when `from` is `None`, or `None` at the
/// end of the map.
///
/// This is the only way to walk a map from userspace: there is no cursor and
/// no ordering worth relying on. A key inserted during the walk may or may not
/// be seen, which is why the drain that uses this takes what it finds and
/// leaves the rest for the next pass rather than claiming a snapshot.
pub fn map_next_key(
    map_fd: RawFd,
    from: Option<&[u8]>,
    key_bytes: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut next = vec![0u8; key_bytes];
    let mut attr = MapElemAttr {
        map_fd: map_fd as u32,
        pad: 0,
        // A null key asks for the first one, which is how the walk starts.
        key: from.map(|key| key.as_ptr() as u64).unwrap_or(0),
        value: next.as_mut_ptr() as u64,
        flags: 0,
    };
    let result = unsafe {
        bpf(
            BPF_MAP_GET_NEXT_KEY,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<MapElemAttr>() as u32,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some(next))
}

/// How many cpus the kernel keeps per-cpu map slots for.
///
/// This is `possible`, not `online`: the kernel sizes per-cpu values by what
/// could come online, and reading fewer slots than it wrote is a buffer
/// overflow in the other direction.
pub fn possible_cpus() -> usize {
    let text = std::fs::read_to_string("/sys/devices/system/cpu/possible").unwrap_or_default();
    parse_cpu_list(&text).unwrap_or_else(|| {
        let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
        if online > 0 {
            online as usize
        } else {
            1
        }
    })
}

/// Counts the cpus in a list such as `0-7` or `0,2-3`, with the one parser
/// the crate has for the kernel's cpu lists: an empty list is none.
fn parse_cpu_list(text: &str) -> Option<usize> {
    crate::capabilities::parse_cpu_list(text)
        .map(|cpus| cpus.len())
        .filter(|count| *count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cpu_list_is_counted_not_parsed_as_a_number() {
        assert_eq!(parse_cpu_list("0-7\n"), Some(8));
        assert_eq!(parse_cpu_list("0"), Some(1));
        assert_eq!(parse_cpu_list("0,2-3"), Some(3));
        assert_eq!(parse_cpu_list(""), None);
    }

    /// Every field of every attribute, against the offsets in
    /// `include/uapi/linux/bpf.h`.
    ///
    /// The kernel reads these structs by offset and checks that everything past
    /// the size passed in is zero. A field one slot out of place is not a crash
    /// and not an error: it is a map created with the wrong value size, or a
    /// program loaded with a log buffer where its license should be. This is
    /// the one thing in this layer that cannot be caught by reading the code.
    #[test]
    fn the_attribute_structs_match_the_kernels_layout() {
        use std::mem::{offset_of, size_of};

        assert_eq!(offset_of!(MapCreateAttr, map_type), 0);
        assert_eq!(offset_of!(MapCreateAttr, key_size), 4);
        assert_eq!(offset_of!(MapCreateAttr, value_size), 8);
        assert_eq!(offset_of!(MapCreateAttr, max_entries), 12);
        assert_eq!(offset_of!(MapCreateAttr, map_flags), 16);
        assert_eq!(offset_of!(MapCreateAttr, numa_node), 24);
        assert_eq!(offset_of!(MapCreateAttr, map_name), 28);
        assert_eq!(offset_of!(MapCreateAttr, map_ifindex), 44);
        assert_eq!(offset_of!(MapCreateAttr, btf_fd), 48);
        assert_eq!(offset_of!(MapCreateAttr, btf_key_type_id), 52);
        assert_eq!(offset_of!(MapCreateAttr, btf_value_type_id), 56);
        assert_eq!(offset_of!(MapCreateAttr, map_extra), 64);
        assert_eq!(size_of::<MapCreateAttr>(), 72);

        assert_eq!(offset_of!(ProgLoadAttr, prog_type), 0);
        assert_eq!(offset_of!(ProgLoadAttr, insn_cnt), 4);
        assert_eq!(offset_of!(ProgLoadAttr, insns), 8);
        assert_eq!(offset_of!(ProgLoadAttr, license), 16);
        assert_eq!(offset_of!(ProgLoadAttr, log_level), 24);
        assert_eq!(offset_of!(ProgLoadAttr, log_size), 28);
        assert_eq!(offset_of!(ProgLoadAttr, log_buf), 32);
        assert_eq!(offset_of!(ProgLoadAttr, kern_version), 40);
        assert_eq!(offset_of!(ProgLoadAttr, prog_flags), 44);
        assert_eq!(offset_of!(ProgLoadAttr, prog_name), 48);
        assert_eq!(offset_of!(ProgLoadAttr, expected_attach_type), 68);
        assert_eq!(size_of::<ProgLoadAttr>(), 72);

        assert_eq!(offset_of!(BtfLoadAttr, btf), 0);
        assert_eq!(offset_of!(BtfLoadAttr, btf_log_buf), 8);
        assert_eq!(offset_of!(BtfLoadAttr, btf_size), 16);
        assert_eq!(offset_of!(BtfLoadAttr, btf_log_size), 20);
        assert_eq!(offset_of!(BtfLoadAttr, btf_log_level), 24);

        assert_eq!(offset_of!(RawTracepointOpenAttr, name), 0);
        assert_eq!(offset_of!(RawTracepointOpenAttr, prog_fd), 8);
        assert_eq!(size_of::<RawTracepointOpenAttr>(), 16);

        assert_eq!(offset_of!(UprobeMultiAttr, attach_type), 8);
        assert_eq!(offset_of!(UprobeMultiAttr, path), 16);
        assert_eq!(offset_of!(UprobeMultiAttr, offsets), 24);
        assert_eq!(offset_of!(UprobeMultiAttr, ref_ctr_offsets), 32);
        assert_eq!(offset_of!(UprobeMultiAttr, cookies), 40);
        assert_eq!(offset_of!(UprobeMultiAttr, cnt), 48);
        assert_eq!(offset_of!(UprobeMultiAttr, multi_flags), 52);
        assert_eq!(offset_of!(UprobeMultiAttr, pid), 56);

        assert_eq!(offset_of!(MapElemAttr, map_fd), 0);
        assert_eq!(offset_of!(MapElemAttr, key), 8);
        assert_eq!(offset_of!(MapElemAttr, value), 16);
        assert_eq!(offset_of!(MapElemAttr, flags), 24);
        assert_eq!(size_of::<MapElemAttr>(), 32);
    }

    /// A zeroed attribute is what the kernel requires of every field the caller
    /// does not set, and `Default` is what produces one here.
    #[test]
    fn an_attribute_starts_out_entirely_zero() {
        let attr = ProgLoadAttr::default();
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &attr as *const _ as *const u8,
                std::mem::size_of::<ProgLoadAttr>(),
            )
        };
        assert!(bytes.iter().all(|byte| *byte == 0));
    }

    /// The load is refused without the capabilities on a normal machine, which
    /// is itself worth asserting: what must never happen is a load that fails
    /// and is reported as anything other than what the kernel said.
    #[test]
    fn a_refused_load_carries_the_kernels_own_error() {
        let program = [
            crate::bpf::insn::Insn::mov64_imm(0, 0),
            crate::bpf::insn::Insn::exit(),
        ];
        match prog_load(
            BPF_PROG_TYPE_RAW_TRACEPOINT,
            &program,
            "omn_test",
            "Dual MIT/GPL",
        ) {
            Ok(_) => eprintln!("this machine grants CAP_BPF: the smallest program loaded"),
            Err(error) => {
                assert!(
                    error.error.raw_os_error().is_some(),
                    "a refusal must carry an errno, not a message invented here"
                );
                assert!(
                    error.is_permission() || !error.verifier_log.is_empty(),
                    "a refusal that is not about privileges must carry the verifier's reason: {error}"
                );
            }
        }
    }

    #[test]
    fn a_kernel_name_keeps_room_for_the_terminator() {
        let name = kernel_name("omn_wakeup_far_too_long");
        assert_eq!(name[15], 0, "the name must stay NUL-terminated");
        assert_eq!(&name[..6], b"omn_wa");
    }
}
