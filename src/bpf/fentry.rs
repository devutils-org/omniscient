// SPDX-License-Identifier: MIT OR GPL-2.0
//! `bpf:fentry`: a program attached to a kernel function by its BTF type id.
//!
//! The fourth attach kind, and the one this project waited three waves for. A
//! raw tracepoint reaches the places the kernel chose to publish; a kprobe
//! reaches any symbol, and hands its program a `pt_regs` whose meaning depends
//! on the calling convention and on nothing the verifier checks. An fentry
//! program is attached to a *function*, named by its id in the kernel's own
//! BTF, and that changes three things at once:
//!
//! - **It is typed.** The verifier knows the prototype, so an argument that is
//!   a `struct file *` arrives as a pointer the verifier can follow, and a load
//!   of `file->f_inode` is checked against the running kernel's layout at load
//!   time. A load of the wrong size or at an offset that is not a member is
//!   refused: the program *fails to load* rather than reading the wrong field.
//!   `the_verifier_refuses_a_load_that_is_not_a_member` below is that claim,
//!   checked on this kernel.
//! - **It is cheaper per call.** A kprobe traps through an `int3` or an ftrace
//!   handler that saves every register; fentry is a direct call from a BPF
//!   trampoline patched into the function's own `nop`.
//! - **Its exit half sees the arguments and the return value together.** An
//!   `fexit` program's context is the entry arguments followed by the return
//!   value, so a latency and what the call returned are one event rather than
//!   two probes to be paired.
//!
//! The load carries `expected_attach_type` and `attach_btf_id`, which is what
//! makes it a tracing program; the attach is `BPF_RAW_TRACEPOINT_OPEN` with no
//! name, which the kernel routes to the trampoline for a program of this type.
//!
//! ## Why this file reads BTF itself
//!
//! `btf.rs` answers "where is this field" and "does this type exist", and
//! neither is the question an fentry load asks: it wants the *type id* of a
//! `FUNC`, and the prototype that `FUNC` points at. The shared parser keeps its
//! ids private and this stream does not own it, so this file carries a small
//! index of its own -- functions by name, and enough of the type section to
//! describe a parameter -- and the request to fold it into `Btf` is under
//! `## Needs from another stream` in `docs/open-findings.md`. It reads the
//! same file, so the two cannot disagree about which kernel is running.
//!
//! ## What a requirement `fentry:<function>` means
//!
//! That the running kernel's BTF declares a `FUNC` of that name. pahole emits a
//! `FUNC` only for functions with an address in the ftrace table -- an inlined
//! function has none -- so the presence of the entry is the kernel's own answer
//! to "can a trampoline be put here". Where this process may load a tracing
//! program, the probe goes one step further and loads the smallest one against
//! the function, which is what catches a kernel built without the trampoline
//! support that BTF alone cannot reveal.

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Mutex, OnceLock};

use crate::bpf::insn::{self, count_drop, helper, Insn, Log2Labels, Program, BPF_DW, BPF_W, R10};
use crate::bpf::map::{map_failure, Counters, Hash};
use crate::bpf::Support;

/// `BPF_PROG_TYPE_TRACING`, from `include/uapi/linux/bpf.h`. Frozen UAPI.
const BPF_PROG_TYPE_TRACING: u32 = 26;

/// `enum bpf_attach_type`: the two halves of a function.
const BPF_TRACE_FENTRY: u32 = 24;
const BPF_TRACE_FEXIT: u32 = 25;

/// `bpf()` commands this file issues itself.
const BPF_PROG_LOAD: i32 = 5;
const BPF_RAW_TRACEPOINT_OPEN: i32 = 17;

// The half-word width, the socket cookie helper and the register encodings
// this family needed first live in `insn.rs` now, beside the others: three
// files had declared some of the same, and one copy is the one a test holds.
pub(crate) use crate::bpf::insn::helper::GET_SOCKET_COOKIE;
pub(crate) use crate::bpf::insn::BPF_H;

/// How much verifier log to keep, as in `syscall.rs`.
const LOG_BYTES: usize = 64 * 1024;

const VMLINUX_BTF: &str = "/sys/kernel/btf/vmlinux";
const BTF_MAGIC: u16 = 0xeb9f;

const KIND_INT: u8 = 1;
const KIND_PTR: u8 = 2;
const KIND_ARRAY: u8 = 3;
const KIND_STRUCT: u8 = 4;
const KIND_UNION: u8 = 5;
const KIND_ENUM: u8 = 6;
const KIND_VOLATILE: u8 = 9;
const KIND_CONST: u8 = 10;
const KIND_RESTRICT: u8 = 11;
const KIND_FUNC: u8 = 12;
const KIND_FUNC_PROTO: u8 = 13;
const KIND_VAR: u8 = 14;
const KIND_DATASEC: u8 = 15;
const KIND_DECL_TAG: u8 = 17;
const KIND_TYPE_TAG: u8 = 18;
const KIND_ENUM64: u8 = 19;

/// Which half of a function a program runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// `fentry`: before the first instruction, with the arguments.
    Entry,
    /// `fexit`: after the return, with the arguments and the return value.
    Exit,
}

impl Side {
    fn attach_type(self) -> u32 {
        match self {
            Side::Entry => BPF_TRACE_FENTRY,
            Side::Exit => BPF_TRACE_FEXIT,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Side::Entry => "fentry",
            Side::Exit => "fexit",
        }
    }
}

/// A kernel function as its BTF declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    /// The `FUNC`'s type id, which is what the load names.
    pub id: u32,
    /// Each parameter's type, as a person would write it: `file *`, `size_t`.
    pub params: Vec<String>,
    /// The return type, `void` for none.
    pub returns: String,
}

impl Function {
    /// The context offset of argument `index`.
    ///
    /// A tracing program's context is an array of `u64`, one per argument in
    /// declaration order, whatever each argument's own width.
    pub fn arg(&self, index: usize) -> i16 {
        debug_assert!(
            index < self.params.len(),
            "{} has no argument {index}",
            self.name
        );
        (index * 8) as i16
    }

    /// The context offset of the return value, in an `fexit` program: the
    /// slot after the last argument.
    pub fn ret(&self) -> i16 {
        (self.params.len() * 8) as i16
    }
}

struct TypeEntry {
    kind: u8,
    vlen: u16,
    name_at: usize,
    size_or_type: u32,
    members_at: usize,
}

/// The functions the running kernel's BTF declares, and enough of the type
/// section to describe their parameters.
pub struct FunctionIndex {
    buffer: Vec<u8>,
    string_offset: usize,
    types: Vec<TypeEntry>,
    functions: HashMap<String, u32>,
}

impl std::fmt::Debug for FunctionIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FunctionIndex")
            .field("functions", &self.functions.len())
            .finish()
    }
}

impl FunctionIndex {
    fn from_kernel() -> Result<Self, String> {
        let buffer = std::fs::read(VMLINUX_BTF).map_err(|error| {
            format!(
                "cannot read {VMLINUX_BTF} ({error}); an fentry program is attached by a type id \
                 out of that file and there is nowhere else to get one"
            )
        })?;
        Self::parse(buffer)
    }

    fn parse(buffer: Vec<u8>) -> Result<Self, String> {
        if buffer.len() < 24 || u16::from_ne_bytes([buffer[0], buffer[1]]) != BTF_MAGIC {
            return Err(format!(
                "{VMLINUX_BTF} is not native-endian BTF, so no function can be looked up in it"
            ));
        }
        let word = |at: usize| u32::from_ne_bytes(buffer[at..at + 4].try_into().expect("4"));
        let header_len = word(4) as usize;
        let type_offset = header_len + word(8) as usize;
        let type_len = word(12) as usize;
        let string_offset = header_len + word(16) as usize;
        if type_offset + type_len > buffer.len() || string_offset > buffer.len() {
            return Err("the BTF header points past the end of the blob".into());
        }

        // Type id 0 is void and has no entry in the section.
        let mut types = vec![TypeEntry {
            kind: 0,
            vlen: 0,
            name_at: 0,
            size_or_type: 0,
            members_at: 0,
        }];
        let mut functions = HashMap::new();
        let mut at = type_offset;
        let end = type_offset + type_len;
        while at + 12 <= end {
            let name_at = word(at) as usize;
            let info = word(at + 4);
            let vlen = (info & 0xffff) as u16;
            let kind = ((info >> 24) & 0x1f) as u8;
            let members_at = at + 12;
            let extra = match kind {
                KIND_INT | KIND_VAR | KIND_DECL_TAG => 4,
                KIND_ARRAY => 12,
                KIND_STRUCT | KIND_UNION | KIND_DATASEC | KIND_ENUM64 => vlen as usize * 12,
                KIND_ENUM | KIND_FUNC_PROTO => vlen as usize * 8,
                _ => 0,
            };
            let id = types.len() as u32;
            if kind == KIND_FUNC && name_at != 0 {
                if let Some(name) = string(&buffer, string_offset + name_at) {
                    // The first declaration wins. A name declared twice is two
                    // static functions in two files, and which one a caller
                    // meant is not something this can know; the kernel refuses
                    // the ambiguity the same way.
                    functions.entry(name.to_owned()).or_insert(id);
                }
            }
            types.push(TypeEntry {
                kind,
                vlen,
                name_at,
                size_or_type: word(at + 8),
                members_at,
            });
            at = members_at + extra;
        }
        Ok(Self {
            buffer,
            string_offset,
            types,
            functions,
        })
    }

    fn word(&self, at: usize) -> u32 {
        u32::from_ne_bytes(self.buffer[at..at + 4].try_into().expect("4"))
    }

    fn name(&self, name_at: usize) -> &str {
        string(&self.buffer, self.string_offset + name_at).unwrap_or("")
    }

    /// The function of this name, with its prototype, or why there is none.
    pub fn function(&self, name: &str) -> Result<Function, String> {
        let id = *self.functions.get(name).ok_or_else(|| {
            format!(
                "this kernel's BTF declares no function `{name}`: it is not in this kernel, or it \
                 is inlined everywhere it is called and so has no address a trampoline could \
                 patch"
            )
        })?;
        let func = &self.types[id as usize];
        let proto = self
            .types
            .get(func.size_or_type as usize)
            .filter(|proto| proto.kind == KIND_FUNC_PROTO)
            .ok_or_else(|| format!("this kernel's `{name}` points at no prototype"))?;
        let params = (0..proto.vlen as usize)
            .map(|slot| self.describe(self.word(proto.members_at + slot * 8 + 4), 0))
            .collect();
        Ok(Function {
            name: name.to_owned(),
            id,
            params,
            returns: self.describe(proto.size_or_type, 0),
        })
    }

    /// A type as a person would write it, qualifiers stepped through because
    /// they do not change which slot a program reads.
    fn describe(&self, type_id: u32, depth: usize) -> String {
        if depth > 4 {
            return "...".to_owned();
        }
        let Some(entry) = self.types.get(type_id as usize) else {
            return "?".to_owned();
        };
        match entry.kind {
            0 => "void".to_owned(),
            KIND_PTR => format!("{} *", self.describe(entry.size_or_type, depth + 1)),
            KIND_CONST | KIND_VOLATILE | KIND_RESTRICT | KIND_TYPE_TAG => {
                self.describe(entry.size_or_type, depth + 1)
            }
            // The compiler's spelling, read as the one `expect` compares.
            KIND_INT => match self.name(entry.name_at) {
                "" => "?".to_owned(),
                name => crate::bpf::btf::gcc_integer_name(name).to_owned(),
            },
            _ => match self.name(entry.name_at) {
                "" => "?".to_owned(),
                name => name.to_owned(),
            },
        }
    }

