// SPDX-License-Identifier: MIT OR GPL-2.0
//! The other way to attach a program: through `perf_event_open`.
//!
//! `BPF_RAW_TRACEPOINT_OPEN` takes a tracepoint's name and needs nothing but
//! the kernel's own BTF, which is why it is what this tool reaches for first.
//! It cannot reach everything. Three things are outside it, and all three are
//! the same mechanism underneath: a perf event is opened on the thing to be
//! traced, and `PERF_EVENT_IOC_SET_BPF` hangs a program off it.
//!
//! | Kind | What it opens on | What it needs |
//! |---|---|---|
//! | `bpf:tp` | a tracepoint's numeric id | that id, out of `tracefs` |
//! | `bpf:kprobe` | a kernel symbol | the `kprobe` PMU, and the symbol |
//! | `bpf:uprobe` | a file and an offset | the `uprobe` PMU, and the offset |
//!
//! The first is a fallback rather than a path forward, and the measurement
//! that settled that is in `LOG_A.md`: thirty-nine of the forty-one
//! tracepoints the catalog names under `bpf:tp` carry a `btf_trace_` type on
//! this kernel, so a raw tracepoint reaches them with strictly fewer
//! requirements. What is left for the classic path is the residue -- a
//! tracepoint defined by a module rather than by the kernel image, whose type
//! is in the module's own BTF and not in `vmlinux`.
//!
//! The other two are not a fallback for anything. A kprobe goes where no
//! tracepoint exists, and a uprobe goes into userspace, which no tracepoint
//! can reach at all.
//!
//! ## `tracefs` is the whole difficulty
//!
//! A tracepoint's numeric id lives in `<tracefs>/events/<category>/<name>/id`,
//! and on a stock machine that file is `-r--r----- root root` while the
//! directory holding it is world-executable. So the directory can be seen and
//! the number cannot be read -- which is worse than either being refused
//! outright, because a probe that checks for the directory reports the
//! requirement as met and the attach then fails much later, after a target has
//! been launched and other collectors have been attached to it.
//!
//! That is what [`classic_tracepoint_support`] exists for: it answers the
//! question the attach will actually ask, by reading the file the attach will
//! actually read.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use perf_event_open_sys as sys;

use super::insn::Insn;
use super::{syscall, Support};

// The fifth attach kind, and the programs built on it.
//
// Declared here rather than beside the other programs in `bpf/mod.rs` because
// that file belongs to no stream of the eighth wave and this one owns only
// `attach.rs`, `fentry.rs` and the `io*`/`net*` programs. `#[path]` resolves
// against this file's directory, so the files sit where they would anyway;
// moving these lines into `mod.rs` is a request in `docs/open-findings.md`,
// and costs nothing but the lines.
#[path = "fentry.rs"]
pub mod fentry;
#[path = "io_fsync.rs"]
pub mod io_fsync;
#[path = "io_vfs.rs"]
pub mod io_vfs;
#[path = "net_connect.rs"]
pub mod net_connect;
#[path = "net_socket.rs"]
pub mod net_socket;

/// Where `tracefs` may be mounted, best first.
const TRACEFS_ROOTS: [&str; 2] = ["/sys/kernel/tracing", "/sys/kernel/debug/tracing"];

/// The `kprobe` and `uprobe` PMUs are dynamic: the kernel assigns them a type
/// number at boot and publishes it here. There is no constant to use instead,
/// and a guessed number opens a different PMU's event.
const KPROBE_PMU: &str = "/sys/bus/event_source/devices/kprobe";
const UPROBE_PMU: &str = "/sys/bus/event_source/devices/uprobe";

/// What the program is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// One thread, on whichever cpu it runs on.
    ///
    /// For a kprobe this scopes the perf event and **not the program**: the
    /// kernel keeps one program array per kprobe and runs it at every hit,
    /// before the event's own task check (`kprobe_perf_func`). Measured: a
    /// program counting the read wrapper on this thread counted 1146 to 1292
    /// calls for 1000 reads while the other tests of its binary read too. A
    /// program that must keep one task's hits compares the task itself.
    Task(i32),
    /// Every cpu, which is what a machine-wide measurement means here: a perf
    /// event is opened per cpu, because one event cannot span them.
    System,
}

/// A loaded program, attached through one perf event per cpu.
///
/// Dropping it detaches: closing the perf event removes the program from it,
/// and the kernel unloads the program when its last reference goes -- which is
/// why the program is held here rather than by the caller.
pub struct PerfAttachment {
    events: Vec<OwnedFd>,
    program: OwnedFd,
}

impl PerfAttachment {
    /// The program the events run, for what the kernel counts about it.
    pub fn program(&self) -> &OwnedFd {
        &self.program
    }

    /// How many perf events this attachment holds open, which is one per cpu
    /// for a machine-wide attach and one for a thread.
    pub fn events(&self) -> usize {
        self.events.len()
    }
}

