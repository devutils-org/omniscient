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
const BPF_PROG_LOAD: i32 = 5;
const BPF_RAW_TRACEPOINT_OPEN: i32 = 17;

/// Map types, same header.
pub const BPF_MAP_TYPE_PERCPU_ARRAY: u32 = 6;
pub const BPF_MAP_TYPE_RINGBUF: u32 = 27;

/// Program types.
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
    let license = CString::new(license).expect("a license has no interior NUL");
    let mut log = vec![0u8; LOG_BYTES];
    let mut attr = ProgLoadAttr {
        prog_type,
        insn_cnt: instructions.len() as u32,
        insns: instructions.as_ptr() as u64,
        license: license.as_ptr() as u64,
        // Level 1 is the rejection and the state around it. Level 2 is every
        // instruction the verifier walked, which is megabytes for anything
        // real.
        log_level: 1,
        log_size: log.len() as u32,
        log_buf: log.as_mut_ptr() as u64,
        prog_name: kernel_name(name),
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
        let error = io::Error::last_os_error();
        let end = log.iter().position(|byte| *byte == 0).unwrap_or(log.len());
        return Err(LoadError {
            error,
            verifier_log: String::from_utf8_lossy(&log[..end]).trim().to_owned(),
        });
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

/// Counts the cpus in a list such as `0-7` or `0,2-3`.
fn parse_cpu_list(text: &str) -> Option<usize> {
    let mut total = 0usize;
    for part in text.trim().split(',') {
        if part.is_empty() {
            return None;
        }
        let count = match part.split_once('-') {
            Some((first, last)) => {
                last.parse::<usize>()
                    .ok()?
                    .checked_sub(first.parse().ok()?)?
                    + 1
            }
            None => {
                part.parse::<usize>().ok()?;
                1
            }
        };
        total += count;
    }
    (total > 0).then_some(total)
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

        assert_eq!(offset_of!(RawTracepointOpenAttr, name), 0);
        assert_eq!(offset_of!(RawTracepointOpenAttr, prog_fd), 8);
        assert_eq!(size_of::<RawTracepointOpenAttr>(), 16);

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
        match prog_load(BPF_PROG_TYPE_RAW_TRACEPOINT, &program, "omn_test", "GPL") {
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