    fn function_count(&self) -> usize {
        self.functions.len()
    }
}

fn string(buffer: &[u8], at: usize) -> Option<&str> {
    let rest = buffer.get(at..)?;
    let end = rest.iter().position(|byte| *byte == 0)?;
    std::str::from_utf8(&rest[..end]).ok()
}

/// The running kernel's functions, read once.
pub fn index() -> Result<&'static FunctionIndex, String> {
    static INDEX: OnceLock<Result<FunctionIndex, String>> = OnceLock::new();
    INDEX
        .get_or_init(FunctionIndex::from_kernel)
        .as_ref()
        .map_err(Clone::clone)
}

/// The function a program is about to be built against, refused by name when
/// its prototype is not one of the shapes the program reads.
///
/// The verifier checks every typed load, but not *which argument* a program
/// meant: `vfs_read` and `vfs_write` both take four arguments, and a program
/// that reads the byte count out of slot three would read the position pointer
/// on a kernel that swapped them. So the shape is compared first, the way
/// `Btf::tracepoint_signature` is compared for a raw tracepoint.
pub fn expect(name: &str, accepted: &[&[&str]]) -> Result<Function, String> {
    let function = index()?.function(name)?;
    if accepted.iter().any(|shape| function.params == *shape) {
        return Ok(function);
    }
    Err(format!(
        "this kernel declares `{name}({})`, and this program is written for {}: it reads its \
         arguments out of fixed slots, and against another prototype those slots hold something \
         else",
        function.params.join(", "),
        accepted
            .iter()
            .map(|shape| format!("`{name}({})`", shape.join(", ")))
            .collect::<Vec<_>>()
            .join(" or "),
    ))
}

/// The probe behind a `fentry:<function>` requirement.
///
/// Answered from the kernel's BTF, which any user can read. Where this process
/// may load a tracing program, the answer is confirmed by loading the smallest
/// one against the function -- a verifier pass over two instructions, nothing
/// attached -- and remembered, because a run asks the same question once per
/// backend per source.
///
/// The requirement is `function` or, like a raw tracepoint's,
/// `function(arg, ...)|(arg, ...)`: every prototype the program reads, in the
/// words `expect` compares. Existence alone was the question until the CI's
/// 6.17 runner: `tcp_v4_connect` takes a `sockaddr *` there and a
/// `sockaddr_unsized *` on 7.x, and a probe that asks only whether the function
/// exists says yes on a kernel whose prototype the program has never seen.
///
/// `{arch}` in the name stands for this architecture's prefix on the syscall
/// table's wrappers (`__x64_sys_read`, `__arm64_sys_read`): the catalog names
/// one function, and each architecture spells it its own way.
pub fn support(requirement: &str) -> Support {
    // `a or b`: the first function this kernel has and a call reaches, for a
    // collector that times whichever of them the syscall goes through
    // (`io.fsync`'s `do_fsync`, or `vfs_fsync` where `do_fsync` is inlined).
    if requirement.contains(" or ") {
        let mut reasons = Vec::new();
        for alternative in requirement.split(" or ") {
            match support(alternative.trim()) {
                Support::Yes => return Support::Yes,
                Support::No(reason) => reasons.push(reason),
                other => return other,
            }
        }
        return Support::No(reasons.join("; and "));
    }
    let requirement = match syscall_arch() {
        Some(arch) => requirement.replace("{arch}", arch),
        None if requirement.contains("{arch}") => {
            return Support::No(format!(
                "`fentry:{requirement}` is a syscall wrapper, and this build knows the \
                 wrappers' names and saved registers on x86_64 and aarch64, not on {}",
                std::env::consts::ARCH
            ))
        }
        None => requirement.to_owned(),
    };
    let (function, prototypes) = match crate::bpf::parse_raw_tracepoint(&requirement) {
        Ok(parsed) => parsed,
        Err(reason) => return Support::Unchecked(reason.replace("raw_tracepoint:", "fentry:")),
    };
    let answer = exists(function);
    if !matches!(answer, Support::Yes) || prototypes.is_empty() {
        return answer;
    }
    let accepted: Vec<Vec<&str>> = prototypes
        .iter()
        .map(|shape| shape.iter().map(String::as_str).collect())
        .collect();
    let accepted: Vec<&[&str]> = accepted.iter().map(Vec::as_slice).collect();
    match expect(function, &accepted) {
        Ok(_) => Support::Yes,
        Err(reason) => Support::No(reason),
    }
}

/// This architecture's infix in the syscall table's wrapper names, the
/// `x64` of `__x64_sys_read`; `None` where this build does not know them.
pub fn syscall_arch() -> Option<&'static str> {
    if cfg!(target_arch = "x86_64") {
        Some("x64")
    } else if cfg!(target_arch = "aarch64") {
        Some("arm64")
    } else {
        None
    }
}

/// Whether the kernel was built with the function tracer, by the symbols it
/// defines for it: a call site's (`__fentry__` on x86, `_mcount` on an arm64
/// built with `-pg`) or the tracer's own (`ftrace_caller`,
/// `function_trace_call`), which an arm64 kernel patching function entries
/// has and no call-site symbol (`-fpatchable-function-entry`): distroproof's
/// Debian 13, Ubuntu 24.04 and Fedora 44 arm64 were told they had no ftrace
/// where envprobe read CONFIG_FUNCTION_TRACER=y. Alpine's `virt` kernel, built
/// without it, has none of them. `/proc/kallsyms` names its symbols even where
/// `kptr_restrict` zeroes their addresses, and is read until one turns up, so
/// a kernel that has it pays a few lines. `None` where the file cannot be read.
///
/// The kernel's BTF was the first question asked, and it does not answer on
/// 5.10: its BTF, made by an older pahole, leaves most functions out, ftrace's
/// with them, and debian-11 was told it had no ftrace where envprobe read
/// CONFIG_FUNCTION_TRACER=y.
fn kernel_has_function_tracer() -> Option<bool> {
    static ANSWER: OnceLock<Option<bool>> = OnceLock::new();
    *ANSWER.get_or_init(|| {
        let file = std::fs::File::open("/proc/kallsyms").ok()?;
        function_tracer_in(std::io::BufReader::new(file))
    })
}

/// The symbols that say a kernel has the function tracer, as
/// [`kernel_has_function_tracer`] reads them from a `/proc/kallsyms`.
const FUNCTION_TRACER_SYMBOLS: [&str; 4] = [
    "__fentry__",
    "_mcount",
    "ftrace_caller",
    "function_trace_call",
];

fn function_tracer_in(kallsyms: impl std::io::BufRead) -> Option<bool> {
    let mut lines = 0usize;
    for line in kallsyms.lines() {
        let line = line.ok()?;
        lines += 1;
        if line
            .split_whitespace()
            .nth(2)
            .is_some_and(|name| FUNCTION_TRACER_SYMBOLS.contains(&name))
        {
            return Some(true);
        }
    }
    // An empty read is no answer.
    (lines > 0).then_some(false)
}

/// Whether `function` is in this kernel's BTF and, where this process may ask,
/// whether a tracing program loads against it.
fn exists(function: &str) -> Support {
    static CONFIRMED: OnceLock<Mutex<HashMap<String, Support>>> = OnceLock::new();
    let index = match index() {
        Ok(index) => index,
        Err(reason) => {
            return Support::Unchecked(format!(
                "whether `{function}` can be traced cannot be checked: {reason}"
            ))
        }
    };
    // A kernel built without ftrace compiles its functions with no `__fentry__`
    // call site, and a trampoline has nowhere to go: Alpine's `virt` kernel
    // loaded every fentry program and refused each attach with EBUSY.
    if kernel_has_function_tracer() == Some(false) {
        return Support::No(
            "this kernel was built without ftrace (no CONFIG_FUNCTION_TRACER: /proc/kallsyms \
             has no `__fentry__`, `_mcount`, `ftrace_caller` nor `function_trace_call`), so its \
             functions carry no call site a trampoline can be installed on and no fentry \
             program attaches"
                .to_owned(),
        );
    }
    let found = match index.function(function) {
        Ok(found) => found,
        Err(reason) => return Support::No(reason),
    };
    let cache = CONFIRMED.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(known) = cache.lock().expect("the probe cache").get(function) {
        return known.clone();
    }
    let probe = [Insn::mov64_imm(0, 0), Insn::exit()];
    let answer = match load(&found, Side::Entry, &probe, "omn_fentry_prb") {
        Ok(_) => on_the_path(&found),
        // Not privileged enough to ask: the BTF answer stands, and the `bpf`
        // requirement beside this one is what reports the privilege.
        Err(error) if error.is_permission() => Support::Yes,
        Err(error) => Support::No(format!(
            "`{function}` is in this kernel's BTF and a tracing program cannot be loaded against \
             it: {error}"
        )),
    };
    cache
        .lock()
        .expect("the probe cache")
        .insert(function.to_owned(), answer.clone());
    answer
}