/// Reads the numeric id of a tracepoint, the way the attach will.
///
/// The error says which of the three things went wrong, because they call for
/// completely different answers: a kernel without the event needs a different
/// kernel, a `tracefs` that is not mounted needs a mount, and a file that is
/// there and unreadable needs privileges this tool deliberately does not ask
/// for.
pub fn tracepoint_id(category: &str, name: &str) -> Result<u32, String> {
    let Some(root) = tracefs_root() else {
        return Err(format!(
            "`tracefs` is not mounted at {}, so no tracepoint's numeric id can be read; \
             a raw tracepoint needs neither",
            TRACEFS_ROOTS.join(" or ")
        ));
    };
    let path = root.join("events").join(category).join(name).join("id");
    match fs::read_to_string(&path) {
        Ok(text) => text.trim().parse().map_err(|_| {
            format!(
                "{} holds `{}`, which is not the number this kernel was supposed to put there",
                path.display(),
                text.trim()
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Err(format!(
            "{} exists and is not readable by uid {}: the classic attach needs that number and \
             cannot get it. This needs root or CAP_DAC_READ_SEARCH, which is more than the \
             CAP_BPF and CAP_PERFMON this project settled on -- a raw tracepoint reaches the \
             same event with neither",
            path.display(),
            current_uid()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(format!(
            "this kernel defines no `{category}:{name}` tracepoint: {} does not exist",
            path.display()
        )),
        Err(error) => Err(format!("{} cannot be read: {error}", path.display())),
    }
}

/// Whether the classic path can attach to `<category>:<name>` here.
///
/// The requirement string the catalog writes is `tracepoint:<category>:<name>`,
/// and this is what a probe of it has to answer. It is not the same question as
/// whether the event exists -- on this project's own development machine the
/// event exists, its directory is listable, and its id is not readable -- and
/// answering the easy one is how a backend comes to be reported available and
/// then fail at attach time.
pub fn classic_tracepoint_support(event: &str) -> Support {
    let Some((category, name)) = event.split_once(':') else {
        return Support::Unchecked(format!(
            "`{event}` is not a tracepoint name: the classic path wants `<category>:<name>`, \
             as in `sched:sched_switch`"
        ));
    };
    match tracepoint_id(category, name) {
        Ok(_) => Support::Yes,
        Err(reason) => Support::No(reason),
    }
}

/// Whether this kernel exposes the `kprobe` PMU, and under which type number.
pub fn kprobe_type() -> Result<u32, String> {
    dynamic_pmu_type(KPROBE_PMU, "kprobe")
}

/// Whether this kernel exposes the `uprobe` PMU, and under which type number.
pub fn uprobe_type() -> Result<u32, String> {
    dynamic_pmu_type(UPROBE_PMU, "uprobe")
}

/// Whether this process can open a kprobe: the `kprobe` requirement, read
/// as the PMU's type number the kernel handed out at boot.
pub fn kprobe_support() -> Support {
    match kprobe_type() {
        Ok(_) => Support::Yes,
        Err(reason) => Support::No(reason),
    }
}

/// Whether this process can put a uprobe anywhere: the `uprobe` requirement.
///
/// There are two routes and they need different things. A uprobe-multi link
/// (Linux 6.6) needs the program's own privileges, `CAP_BPF` and
/// `CAP_PERFMON`; the `uprobe` PMU the older kernels leave needs
/// `CAP_SYS_ADMIN`, which this project does not ask for. Until the twelfth
/// wave this answered whether the PMU existed, which is neither question: on
/// 5.15 and 6.1 with the two capabilities every USDT source was listed usable,
/// the link was refused, the fallback was refused, and the target had already
/// been launched (P3 in `docs/portability.md`).
pub fn uprobe_support() -> &'static Support {
    static SUPPORT: std::sync::OnceLock<Support> = std::sync::OnceLock::new();
    SUPPORT.get_or_init(|| {
        let multi = match super::usdt::uprobe_multi_support() {
            Support::Yes => return Support::Yes,
            Support::No(reason) | Support::Unchecked(reason) => reason.clone(),
        };
        match uprobe_pmu_permitted() {
            Ok(()) => Support::Yes,
            Err(pmu) => Support::No(format!(
                "{multi}; and the uprobe PMU it would fall back to {pmu}"
            )),
        }
    })
}

/// Whether the `uprobe` PMU lets this process open an event at all, asked
/// with a path that does not exist.
///
/// The kernel checks the privilege before it looks the path up, so a refusal
/// on privilege grounds is `EACCES` and anything else -- `ENOENT` for the
/// missing file, above all -- means the privilege check passed. Nothing is
/// registered either way: there is no file to put a breakpoint in.
pub fn uprobe_pmu_permitted() -> Result<(), String> {
    let kind = uprobe_type()?;
    let path = CString::new("/proc/self/omn-uprobe-permission-probe")
        .expect("a literal has no interior NUL");
    let mut attr = sys::bindings::perf_event_attr {
        type_: kind,
        size: std::mem::size_of::<sys::bindings::perf_event_attr>() as u32,
        ..Default::default()
    };
    attr.set_disabled(1);
    attr.__bindgen_anon_3.uprobe_path = path.as_ptr() as u64;
    attr.__bindgen_anon_4.probe_offset = 0;
    let flags = u64::from(sys::bindings::PERF_FLAG_FD_CLOEXEC);
    // SAFETY: `attr` is a valid perf_event_attr and `path` outlives the call.
    let fd = unsafe { sys::perf_event_open(&mut attr, 0, -1, -1, flags as libc::c_ulong) };
    if fd >= 0 {
        // SAFETY: a descriptor the kernel just returned, closed here.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => Err(format!(
            "refuses this process ({error}): the kernel opens a `uprobe` PMU event only for \
             CAP_SYS_ADMIN, which CAP_BPF and CAP_PERFMON do not include"
        )),
        _ => Ok(()),
    }
}

fn dynamic_pmu_type(pmu: &str, kind: &str) -> Result<u32, String> {
    let path = Path::new(pmu).join("type");
    match fs::read_to_string(&path) {
        Ok(text) => text.trim().parse().map_err(|_| {
            format!(
                "{} holds `{}`, which is not a PMU type number",
                path.display(),
                text.trim()
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(format!(
            "this kernel has no `{kind}` PMU: {} does not exist, which means it was built \
             without CONFIG_KPROBE_EVENTS or CONFIG_UPROBE_EVENTS",
            path.display()
        )),
        Err(error) => Err(format!("{} cannot be read: {error}", path.display())),
    }
}

/// Which bit of `config` asks the PMU for a return probe.
///
/// Published as `config:N` in the PMU's own format directory. It has been bit
/// zero since these PMUs existed, and it is still read rather than written
/// down: a constant here would be this tool's opinion about the kernel's ABI,
/// and the kernel publishes the answer in a file any user can read.
fn retprobe_bit(pmu: &str) -> Result<u32, String> {
    let path = Path::new(pmu).join("format/retprobe");
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    text.trim()
        .strip_prefix("config:")
        .and_then(|bit| bit.parse().ok())
        .ok_or_else(|| {
            format!(
                "{} says `{}`, and this expects `config:<bit>`",
                path.display(),
                text.trim()
            )
        })
}

/// Loads a program of the type this attach kind requires.
///
/// The three attach functions load their own programs, rather than taking one
/// loaded elsewhere, because a program's type is fixed at load time and has to
/// match what it is attached to -- and the kernel reports a mismatch as
/// `EINVAL` from `PERF_EVENT_IOC_SET_BPF`, which is a long way from the line
/// that chose the wrong type. Pairing them here means the mistake cannot be
/// made.
fn load(prog_type: u32, instructions: &[Insn], name: &str) -> Result<OwnedFd, String> {
    crate::bpf::program::load(prog_type, instructions, name)
}

/// Attaches a program to a tracepoint, the classic way.
pub fn tracepoint(
    category: &str,
    name: &str,
    instructions: &[Insn],
    prog_name: &str,
    scope: Scope,
) -> Result<PerfAttachment, String> {
    // Read before anything is loaded: the id is the requirement most likely to
    // be out of reach, and there is no reason to have a program sitting in the
    // kernel while finding that out.
    let id = tracepoint_id(category, name)?;
    let program = load(syscall::BPF_PROG_TYPE_TRACEPOINT, instructions, prog_name)?;
    let mut attr = sys::bindings::perf_event_attr {
        type_: sys::bindings::PERF_TYPE_TRACEPOINT,
        size: std::mem::size_of::<sys::bindings::perf_event_attr>() as u32,
        config: u64::from(id),
        ..Default::default()
    };
    // One sample per hit, carrying the tracepoint's own record. Nothing here
    // reads that ring -- the program does the reading, in the kernel -- but an
    // event with no period is an event that never fires.
    attr.__bindgen_anon_1.sample_period = 1;
    attr.sample_type = u64::from(sys::bindings::PERF_SAMPLE_RAW);
    open_and_set(&mut attr, program, scope, &format!("{category}:{name}"))
}

/// Attaches a `BPF_PROG_TYPE_PERF_EVENT` program to a cpu-clock event on
/// every cpu, firing about `hz` times a second on each: a program that looks
/// at what each cpu is doing at a steady rate (`runqlen`'s shape).
pub fn cpu_clock(
    hz: u64,
    instructions: &[Insn],
    prog_name: &str,
) -> Result<PerfAttachment, String> {
    let program = load(syscall::BPF_PROG_TYPE_PERF_EVENT, instructions, prog_name)?;
    let mut attr = sys::bindings::perf_event_attr {
        type_: sys::bindings::PERF_TYPE_SOFTWARE,
        size: std::mem::size_of::<sys::bindings::perf_event_attr>() as u32,
        config: u64::from(sys::bindings::PERF_COUNT_SW_CPU_CLOCK),
        ..Default::default()
    };
    attr.set_freq(1);
    attr.__bindgen_anon_1.sample_freq = hz;
    open_and_set(&mut attr, program, Scope::System, "the cpu clock")
}

/// Attaches a loaded `BPF_PROG_TYPE_KPROBE` program to a kernel symbol.
///
/// `retprobe` asks for the function's return rather than its entry, which is
/// where a duration's second half is: a kprobe pair around one function is how
/// anything without a tracepoint gets timed.
pub fn kprobe(
    symbol: &str,
    offset: u64,
    retprobe: bool,
    instructions: &[Insn],
    prog_name: &str,
    scope: Scope,
) -> Result<PerfAttachment, String> {
    let kind = kprobe_type()?;
    let bit = retprobe_bit(KPROBE_PMU)?;
    let program = load(syscall::BPF_PROG_TYPE_KPROBE, instructions, prog_name)?;
    let symbol =
        CString::new(symbol).map_err(|_| "a kernel symbol name has no interior NUL".to_owned())?;
    let mut attr = sys::bindings::perf_event_attr {
        type_: kind,
        size: std::mem::size_of::<sys::bindings::perf_event_attr>() as u32,
        config: if retprobe { 1 << bit } else { 0 },
        ..Default::default()
    };
    // The symbol travels as a pointer the kernel reads during the call, which
    // is why the `CString` above outlives it.
    attr.__bindgen_anon_3.kprobe_func = symbol.as_ptr() as u64;
    attr.__bindgen_anon_4.probe_offset = offset;
    let attachment = open_and_set(
        &mut attr,
        program,
        scope,
        &format!("the kernel symbol `{}`", symbol.to_string_lossy()),
    );
    drop(symbol);
    attachment
}

/// Attaches a loaded `BPF_PROG_TYPE_KPROBE` program to a place in a file.
///
/// The offset is into the file, not into the running process: the kernel maps
/// it onto every mapping of that file, which is what makes one uprobe cover
/// every process using a shared library.
pub fn uprobe(
    path: &Path,
    offset: u64,
    retprobe: bool,
    instructions: &[Insn],
    prog_name: &str,
    scope: Scope,
) -> Result<PerfAttachment, String> {
    let kind = uprobe_type()?;
    let bit = retprobe_bit(UPROBE_PMU)?;
    // A uprobe carries the same program type as a kprobe: the kernel tells the
    // two apart by the PMU the event was opened on, not by the program.
    let program = load(syscall::BPF_PROG_TYPE_KPROBE, instructions, prog_name)?;
    let name = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| "a path has no interior NUL".to_owned())?;
    let mut attr = sys::bindings::perf_event_attr {
        type_: kind,
        size: std::mem::size_of::<sys::bindings::perf_event_attr>() as u32,
        config: if retprobe { 1 << bit } else { 0 },
        ..Default::default()
    };
    attr.__bindgen_anon_3.uprobe_path = name.as_ptr() as u64;
    attr.__bindgen_anon_4.probe_offset = offset;
    let attachment = open_and_set(
        &mut attr,
        program,
        scope,
        &format!("{}+{offset:#x}", path.display()),
    );
    drop(name);
    attachment
}

/// Opens the event, hangs the program off it and enables it.
///
/// The order is not a style: a program set on an already-enabled event misses
/// whatever fires in between, and an event enabled with no program attached is
/// a sampling event nobody is reading, which the kernel will happily throttle.
fn open_and_set(
    attr: &mut sys::bindings::perf_event_attr,
    program: OwnedFd,
    scope: Scope,
    what: &str,
) -> Result<PerfAttachment, String> {
    // Opened disabled, so that nothing fires between the open and the program
    // being set.
    attr.set_disabled(1);
    // The fd must not survive into a program this tool launches: a target that
    // inherits the tracer's event descriptors keeps them alive past the run.
    let flags = u64::from(sys::bindings::PERF_FLAG_FD_CLOEXEC);

    let targets: Vec<(i32, i32)> = match scope {
        // One thread, following it across cpus.
        Scope::Task(pid) => vec![(pid, -1)],
        // Every cpu, because one perf event covers one cpu and a machine-wide
        // measurement that opened only cpu zero would report a quiet machine
        // whenever the work happened elsewhere.
        Scope::System => online_cpus()?.into_iter().map(|cpu| (-1, cpu)).collect(),
    };

    let mut events = Vec::with_capacity(targets.len());
    for (pid, cpu) in targets {
        let fd = unsafe { sys::perf_event_open(attr, pid, cpu, -1, flags as libc::c_ulong) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::EACCES) | Some(libc::EPERM) => {
                    super::permission_advice(&format!("opening a perf event on {what}"))
                }
                _ => format!("perf_event_open on {what} was refused: {error}"),
            });
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        // This is the attachment. Everything above it is a perf event like any
        // other; this is what makes it run a program.
        if unsafe { sys::ioctls::SET_BPF(fd.as_raw_fd(), program.as_raw_fd() as u32) } < 0 {
            return Err(format!(
                "the kernel refused to attach the program to {what}: {}. A program's type has \
                 to match what it is attached to -- BPF_PROG_TYPE_TRACEPOINT for a tracepoint, \
                 BPF_PROG_TYPE_KPROBE for a kprobe or a uprobe -- and the kernel reports a \
                 mismatch as EINVAL here rather than at load time",
                io::Error::last_os_error()
            ));
        }
        if unsafe { sys::ioctls::ENABLE(fd.as_raw_fd(), 0) } < 0 {
            return Err(format!(
                "the perf event on {what} carries the program and will not start: {}",
                io::Error::last_os_error()
            ));
        }
        events.push(fd);
    }
    Ok(PerfAttachment { events, program })
}

/// Where this machine has `tracefs`, if it has it anywhere.
///
/// Published because the classic tracepoint path needs more than the numeric
/// id: a program reading a record's fields needs their offsets, and both live
/// under the same root. `sched_drm.rs` carried a second copy of these two paths
/// until the seventh wave, and whoever took the next classic tracepoint would
/// have written a third.
pub fn tracefs_root() -> Option<PathBuf> {
    TRACEFS_ROOTS
        .iter()
        .map(PathBuf::from)
        .find(|root| root.join("events").is_dir())
}

/// One field of a classic tracepoint's record, as its `format` file states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub offset: i16,
    pub size: usize,
    /// Whether the field is a `__data_loc`, which holds a packed
    /// `(offset, length)` pair rather than the value itself.
    pub dynamic: bool,
}

/// Every field of a tracepoint's record, by name.
pub type Layout = BTreeMap<String, Field>;

/// Reads and parses `<tracefs>/events/<category>/<name>/format`.
///
/// A raw tracepoint needs none of this -- it is handed the arguments and BTF
/// says what they are. A classic one hands over a record, and the offsets
/// within it are published here and nowhere else.
pub fn tracepoint_layout(category: &str, name: &str) -> Result<Layout, String> {
    let Some(root) = tracefs_root() else {
        return Err(format!(
            "`tracefs` is not mounted at {}, so `{category}:{name}` has no field layout to read",
            TRACEFS_ROOTS.join(" or ")
        ));
    };
    let path = root.join("events").join(category).join(name).join("format");
    let text = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "{} cannot be read ({error}); the classic tracepoint path needs this file's field \
             offsets and there is no other place they are published",
            path.display()
        )
    })?;
    Ok(parse_layout(&text))
}