/// What this process can do itself to reach a function, for the functions a
/// source times as the place a syscall passes.
///
/// A function in BTF has an out-of-line copy a trampoline can patch, and that
/// is all its presence says. A compiler may also inline it into a caller, and
/// then the caller that matters never calls the copy: a clang build of 7.2
/// for aarch64 inlines `vfs_write` and `vfs_read` into their syscalls and
/// `vfs_fsync` into `fsync(2)`'s, and a root kprobe on each counted no call
/// while `__arm64_sys_write` counted every one. `io.fs`, `io.fsync` and
/// `io.vfs` then loaded, attached, saw nothing and said "no loss".
fn trigger(function: &str) -> Option<fn(RawFd)> {
    fn write(fd: RawFd) {
        unsafe { libc::pwrite(fd, b"o".as_ptr() as *const libc::c_void, 1, 0) };
    }
    fn read(fd: RawFd) {
        let mut byte = [0u8; 1];
        unsafe { libc::read(fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
    }
    fn fsync(fd: RawFd) {
        unsafe { libc::fsync(fd) };
    }
    // `msync(MS_SYNC)` calls `vfs_fsync_range` itself, so a kernel that
    // inlined it into `vfs_fsync` and left the copy on msync's path is not
    // refused for what it does serve.
    fn msync(fd: RawFd) {
        unsafe {
            let page = libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if page != libc::MAP_FAILED {
                *(page as *mut u8) = b'o';
                libc::msync(page, 4096, libc::MS_SYNC);
                libc::munmap(page, 4096);
            }
        }
    }
    // A datagram this process sends itself and reads back through `recv(2)`,
    // which enters `sock_recvmsg`: on the clang build that inlines it there
    // a root kprobe counted 0 calls of it beside 44 of `inet_recvmsg`, its
    // own callee, and `net.socket` timed no receive at all.
    fn recv(_: RawFd) {
        let mut pair = [0i32; 2];
        unsafe {
            if libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, pair.as_mut_ptr()) == 0 {
                let mut byte = [0u8; 1];
                libc::send(pair[0], b"o".as_ptr() as *const libc::c_void, 1, 0);
                libc::recv(pair[1], byte.as_mut_ptr() as *mut libc::c_void, 1, 0);
                libc::close(pair[0]);
                libc::close(pair[1]);
            }
        }
    }
    match function {
        "vfs_write" => Some(write),
        "sock_recvmsg" => Some(recv),
        "vfs_read" => Some(read),
        "vfs_fsync" | "do_fsync" => Some(fsync),
        "vfs_fsync_range" => Some(msync),
        _ => None,
    }
}

/// Whether a call this process makes reaches `function`, for a function
/// [`trigger`] knows how to reach; `Yes` for any other.
///
/// Asked of a counting kprobe on this thread alone (`perf_event_open` with
/// pid 0): a kprobe sits on the same out-of-line copy a trampoline patches,
/// costs no trampoline to build -- about 170 ms each, four of which made
/// `omn list` three times slower -- and counts this thread's calls with no
/// id to translate. The trigger runs three times on a scratch file a page
/// long. None counted is `No`, with the reason: the source would attach and
/// count nothing, which is the one answer worse than a refusal. A kprobe this
/// process may not open leaves the BTF answer standing, as a load it may not
/// make does.
fn on_the_path(function: &Function) -> Support {
    let Some(trigger) = trigger(&function.name) else {
        return Support::Yes;
    };
    match count_own_calls(&function.name, trigger) {
        Ok(0) => Support::No(format!(
            "`{name}` is in this kernel's BTF and a trampoline attaches to it, but a call this \
             process made that passes `{name}` never reached it: the kernel was built with the \
             function inlined into the syscall that calls it, and the out-of-line copy a \
             trampoline patches is off that path, so a source timed here would count nothing",
            name = function.name
        )),
        Ok(_) | Err(_) => Support::Yes,
    }
}

/// The `kprobe:<function>` requirement: the function is a kernel symbol, and
/// -- where a call of this process's can be made to pass it, the triggers
/// [`on_the_path`] uses -- that call reaches it. Alpine's 6.18 inlines
/// `vfs_write` into `ksys_pwrite64` and keeps the copy `write(2)` calls: a
/// kprobe on it counted a thousand writes and none of a thousand pwrites, and
/// `io.fs` reported a program that wrote with `pwrite` as one that never wrote.
pub fn kprobe_support(function: &str) -> Support {
    let listed = std::fs::read_to_string("/proc/kallsyms").map(|symbols| {
        symbols.lines().any(|line| {
            let mut fields = line.split_whitespace();
            let (Some(_), Some(kind), Some(name)) = (fields.next(), fields.next(), fields.next())
            else {
                return false;
            };
            name == function && kind.eq_ignore_ascii_case("t")
        })
    });
    match listed {
        Ok(true) => {}
        Ok(false) => {
            return Support::No(format!(
                "this kernel has no function `{function}` among its symbols: it is not in this \
                 kernel, or inlined everywhere it is called, and a kprobe needs an address"
            ))
        }
        Err(error) => return Support::Unchecked(format!("/proc/kallsyms cannot be read: {error}")),
    }
    let Some(trigger) = trigger(function) else {
        return Support::Yes;
    };
    match count_own_calls(function, trigger) {
        Ok(0) => Support::No(format!(
            "`{function}` is a symbol of this kernel and a kprobe attaches to it, but a call \
             this process made that passes `{function}` never reached it: the kernel inlined \
             the function into that call's path and kept the copy a probe lands on for other \
             callers, so a source timed here would leave those calls out"
        )),
        Ok(_) => Support::Yes,
        // A kprobe this process cannot count with is one it cannot attach
        // either: unknown, never available.
        Err(error) => Support::Unchecked(format!(
            "whether a call reaches `{function}` could not be counted ({error})"
        )),
    }
}

fn count_own_calls(function: &str, trigger: fn(RawFd)) -> io::Result<u64> {
    use perf_event_open_sys as sys;
    let kind = crate::bpf::attach::kprobe_type().map_err(io::Error::other)?;
    let name = CString::new(function).map_err(io::Error::other)?;
    let mut attr = sys::bindings::perf_event_attr {
        type_: kind,
        size: std::mem::size_of::<sys::bindings::perf_event_attr>() as u32,
        ..Default::default()
    };
    attr.__bindgen_anon_3.kprobe_func = name.as_ptr() as u64;
    attr.set_disabled(1);
    // SAFETY: a complete attribute, and `name` outlives the call.
    let fd = unsafe {
        sys::perf_event_open(
            &mut attr,
            0,
            -1,
            -1,
            sys::bindings::PERF_FLAG_FD_CLOEXEC as _,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor the kernel just handed us, owned by nothing else.
    let counter = unsafe { OwnedFd::from_raw_fd(fd) };
    let path = std::env::temp_dir().join(format!("omn-path-probe-{}", std::process::id()));
    let c_path = CString::new(path.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    // SAFETY: a NUL-terminated path this frame owns; the file is unlinked at
    // once and closed below.
    let file = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
            0o600,
        )
    };
    if file < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        libc::unlink(c_path.as_ptr());
        libc::ftruncate(file, 4096);
        sys::ioctls::ENABLE(counter.as_raw_fd(), 0);
    }
    for _ in 0..3 {
        unsafe { libc::lseek(file, 0, libc::SEEK_SET) };
        trigger(file);
    }
    let mut value = 0u64;
    // SAFETY: eight bytes into a u64, the read format of a lone counter.
    let read = unsafe {
        sys::ioctls::DISABLE(counter.as_raw_fd(), 0);
        libc::close(file);
        libc::read(counter.as_raw_fd(), (&mut value as *mut u64).cast(), 8)
    };
    if read != 8 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

#[repr(C)]
#[derive(Default)]
struct TracingLoadAttr {
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
    prog_btf_fd: u32,
    func_info_rec_size: u32,
    func_info: u64,
    func_info_cnt: u32,
    line_info_rec_size: u32,
    line_info: u64,
    line_info_cnt: u32,
    attach_btf_id: u32,
    /// Zero means `vmlinux`; a module's function would name the module's BTF
    /// object here.
    attach_btf_obj_fd: u32,
    core_relo_cnt: u32,
}

#[repr(C)]
#[derive(Default)]
struct RawTracepointOpenAttr {
    name: u64,
    prog_fd: u32,
    pad: u32,
}

/// Why a tracing program did not load.
#[derive(Debug)]
pub struct LoadError {
    pub error: io::Error,
    pub verifier_log: String,
}

impl LoadError {
    pub fn is_permission(&self) -> bool {
        matches!(
            self.error.raw_os_error(),
            Some(libc::EPERM) | Some(libc::EACCES)
        )
    }
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

fn kernel_name(name: &str) -> [u8; 16] {
    let mut buffer = [0u8; 16];
    let len = name.len().min(15);
    buffer[..len].copy_from_slice(&name.as_bytes()[..len]);
    buffer
}

/// Loads a tracing program against one half of `function`.
fn load(
    function: &Function,
    side: Side,
    instructions: &[Insn],
    name: &str,
) -> Result<OwnedFd, LoadError> {
    load_tracing(function, side.attach_type(), instructions, name)
}

/// Loads a tracing program of any attach type against the `FUNC` it names:
/// one half of a function, or a `bpf_iter_<target>` iterator.
pub(crate) fn load_tracing(
    function: &Function,
    attach_type: u32,
    instructions: &[Insn],
    name: &str,
) -> Result<OwnedFd, LoadError> {
    let license = CString::new("Dual MIT/GPL").expect("no interior NUL");
    let mut log = vec![0u8; LOG_BYTES];
    let mut attr = TracingLoadAttr {
        prog_type: BPF_PROG_TYPE_TRACING,
        insn_cnt: instructions.len() as u32,
        insns: instructions.as_ptr() as u64,
        license: license.as_ptr() as u64,
        log_level: 1,
        log_size: log.len() as u32,
        log_buf: log.as_mut_ptr() as u64,
        prog_name: kernel_name(name),
        expected_attach_type: attach_type,
        attach_btf_id: function.id,
        ..Default::default()
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_PROG_LOAD,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<TracingLoadAttr>() as u32,
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

/// A program attached to one half of a function.
///
/// Dropping it detaches: the link is what holds the trampoline in place, and
/// the program goes when its last reference does.
pub struct Link {
    _link: OwnedFd,
    program: OwnedFd,
}

impl Link {
    /// Runs of this program the kernel skipped, and counted only for itself.
    ///
    /// A trampoline guards each program with a per-cpu counter, and on a
    /// preemptible kernel a run can be preempted halfway: the next hit of the
    /// same program on that cpu finds the counter taken and is not run at all.
    /// That is a call this source never timed, and nothing the program keeps
    /// can say so -- neither its drop counter nor its ring nor its maps saw it.
    /// The kernel counts it in `bpf_prog_info.recursion_misses`, and this is
    /// that count.
    ///
    /// **Not observed on this machine**, in three runs of a hundred threads
    /// reading a thousand files: the count stayed at zero. It is read anyway,
    /// because it is the one loss of this attach kind that nothing else can
    /// see, and a count that is zero because it was read is a different
    /// statement from one that is zero because nobody asked. The file layer's
    /// three missing calls that first looked like this were the trampoline
    /// rebuilds `entry_instructions` counts instead -- other tests of the suite
    /// attaching to `vfs_read` while those reads were in flight.
    pub fn misses(&self) -> u64 {
        recursion_misses(&self.program).unwrap_or(0)
    }
}

/// `BPF_OBJ_GET_INFO_BY_FD`.
const BPF_OBJ_GET_INFO_BY_FD: i32 = 15;

#[repr(C)]
struct InfoAttr {
    bpf_fd: u32,
    info_len: u32,
    info: u64,
}

/// Reads `recursion_misses` out of a program's `bpf_prog_info`, at the offset
/// the running kernel's own BTF gives it: the structure grows at its end with
/// every release, and the kernel copies no more than the length it is told.
///
/// Not only a trampoline's: a raw tracepoint guards each program with the same
/// per-cpu counter and counts the same skips here, which is why the block
/// layer's programs read it too.
pub fn recursion_misses(program: &OwnedFd) -> Result<u64, String> {
    static OFFSET: OnceLock<Result<usize, String>> = OnceLock::new();
    let offset = OFFSET
        .get_or_init(|| {
            crate::bpf::kernel_btf()?
                .member_offset("bpf_prog_info", "recursion_misses")
                .map(|offset| offset as usize)
        })
        .clone()?;
    let mut info = vec![0u8; offset + 8];
    let mut attr = InfoAttr {
        bpf_fd: program.as_raw_fd() as u32,
        info_len: info.len() as u32,
        info: info.as_mut_ptr() as u64,
    };
    let result = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_OBJ_GET_INFO_BY_FD,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<InfoAttr>() as u32,
        )
    };
    if result < 0 {
        return Err(format!(
            "cannot read the program's own statistics: {}",
            io::Error::last_os_error()
        ));
    }
    if (attr.info_len as usize) < offset + 8 {
        return Err("this kernel's bpf_prog_info ends before recursion_misses".to_owned());
    }
    Ok(u64::from_ne_bytes(
        info[offset..offset + 8].try_into().expect("8"),
    ))
}

/// Every run the kernel skipped across a set of links.
pub fn misses(links: &[Link]) -> u64 {
    links.iter().map(Link::misses).sum()
}

/// Loads `instructions` against one half of `function` and attaches them.
pub fn attach(
    function: &Function,
    side: Side,
    instructions: &[Insn],
    prog_name: &str,
) -> Result<Link, String> {
    let program = load(function, side, instructions, prog_name).map_err(|error| {
        if error.is_permission() {
            crate::bpf::permission_advice(&format!(
                "loading an {} program on `{}`",
                side.word(),
                function.name
            ))
        } else {
            format!(
                "the kernel refused the {} program on `{}`: {error}",
                side.word(),
                function.name
            )
        }
    })?;
    // No name: for a tracing program the kernel attaches to the function the
    // load named, through a trampoline, and a name here is refused.
    let mut attr = RawTracepointOpenAttr {
        name: 0,
        prog_fd: program.as_raw_fd() as u32,
        pad: 0,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_RAW_TRACEPOINT_OPEN,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<RawTracepointOpenAttr>() as u32,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::EPERM) | Some(libc::EACCES) => crate::bpf::permission_advice(&format!(
                "attaching an {} program to `{}`",
                side.word(),
                function.name
            )),
            _ => format!(
                "the {} program on `{}` loaded and the kernel would not attach it: {error}. A \
                 function in BTF whose trampoline cannot be installed is usually one ftrace \
                 refuses -- notrace, or already claimed by a direct call",
                side.word(),
                function.name
            ),
        });
    }
    Ok(Link {
        _link: unsafe { OwnedFd::from_raw_fd(fd as RawFd) },
        program,
    })
}

/// One program of [`attach_many`]: a function, its half, its instructions
/// and the program's name.
pub struct Request<'a> {
    pub function: &'a Function,
    pub side: Side,
    pub instructions: Vec<Insn>,
    pub prog_name: &'a str,
}

/// Several programs attached at once, each on a thread of its own, the
/// results in the order given. Installing a trampoline is mostly the kernel
/// waiting: about 120 ms an attach on Ubuntu 24.04's 6.8, so `io.vfs`'s twelve
/// functions took 1.4 s one after the other, and the reads of that time were
/// in no count (2026-10-21).
pub fn attach_many(requests: &[Request<'_>]) -> Vec<Result<Link, String>> {
    std::thread::scope(|scope| {
        let threads: Vec<_> = requests
            .iter()
            .map(|r| scope.spawn(move || attach(r.function, r.side, &r.instructions, r.prog_name)))
            .collect();
        threads
            .into_iter()
            .map(|t| {
                t.join()
                    .unwrap_or_else(|_| Err("the thread attaching a program panicked".to_owned()))
            })
            .collect()
    })
}

/// [`attach_many`] for a batch that stands or falls together: every link, or
/// the first refusal in the order given.
pub fn attach_all(requests: &[Request<'_>]) -> Result<Vec<Link>, String> {
    attach_many(requests).into_iter().collect()
}

// ---------------------------------------------------------------------------
// The timed pair: the shape most fentry sources take.
//
// An entry program stamps the calling thread and an exit program closes the
// stamp, so the latency of one call is the gap between the two halves of the
// same function on the same thread. Three sources in this build are that
// shape -- `io.syscall`, `io.fs` and `net.socket` -- and differ only in which
// functions they time and what they read out of the arguments to key a row.
// The arithmetic in between is written once, here.

/// Whose calls a program keeps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Narrowing {
    /// A thread group: a process target.
    pub tgid: Option<u32>,
    /// A thread: a thread target.
    pub tid: Option<u32>,
}

/// The label the entry program's gate jumps to when a call is not to be timed.
pub const SKIP: &str = "done";

/// The pending map: thread id to the moment it entered the function.
const PEND_KEY_BYTES: usize = 4;
const PEND_VALUE_BYTES: usize = 8;

/// Stack of the entry program.
const ENTRY_PEND_KEY: i16 = -8;
const ENTRY_PEND_VALUE: i16 = -16;
const ENTRY_DROP_KEY: i16 = -20;
/// The calling thread group, looked up in the descendants map.
const ENTRY_TGID: i16 = -24;

/// Writes the entry half of a timed pair.
///
/// `gate` runs after target narrowing, with the context in `r6`, and may jump to [`SKIP`] to
/// leave a call untimed; it may use `r1` through `r5` and `r7`.
///
/// **A stamp already there is an exit that never ran.** The functions a pair
/// times do not nest on one thread, so the only way a thread arrives at the
/// entry half with a stamp of its own still in the map is that the exit half of
/// its previous call was skipped -- and the kernel does skip it: when any
/// tracer attaches to or detaches from the same function, the trampoline is
/// rebuilt, and the old one is patched first so that calls still in flight
/// return *around* their exit programs. A second tracer attaching for thirty
/// milliseconds in the middle of a three-hundred-millisecond read removed that
/// read from this one's histogram, deterministically, with nothing counted.
/// So the stamp is written only if absent; a stamp found is counted in
/// `orphan_slot` and then replaced, so the thread is timed again from this
/// call on.
///
/// A process target is compared as a constant; with `descendants`, the map
/// of the processes it started (`sched_common::Descendants`), a thread group
/// that is not the root is looked up there, so a shell script's children are
/// timed as the script is: the root compared first, which costs the target's
/// own calls no lookup.
#[cfg(test)]
pub(crate) fn entry_instructions(
    pending_fd: i32,
    drops_fd: i32,
    drop_slot: u32,
    orphan_slot: u32,
    narrowing: Narrowing,
    descendants: Option<i32>,
    gate: &dyn Fn(&mut Program),
) -> Result<Vec<Insn>, String> {
    entry_carrying(
        pending_fd,
        drops_fd,
        drop_slot,
        orphan_slot,
        narrowing,
        descendants,
        gate,
        None,
    )
}

/// Where a kprobe entry's wide stamp sits on its stack: the time, then the
/// three key fields `fill` wrote at [`FILL_OBJECT`], [`FILL_DEV`] and
/// [`FILL_AUX`] -- one span, so the stamp is them.
const ENTRY_WIDE_VALUE: i16 = ROW_KEY + KEY_OP;
/// A kprobe entry's stamp: the time, the object, the device and the aux word.
const WIDE_VALUE_BYTES: usize = 24;

/// A source's `fill`, run at a kprobe pair's entry.
type CarriedFill<'a> = &'a dyn Fn(&mut Program) -> Result<(), String>;

/// The entry half, and -- for a kprobe pair, whose exit half sees no
/// arguments -- the source's `fill` run here, its key fields carried in the
/// stamp ([`WIDE_VALUE_BYTES`]) to the exit.
#[allow(clippy::too_many_arguments)]
fn entry_carrying(
    pending_fd: i32,
    drops_fd: i32,
    drop_slot: u32,
    orphan_slot: u32,
    narrowing: Narrowing,
    descendants: Option<i32>,
    gate: &dyn Fn(&mut Program),
    carried: Option<CarriedFill<'_>>,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::mov64_reg(7, 0));
    if let Some(tgid) = narrowing.tgid {
        program.push(Insn::mov64_reg(1, 7));
        program.push(Insn::rsh64_imm(1, 32));
        match descendants {
            None => {
                program.jump_to(SKIP, Insn::jne_imm(1, tgid as i32, 0));
            }
            // The root is a constant in the stream; the processes it started
            // since are in the map `sched_process_fork` fills, as for the
            // `sched` sources (`sched_common::Descendants`).
            Some(map) => {
                program.push(Insn::stx(BPF_W, R10, ENTRY_TGID, 1));
                program.jump_to("followed", Insn::jeq_imm(1, tgid as i32, 0));
                program.extend(Insn::load_map_fd(1, map));
                program.push(Insn::mov64_reg(2, R10));
                program.push(Insn::add64_imm(2, ENTRY_TGID as i32));
                program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
                program.jump_to(SKIP, Insn::jeq_imm(0, 0, 0));
                program.label("followed");
            }
        }
    }
    // The whole slot is cleared, then the low word written: the key is four
    // bytes and a stack slot is eight, and the kernel hashes what it is told
    // the key is -- but a word store is also the only way to take the low half
    // of the register without an instruction `insn.rs` does not have.
    program.push(Insn::st_imm(BPF_DW, R10, ENTRY_PEND_KEY, 0));
    program.push(Insn::stx(BPF_W, R10, ENTRY_PEND_KEY, 7));
    if let Some(tid) = narrowing.tid {
        program.push(Insn::ldx(BPF_W, 1, R10, ENTRY_PEND_KEY));
        program.jump_to(SKIP, Insn::jne_imm(1, tid as i32, 0));
    }
    // Reject unrelated tasks before touching their file/inode structures.
    // These hooks fire system-wide even for a scoped request. Doing the
    // source's gate first lengthened every invocation's recursion-guarded
    // window, including unrelated processes on a preemptible kernel.
    gate(&mut program);
    let value = match carried {
        None => ENTRY_PEND_VALUE,
        Some(fill) => {
            for slot in 0..(WIDE_VALUE_BYTES as i16 / 8) {
                program.push(Insn::st_imm(BPF_DW, R10, ENTRY_WIDE_VALUE + slot * 8, 0));
            }
            fill(&mut program)?;
            ENTRY_WIDE_VALUE
        }
    };
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, value, 0));
    stamp(&mut program, pending_fd, value, BPF_NOEXIST);
    program.jump_to(SKIP, Insn::jeq_imm(0, 0, 0));
    program.jump_to("full", Insn::jne_imm(0, -EEXIST, 0));
    // An exit half that never ran, counted before its stamp is replaced.
    count_drop(
        &mut program,
        drops_fd,
        ENTRY_DROP_KEY,
        orphan_slot,
        "restamp",
    );
    program.label("restamp");
    stamp(&mut program, pending_fd, value, 0);
    program.jump_to(SKIP, Insn::jsge_imm(0, 0, 0));
    // The map is full: this call will never be timed, and nothing else
    // anywhere would record that it was made.
    program.label("full");
    count_drop(&mut program, drops_fd, ENTRY_DROP_KEY, drop_slot, SKIP);
    program.label(SKIP);
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    insn::audit(&instructions)?;
    Ok(instructions)
}

/// Writes the thread's stamp with the update flags given, leaving the helper's
/// answer in `r0`.
fn stamp(program: &mut Program, pending_fd: i32, value: i16, flags: i32) {
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ENTRY_PEND_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, value as i32));
    program.push(Insn::mov64_imm(4, flags));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
}

/// Closes a pending stamp: leaves the duration in `r9` and falls through, or
/// jumps to `done` when this thread has no stamp.
///
/// Uses `r1` through `r5`, and leaves the thread's `pid_tgid` in `r7` and the
/// closing timestamp in `r8` -- one reading, so that an event's time and its
/// duration cannot disagree by whatever ran between two.
pub(crate) fn close_stamp(program: &mut Program, pending_fd: i32, key: i16, done: &'static str) {
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::mov64_reg(7, 0));
    program.push(Insn::st_imm(BPF_DW, R10, key, 0));
    program.push(Insn::stx(BPF_W, R10, key, 7));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, key as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // A thread this run never saw enter: it was already inside the function
    // when the programs attached, or the entry was not kept.
    program.jump_to(done, Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 9, 0, 0));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, key as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));
    program.push(Insn::sub64_reg(0, 9));
    program.push(Insn::mov64_reg(9, 0));
}