/// Parses the `field:` lines of a `format` file.
///
/// The shape has been stable for as long as tracefs has existed:
///
/// ```text
///     field:u64 fence_context;   offset:24;   size:8;   signed:0;
///     field:__data_loc char[] name;   offset:12;   size:4;   signed:0;
/// ```
///
/// The separators the kernel writes are tabs; they are spaces above only so
/// that this doc comment does not carry one.
///
/// The declaration before the semicolon is C, so the field's name is its last
/// identifier -- after any array brackets, which belong to the type.
pub fn parse_layout(text: &str) -> Layout {
    let mut layout = Layout::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("field:") else {
            continue;
        };
        let mut offset = None;
        let mut size = None;
        let mut declaration = "";
        for part in rest.split(';') {
            let part = part.trim();
            if let Some(value) = part.strip_prefix("offset:") {
                offset = value.trim().parse::<i16>().ok();
            } else if let Some(value) = part.strip_prefix("size:") {
                size = value.trim().parse::<usize>().ok();
            } else if !part.is_empty() && declaration.is_empty() {
                declaration = part;
            }
        }
        let (Some(offset), Some(size)) = (offset, size) else {
            continue;
        };
        // The name is the last identifier of the declaration, with any array
        // bounds stripped: `char name[16]` names `name`, not `name[16]`.
        let Some(name) = declaration
            .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
            .find(|token| !token.is_empty())
        else {
            continue;
        };
        layout.insert(
            name.to_owned(),
            Field {
                offset,
                size,
                dynamic: declaration.contains("__data_loc"),
            },
        );
    }
    layout
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// The cpus that are online right now, by number.
///
/// Online rather than possible: an event cannot be opened on a cpu that is not
/// there, and the call fails with `ENODEV` rather than being skipped. The
/// inverse question -- how many slots a per-cpu map has -- is `possible`, and
/// `syscall::possible_cpus` answers that one.
///
/// Read through `capabilities::online_cpus`, the one reader of that file
/// (finding 26): this layer kept a parser of its own until the fourteenth
/// wave.
fn online_cpus() -> Result<Vec<i32>, String> {
    let cpus = crate::capabilities::online_cpus().map_err(|error| {
        format!(
            "{} cannot be read, so the online cpus are unknown: {error}",
            crate::capabilities::ONLINE_CPUS
        )
    })?;
    if cpus.is_empty() {
        return Err(format!("{} names no cpu", crate::capabilities::ONLINE_CPUS));
    }
    Ok(cpus.into_iter().map(|cpu| cpu as i32).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every cpu the machine reports online has to be opened on, because one
    /// perf event covers one cpu. A machine-wide attach that opened cpu zero
    /// alone would report nothing whenever the work ran elsewhere.
    #[test]
    fn the_online_cpus_are_the_ones_the_machine_names() {
        let cpus = online_cpus().expect("this machine reports its online cpus");
        let counted = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
        assert!(!cpus.is_empty());
        assert!(
            cpus.len() >= counted,
            "the kernel lists {} online cpus and this process can use {counted}",
            cpus.len()
        );
    }

    /// The whole reason `bpf:raw_tp` is ordered first, checked against this
    /// machine as it is rather than against an assumption about it.
    ///
    /// Both answers are real answers and both are asserted. Where the id is
    /// readable the classic path works and must say so; where it is not -- the
    /// stock arrangement, and this project's own development machine, where
    /// the event's directory is world-executable and its `id` is
    /// `-r--r----- root root` -- the requirement is missing, and the refusal
    /// has to name the file and the privilege rather than the event.
    ///
    /// Neither branch skips. A tracepoint every Linux kernel has is not a
    /// question this machine can decline to answer.
    #[test]
    fn a_tracepoint_id_this_user_cannot_read_is_a_missing_requirement() {
        let Some(root) = tracefs_root() else {
            eprintln!("skipped: `tracefs` is not mounted, so there is no id to read either way");
            return;
        };
        let path = root.join("events/sched/sched_switch/id");
        assert!(
            path.exists(),
            "every kernel this project supports has a `sched:sched_switch` tracepoint"
        );

        let readable = fs::read_to_string(&path).is_ok();
        match classic_tracepoint_support("sched:sched_switch") {
            Support::Yes => {
                assert!(
                    readable,
                    "the probe said the classic path can attach, and {} cannot be read",
                    path.display()
                );
                let id = tracepoint_id("sched", "sched_switch").expect("a numeric id");
                assert!(id > 0, "a tracepoint's id is not zero");
            }
            Support::No(reason) => {
                assert!(
                    !readable,
                    "the probe said the classic path is out of reach, and {} reads fine",
                    path.display()
                );
                assert!(
                    reason.contains("not readable") && reason.contains("id"),
                    "the refusal must name the file it could not read: {reason}"
                );
                assert!(
                    reason.contains("raw tracepoint"),
                    "and where the same event is reachable another way, it must say so: {reason}"
                );
            }
            Support::Unchecked(reason) => {
                panic!("a tracepoint this kernel has is not an unanswerable question: {reason}")
            }
        }
    }

    /// A kernel that does not have the event and a kernel that will not show
    /// its id are different answers, and only one of them calls for a different
    /// kernel.
    #[test]
    fn a_tracepoint_the_kernel_does_not_have_is_refused_for_a_different_reason() {
        if tracefs_root().is_none() {
            eprintln!("skipped: `tracefs` is not mounted");
            return;
        }
        let Support::No(reason) = classic_tracepoint_support("sched:not_a_tracepoint") else {
            panic!("a tracepoint no kernel has must be reported missing");
        };
        assert!(
            reason.contains("defines no"),
            "an absent event must not be reported as an unreadable one: {reason}"
        );
    }

    /// The requirement string has to be the one the catalog writes, and
    /// anything else is unknown rather than missing: a requirement nobody could
    /// check must never be reported as checked.
    #[test]
    fn a_requirement_that_is_not_a_tracepoint_name_is_unknown_not_missing() {
        assert!(matches!(
            classic_tracepoint_support("sched_switch"),
            Support::Unchecked(_)
        ));
    }

    /// The `kprobe` and `uprobe` PMU types are assigned at boot, so there is no
    /// constant that could be right. The probe has to agree with the file, and
    /// a machine without the PMU has to be told apart from one this cannot ask.
    #[test]
    fn the_probe_pmu_types_are_read_from_the_machine() {
        for (pmu, kind) in [(KPROBE_PMU, "kprobe"), (UPROBE_PMU, "uprobe")] {
            let published = fs::read_to_string(Path::new(pmu).join("type"))
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok());
            // `uprobe` as a requirement asks whether this process can place
            // one, which is more than the PMU's existence (see
            // `uprobe_support`); the type number is what is checked here.
            let support = match kind {
                "kprobe" => kprobe_support(),
                _ => match uprobe_type() {
                    Ok(_) => Support::Yes,
                    Err(reason) => Support::No(reason),
                },
            };
            match (published, support) {
                (Some(kind_number), Support::Yes) => {
                    let read = if kind == "kprobe" {
                        kprobe_type()
                    } else {
                        uprobe_type()
                    };
                    assert_eq!(read.expect("a type"), kind_number);
                    // A dynamic PMU is numbered above the fixed ones, which
                    // stop at `PERF_TYPE_MAX`. A number inside that range would
                    // mean an event opened on the wrong PMU entirely.
                    assert!(
                        kind_number >= sys::bindings::PERF_TYPE_MAX,
                        "{kind} is type {kind_number}, which is one of the fixed perf types"
                    );
                }
                (None, Support::No(reason)) => {
                    assert!(reason.contains(kind), "{reason}");
                }
                (published, _) => panic!("{pmu} publishes {published:?} and the probe disagrees"),
            }
        }
    }

    /// `uprobe` is yes when either route is open to this process, and a no
    /// names both, because either is what the reader can change: a kernel of
    /// 6.6 or later, or a privilege this project does not ask for.
    #[test]
    fn a_uprobe_refusal_names_both_routes() {
        let multi = super::super::usdt::uprobe_multi_support();
        let pmu = uprobe_pmu_permitted();
        match uprobe_support() {
            Support::Yes => assert!(*multi == Support::Yes || pmu.is_ok(), "{multi:?} {pmu:?}"),
            Support::No(reason) => {
                assert!(*multi != Support::Yes && pmu.is_err(), "{multi:?} {pmu:?}");
                assert!(reason.contains("uprobe-multi"), "{reason}");
                assert!(reason.contains("uprobe PMU"), "{reason}");
            }
            Support::Unchecked(reason) => panic!("`uprobe` is always answered: {reason}"),
        }
    }

    /// The retprobe bit is the kernel's to state, and it states it. A constant
    /// would be this tool's opinion about an ABI it can simply read.
    #[test]
    fn the_return_probe_bit_is_read_rather_than_assumed() {
        if !Path::new(KPROBE_PMU).exists() {
            eprintln!("skipped: this kernel has no `kprobe` PMU");
            return;
        }
        let bit = retprobe_bit(KPROBE_PMU).expect("the kprobe PMU publishes its format");
        assert!(bit < 64, "a config bit of a u64");
    }
}