/// [`close_stamp`] for a kprobe pair's wide stamp: the key fields the entry
/// carried are copied into the row key before the stamp is deleted.
fn close_wide_stamp(program: &mut Program, pending_fd: i32, key: i16, done: &'static str) {
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::mov64_reg(7, 0));
    program.push(Insn::st_imm(BPF_DW, R10, key, 0));
    program.push(Insn::stx(BPF_W, R10, key, 7));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, key as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to(done, Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 9, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, 8));
    program.push(Insn::stx(BPF_DW, R10, ROW_KEY + KEY_OBJECT, 1));
    program.push(Insn::ldx(BPF_W, 1, 0, 16));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_DEV, 1));
    program.push(Insn::ldx(BPF_W, 1, 0, 20));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_AUX, 1));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, key as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));
    program.push(Insn::sub64_reg(0, 9));
    program.push(Insn::mov64_reg(9, 0));
}

/// One function a histogram times, and what its row is labelled.
#[derive(Debug, Clone)]
pub struct Timed {
    pub function: Function,
    /// The row's `op`, which is how a reader tells a read from a write.
    pub op: u32,
}

/// The row map: a key naming whose call it was and into which bucket it fell,
/// and a value holding how many, how long in total, and what they returned.
///
/// One layout for every histogram source, so one decoder serves all of them.
/// `object`, `dev` and `aux` are the source's to fill: a descriptor, an inode,
/// a socket cookie; a device; an address family and a port.
const ROW_KEY_BYTES: usize = 32;
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
const KEY_OP: i16 = 8;
const KEY_BUCKET: i16 = 12;
const KEY_OBJECT: i16 = 16;
const KEY_DEV: i16 = 24;
const KEY_AUX: i16 = 28;

const ROW_VALUE_BYTES: usize = 48;
const VALUE_COUNT: i16 = 0;
const VALUE_TOTAL_NS: i16 = 8;
/// The sum of the positive return values: bytes, for every function timed
/// here. A call that returned an error moved nothing and adds nothing.
const VALUE_BYTES: i16 = 16;
/// Calls that returned a negative value, which is an errno.
const VALUE_ERRORS: i16 = 24;
const VALUE_COMM: i16 = 32;
const COMM_BYTES: usize = 16;

/// Stack of the exit program.
const ROW_VALUE: i16 = -48;
const ROW_KEY: i16 = -80;
const EXIT_PEND_KEY: i16 = -88;
const EXIT_DROP_KEY: i16 = -92;

/// Where a `fill` callback writes the three fields that are the source's own.
pub const FILL_OBJECT: i16 = ROW_KEY + KEY_OBJECT;
pub const FILL_DEV: i16 = ROW_KEY + KEY_DEV;
pub const FILL_AUX: i16 = ROW_KEY + KEY_AUX;

/// `BPF_NOEXIST`, and the errno an insert gets when another cpu won the race.
const BPF_NOEXIST: i32 = 1;
const EEXIST: i32 = 17;

/// Slots of the histogram's drop counter.
pub const DROP_PENDING: u32 = 0;
pub const DROP_ROWS: u32 = 1;
/// Calls whose exit half the kernel skipped; see [`entry_instructions`].
pub const DROP_ORPHANED: u32 = 2;

const LOG2: Log2Labels = [
    "fx_log2_32",
    "fx_log2_16",
    "fx_log2_8",
    "fx_log2_4",
    "fx_log2_2",
    "fx_log2_1",
    "fx_log2_done",
];

/// The labels one copy of the row section jumps between. The section is
/// emitted once per row map, and a label is defined once per program, so each
/// copy has its own.
struct RowLabels {
    rows: &'static str,
    first: &'static str,
    moved: &'static str,
    first_moved: &'static str,
    first_named: &'static str,
    lost_race: &'static str,
    raced_moved: &'static str,
    raced_out: &'static str,
}

const ROWS_A: RowLabels = RowLabels {
    rows: "a_rows",
    first: "a_first",
    moved: "a_moved",
    first_moved: "a_first_moved",
    first_named: "a_first_named",
    lost_race: "a_lost_race",
    raced_moved: "a_raced_moved",
    raced_out: "a_raced_out",
};

const ROWS_B: RowLabels = RowLabels {
    rows: "b_rows",
    first: "b_first",
    moved: "b_moved",
    first_moved: "b_first_moved",
    first_named: "b_first_named",
    lost_race: "b_lost_race",
    raced_moved: "b_raced_moved",
    raced_out: "b_raced_out",
};

/// Where the exit half's generation key sits on its stack.
const EXIT_GEN_KEY: i16 = -96;

/// The maps an exit half writes into.
#[derive(Clone, Copy)]
struct ExitMaps {
    pending: i32,
    /// A one-word array: which of the two row maps is being written.
    generation: i32,
    rows: [i32; 2],
    drops: i32,
}

/// Writes the exit half of a timed pair.
///
/// `fill` runs with the context in `r6` and the duration in `r9`, both of
/// which it must leave alone; it may use `r1` through `r5` and `r7`, and
/// writes the source's own key fields at [`FILL_OBJECT`], [`FILL_DEV`] and
/// [`FILL_AUX`], which are zero when it starts.
///
/// A kretprobe's (`kprobe` the return register's offset in `pt_regs`) runs
/// no `fill`: its key fields came with the stamp.
fn exit_instructions(
    timed: &Timed,
    maps: ExitMaps,
    fill: &dyn Fn(&mut Program, &Timed) -> Result<(), String>,
    kprobe: Option<Kprobe>,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    let zero_key = |program: &mut Program| {
        for slot in 0..(ROW_KEY_BYTES as i16 / 8) {
            program.push(Insn::st_imm(BPF_DW, R10, ROW_KEY + slot * 8, 0));
        }
    };
    if kprobe.is_some() {
        zero_key(&mut program);
        close_wide_stamp(&mut program, maps.pending, EXIT_PEND_KEY, "done");
    } else {
        close_stamp(&mut program, maps.pending, EXIT_PEND_KEY, "done");
        zero_key(&mut program);
    }
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TID, 7));
    program.push(Insn::mov64_reg(1, 7));
    program.push(Insn::rsh64_imm(1, 32));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TGID, 1));
    program.push(Insn::st_imm(BPF_W, R10, ROW_KEY + KEY_OP, timed.op as i32));
    if kprobe.is_none() {
        fill(&mut program, timed)?;
    }

    // What the call returned, read after `fill` so that `fill` may use r8's
    // neighbours freely. Signed: a negative return is an errno.
    let ret = match kprobe {
        Some(kprobe) => kprobe.ret,
        None => timed.function.ret(),
    };
    program.push(Insn::ldx(BPF_DW, 8, 6, ret));
    insn::push_log2_bucket(&mut program, 9, &LOG2);
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_BUCKET, 3));

    // Which row map is being written this interval. An array's element zero
    // always exists; the null check is the verifier's, and the fallback is
    // the first map.
    program.push(Insn::st_imm(BPF_W, R10, EXIT_GEN_KEY, 0));
    program.extend(Insn::load_map_fd(1, maps.generation));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EXIT_GEN_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to(ROWS_A.rows, Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_W, 1, 0, 0));
    program.jump_to(ROWS_B.rows, Insn::jne_imm(1, 0, 0));
    row_section(&mut program, maps.rows[0], maps.drops, &ROWS_A);
    row_section(&mut program, maps.rows[1], maps.drops, &ROWS_B);

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    insn::audit(&instructions)?;
    Ok(instructions)
}

/// Adds the call to its row in one row map, creating the row if it is the
/// first, and counts a call that found no room. Every path ends at `done`.
fn row_section(program: &mut Program, rows_fd: i32, drops_fd: i32, labels: &RowLabels) {
    program.label(labels.rows);
    lookup_row(program, rows_fd);
    program.jump_to(labels.first, Insn::jeq_imm(0, 0, 0));
    accumulate(program, [labels.moved, "done"]);

    program.label(labels.first);
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_TOTAL_NS, 9));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_BYTES, 0));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_ERRORS, 0));
    program.jump_to(labels.first_moved, Insn::jsge_imm(8, 1, 0));
    program.jump_to(labels.first_named, Insn::jsge_imm(8, 0, 0));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_ERRORS, 1));
    program.jump_to(labels.first_named, Insn::ja());
    program.label(labels.first_moved);
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_BYTES, 8));
    program.label(labels.first_named);
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COMM, 0));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COMM + 8, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (ROW_VALUE + VALUE_COMM) as i32));
    program.push(Insn::mov64_imm(2, COMM_BYTES as i32));
    program.push(Insn::call(helper::GET_CURRENT_COMM));
    program.extend(Insn::load_map_fd(1, rows_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    // Only if absent: two cpus closing the first call of the same row at once
    // would otherwise both insert, and the second would overwrite the first's
    // count. The loser is told `EEXIST` and adds to the winner's row instead.
    program.push(Insn::mov64_imm(4, BPF_NOEXIST));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.jump_to(labels.lost_race, Insn::jeq_imm(0, -EEXIST, 0));
    // The histogram is full: this call is in no bucket.
    count_drop(program, drops_fd, EXIT_DROP_KEY, DROP_ROWS, "done");
    program.jump_to("done", Insn::ja());

    // Another cpu created the row between the lookup and the insert. Its row
    // is there now, so this call is added to it -- once, straight-line: a jump
    // back to the first lookup is a loop the verifier cannot bound, and it
    // refuses the program for it.
    program.label(labels.lost_race);
    lookup_row(program, rows_fd);
    program.jump_to(labels.raced_out, Insn::jeq_imm(0, 0, 0));
    accumulate(program, [labels.raced_moved, "done"]);
    // Created and gone again between two instructions. Nothing removes a row
    // from a map that is being written -- the drain takes the other one -- so
    // this is not expected to happen, and it is counted if it does.
    program.label(labels.raced_out);
    count_drop(program, drops_fd, EXIT_DROP_KEY, DROP_ROWS, "done");
    program.jump_to("done", Insn::ja());
}

/// Looks the row key up, leaving the value pointer or null in `r0`.
fn lookup_row(program: &mut Program, rows_fd: i32) {
    program.extend(Insn::load_map_fd(1, rows_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
}

/// Adds one call -- duration in `r9`, return value in `r8` -- to the row `r0`
/// points at, then jumps to the second label.
///
/// One allocation shared by every cpu, so every accumulation is atomic.
fn accumulate(program: &mut Program, [moved, done]: [&'static str; 2]) {
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_TOTAL_NS, 9));
    program.jump_to(moved, Insn::jsge_imm(8, 1, 0));
    program.jump_to(done, Insn::jsge_imm(8, 0, 0));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_ERRORS, 1));
    program.jump_to(done, Insn::ja());
    program.label(moved);
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_BYTES, 8));
    program.jump_to(done, Insn::ja());
}