/// A program that counts every hit into slot zero of a per-cpu array, shared by
/// the tests of each attach kind.
///
/// The smallest thing that can prove an attachment happened: if the number
/// comes out right, the program was loaded, the perf event was opened on the
/// right thing, `PERF_EVENT_IOC_SET_BPF` bound the two, and the event was
/// enabled. Any one of those going wrong gives zero.
#[cfg(test)]
mod ground_truth_support {
    use crate::bpf::insn::{audit, helper, Insn, BPF_DW, BPF_W, R10};

    pub fn counting_program(counter_fd: i32) -> Vec<Insn> {
        let mut program = Vec::new();
        program.push(Insn::st_imm(BPF_W, R10, -4, 0));
        program.extend(Insn::load_map_fd(1, counter_fd));
        program.push(Insn::mov64_reg(2, R10));
        program.push(Insn::add64_imm(2, -4));
        program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
        // A per-cpu array is one value per cpu, so the increment needs no
        // atomic: this cpu is the only one that can be running this program on
        // this slot.
        program.push(Insn::jeq_imm(0, 0, 3));
        program.push(Insn::ldx(BPF_DW, 1, 0, 0));
        program.push(Insn::add64_imm(1, 1));
        program.push(Insn::stx(BPF_DW, 0, 0, 1));
        program.push(Insn::mov64_imm(0, 0));
        program.push(Insn::exit());
        audit(&program).expect("a well-formed counting program");
        program
    }
}

#[cfg(test)]
mod ground_truth {
    use super::ground_truth_support::counting_program;
    use super::*;
    use crate::bpf::map::Counters;

    /// Ground truth for the whole classic attach path: a kernel function called
    /// a known number of times must be counted that number of times.
    ///
    /// `vfs_read` is entered exactly once per `read(2)`, so the independent
    /// quantity is the loop bound -- nothing here counts the reads a second way
    /// and compares the two. The perf event is opened on this thread alone, so
    /// the reads that other threads of the suite are doing at the same moment
    /// are not in the number.
    ///
    /// **This test skips without `CAP_BPF` and `CAP_PERFMON`, and on this
    /// project's development machine that is always.** A unit test's binary is
    /// rebuilt under a fresh hash every run and cannot carry file capabilities;
    /// only `target/debug/omn` can, which is why every measurement this project
    /// makes of its own eBPF layer is an integration test. `LOG_A.md` says what
    /// that leaves unproven here.
    #[test]
    fn a_kprobe_counts_the_calls_that_were_actually_made() {
        use std::io::Read;

        const READS: usize = 1000;

        let counter = match Counters::create(1, "omn_kprobe_t") {
            Ok(counter) => counter,
            Err(error) => {
                eprintln!("skipped: this process cannot create a bpf map: {error}");
                return;
            }
        };
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        // The read syscall's wrapper, which every read(2) enters: `vfs_read`
        // was the function here until a clang build inlined it into the
        // syscall and its out-of-line copy counted nothing (fentry's
        // `on_the_path`).
        let Some(arch) = fentry::syscall_arch() else {
            return;
        };
        let wrapper = format!("__{arch}_sys_read");
        let attachment = match kprobe(
            &wrapper,
            0,
            false,
            &counting_program(counter.as_raw_fd()),
            "omn_kprobe_t",
            Scope::Task(tid),
        ) {
            Ok(attachment) => attachment,
            Err(reason) => {
                // A kernel that will not probe `vfs_read` -- it is inlined
                // away, or kprobes are off -- cannot serve the request, which
                // is the one reason to stop quietly. Anything else is not.
                assert!(
                    reason.contains("CAP_BPF") || reason.contains("kprobe"),
                    "this is not a machine that cannot serve the request: {reason}"
                );
                eprintln!("skipped: {reason}");
                return;
            }
        };
        assert_eq!(attachment.events(), 1, "one thread is one perf event");

        let mut zero = std::fs::File::open("/dev/zero").expect("/dev/zero");
        let mut byte = [0u8; 1];
        for _ in 0..READS {
            zero.read_exact(&mut byte).expect("a read of /dev/zero");
        }
        let counted = counter.read(0);
        drop(attachment);

        assert!(
            counted >= READS as u64,
            "{READS} reads entered `{wrapper}` {READS} times and the kprobe counted {counted}"
        );
        // No ceiling: a program on a kprobe runs at every hit, whatever task
        // the perf event names (`Scope::Task`), so the other tests of this
        // binary reading at the same time are in the count too.
    }
}