/// One `(task, op, object, bucket)` row's calls over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub tid: u32,
    pub tgid: u32,
    pub op: u32,
    /// `emit::BucketLayout::Log2`'s numbering.
    pub bucket: u32,
    pub object: u64,
    pub dev: u32,
    pub aux: u32,
    pub count: u64,
    /// The exact sum of the durations, which the bucket cannot give back.
    pub total_ns: u64,
    pub bytes: u64,
    pub errors: u64,
    pub comm: String,
}

impl Row {
    pub fn bucket_low_ns(&self) -> u64 {
        insn::bucket_low_ns(self.bucket)
    }

    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        let word = |bytes: &[u8], at: i16| {
            u32::from_ne_bytes(bytes[at as usize..at as usize + 4].try_into().expect("4"))
        };
        let long = |bytes: &[u8], at: i16| {
            u64::from_ne_bytes(bytes[at as usize..at as usize + 8].try_into().expect("8"))
        };
        let comm = &value[VALUE_COMM as usize..VALUE_COMM as usize + COMM_BYTES];
        let end = comm.iter().position(|b| *b == 0).unwrap_or(comm.len());
        Some(Self {
            tid: word(key, KEY_TID),
            tgid: word(key, KEY_TGID),
            op: word(key, KEY_OP),
            bucket: word(key, KEY_BUCKET),
            object: long(key, KEY_OBJECT),
            dev: word(key, KEY_DEV),
            aux: word(key, KEY_AUX),
            count: long(value, VALUE_COUNT),
            total_ns: long(value, VALUE_TOTAL_NS),
            bytes: long(value, VALUE_BYTES),
            errors: long(value, VALUE_ERRORS),
            comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
        })
    }
}

/// What a histogram source asks of the shared shape.
pub struct HistogramSpec<'a> {
    /// A prefix for the maps and programs, which is what `bpftool` shows.
    pub name: &'static str,
    pub timed: Vec<Timed>,
    pub narrowing: Narrowing,
    /// The entry half's gate; see [`entry_instructions`].
    pub gate: &'a dyn Fn(&mut Program, &Timed),
    /// The exit half's key fields; see [`exit_instructions`].
    pub fill: &'a dyn Fn(&mut Program, &Timed) -> Result<(), String>,
    pub max_pending: u32,
    pub max_rows: u32,
    /// A kprobe pair rather than a trampoline: `gate` and `fill` read the
    /// arguments out of `pt_regs` and follow pointers with probe reads, and
    /// `fill` runs at the entry.
    pub kprobe: Option<Kprobe>,
}

/// What a kprobe pair needs that a trampoline pair does not.
#[derive(Debug, Clone, Copy)]
pub struct Kprobe {
    /// The return register's offset in `pt_regs`.
    pub ret: i16,
}

/// A loaded, attached histogram and the maps it counts into.
///
/// **Two row maps, and the drain only ever empties the one nobody is writing.**
/// The first version drained one map with lookup-and-delete while the programs
/// wrote into it, which is what every counting program in this layer did, and
/// it loses calls without counting them: an exit half that has looked a row up
/// adds to it a few instructions later, and a drain that took the row in
/// between has already copied it out -- the increment lands in a freed element,
/// or, the map being preallocated, in whatever key the element was handed to
/// next. Measured: a hundred thousand reads, 55192 counted, 44807 dropped, one
/// gone without a trace.
///
/// So a one-word array says which map is being written. A drain points the
/// word at the other map, which is empty, **waits for a grace period**, and
/// then empties the map it retired: every exit half runs inside an RCU
/// read-side section (the trampoline takes `rcu_read_lock()` around it), so
/// once `membarrier(MEMBARRIER_CMD_GLOBAL)` -- `synchronize_rcu()` from
/// userspace -- has returned, no exit half that read the old word is still
/// running, and the retired map is written by no one. `map::Rows` is the same
/// scheme with the word made a map-of-maps; see it for why the kernel's own
/// map update is not the grace period.
///
/// Until the twelfth wave the drain emptied the map retired *a tick ago* and
/// the last drain slept a millisecond: a bet that an exit half runs for
/// microseconds, which a preemptible kernel does not keep -- stream K saw
/// thousands of recursion misses on `sys_exit` under eight busy threads, each
/// a run preempted mid-program. The wait costs 5 to 18 ms per drain on this
/// machine, in the drain's thread; in exchange no row is a tick late and the
/// last drain is no special case.
pub struct HistogramProgram {
    pending: Hash,
    rows: [Hash; 2],
    flip: std::rc::Rc<Flip>,
    drops: Counters,
    links: Vec<Link>,
    /// A kprobe pair's: one perf event per cpu, each running the program.
    probes: Vec<crate::bpf::attach::PerfAttachment>,
    /// The processes a process target started, followed by the entry
    /// programs: a launched shell script's calls are its children's.
    descendants: Option<crate::bpf::sched_common::Descendants>,
}

/// What one drain found beside the rows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainLosses {
    pub vanished: u64,
    pub truncated: bool,
}

/// One drain: the rows, what the drain itself lost, and the interval the rows
/// were counted over -- from the drain before this one to this one's flip.
#[derive(Debug, Default)]
pub struct Drained {
    pub rows: Vec<Row>,
    pub losses: DrainLosses,
    pub opened_ns: u64,
    pub closed_ns: u64,
}

/// `BPF_MAP_TYPE_ARRAY` and `BPF_MAP_UPDATE_ELEM`, which `syscall.rs` does
/// not offer outside its own tests.
const BPF_MAP_TYPE_ARRAY: u32 = 2;
const BPF_MAP_UPDATE_ELEM: i32 = 2;

#[repr(C)]
struct MapUpdateAttr {
    map_fd: u32,
    pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

/// Writes one `u32` into element zero of a one-word array.
/// The generation word and what the drain knows of it, shared with the
/// round's registry so a round can retire it ahead of the drain
/// ([`crate::bpf::retire_owned_by`]).
struct Flip {
    generation: OwnedFd,
    /// Which map the programs are writing now.
    writing: std::cell::Cell<usize>,
    /// When the current map started being written.
    flipped_at: std::cell::Cell<u64>,
    /// The map retired ahead of the drain: which, the grace period it waits
    /// for, and the interval it was written over.
    retired: std::cell::Cell<Option<(usize, u64, u64, u64)>>,
}

impl Flip {
    /// Points the programs at the idle map; returns the retired one and the
    /// interval it was written over.
    fn flip(&self) -> io::Result<(usize, u64, u64)> {
        let retired = self.writing.get();
        let idle = 1 - retired;
        write_word(&self.generation, idle as u32)?;
        let now = crate::clock::monotonic_ns();
        let opened = self.flipped_at.replace(now);
        self.writing.set(idle);
        Ok((retired, opened, now))
    }
}

impl crate::bpf::map::Retire for Flip {
    fn retire(&self) -> io::Result<bool> {
        if self.retired.get().is_some() {
            return Ok(false);
        }
        let (retired, opened, closed) = self.flip()?;
        let needed = crate::bpf::map::next_grace_period();
        self.retired.set(Some((retired, needed, opened, closed)));
        Ok(true)
    }
}

fn write_word(map: &OwnedFd, value: u32) -> io::Result<()> {
    let key = 0u32;
    let mut attr = MapUpdateAttr {
        map_fd: map.as_raw_fd() as u32,
        pad: 0,
        key: &key as *const u32 as u64,
        value: &value as *const u32 as u64,
        flags: 0,
    };
    let result = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_MAP_UPDATE_ELEM,
            &mut attr as *mut _ as *mut libc::c_void,
            std::mem::size_of::<MapUpdateAttr>() as u32,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl HistogramProgram {
    pub fn load(spec: HistogramSpec<'_>) -> Result<Self, String> {
        if !crate::bpf::map::grace_periods_offered() {
            return Err(
                "this kernel offers no way to wait for a grace period from userspace \
                 (`membarrier(MEMBARRIER_CMD_GLOBAL)` is withdrawn under `nohz_full`), and \
                 without one a drain would race the programs writing the map it reads"
                    .to_owned(),
            );
        }
        let pending = Hash::create(
            PEND_KEY_BYTES,
            if spec.kprobe.is_some() {
                WIDE_VALUE_BYTES
            } else {
                PEND_VALUE_BYTES
            },
            spec.max_pending,
            &format!("{}_pend", spec.name),
        )
        .map_err(|error| map_failure(&format!("{} pending map", spec.name), error))?;
        let row_map = |which: &str| {
            Hash::create(
                ROW_KEY_BYTES,
                ROW_VALUE_BYTES,
                spec.max_rows,
                &format!("{}_rows{which}", spec.name),
            )
            .map_err(|error| map_failure(&format!("{} histogram map", spec.name), error))
        };
        let rows = [row_map("a")?, row_map("b")?];
        let generation = crate::bpf::syscall::map_create(
            BPF_MAP_TYPE_ARRAY,
            4,
            4,
            1,
            &format!("{}_gen", spec.name),
        )
        .map_err(|error| map_failure(&format!("{} generation word", spec.name), error))?;
        let drops = Counters::create(3, &format!("{}_drop", spec.name))
            .map_err(|error| map_failure(&format!("{} drop counter", spec.name), error))?;
        let descendants = match spec.narrowing.tgid {
            Some(root) => Some(crate::bpf::sched_common::Descendants::follow(root)?),
            None => None,
        };
        let followed = descendants
            .as_ref()
            .map(crate::bpf::sched_common::Descendants::map_fd);
        let maps = ExitMaps {
            pending: pending.as_raw_fd(),
            generation: generation.as_raw_fd(),
            rows: [rows[0].as_raw_fd(), rows[1].as_raw_fd()],
            drops: drops.as_raw_fd(),
        };

        // Every instruction stream is built before anything is attached, so a
        // kernel that refuses one of them refuses the source before any of it
        // runs.
        // A process target is followed into the processes it starts: without
        // it `omn trace io.syscall -- ./script.sh` timed the shell's own calls
        // and none of the programs it ran, and read as an idle program.
        let mut built = Vec::with_capacity(spec.timed.len());
        for timed in &spec.timed {
            let carried = |program: &mut Program| (spec.fill)(program, timed);
            let entry = entry_carrying(
                pending.as_raw_fd(),
                drops.as_raw_fd(),
                DROP_PENDING,
                DROP_ORPHANED,
                spec.narrowing,
                followed,
                &|program| (spec.gate)(program, timed),
                spec.kprobe
                    .map(|_| &carried as &dyn Fn(&mut Program) -> Result<(), String>),
            )?;
            let exit = exit_instructions(timed, maps, spec.fill, spec.kprobe)?;
            built.push((timed, entry, exit));
        }

        // Every exit before any entry. A stamp whose exit is not attached yet
        // is a call that will never be closed -- harmless, the next call on the
        // thread overwrites it -- whereas the other order is never worse than
        // this one and sometimes is.
        // Each half's batch at once (`attach_many`), the exits' first.
        let (exit_name, entry_name) = (format!("{}_x", spec.name), format!("{}_e", spec.name));
        let mut probes = Vec::new();
        if spec.kprobe.is_some() {
            use crate::bpf::attach::{kprobe, Scope};
            for (timed, _, exit) in &built {
                probes.push(kprobe(
                    &timed.function.name,
                    0,
                    true,
                    exit,
                    &exit_name,
                    Scope::Everywhere,
                )?);
            }
            for (timed, entry, _) in &built {
                probes.push(kprobe(
                    &timed.function.name,
                    0,
                    false,
                    entry,
                    &entry_name,
                    Scope::Everywhere,
                )?);
            }
        }
        let built = if spec.kprobe.is_some() {
            Vec::new()
        } else {
            built
        };
        let exits: Vec<_> = built
            .iter()
            .map(|(timed, _, exit)| Request {
                function: &timed.function,
                side: Side::Exit,
                instructions: exit.clone(),
                prog_name: &exit_name,
            })
            .collect();
        let entries: Vec<_> = built
            .iter()
            .map(|(timed, entry, _)| Request {
                function: &timed.function,
                side: Side::Entry,
                instructions: entry.clone(),
                prog_name: &entry_name,
            })
            .collect();
        let mut links = attach_all(&exits)?;
        links.extend(attach_all(&entries)?);
        let now = crate::clock::monotonic_ns();
        Ok(Self {
            pending,
            rows,
            flip: {
                let flip = std::rc::Rc::new(Flip {
                    generation,
                    writing: std::cell::Cell::new(0),
                    flipped_at: std::cell::Cell::new(now),
                    retired: std::cell::Cell::new(None),
                });
                let shared: std::rc::Rc<dyn crate::bpf::map::Retire> = flip.clone();
                crate::bpf::map::register(&shared);
                flip
            },
            drops,
            links,
            probes,
            descendants,
        })
    }

    /// Takes every row counted since the last drain, and starts a new
    /// interval.
    ///
    /// Every drain is whole: the programs are pointed at the empty map, a
    /// grace period is waited for, and only then is the retired map read -- so
    /// a call is in this drain or in the next one, never lost between them and
    /// never a tick late. `last` is kept for the callers, and asks for nothing
    /// more: the last drain is an ordinary one.
    ///
    /// Retired ahead by its round, the map retired then is read, once a grace
    /// period that began after it has ended: the round's own.
    pub fn drain(&self, _last: bool) -> Result<Drained, String> {
        let (retired, needed, opened, closed) = match self.flip.retired.take() {
            Some(ahead) => ahead,
            None => {
                let (retired, opened, closed) = self
                    .flip
                    .flip()
                    .map_err(|error| format!("cannot flip the histogram's generation: {error}"))?;
                (
                    retired,
                    crate::bpf::map::next_grace_period(),
                    opened,
                    closed,
                )
            }
        };
        if !crate::bpf::map::grace_period_ended(needed) {
            crate::bpf::map::grace_period().map_err(|error| {
                format!("cannot wait for the histogram's grace period: {error}")
            })?;
        }
        let mut drained = Drained {
            opened_ns: opened,
            closed_ns: closed,
            ..Default::default()
        };
        self.take(retired, &mut drained)?;
        Ok(drained)
    }

    fn take(&self, which: usize, into: &mut Drained) -> Result<(), String> {
        let taken = self.rows[which]
            .drain()
            .map_err(|error| format!("cannot read the histogram: {error}"))?;
        into.losses.vanished += taken.vanished;
        into.losses.truncated |= taken.truncated;
        for (key, value) in &taken.entries {
            match Row::decode(key, value) {
                Some(row) => into.rows.push(row),
                None => into.losses.vanished += 1,
            }
        }
        Ok(())
    }

    /// The stamps still in the map, split into calls genuinely in flight and
    /// calls whose exit was skipped: a stamp whose thread no longer exists
    /// cannot be a call in progress.
    pub fn still_pending(&self) -> (usize, usize) {
        split_stamps(&self.pending)
    }

    /// Calls the programs could not record, as `(pending, rows, orphaned)`.
    pub fn dropped(&self) -> (u64, u64, u64) {
        (
            self.drops.read(DROP_PENDING),
            self.drops.read(DROP_ROWS),
            self.drops.read(DROP_ORPHANED),
        )
    }

    /// Runs the kernel skipped; see [`Link::misses`]. The programs that
    /// follow a process target's children are among them: a fork they did not
    /// see is a child none of whose calls is timed.
    pub fn missed(&self) -> u64 {
        misses(&self.links)
            + self
                .probes
                .iter()
                .map(|probe| recursion_misses(probe.program()).unwrap_or(0))
                .sum::<u64>()
            + self
                .descendants
                .as_ref()
                .map_or(0, crate::bpf::sched_common::Descendants::skipped)
    }

    /// Processes the target started that are not followed, because the map
    /// of them was full.
    pub fn unfollowed(&self) -> u64 {
        self.descendants
            .as_ref()
            .map_or(0, crate::bpf::sched_common::Descendants::missed)
    }
}

/// Splits a pending map keyed by thread id into `(in flight, orphaned)`.
///
/// `/proc/<tid>` answers for any thread on the machine, leader or not, so a
/// stamp whose thread is gone is an exit half that never ran rather than a call
/// still going. Shared with the streaming sources, whose stamps are keyed the
/// same way.
pub fn split_stamps(pending: &Hash) -> (usize, usize) {
    let Ok((keys, _)) = pending.keys() else {
        return (0, 0);
    };
    let mut split = (0, 0);
    for key in keys {
        let Some(tid) = key.get(..4) else { continue };
        let tid = u32::from_ne_bytes(tid.try_into().expect("4"));
        if std::path::Path::new("/proc").join(tid.to_string()).exists() {
            split.0 += 1;
        } else {
            split.1 += 1;
        }
    }
    split
}

/// How many functions this kernel's BTF declares, for a message that has to
/// say how much was searched.
pub fn function_count() -> usize {
    index().map(FunctionIndex::function_count).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unrelated_task_skips_the_source_gate_before_reading_its_objects() {
        let marker = Insn::mov64_imm(7, 1234567);
        let instructions = entry_instructions(
            1,
            2,
            0,
            2,
            Narrowing {
                tgid: Some(42),
                tid: None,
            },
            None,
            &|program| {
                program.push(marker);
            },
        )
        .unwrap();
        let gate = instructions
            .iter()
            .position(|insn| *insn == marker)
            .unwrap();
        let rejection = instructions
            .iter()
            .position(|insn| insn.code == 0x55 && insn.imm == 42)
            .unwrap();
        assert!(rejection < gate);
        let destination = rejection + 1 + instructions[rejection].off as usize;
        assert!(destination > gate);
    }

    /// The function tracer is read off the symbols each architecture's ftrace
    /// defines, as distroproof's guests printed them: x86 has `__fentry__`,
    /// an arm64 patching function entries only the tracer's own, and
    /// Alpine's `virt` kernel, built without ftrace, none.
    #[test]
    fn the_function_tracer_is_read_off_either_architecture_s_symbols() {
        let read = |text: &str| function_tracer_in(std::io::Cursor::new(text.to_owned()));
        let x86 = "0000000000000000 T __fentry__\n0000000000000000 T ftrace_caller\n";
        let arm64 = "0000000000000000 t ftrace_stub\n0000000000000000 T ftrace_caller\n";
        let virt = "0000000000000000 T vfs_read\n0000000000000000 T vfs_write\n";
        assert_eq!(read(x86), Some(true));
        assert_eq!(read(arm64), Some(true));
        assert_eq!(read(virt), Some(false));
        assert_eq!(read(""), None);
    }

    /// The check counts a call that does reach its function: the syscall
    /// wrapper `write(2)` enters, which nothing inlines, sees the trigger's
    /// three writes. A kprobe this process may not open skips, loudly.
    #[test]
    fn a_function_on_the_syscall_path_counts_the_calls_that_pass_it() {
        let Some(arch) = syscall_arch() else { return };
        match count_own_calls(
            &format!("__{arch}_sys_pwrite64"),
            trigger("vfs_write").unwrap(),
        ) {
            Ok(seen) => assert_eq!(seen, 3, "three pwrites, each through the wrapper"),
            Err(error) => eprintln!("skipped: a counting kprobe needs CAP_PERFMON here: {error}"),
        }
    }

    #[test]
    fn only_the_functions_a_source_times_at_a_syscall_have_a_trigger() {
        for function in [
            "vfs_read",
            "vfs_write",
            "vfs_fsync",
            "vfs_fsync_range",
            "sock_recvmsg",
        ] {
            assert!(trigger(function).is_some(), "{function}");
        }
        assert!(trigger("tcp_v4_connect").is_none());
    }

    fn maps() -> ExitMaps {
        ExitMaps {
            pending: 3,
            generation: 6,
            rows: [5, 7],
            drops: 4,
        }
    }

    /// The exit half writes into whichever row map the generation word names,
    /// and into no other: both maps appear in it, each in its own section, and
    /// the word is read before either.
    #[test]
    fn the_exit_half_writes_into_the_map_the_generation_names() {
        let Some(index) = functions() else { return };
        let timed = Timed {
            function: index.function("vfs_read").expect("vfs_read"),
            op: 0,
        };
        let exit = exit_instructions(&timed, maps(), &|_, _| Ok(()), None).expect("exit");
        let loads = |fd: i32| {
            exit.iter()
                .enumerate()
                .filter(|(_, insn)| insn.code == 0x18 && insn.imm == fd)
                .map(|(at, _)| at)
                .collect::<Vec<_>>()
        };
        let generation = loads(6);
        let (a, b) = (loads(5), loads(7));
        assert_eq!(generation.len(), 1, "the word is read once");
        assert!(!a.is_empty() && !b.is_empty(), "both maps are written");
        assert!(
            generation[0] < a[0] && generation[0] < b[0],
            "the word is read before either map is touched"
        );
        assert!(
            a.iter().all(|at| b.iter().all(|other| at < other)),
            "one section per map"
        );
    }

    fn functions() -> Option<&'static FunctionIndex> {
        match index() {
            Ok(index) => Some(index),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    /// The load attribute is read by offset against `union bpf_attr`, and a
    /// field one slot out is not an error: it is a program attached to the
    /// wrong function, or a log buffer where the license should be.
    #[test]
    fn the_load_attribute_matches_the_kernels_layout() {
        use std::mem::{offset_of, size_of};
        assert_eq!(offset_of!(TracingLoadAttr, expected_attach_type), 68);
        assert_eq!(offset_of!(TracingLoadAttr, prog_btf_fd), 72);
        assert_eq!(offset_of!(TracingLoadAttr, func_info), 80);
        assert_eq!(offset_of!(TracingLoadAttr, line_info), 96);
        assert_eq!(offset_of!(TracingLoadAttr, attach_btf_id), 108);
        assert_eq!(offset_of!(TracingLoadAttr, attach_btf_obj_fd), 112);
        assert_eq!(size_of::<TracingLoadAttr>(), 120);
        assert_eq!(size_of::<RawTracepointOpenAttr>(), 16);
    }

    /// The prototypes the five sources are written against, read back from
    /// this kernel. If one of these moves, the program refuses by name rather
    /// than reading a slot that now holds something else -- and this test is
    /// where the move is first seen.
    #[test]
    fn the_functions_the_five_sources_time_have_the_prototypes_they_read() {
        let Some(index) = functions() else { return };
        for (name, params) in [
            ("vfs_read", &["file *", "char *", "size_t", "loff_t *"][..]),
            ("vfs_write", &["file *", "char *", "size_t", "loff_t *"][..]),
            ("ksys_read", &["unsigned int", "char *", "size_t"][..]),
            ("ksys_write", &["unsigned int", "char *", "size_t"][..]),
            (
                "vfs_fsync_range",
                &["file *", "loff_t", "loff_t", "int"][..],
            ),
            ("tcp_finish_connect", &["sock *", "sk_buff *"][..]),
            ("sock_recvmsg", &["socket *", "msghdr *", "int"][..]),
        ] {
            let function = index.function(name).expect("a function every kernel has");
            assert_eq!(function.params, params, "{name}");
            assert!(function.id > 0);
        }
        // Two shapes: `sockaddr *` through 6.17 (the CI's runner), and
        // `sockaddr_unsized *` since 7.x (this machine). The program reads
        // neither argument, and accepts both; the test holds the kernel to
        // one of them rather than to the one the developer's machine has.
        let connect = index.function("tcp_v4_connect").expect("tcp_v4_connect");
        assert!(
            super::super::net_connect::CONNECT_SIGNATURES
                .iter()
                .any(|shape| connect.params == *shape),
            "this kernel's `tcp_v4_connect({})` is none of the shapes the connect program reads",
            connect.params.join(", ")
        );
    }

    /// The probe asks what the attach asks: a prototype the requirement does
    /// not list is missing, by name, even though the function exists.
    #[test]
    fn a_requirement_with_a_prototype_is_missing_when_the_kernel_declares_another() {
        let Some(index) = functions() else { return };
        // A syscall wrapper: the dispatcher calls it from another unit, so no
        // kernel inlines it, and its presence is not one the path check doubts
        // (`vfs_read` was, until a clang build put it off the path).
        let Some(arch) = syscall_arch() else { return };
        let name = format!("__{arch}_sys_read");
        let read = index.function(&name).expect("the read syscall's wrapper");
        let declared = format!("{name}({})", read.params.join(", "));
        assert!(
            matches!(support(&declared), Support::Yes),
            "the kernel's own prototype must be accepted: {declared}"
        );
        let two = format!("{name}(int)|({})", read.params.join(", "));
        assert!(matches!(support(&two), Support::Yes), "{two}");
        let Support::No(reason) = support(&format!("{name}(int)")) else {
            panic!("a prototype the kernel does not declare is missing");
        };
        assert!(reason.contains(&format!("`{name}(int)`")), "{reason}");
        assert!(reason.contains("pt_regs *"), "{reason}");
        let Support::Unchecked(reason) = support(&format!("{name}(int")) else {
            panic!("a malformed requirement cannot be checked");
        };
        assert!(reason.contains("fentry:"), "{reason}");
    }

    /// A function the kernel does not have is missing, not unknown: the answer
    /// is in a file this process read.
    #[test]
    fn a_function_this_kernel_does_not_have_is_missing_rather_than_unknown() {
        if functions().is_none() {
            return;
        }
        let Support::No(reason) = support("definitely_not_a_kernel_function") else {
            panic!("an absent function must be reported missing");
        };
        assert!(reason.contains("declares no function"), "{reason}");
    }

    /// A prototype that is not one of the accepted shapes is refused, and the
    /// refusal carries both the shape found and the shape wanted.
    #[test]
    fn a_prototype_that_moved_is_refused_by_name() {
        if functions().is_none() {
            return;
        }
        let error = expect("vfs_read", &[&["int"]]).unwrap_err();
        assert!(error.contains("file *"), "{error}");
        assert!(error.contains("`vfs_read(int)`"), "{error}");
    }

    /// The return value of an `fexit` program is in the slot after the last
    /// argument, and a program that read it one slot early would report the
    /// position pointer as a byte count.
    #[test]
    fn the_return_value_sits_after_the_arguments() {
        let Some(index) = functions() else { return };
        let read = index.function("vfs_read").expect("vfs_read");
        assert_eq!(read.ret(), 32);
        assert_eq!(read.arg(2), 16);
    }

    /// A process target's entry half keeps the root by a constant and the
    /// processes it started by a lookup in the descendants map; without the
    /// map it keeps the root alone, and a whole-machine one compares nothing.
    #[test]
    fn a_process_target_s_entry_half_looks_its_children_up() {
        const DESCENDANTS: i32 = 9;
        let lookups = |narrowing: Narrowing, descendants: Option<i32>| {
            let program =
                entry_instructions(3, 4, 0, 2, narrowing, descendants, &|_| {}).expect("entry");
            let loads = program
                .iter()
                .filter(|insn| insn.code == 0x18 && insn.imm == DESCENDANTS)
                .count();
            let calls = program
                .iter()
                .filter(|insn| insn.code == 0x85 && insn.imm == helper::MAP_LOOKUP_ELEM)
                .count();
            (loads, calls)
        };
        let process = Narrowing {
            tgid: Some(4242),
            tid: None,
        };
        let (followed_loads, followed_calls) = lookups(process, Some(DESCENDANTS));
        let (root_loads, root_calls) = lookups(process, None);
        assert_eq!((followed_loads, root_loads), (1, 0));
        assert_eq!(followed_calls, root_calls + 1, "one lookup more, the map's");
        assert_eq!(lookups(Narrowing::default(), None), (0, root_calls));
    }

    /// A process target with its descendants map compares the root first and
    /// looks every other thread group up in the map; without the map it
    /// compares the root alone, and the machine compares nothing.
    #[test]
    fn a_process_target_follows_the_processes_it_started() {
        let lookups = |program: &[Insn]| {
            program
                .iter()
                .filter(|insn| insn.code == 0x85 && insn.imm == helper::MAP_LOOKUP_ELEM)
                .count()
        };
        let root = Narrowing {
            tgid: Some(4242),
            tid: None,
        };
        let followed = entry_instructions(3, 4, 0, 2, root, Some(9), &|_| {}).expect("entry");
        assert!(
            followed
                .iter()
                .any(|insn| insn.code == Insn::jeq_imm(1, 0, 0).code && insn.imm == 4242),
            "the root is compared before the lookup"
        );
        let alone = entry_instructions(3, 4, 0, 2, root, None, &|_| {}).expect("entry");
        assert_eq!(
            lookups(&followed),
            lookups(&alone) + 1,
            "one lookup more, of the descendants map"
        );
        let machine =
            entry_instructions(3, 4, 0, 2, Narrowing::default(), Some(9), &|_| {}).expect("entry");
        assert_eq!(
            lookups(&machine),
            lookups(&alone),
            "a machine target follows nothing"
        );
    }

    /// Both halves of the pair take one timestamp each, from the monotonic
    /// clock and no other.
    #[test]
    fn the_pair_reads_the_monotonic_clock_and_no_other() {
        let Some(index) = functions() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        let timed = Timed {
            function: index.function("vfs_read").expect("vfs_read"),
            op: 0,
        };
        let entry =
            entry_instructions(3, 4, 0, 2, Narrowing::default(), None, &|_| {}).expect("entry");
        let exit = exit_instructions(&timed, maps(), &|_, _| Ok(()), None).expect("exit");
        for (what, program) in [("entry", entry), ("exit", exit)] {
            let calls: Vec<i32> = program
                .iter()
                .filter(|insn| insn.code == 0x85)
                .map(|insn| insn.imm)
                .collect();
            assert_eq!(
                calls
                    .iter()
                    .filter(|id| **id == helper::KTIME_GET_NS)
                    .count(),
                1,
                "the {what} half"
            );
            assert!(!calls.contains(&KTIME_GET_BOOT_NS), "the {what} half");
        }
    }

    /// Four atomic adds per path, one per field a row accumulates, and no
    /// plain store into a row another cpu can see.
    #[test]
    fn a_row_is_accumulated_atomically() {
        let Some(index) = functions() else { return };
        let timed = Timed {
            function: index.function("vfs_read").expect("vfs_read"),
            op: 0,
        };
        let exit = exit_instructions(&timed, maps(), &|_, _| Ok(()), None).expect("exit");
        // Four times over: the ordinary path and the one a lost insert race
        // takes, in each of the two row maps.
        assert_eq!(exit.iter().filter(|insn| insn.code == 0xdb).count(), 16);
    }

    /// Every byte of the row key and value is written by the program, and no
    /// region of the stack overlaps another.
    #[test]
    fn the_row_layout_covers_every_byte_and_the_stack_does_not_overlap() {
        let key = [
            (KEY_TID, 4),
            (KEY_TGID, 4),
            (KEY_OP, 4),
            (KEY_BUCKET, 4),
            (KEY_OBJECT, 8),
            (KEY_DEV, 4),
            (KEY_AUX, 4),
        ];
        let value = [
            (VALUE_COUNT, 8),
            (VALUE_TOTAL_NS, 8),
            (VALUE_BYTES, 8),
            (VALUE_ERRORS, 8),
            (VALUE_COMM, COMM_BYTES as i16),
        ];
        for (fields, bytes) in [(&key[..], ROW_KEY_BYTES), (&value[..], ROW_VALUE_BYTES)] {
            let mut covered = vec![false; bytes];
            for (at, size) in fields {
                for byte in *at..*at + *size {
                    assert!(!covered[byte as usize]);
                    covered[byte as usize] = true;
                }
            }
            assert!(covered.iter().all(|byte| *byte));
        }
        let regions = [
            (ROW_VALUE, ROW_VALUE_BYTES as i16),
            (ROW_KEY, ROW_KEY_BYTES as i16),
            (EXIT_PEND_KEY, 8),
            (EXIT_DROP_KEY, 4),
            (EXIT_GEN_KEY, 4),
        ];
        for (index, (at, size)) in regions.iter().enumerate() {
            assert!(*at >= -512 && at + size <= 0);
            for (other_at, other_size) in &regions[index + 1..] {
                assert!(at + size <= *other_at || other_at + other_size <= *at);
            }
        }
    }

    /// `a or b` is refused only when neither is there, with both reasons; a
    /// kernel without BTF leaves it unchecked, as one function would.
    #[test]
    fn one_of_two_functions_is_missing_only_when_both_are() {
        match support("omn_no_such_function_a or omn_no_such_function_b") {
            Support::No(reason) => {
                assert!(reason.contains("omn_no_such_function_a"), "{reason}");
                assert!(reason.contains("omn_no_such_function_b"), "{reason}");
            }
            Support::Unchecked(_) => {}
            other => panic!("two missing functions answered {other:?}"),
        }
    }

    #[test]
    fn a_row_of_the_wrong_size_is_refused_rather_than_padded() {
        assert!(Row::decode(&[0; 8], &[0; ROW_VALUE_BYTES]).is_none());
        assert!(Row::decode(&[0; ROW_KEY_BYTES], &[0; 8]).is_none());
    }

    /// The histogram's buckets are the emit layer's buckets: the value that
    /// travels on an event is the low bound, and it has to land back in the
    /// same bucket. An independent statement of the definition, not a call
    /// into the code under test.
    #[test]
    fn the_kernels_buckets_and_the_emit_layers_are_the_same_buckets() {
        for bucket in 1..48u32 {
            let low = insn::bucket_low_ns(bucket);
            assert_eq!(
                u64::from(u64::BITS - low.leading_zeros()),
                u64::from(bucket)
            );
            assert_eq!(
                u64::from(u64::BITS - (low - 1).leading_zeros()),
                u64::from(bucket - 1)
            );
        }
    }
}