#[cfg(test)]
mod classic_tracepoint {
    use super::ground_truth_support::counting_program;
    use super::*;
    use crate::bpf::map::Counters;

    /// The classic tracepoint attach, as far as this machine allows it to go,
    /// and both of the two places it can stop are real answers.
    ///
    /// Where the numeric id is readable, the whole path runs: a
    /// `BPF_PROG_TYPE_TRACEPOINT` program is loaded, a perf event is opened on
    /// the id, `PERF_EVENT_IOC_SET_BPF` binds them, and `sched_switch` -- which
    /// fires on any machine that is doing anything -- must be counted.
    ///
    /// Where it is not readable, which is the stock arrangement and this
    /// project's development machine, the attach must refuse before it opens
    /// anything, and name the file it could not read. That is the whole point
    /// of the refusal: the alternative is a backend that resolves, launches a
    /// target, and only then discovers it cannot attach.
    #[test]
    fn a_classic_tracepoint_attach_either_counts_or_names_what_it_could_not_read() {
        let readable = tracepoint_id("sched", "sched_switch").is_ok();
        let counter = match Counters::create(1, "omn_tp_test") {
            Ok(counter) => counter,
            Err(error) => {
                assert!(
                    !readable || error.raw_os_error().is_some(),
                    "a refusal must carry the kernel's own errno"
                );
                eprintln!("skipped: this process cannot create a bpf map: {error}");
                return;
            }
        };
        match tracepoint(
            "sched",
            "sched_switch",
            &counting_program(counter.as_raw_fd()),
            "omn_tp_test",
            Scope::System,
        ) {
            Ok(attachment) => {
                assert!(readable, "the attach succeeded where the id cannot be read");
                assert_eq!(
                    attachment.events(),
                    online_cpus().expect("the online cpus").len(),
                    "a machine-wide attach is one perf event per cpu"
                );
                // Any machine running this test is switching tasks: this test
                // is one of them.
                std::thread::sleep(std::time::Duration::from_millis(50));
                let counted = counter.read(0);
                drop(attachment);
                assert!(
                    counted > 0,
                    "fifty milliseconds of a running machine contain context switches, and the \
                     attached program counted none"
                );
            }
            Err(reason) if reason.contains("CAP_BPF") => {
                eprintln!("skipped: {reason}");
            }
            Err(reason) => {
                assert!(
                    !readable,
                    "the attach failed where the id reads fine, which is not a machine that \
                     cannot serve the request: {reason}"
                );
                assert!(
                    reason.contains("/id") && reason.contains("not readable"),
                    "the refusal must name the file the attach could not read: {reason}"
                );
                eprintln!("skipped: {reason}");
            }
        }
    }
}
