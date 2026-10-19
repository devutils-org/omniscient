//! Machine introspection.
//!
//! Nothing in the catalog is taken to be available. Every backend declares what
//! it needs and this module goes and looks. The important part is the third
//! answer: besides "there" and "missing", a probe may say **unknown**, meaning
//! no check has been written for that class of requirement yet.
//!
//! Unknown is not treated as available. A backend with an unverified
//! requirement is usable only when nothing better exists, and the fact is
//! carried all the way to the output. The alternative -- assuming what was not
//! checked -- is how a tool ends up claiming a measurement it never took.
//!
//! The environment is captured whether or not anyone asked. Two measurements are
//! comparable only if the governor, the THP mode, SMT and the clocksource were
//! the same, so those values belong next to the numbers rather than in someone's
//! memory of how the machine was set up.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The answer to "can this machine do that".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum Probe {
    /// Checked, and there.
    Available,
    /// Checked, and not there. The string says what was looked for.
    Missing(String),
    /// Not checked: no probe exists for this class of requirement yet.
    Unknown(String),
}

impl Probe {
    pub fn is_available(&self) -> bool {
        matches!(self, Probe::Available)
    }

    pub fn is_missing(&self) -> bool {
        matches!(self, Probe::Missing(_))
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Probe::Unknown(_))
    }
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Probe::Available => f.write_str("available"),
            Probe::Missing(detail) => write!(f, "missing: {detail}"),
            Probe::Unknown(detail) => write!(f, "unverified: {detail}"),
        }
    }
}

impl From<&crate::bpf::Support> for Probe {
    fn from(support: &crate::bpf::Support) -> Self {
        match support {
            crate::bpf::Support::Yes => Probe::Available,
            crate::bpf::Support::No(reason) => Probe::Missing(reason.clone()),
            crate::bpf::Support::Unchecked(reason) => Probe::Unknown(reason.clone()),
        }
    }
}

impl From<crate::bpf::Support> for Probe {
    fn from(support: crate::bpf::Support) -> Self {
        Probe::from(&support)
    }
}

/// Whether something is there, and whether this user may look at it.
///
/// The distinction matters: tracefs is mounted on almost every machine and
/// readable on almost none of them without privileges. Reporting "absent" for
/// something that is merely out of reach sends the reader looking for a kernel
/// config option instead of for `sudo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Readable,
    /// Present, but this user may not read it.
    Restricted,
    Absent,
}

impl Access {
    pub fn is_readable(self) -> bool {
        self == Access::Readable
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Access::Readable => "readable",
            Access::Restricted => "present but not readable by this user",
            Access::Absent => "absent",
        }
    }
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Looks at a path and says which of the three it is.
///
/// The question is whether this process can *read* it, which is not what `stat`
/// answers. `stat` needs search permission on the directories above a file, not
/// read permission on the file, so a root-only counter under a world-readable
/// directory passes it and then fails at `open`. Measured:
/// `/sys/class/powercap/intel-rapl:0/energy_uj` stats fine, reads `EACCES`, and
/// was being reported as available -- which is the "restricted is never absent"
/// rule broken by the one function that exists to keep it.
fn access(path: &Path) -> Access {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return Access::Absent;
    };
    // `AT_EACCESS` asks about the effective identity, which is the one that will
    // do the reading.
    // SAFETY: a NUL-terminated path this function owns for the duration.
    let readable = unsafe {
        libc::faccessat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            libc::R_OK,
            libc::AT_EACCESS,
        )
    };
    if readable == 0 {
        return Access::Readable;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => Access::Restricted,
        _ => Access::Absent,
    }
}

/// A kernel version, compared the way requirements write it: `kernel>=6.1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct KernelVersion {
    pub major: u32,
    pub minor: u32,
}

impl KernelVersion {
    /// Parses the leading `major.minor` of a release string such as
    /// `6.12.4-arch1-1`.
    pub fn parse(release: &str) -> Option<Self> {
        let mut parts = release.split(|c: char| !c.is_ascii_digit());
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
        Some(Self { major, minor })
    }
}

impl fmt::Display for KernelVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// What the machine is, at the moment of measuring.
///
/// Captured on every session. This is what makes two runs on two machines
/// comparable, and what makes a run comparable to itself an hour later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub kernel_release: String,
    pub kernel_version: Option<KernelVersion>,
    pub arch: String,
    pub cpus: usize,
    /// The clocksource in use. A run on `hpet` and a run on `tsc` do not have
    /// comparable timestamp costs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clocksource: Option<String>,
    /// The scaling governor. `powersave` and `performance` differ by tens of
    /// percent on the same workload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_governor: Option<String>,
    /// Transparent huge pages. `always` makes RSS grow in 2 MB steps in the
    /// background, which is enough on its own to make two runs disagree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transparent_hugepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smt: Option<String>,
    /// `perf_event_paranoid`: decides half the backends on its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub perf_event_paranoid: Option<i32>,
    pub kernel_btf: Access,
    pub cgroup2: Access,
    /// Where the block, sched and other tracepoints are enumerated. Root-only
    /// by default, which is why this is not a boolean.
    pub tracefs: Access,
    /// Speculative execution mitigations, which cost what they cost.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mitigations: BTreeMap<String, String>,
    /// The graphics devices, as the kernel describes them. Two `gpu` runs are
    /// comparable only if they were taken on the same device under the same
    /// driver, and neither of those is visible in any other field here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<GpuDevice>,
    /// The NVIDIA devices' clock states -- shader clock lock, application
    /// clocks, persistence, event reasons -- read when a `gpu` source is
    /// resolved and only then (NVML costs 3 to 5 ms to open): a GPU duration
    /// depends on the clock it ran at, and two runs are comparable only if
    /// this says the same. Never set by `omn`, only read.
    ///
    /// Empty for a run that asked no `gpu` source, and in files written
    /// before it existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu_clocks: Vec<crate::collect::gpu::ClockState>,
    /// The hardware prefetchers, **captured and never controlled**.
    ///
    /// Turning them off is a real lever for isolating a memory effect, and it
    /// is also a modification of the machine under measurement -- boundary 5.
    /// So the state is read and written here, beside the governor and the THP
    /// mode, and whoever wants them off turns them off themselves and gets a
    /// resolved file that says so. Two runs with different settings are not
    /// comparable, and before this field nothing said why.
    ///
    /// Absent only from files written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefetchers: Option<Prefetchers>,
    /// What this process may do, as the kernel granted it: the difference
    /// between two runs on one machine is as often the binary's capabilities
    /// as the machine, and a resolved file that did not say so could not
    /// explain it (ROADMAP phase 7, "the resolved file is enough to explain a
    /// gap between two machines").
    ///
    /// Absent only from files written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privileges: Option<Privileges>,
    /// Whether this is a virtual machine, and which: `none` where the cpu
    /// reports no hypervisor, otherwise what the firmware names it (`QEMU
    /// Standard PC (Q35 + ICH9, 2009)`). A guest and its host differ by
    /// devices -- hwmon, RAPL, a PTP clock, cpufreq -- that no other field
    /// here names.
    ///
    /// Absent only from files written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtualization: Option<String>,
    /// What is deliberately not captured yet, so its absence is not read as
    /// "this machine has none of it".
    pub not_captured: Vec<String>,
}

/// The privileges this process holds, read from the kernel's own account of
/// it (`/proc/self/status`), and what the binary's file carries that the
/// process was not given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Privileges {
    /// The effective user id.
    pub uid: u32,
    /// The capabilities this tool's mechanisms ask for, of those in the
    /// effective set, by name (`CAP_BPF`, `CAP_PERFMON`, ...). Root holds all
    /// of them; a binary granted `cap_bpf,cap_perfmon+ep` holds those two.
    pub effective: Vec<String>,
    /// `NoNewPrivs`: set by a sandbox, and the reason a binary's file
    /// capabilities are not granted at exec.
    pub no_new_privs: bool,
    /// The binary carries capabilities this process was not given, and why:
    /// a `nosuid` mount, `no_new_privs`, or a grant without `e`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<String>,
}

/// The capabilities a mechanism of this tool asks for, by number and name.
/// The list is what `Privileges::effective` reports from, so a capability
/// nothing here uses does not make two records differ.
const CAPABILITIES_ASKED: [(u32, &str); 9] = [
    (1, "CAP_DAC_OVERRIDE"),
    (2, "CAP_DAC_READ_SEARCH"),
    (12, "CAP_NET_ADMIN"),
    (13, "CAP_NET_RAW"),
    (17, "CAP_SYS_RAWIO"),
    (19, "CAP_SYS_PTRACE"),
    (21, "CAP_SYS_ADMIN"),
    (38, "CAP_PERFMON"),
    (39, "CAP_BPF"),
];

/// `CAP_PERFMON` and `CAP_BPF`: the two `make` grants to the binary.
const GRANTED_TO_THE_BINARY: [(u32, &str); 2] = [(39, "CAP_BPF"), (38, "CAP_PERFMON")];

/// One hexadecimal capability set of `/proc/<pid>/status` (`CapEff:`,
/// `CapPrm:`), and `NoNewPrivs:`.
fn status_field<'a>(status: &'a str, field: &str) -> Option<&'a str> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
        .map(str::trim)
}

fn capability_set(status: &str, field: &str) -> Option<u64> {
    u64::from_str_radix(status_field(status, field)?, 16).ok()
}

/// What the binary's file carries, from its `security.capability` attribute:
/// the permitted set and whether it is effective at exec. `None` where the
/// file carries none, or the attribute cannot be read.
///
/// The layout is the kernel's `vfs_cap_data` (`linux/capability.h`): a
/// `magic_etc` word whose top byte is the revision (2 or 3) and whose bit 0 is
/// the effective flag, then the permitted and inheritable words of the low
/// and the high half. Revision 1 has one half, and no capability above 31.
fn file_capabilities(path: &Path) -> Option<(u64, bool)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let name = c"security.capability";
    let mut buffer = [0u8; 24];
    // SAFETY: both strings are NUL-terminated and the buffer's length is given.
    let length = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if length < 12 {
        return None;
    }
    decode_file_capabilities(&buffer[..length as usize])
}

fn decode_file_capabilities(data: &[u8]) -> Option<(u64, bool)> {
    let word = |index: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            data.get(index * 4..index * 4 + 4)?.try_into().ok()?,
        ))
    };
    let magic = word(0)?;
    let effective = magic & 1 == 1;
    let low = u64::from(word(1)?);
    let high = match magic & 0xff00_0000 {
        0x0100_0000 => 0,
        0x0200_0000 | 0x0300_0000 => u64::from(word(3)?),
        _ => return None,
    };
    Some((low | high << 32, effective))
}

/// The mount a path is on and whether it is `nosuid`, from a `mountinfo`
/// text: the longest mount point that is a prefix of the path, and its
/// per-mount options (the sixth field).
fn mount_of(mountinfo: &str, path: &Path) -> Option<(PathBuf, bool)> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let point = PathBuf::from(unescape_mount(fields.get(4)?));
            let options = fields.get(5)?;
            path.starts_with(&point)
                .then(|| (point, options.split(',').any(|option| option == "nosuid")))
        })
        .max_by_key(|(point, _)| point.as_os_str().len())
}

/// `mountinfo` writes a space, a tab, a newline and a backslash in a path as
/// three octal digits after a backslash.
fn unescape_mount(field: &str) -> String {
    let mut out = String::new();
    let mut chars = field.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let digits: String = chars.clone().take(3).collect();
            if digits.len() == 3 {
                if let Ok(code) = u8::from_str_radix(&digits, 8) {
                    out.push(code as char);
                    chars.nth(2);
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

/// Why this process lacks capabilities the binary's own file carries, or
/// `None` when it lacks none of `CAP_BPF` and `CAP_PERFMON` that the file
/// grants.
///
/// A binary on a `nosuid` filesystem keeps `security.capability`, `getcap`
/// prints it, and the kernel ignores it at exec; a process under
/// `no_new_privs` is not given it either. Advising `setcap` there tells the
/// reader to run the command they already ran. Measured by stream D on the
/// development machine, whose `/tmp` is `nosuid`: `getcap` printed
/// `cap_perfmon,cap_bpf=ep` and `omn list` said 57 usable, the unprivileged
/// number.
pub fn withheld_file_capabilities() -> Option<String> {
    static ANSWER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ANSWER
        .get_or_init(|| {
            let exe = std::fs::read_link("/proc/self/exe").ok()?;
            let status = std::fs::read_to_string("/proc/self/status").ok()?;
            let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
            let file = file_capabilities(&exe);
            withheld_reason(&exe, file, &status, &mountinfo)
        })
        .clone()
}

/// [`withheld_file_capabilities`] over what it read, so the reasoning can be
/// held to texts the kernel wrote for another process.
fn withheld_reason(
    exe: &Path,
    file: Option<(u64, bool)>,
    status: &str,
    mountinfo: &str,
) -> Option<String> {
    let (permitted, effective_flag) = file?;
    let effective = capability_set(status, "CapEff")?;
    let withheld: Vec<&str> = GRANTED_TO_THE_BINARY
        .iter()
        .filter(|(bit, _)| permitted >> bit & 1 == 1 && effective >> bit & 1 == 0)
        .map(|(_, name)| *name)
        .collect();
    if withheld.is_empty() {
        return None;
    }
    let names = withheld.join(" and ");
    let carried = format!(
        "The binary {} carries {names}, but this process was not given {}",
        exe.display(),
        if withheld.len() == 1 { "it" } else { "them" }
    );
    let no_new_privs = status_field(status, "NoNewPrivs") == Some("1");
    let why = match mount_of(mountinfo, exe) {
        Some((point, true)) => format!(
            "the filesystem at {} is mounted nosuid, and the kernel ignores file capabilities \
             there (`findmnt -no OPTIONS -T {}`): the grant is done, and it takes effect \
             from a filesystem mounted without nosuid",
            point.display(),
            exe.display()
        ),
        _ if no_new_privs => "it runs under no_new_privs (`NoNewPrivs: 1` in \
             /proc/self/status), which a sandbox or `NoNewPrivileges=yes` sets and which drops \
             file capabilities at exec: run it outside that sandbox"
            .to_owned(),
        _ if !effective_flag => format!(
            "the file's capabilities are permitted but not effective (granted without `e`): \
             `sudo setcap cap_bpf,cap_perfmon+ep {}`",
            exe.display()
        ),
        _ => "neither a nosuid mount nor no_new_privs explains it: compare `getcap` on the \
              binary with `grep Cap /proc/self/status` in the process"
            .to_owned(),
    };
    Some(format!("{carried}: {why}"))
}

/// Why a perf kprobe would be refused to a process holding `privileges`, or
/// `None` when the kernel lets it open one. Privileges that cannot be read
/// answer nothing here: the type probe still runs, and the attach says the rest.
/// Why this process may open no perf event at all, if it may not: a
/// disabled software `dummy` event on this thread, which counts nothing and is
/// refused only by the kernel's door itself.
fn perf_event_refused() -> Option<String> {
    use perf_event_open_sys::bindings as perf;
    let mut attr: perf::perf_event_attr = unsafe { std::mem::zeroed() };
    attr.type_ = perf::PERF_TYPE_SOFTWARE;
    attr.size = std::mem::size_of::<perf::perf_event_attr>() as u32;
    attr.config = perf::PERF_COUNT_SW_DUMMY as u64;
    attr.set_disabled(1);
    attr.set_exclude_kernel(1);
    attr.set_exclude_hv(1);
    // SAFETY: a zeroed attr with its size set, opened on this thread.
    let fd = unsafe {
        perf_event_open_sys::perf_event_open(
            &mut attr,
            0,
            -1,
            -1,
            perf::PERF_FLAG_FD_CLOEXEC as u64,
        )
    };
    if fd >= 0 {
        // SAFETY: closing a descriptor just opened here.
        unsafe { libc::close(fd) };
        return None;
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EPERM) if seccomp_refusal().is_some() => Some(format!(
            "perf_event_open refuses this process every event ({error}): {}",
            seccomp_refusal().unwrap_or_default()
        )),
        Some(libc::EACCES) | Some(libc::EPERM) => {
            let paranoid = read_trimmed("/proc/sys/kernel/perf_event_paranoid")
                .unwrap_or_else(|| "unknown".into());
            Some(format!(
                "perf_event_open refuses this process every event ({error}): \
                 perf_event_paranoid is {paranoid}{}",
                paranoid_beyond_capabilities()
            ))
        }
        _ => None,
    }
}

/// What `perf_event_paranoid` above 2 means, appended where a refusal names
/// the level: Debian's and Ubuntu's kernels read 3 and above as refusing every
/// `perf_event_open` to a process without CAP_SYS_ADMIN -- a patch older than
/// CAP_PERFMON, which therefore does not lift it. Empty at 2 and below, where
/// the level means what the kernel's own documentation says.
pub fn paranoid_beyond_capabilities() -> String {
    paranoid_explanation(
        read_trimmed("/proc/sys/kernel/perf_event_paranoid").and_then(|v| v.parse::<i32>().ok()),
    )
}

fn paranoid_explanation(level: Option<i32>) -> String {
    match level {
        Some(level) if level > 2 => ", which this kernel reads as refusing every perf event to \
             a process without CAP_SYS_ADMIN, CAP_PERFMON included (Debian's and Ubuntu's \
             patch); `sysctl kernel.perf_event_paranoid=2` lets CAP_PERFMON in"
            .to_owned(),
        _ => String::new(),
    }
}

/// Why `perf_event_open` answered EPERM to an event this process asked of
/// itself in user mode, when `perf_event_paranoid` would have allowed it: a
/// seccomp filter, which is what a container's default profile is -- podman's
/// and docker's refuse `perf_event_open` unless the container is given
/// CAP_PERFMON or CAP_SYS_ADMIN. `None` when the level explains the refusal or
/// no filter is installed. Found in a rootless podman container, where the
/// refusal read "perf_event_paranoid is 2" and envprobe read `seccomp: filter`.
pub fn seccomp_refusal() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    seccomp_explanation(
        status
            .lines()
            .find_map(|line| line.strip_prefix("Seccomp:"))
            .and_then(|mode| mode.trim().parse().ok()),
        read_trimmed("/proc/sys/kernel/perf_event_paranoid").and_then(|v| v.parse().ok()),
    )
}

fn seccomp_explanation(mode: Option<u8>, paranoid: Option<i32>) -> Option<String> {
    // Mode 2 is a filter; at 2 and below the level lets a process count its
    // own user mode, so it is not what refused.
    (mode == Some(2) && paranoid.is_some_and(|level| level <= 2)).then(|| {
        format!(
            "perf_event_paranoid {} allows it, and this process runs under a seccomp filter \
             (`Seccomp: 2` in /proc/self/status), which refuses perf_event_open: a container's \
             default profile does, unless the container is given CAP_PERFMON or CAP_SYS_ADMIN",
            paranoid.unwrap_or_default()
        )
    })
}

fn kprobe_not_permitted(privileges: Option<&Privileges>) -> Option<String> {
    let privileges = privileges?;
    let holds = |name: &str| privileges.effective.iter().any(|held| held == name);
    if privileges.uid == 0 || holds("CAP_PERFMON") || holds("CAP_SYS_ADMIN") {
        return None;
    }
    Some(
        "a kprobe needs CAP_PERFMON (or CAP_SYS_ADMIN) whatever kernel.perf_event_paranoid \
         says, and this process holds neither"
            .to_owned(),
    )
}

/// This process's privileges, as the kernel reports them.
fn privileges() -> Option<Privileges> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let effective = capability_set(&status, "CapEff")?;
    Some(Privileges {
        uid: current_uid(),
        effective: CAPABILITIES_ASKED
            .iter()
            .filter(|(bit, _)| effective >> bit & 1 == 1)
            .map(|(_, name)| (*name).to_owned())
            .collect(),
        no_new_privs: status_field(&status, "NoNewPrivs") == Some("1"),
        withheld: withheld_file_capabilities(),
    })
}

/// `none` where the cpu reports no hypervisor; otherwise the firmware's name
/// for the machine, or `a hypervisor` where the firmware says nothing.
fn virtualization(cpuinfo: &str, vendor: Option<String>, product: Option<String>) -> String {
    let guest = cpuinfo
        .lines()
        .find(|line| line.starts_with("flags"))
        .is_some_and(|line| line.split_whitespace().any(|flag| flag == "hypervisor"));
    if !guest {
        return "none".to_owned();
    }
    match (vendor, product) {
        (Some(vendor), Some(product)) if !product.starts_with(&vendor) => {
            format!("{vendor} {product}")
        }
        (_, Some(product)) => product,
        (Some(vendor), None) => vendor,
        (None, None) => "a hypervisor".to_owned(),
    }
}

/// Rewrites a privilege refusal where the binary carries what was refused and
/// the process was not given it: the advice to grant it is replaced by why it
/// did not take effect.
fn explain_withheld(probe: Probe) -> Probe {
    let rewrite = |reason: String| -> String {
        if !(reason.contains("CAP_BPF") || reason.contains("CAP_PERFMON")) {
            return reason;
        }
        match withheld_file_capabilities() {
            Some(withheld) => rewrite_withheld(reason, &withheld),
            None => reason,
        }
    };
    match probe {
        Probe::Missing(reason) => Probe::Missing(rewrite(reason)),
        Probe::Unknown(reason) => Probe::Unknown(rewrite(reason)),
        available => available,
    }
}

/// [`explain_withheld`] over one reason: the `setcap` advice replaced by why
/// the grant did not take effect, or that reason appended -- unless the reason
/// already says it, as [`crate::bpf::permission_advice`] has since the
/// sixteenth wave, which would otherwise print it twice.
fn rewrite_withheld(reason: String, withheld: &str) -> String {
    if reason.contains(withheld) {
        return reason;
    }
    const ADVICE: &str = "Grant them to the binary with";
    match reason.find(ADVICE) {
        Some(at) => {
            let rest = &reason[at..];
            let after = rest
                .find("sudo. ")
                .map_or("", |end| &rest[end + "sudo. ".len()..]);
            format!("{}{withheld}. {after}", &reason[..at])
                .trim_end()
                .to_owned()
        }
        None => format!("{reason}; {withheld}"),
    }
}

/// One graphics device, read from the kernel rather than from a vendor library.
///
/// The obvious way to capture this is `nvmlInit` plus `nvmlDeviceGetName`, and
/// it is the wrong one: `nvmlInit` costs 3.3 to 4.9 ms here, `Capabilities`
/// is detected on every command including those that measure nothing, and the
/// result would be nothing for the machines whose GPU is not an NVIDIA one.
/// Everything below is a handful of small sysfs reads -- 47 µs for the whole
/// walk on the development machine -- and it covers every driver the same way.
///
/// The price is that the device is named by its PCI ids rather than by its
/// marketing name: matching `0x10de:0x1e07` to "RTX 2060" needs a database
/// this tool does not carry. The ids are exact, which is what a comparison
/// between two runs needs; the name is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
    /// The card as the kernel names it, such as `card1`.
    pub card: String,
    /// PCI vendor and device, such as `0x10de:0x1e07`.
    pub pci_id: String,
    /// The kernel driver bound to it: `nvidia`, `amdgpu`, `i915`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// The version that driver publishes for itself, where it publishes one.
    /// An out-of-tree driver such as `nvidia` does; an in-tree one such as
    /// `amdgpu` ships with the kernel and is identified by `kernel_release`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
}

/// What the prefetcher capture found.
///
/// Three answers, for the same reason `Probe` has three: a state that could
/// not be read is not "enabled". On most machines the register is readable
/// only by root, and the honest capture there is `unreadable`, with why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Prefetchers {
    /// Read from the register that controls them, on every online cpu.
    Read {
        /// The model-specific register, such as `0x1a4`.
        register: String,
        /// Each prefetcher the register controls, by name, and whether it is
        /// on. Read from the first cpu.
        enabled: BTreeMap<String, bool>,
        /// Whether every online cpu agreed with the first. A machine with
        /// prefetchers off on some cores only is a machine whose runs depend
        /// on where the scheduler put them.
        uniform: bool,
    },
    /// This cpu has a register this tool knows, and this process may not
    /// read it.
    Unreadable { register: String, reason: String },
    /// No register this tool knows how to read on this cpu.
    Unknown { reason: String },
}

/// Where a vendor keeps its prefetcher switches, and what each bit is.
///
/// A set bit **disables** its prefetcher in both layouts below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrefetchRegister {
    msr: u32,
    bits: &'static [(u32, &'static str)],
}

/// `MSR_MISC_FEATURE_CONTROL`, as Intel documents it for its Core and Xeon
/// parts ("Disclosure of hardware prefetcher control on some Intel
/// processors").
const INTEL_PREFETCH: PrefetchRegister = PrefetchRegister {
    msr: 0x1a4,
    bits: &[
        (0, "l2_hardware"),
        (1, "l2_adjacent_line"),
        (2, "dcu"),
        (3, "dcu_ip"),
    ],
};

/// `PrefetchControl`, as AMD documents it from family 19h (Zen 3) on.
const AMD_PREFETCH: PrefetchRegister = PrefetchRegister {
    msr: 0xc000_0108,
    bits: &[
        (0, "l1_stream"),
        (1, "l1_stride"),
        (2, "l1_region"),
        (3, "l2_stream"),
        (5, "l2_up_down"),
    ],
};

/// The register for a vendor and family, or why there is none.
fn prefetch_register(vendor: &str, family: u32) -> Result<PrefetchRegister, String> {
    match vendor {
        "GenuineIntel" if family == 6 => Ok(INTEL_PREFETCH),
        "AuthenticAMD" if family >= 0x19 => Ok(AMD_PREFETCH),
        "AuthenticAMD" => Err(format!(
            "AMD family {family:#x} documents no prefetcher-control register (PrefetchControl \
             starts with family 0x19), so the state of this cpu's prefetchers cannot be read"
        )),
        vendor => Err(format!(
            "no prefetcher-control register is known for `{vendor}` family {family:#x}"
        )),
    }
}

/// Each prefetcher of a register value, and whether it is on.
fn decode_prefetchers(register: &PrefetchRegister, value: u64) -> BTreeMap<String, bool> {
    register
        .bits
        .iter()
        .map(|(bit, name)| ((*name).to_owned(), value & (1 << bit) == 0))
        .collect()
}

/// The vendor and family of the first cpu, from the first block of
/// `/proc/cpuinfo` -- which is all that is read, because the whole file is
/// tens of kilobytes on a large machine and this runs on every command.
fn cpu_identity(cpuinfo: impl std::io::BufRead) -> Option<(String, u32)> {
    let (mut vendor, mut family) = (None, None);
    for line in cpuinfo.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "vendor_id" => vendor = Some(value.trim().to_owned()),
            "cpu family" => family = value.trim().parse().ok(),
            _ => {}
        }
    }
    Some((vendor?, family?))
}

/// Reads one model-specific register of one cpu.
fn read_msr(cpu: usize, msr: u32) -> std::io::Result<u64> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::File::open(format!("/dev/cpu/{cpu}/msr"))?;
    let mut bytes = [0u8; 8];
    file.read_exact_at(&mut bytes, u64::from(msr))?;
    Ok(u64::from_le_bytes(bytes))
}

fn prefetchers() -> Prefetchers {
    let identity = std::fs::File::open("/proc/cpuinfo")
        .ok()
        .and_then(|file| cpu_identity(std::io::BufReader::new(file)));
    let Some((vendor, family)) = identity else {
        return Prefetchers::Unknown {
            reason: "/proc/cpuinfo names no vendor and family".into(),
        };
    };
    let register = match prefetch_register(&vendor, family) {
        Ok(register) => register,
        Err(reason) => return Prefetchers::Unknown { reason },
    };
    let name = format!("{:#x}", register.msr);
    let first = match read_msr(0, register.msr) {
        Ok(value) => value,
        Err(error) => {
            let reason = match error.raw_os_error() {
                Some(libc::EACCES) | Some(libc::EPERM) => format!(
                    "/dev/cpu/0/msr is not readable by uid {}: reading a model-specific \
                     register needs root or CAP_SYS_RAWIO, which this tool does not ask for",
                    current_uid()
                ),
                Some(libc::ENOENT) => {
                    "/dev/cpu/0/msr does not exist: the `msr` module is not loaded".into()
                }
                _ => format!("reading {name} on cpu 0 failed: {error}"),
            };
            return Prefetchers::Unreadable {
                register: name,
                reason,
            };
        }
    };
    // Every cpu the msr driver exposes, rather than a count: the numbering
    // can have holes, and `available_parallelism` is this process's affinity.
    let cpus: Vec<usize> = std::fs::read_dir("/dev/cpu")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    let uniform = cpus
        .iter()
        .all(|&cpu| read_msr(cpu, register.msr).map_or(true, |value| value == first));
    Prefetchers::Read {
        register: name,
        enabled: decode_prefetchers(&register, first),
        uniform,
    }
}

/// Everything the resolver knows about this machine.
#[derive(Debug, Clone)]
pub struct Capabilities {
    environment: Environment,
}

impl Capabilities {
    /// Looks at the machine. Every field is read, never assumed.
    pub fn detect() -> Self {
        let kernel_release =
            read_trimmed("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".into());
        let environment = Environment {
            kernel_version: KernelVersion::parse(&kernel_release),
            kernel_release,
            arch: arch(),
            cpus: available_cpus(),
            clocksource: read_trimmed(
                "/sys/devices/system/clocksource/clocksource0/current_clocksource",
            ),
            cpu_governor: read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
            transparent_hugepage: read_trimmed("/sys/kernel/mm/transparent_hugepage/enabled")
                .and_then(|line| selected_bracket(&line)),
            smt: read_trimmed("/sys/devices/system/cpu/smt/control"),
            perf_event_paranoid: read_trimmed("/proc/sys/kernel/perf_event_paranoid")
                .and_then(|value| value.parse().ok()),
            kernel_btf: access(Path::new("/sys/kernel/btf/vmlinux")),
            cgroup2: access(Path::new("/sys/fs/cgroup/cgroup.controllers")),
            tracefs: tracefs_access(),
            mitigations: mitigations(),
            gpus: gpus(),
            gpu_clocks: Vec::new(),
            prefetchers: Some(prefetchers()),
            privileges: privileges(),
            virtualization: std::fs::read_to_string("/proc/cpuinfo")
                .ok()
                .map(|cpuinfo| {
                    virtualization(
                        &cpuinfo,
                        read_trimmed("/sys/devices/virtual/dmi/id/sys_vendor"),
                        read_trimmed("/sys/devices/virtual/dmi/id/product_name"),
                    )
                }),
            not_captured: vec![
                "microarchitecture".into(),
                "NUMA topology".into(),
                "steal time".into(),
                // The kernel driver behind each GPU is captured above; the
                // userspace half is not, and a CUDA or ROCm upgrade changes
                // what a `gpu` measurement means without changing anything
                // this module reads.
                "vendor userspace library versions (NVML, CUDA, ROCm)".into(),
            ],
        };
        Self { environment }
    }

    pub fn environment(&self) -> &Environment {
        &self.environment
    }

    /// Checks a cgroup v2 interface file.
    ///
    /// The file is looked for at the root, but its absence there proves
    /// nothing: the root cgroup carries no `memory.current`, while every child
    /// does. So the controller is checked against `cgroup.controllers`, which
    /// is what actually decides whether the file exists where it matters.
    fn probe_cgroup_file(&self, file: &str) -> Probe {
        if self.environment.cgroup2 == Access::Absent {
            return Probe::Missing("cgroup v2 is not mounted at /sys/fs/cgroup".into());
        }
        if let Some((file, key)) = file.split_once(':') {
            return probe_cgroup_key(file, key);
        }
        let root = PathBuf::from("/sys/fs/cgroup");
        if access(&root.join(file)).is_readable() {
            return Probe::Available;
        }
        // A pressure file is PSI's, whatever controller its name begins
        // with: a kernel without PSI (or booted with `psi=0`) publishes it in
        // no cgroup, and `/proc/pressure` is where it says so. The controller
        // being enabled is no answer for it.
        if file.ends_with(".pressure") && !Path::new("/proc/pressure").exists() {
            return Probe::Missing(format!(
                "this kernel publishes no PSI (/proc/pressure does not exist), and a cgroup's \
                 `{file}` is the same accounting: no cgroup has one"
            ));
        }
        let controller = file.split('.').next().unwrap_or(file);
        match std::fs::read_to_string(root.join("cgroup.controllers")) {
            Ok(controllers)
                if controllers
                    .split_whitespace()
                    .any(|name| name == controller) =>
            {
                Probe::Available
            }
            Ok(_) => Probe::Missing(format!(
                "the `{controller}` controller is not enabled on this cgroup hierarchy"
            )),
            Err(error) => Probe::Missing(format!("cannot read cgroup.controllers: {error}")),
        }
    }

    /// Answers one requirement string from a catalog backend.
    ///
    /// `pid` supplies the process a `/proc/[pid]/...` requirement refers to;
    /// without one, such a requirement can only be checked for the current
    /// process, which is what `[pid]` means for a self-test.
    pub fn probe(&self, requirement: &str, pid: Option<i32>) -> Probe {
        explain_withheld(self.probe_requirement(requirement, pid))
    }

    fn probe_requirement(&self, requirement: &str, pid: Option<i32>) -> Probe {
        if let Some(rest) = requirement.strip_prefix("kernel>=") {
            return match (KernelVersion::parse(rest), self.environment.kernel_version) {
                (Some(wanted), Some(actual)) if actual >= wanted => Probe::Available,
                (Some(wanted), Some(actual)) => {
                    Probe::Missing(format!("kernel {actual} is older than {wanted}"))
                }
                _ => Probe::Unknown(format!("cannot compare kernel version against `{rest}`")),
            };
        }

        if requirement.starts_with('/') {
            let path = substitute_pid(requirement, pid);
            return probe_path(&path);
        }

        // A control file is used by writing it, and some are write-only:
        // `/proc/[pid]/clear_refs` is `--w-------`, and asked by reading it
        // answered "not readable by uid 1000" about a file that uid may use.
        // Finding 21 of `docs/open-findings.md`.
        if let Some(path) = requirement.strip_prefix("writable:") {
            return probe_writable(&substitute_pid(path, pid));
        }

        if let Some(file) = requirement.strip_prefix("cgroup2:") {
            return self.probe_cgroup_file(file);
        }

        if let Some(event) = requirement.strip_prefix("tracepoint:") {
            return match self.environment.tracefs {
                Access::Absent => Probe::Missing("tracefs is not mounted".into()),
                Access::Restricted => Probe::Missing(format!(
                    "tracefs is mounted but not readable by uid {}, so `{event}` cannot even be \
                     looked for: this needs root or CAP_SYS_ADMIN, not a different kernel",
                    current_uid()
                )),
                // Not `probe_path` on the event's directory, which is the
                // easier question and the wrong one. The attach reads the
                // event's `id` file, and on a stock machine the directory is
                // listable by anyone while that file is root-only -- so a
                // directory probe reports the backend available, the resolver
                // selects it, the run launches its target, and only then does
                // the attach discover it cannot read the number.
                Access::Readable => {
                    Probe::from(crate::bpf::attach::classic_tracepoint_support(event))
                }
            };
        }

        // `kprobe` and `uprobe` are PMUs whose type numbers the kernel hands
        // out at boot, in the order the PMUs register. There is no constant
        // that could be right, and an event opened on the wrong type does not
        // fail -- it counts something else. Until this was asked, both read
        // `unverified`, which the resolver treats as a reason to prefer a
        // backend it could check.
        // The `kprobe` PMU existing is not the process being let in: the kernel
        // refuses a perf kprobe to anyone not `perfmon_capable()` --
        // `CAP_PERFMON` or `CAP_SYS_ADMIN` -- whatever `perf_event_paranoid`
        // says. The eBPF kprobe backends also ask `bpf`, which hid this until
        // `mem.swap`'s counting kprobe was listed usable without either.
        if requirement == "kprobe" {
            if let Some(missing) = kprobe_not_permitted(privileges().as_ref()) {
                return Probe::Missing(missing);
            }
            // The capability held is not the kernel letting it in: Debian's and
            // Ubuntu's `perf_event_paranoid` 3 refuses every event to a process
            // without CAP_SYS_ADMIN, CAP_PERFMON included. So an event is
            // opened, the cheapest there is; debian-12 listed `mem.swap`'s
            // counting kprobe usable and refused it at start until it was.
            if let Some(refused) = perf_event_refused() {
                return Probe::Missing(format!("a kprobe is a perf event, and {refused}"));
            }
        }
        if let Some(support) = crate::bpf::attach::probe_type_support(requirement) {
            return Probe::from(support);
        }

        if let Some(event) = requirement.strip_prefix("pmu:") {
            return probe_pmu(event);
        }

        // A model-specific register, read through `/dev/cpu/<n>/msr`. The
        // question is not whether the register exists -- `aperf` and `mperf`
        // are architectural on every x86 this tool targets -- but whether this
        // process may read it, which is a driver and a permission rather than a
        // cpu feature. Both of this catalog's `msr:` requirements read as
        // `unverified` until this was written, and the resolver treats
        // unverified as a reason to prefer a backend it could check.
        if requirement.starts_with("msr:") {
            return probe_msr(requirement);
        }

        // A raw tracepoint is looked for in the kernel's own BTF rather than in
        // tracefs, because that is how this tool attaches to one: through
        // `BPF_RAW_TRACEPOINT_OPEN`, which takes a name and needs no tracefs.
        // With a prototype -- `raw_tracepoint:sched_process_exit(task_struct *,
        // bool)` -- the probe also compares it with the kernel's, which is the
        // check the program makes at attach.
        if let Some(event) = requirement.strip_prefix("raw_tracepoint:") {
            return Probe::from(crate::bpf::raw_tracepoint_support(event));
        }

        // A member of a kernel struct a program reads, as `struct.member`,
        // looked up in BTF the way the program looks it up when it is built.
        if let Some(field) = requirement.strip_prefix("btf_member:") {
            return Probe::from(crate::bpf::btf_member_support(field));
        }

        // Netlink families: the question is whether this kernel exposes the
        // protocol at all, which is answered by trying to open a socket on it.
        // Opening one is free and changes nothing.
        if let Some(protocol) = netlink_protocol(requirement) {
            return probe_netlink(requirement, protocol);
        }

        // Packet capture is a privilege question, not a kernel-build question,
        // and the two call for different answers from whoever reads the report.
        if requirement.starts_with("af_packet") {
            return probe_af_packet(requirement);
        }

        // Whether the *target* is dynamically linked cannot be known before it
        // exists; whether this *build* can preload anything can, and that is
        // what every preload source is waiting on. Answering it also turns a
        // `cargo build` without `--workspace` -- no shim beside the binary --
        // into a `missing` that names the path, where it used to be a `?`.
        if requirement == "preload:shim" || requirement == "dynamic-linking" {
            return match crate::collect::find_shim() {
                Ok(_) => Probe::Available,
                Err(reason) => Probe::Missing(reason),
            };
        }

        // A family may answer requirement classes of its own -- a vendor
        // library it loads, a sysfs layout only it reads. The probe lives with
        // the code that depends on it, so a family cannot be added without the
        // means to check what it needs.
        if let Some(probe) = crate::collect::gpu::probe(requirement) {
            return probe;
        }

        match requirement {
            // `power.temp` reads every temperature input hwmon publishes, so
            // the requirement is that one of them can be read. Unanswered, it
            // read `?` on every run of a source this machine serves.
            "hwmon" => probe_hwmon(Path::new("/sys/class/hwmon")),
            "tracefs" => match self.environment.tracefs {
                Access::Readable => Probe::Available,
                Access::Restricted => Probe::Missing(format!(
                    "tracefs is mounted but not readable by uid {}: this needs root, not a \
                     different kernel",
                    current_uid()
                )),
                Access::Absent => Probe::Missing("tracefs is not mounted".into()),
            },
            // Asked by loading the smallest possible tracing program. The
            // answer depends on capabilities, on a sysctl and on how the kernel
            // was built, and only the kernel knows all three.
            // Asked by opening the event a profile would open. Whether the
            // kernel hands a callchain to this user depends on
            // `perf_event_paranoid`, on `CAP_PERFMON` and on how the kernel was
            // built, and the cheapest way to know all three is to ask for one.
            "callchain" => match crate::perf::SampledEvent::callchain_supported() {
                Ok(()) => Probe::Available,
                Err(error) => Probe::Missing(format!(
                    "perf_event_open refused a sampling event carrying a callchain ({error}); \
                     check /proc/sys/kernel/perf_event_paranoid"
                )),
            },
            "bpf" => Probe::from(crate::bpf::load_support()),
            "btf" => match self.environment.kernel_btf {
                Access::Readable => Probe::Available,
                Access::Restricted => {
                    Probe::Missing("/sys/kernel/btf/vmlinux is not readable by this user".into())
                }
                Access::Absent => Probe::Missing(
                    "/sys/kernel/btf/vmlinux is absent, so CO-RE is unavailable".into(),
                ),
            },
            other => Probe::Unknown(format!("no probe is written for `{other}` yet")),
        }
    }
}

/// Whether a cgroup's flat-keyed file carries a key: `cgroup2:memory.stat:pswpout`.
///
/// Which keys `memory.stat` has is the kernel's choice and changes between
/// releases -- 7.2 counts `pswpin`/`pswpout` per cgroup and 6.6 does not -- so
/// a requirement on the file alone is answered yes by a kernel whose collector
/// then refuses (P10 in `docs/portability.md`). The file is read where this
/// process lives, walking up to the root until one is there: the keys a kernel
/// writes do not depend on which cgroup it writes them for, and the nearest one
/// is the one most like a target's.
fn probe_cgroup_key(file: &str, key: &str) -> Probe {
    let root = Path::new("/sys/fs/cgroup");
    let own = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("0::").map(str::to_owned))
        })
        .unwrap_or_else(|| "/".to_owned());
    let mut directory = root.join(own.trim_start_matches('/'));
    loop {
        let path = directory.join(file);
        if let Ok(text) = std::fs::read_to_string(&path) {
            let present = text
                .lines()
                .any(|line| line.split_whitespace().next() == Some(key));
            return if present {
                Probe::Available
            } else {
                Probe::Missing(format!(
                    "{} carries no `{key}`, so this kernel does not publish it per cgroup",
                    path.display()
                ))
            };
        }
        if directory == root || !directory.pop() || !directory.starts_with(root) {
            return Probe::Unknown(format!(
                "no `{file}` is readable from this process's cgroup up to {}, so whether it \
                 carries `{key}` cannot be checked",
                root.display()
            ));
        }
    }
}

/// Whether any hwmon chip under `root` publishes a temperature this process
/// can read, named in the answer either way.
fn probe_hwmon(root: &Path) -> Probe {
    let Ok(chips) = std::fs::read_dir(root) else {
        return Probe::Missing(format!(
            "{} is absent: this kernel publishes no hardware monitoring",
            root.display()
        ));
    };
    let mut restricted = None;
    for chip in chips.flatten() {
        let Ok(files) = std::fs::read_dir(chip.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("temp") && name.ends_with("_input")) {
                continue;
            }
            match access(&file.path()) {
                Access::Readable => return Probe::Available,
                Access::Restricted => restricted = Some(file.path()),
                Access::Absent => {}
            }
        }
    }
    Probe::Missing(match restricted {
        Some(path) => format!(
            "{} exists but is not readable by uid {}",
            path.display(),
            current_uid()
        ),
        None => format!("no chip under {} publishes a temperature", root.display()),
    })
}

/// Replaces `[pid]` in a procfs path, defaulting to this process.
fn substitute_pid(requirement: &str, pid: Option<i32>) -> PathBuf {
    let pid = pid.unwrap_or(std::process::id() as i32);
    PathBuf::from(requirement.replace("[pid]", &pid.to_string()))
}

/// Reports a path as available, out of reach, or absent -- never conflating the
/// last two.
fn probe_path(path: &Path) -> Probe {
    match access(path) {
        Access::Readable => Probe::Available,
        Access::Restricted => Probe::Missing(format!(
            "{} exists but is not readable by uid {}: reading it needs root, or a mode that \
             lets this user read the file",
            path.display(),
            current_uid()
        )),
        Access::Absent => Probe::Missing(format!("{} does not exist", path.display())),
    }
}

/// Whether this process may write `path`, asked of the kernel (`access(2)`
/// with `W_OK`), and absent kept apart from forbidden.
fn probe_writable(path: &Path) -> Probe {
    let Ok(text) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return Probe::Missing(format!("{} is not a path", path.display()));
    };
    // SAFETY: a NUL-terminated path the kernel only reads.
    if unsafe { libc::access(text.as_ptr(), libc::W_OK) } == 0 {
        return Probe::Available;
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOENT) | Some(libc::ENOTDIR) => {
            Probe::Missing(format!("{} does not exist", path.display()))
        }
        Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::EROFS) => Probe::Missing(format!(
            "{} exists but is not writable by uid {} ({error}): writing it needs its owner or \
             root",
            path.display(),
            current_uid()
        )),
        _ => Probe::Missing(format!("{} cannot be written: {error}", path.display())),
    }
}

const TRACEFS_ROOTS: [&str; 2] = ["/sys/kernel/tracing", "/sys/kernel/debug/tracing"];

// This file used to have a `tracefs_root` of its own, to build the path a
// `tracepoint:` probe would look at. `src/bpf/attach.rs` has the same function
// because it is where the attach happens, and two readers of one thing is the
// shape findings 23 and 26 are about. Now that the probe asks the attach layer
// its own question, only the attach layer's copy is left.

/// Readable wins over restricted, restricted over absent: the best of the
/// candidate mount points is what the machine offers.
fn tracefs_access() -> Access {
    let mut best = Access::Absent;
    for candidate in TRACEFS_ROOTS {
        match access(&PathBuf::from(candidate).join("events")) {
            Access::Readable => return Access::Readable,
            Access::Restricted => best = Access::Restricted,
            Access::Absent => {}
        }
    }
    best
}

/// The named `perf_event_open` events this tool knows how to ask for.
///
/// Everything else under `pmu:` -- uncore boxes, PEBS, topdown slots, cache
/// descriptors -- needs an encoding this does not build yet, and stays
/// unverified rather than being guessed at.
fn named_pmu_event(name: &str) -> Option<(u32, u64)> {
    use perf_event_open_sys::bindings as perf;
    let hardware = |config: u32| Some((perf::PERF_TYPE_HARDWARE, config as u64));
    let software = |config: u32| Some((perf::PERF_TYPE_SOFTWARE, config as u64));
    match name {
        "PERF_COUNT_HW_CPU_CYCLES" => hardware(perf::PERF_COUNT_HW_CPU_CYCLES),
        "PERF_COUNT_HW_INSTRUCTIONS" => hardware(perf::PERF_COUNT_HW_INSTRUCTIONS),
        "PERF_COUNT_HW_CACHE_REFERENCES" => hardware(perf::PERF_COUNT_HW_CACHE_REFERENCES),
        "PERF_COUNT_HW_CACHE_MISSES" => hardware(perf::PERF_COUNT_HW_CACHE_MISSES),
        "PERF_COUNT_HW_BRANCH_INSTRUCTIONS" => hardware(perf::PERF_COUNT_HW_BRANCH_INSTRUCTIONS),
        "PERF_COUNT_HW_BRANCH_MISSES" => hardware(perf::PERF_COUNT_HW_BRANCH_MISSES),
        "PERF_COUNT_HW_REF_CPU_CYCLES" => hardware(perf::PERF_COUNT_HW_REF_CPU_CYCLES),
        "PERF_COUNT_SW_CPU_CLOCK" => software(perf::PERF_COUNT_SW_CPU_CLOCK),
        "PERF_COUNT_SW_TASK_CLOCK" => software(perf::PERF_COUNT_SW_TASK_CLOCK),
        "PERF_COUNT_SW_PAGE_FAULTS" => software(perf::PERF_COUNT_SW_PAGE_FAULTS),
        "PERF_COUNT_SW_PAGE_FAULTS_MIN" => software(perf::PERF_COUNT_SW_PAGE_FAULTS_MIN),
        "PERF_COUNT_SW_PAGE_FAULTS_MAJ" => software(perf::PERF_COUNT_SW_PAGE_FAULTS_MAJ),
        "PERF_COUNT_SW_CONTEXT_SWITCHES" => software(perf::PERF_COUNT_SW_CONTEXT_SWITCHES),
        "PERF_COUNT_SW_CPU_MIGRATIONS" => software(perf::PERF_COUNT_SW_CPU_MIGRATIONS),
        "PERF_COUNT_SW_ALIGNMENT_FAULTS" => software(perf::PERF_COUNT_SW_ALIGNMENT_FAULTS),
        "PERF_COUNT_SW_EMULATION_FAULTS" => software(perf::PERF_COUNT_SW_EMULATION_FAULTS),
        _ => None,
    }
}

/// The directory the kernel describes its PMUs in.
const PMU_DEVICES: &str = "/sys/bus/event_source/devices";

/// Encodes a cache descriptor such as `LLC-load-misses`.
///
/// `PERF_TYPE_HW_CACHE` packs three small numbers into the config: which cache,
/// which operation, and whether the count is of accesses or of misses.
fn cache_event(name: &str) -> Option<(u32, u64)> {
    use perf_event_open_sys::bindings as perf;
    let (cache, rest) = match name {
        _ if name.starts_with("L1-dcache-") => (perf::PERF_COUNT_HW_CACHE_L1D, &name[10..]),
        _ if name.starts_with("L1-icache-") => (perf::PERF_COUNT_HW_CACHE_L1I, &name[10..]),
        _ if name.starts_with("LLC-") => (perf::PERF_COUNT_HW_CACHE_LL, &name[4..]),
        _ if name.starts_with("dTLB-") => (perf::PERF_COUNT_HW_CACHE_DTLB, &name[5..]),
        _ if name.starts_with("iTLB-") => (perf::PERF_COUNT_HW_CACHE_ITLB, &name[5..]),
        _ => return None,
    };
    let (operation, result) = match rest {
        "load-misses" => (
            perf::PERF_COUNT_HW_CACHE_OP_READ,
            perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
        ),
        "loads" => (
            perf::PERF_COUNT_HW_CACHE_OP_READ,
            perf::PERF_COUNT_HW_CACHE_RESULT_ACCESS,
        ),
        "store-misses" => (
            perf::PERF_COUNT_HW_CACHE_OP_WRITE,
            perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
        ),
        "stores" => (
            perf::PERF_COUNT_HW_CACHE_OP_WRITE,
            perf::PERF_COUNT_HW_CACHE_RESULT_ACCESS,
        ),
        _ => return None,
    };
    let config = (cache as u64) | ((operation as u64) << 8) | ((result as u64) << 16);
    Some((perf::PERF_TYPE_HW_CACHE, config))
}

/// Reads an event the kernel describes for itself, as `power/energy-pkg/`.
///
/// Every PMU beyond the architectural ones publishes its own type number and
/// the config of each event it offers. Only the `event=` term is read, which is
/// the whole config for the PMUs that matter here; a config built wrong makes
/// the open fail and the answer come back missing, so the error is always in
/// the safe direction -- never a source reported as available that is not.
fn sysfs_event(name: &str) -> Option<(u32, u64)> {
    // `pebs:mem-loads` is how the catalog names an event the `cpu` PMU
    // publishes on the hardware that has it. Machines without it publish no
    // such event and the answer comes back missing, which is the truth.
    let name = name.replace("pebs:", "cpu/");
    let name = name.as_str();
    let (pmu, event) = name.trim_end_matches('/').split_once('/')?;
    let kind: u32 = read_trimmed(&format!("{PMU_DEVICES}/{pmu}/type"))?
        .parse()
        .ok()?;
    let terms = read_trimmed(&format!("{PMU_DEVICES}/{pmu}/events/{event}"))?;

    let mut config = 0u64;
    for term in terms.split(',') {
        let Some((key, value)) = term.split_once('=') else {
            continue;
        };
        let value = value.trim_start_matches("0x");
        let Ok(value) = u64::from_str_radix(value, 16) else {
            continue;
        };
        match key {
            "event" => config |= value,
            // The layout of the remaining terms is in the PMU's `format`
            // directory; these two are the common placement and the rest are
            // left alone rather than guessed at.
            "umask" => config |= value << 8,
            _ => {}
        }
    }
    Some((kind, config))
}

/// Answers a bare PMU name -- `intel_pt`, `uncore_imc`, `ibs_op` -- by whether
/// the kernel publishes a device for it.
///
/// These name hardware rather than an event, and a machine either has the unit
/// or does not. Saying which is more useful than "no probe written", because
/// the reader learns it is the cpu and not the tool.
fn sysfs_device_state(name: &str) -> Option<Probe> {
    // The qualified form, `unit/event`: the unit's own directory answers
    // whether it exists and publishes the event. Absent is missing, with whose
    // it is; only a unit that publishes the event and whose encoding this tool
    // could not build is left unknown. Finding 25 of `docs/open-findings.md`:
    // `pmu:power/energy-pkg/` on a machine with no RAPL read "needs an event
    // encoding this tool does not build yet", which is unknown, which is never
    // available -- and never missing either, so the source was listed as
    // assumed and refused at the start.
    if let Some((unit, event)) = name.trim_end_matches('/').split_once('/') {
        let directory = format!("{PMU_DEVICES}/{unit}");
        if !std::path::Path::new(&directory).exists() {
            return Some(Probe::Missing(format!(
                "this machine publishes no `{unit}` performance unit under {PMU_DEVICES}"
            )));
        }
        if !std::path::Path::new(&format!("{directory}/events/{event}")).exists() {
            return Some(Probe::Missing(format!(
                "the `{unit}` performance unit publishes no `{event}` event under \
                 {directory}/events"
            )));
        }
        return None;
    }
    // `pebs:<event>` names an event of the cpu PMU on the hardware that has it.
    // A machine without it publishes no such event, which is an answer.
    if let Some(event) = name.strip_prefix("pebs:") {
        return Some(Probe::Missing(format!(
            "this cpu publishes no `{event}` event, so precise sampling of it is not available"
        )));
    }
    if name.contains(':') {
        return None;
    }
    let Ok(entries) = std::fs::read_dir(PMU_DEVICES) else {
        return None;
    };
    let mut present = false;
    for entry in entries.flatten() {
        let Some(device) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // `uncore_imc` names a family whose members are `uncore_imc_0` and so
        // on, so a prefix match is the right question for it.
        if device == name || device.starts_with(&format!("{name}_")) {
            present = true;
            break;
        }
        if std::path::Path::new(&format!("{PMU_DEVICES}/{device}/events/{name}")).exists() {
            present = true;
            break;
        }
    }
    Some(if present {
        Probe::Available
    } else {
        Probe::Missing(format!(
            "this machine publishes no `{name}` performance unit under {PMU_DEVICES}"
        ))
    })
}

/// Asks the kernel whether an event can be opened, rather than inferring it
/// from the cpu vendor or the kernel version.
///
/// The answer distinguishes "this cpu has no such counter" from "this machine
/// will not let you read it", because one calls for different hardware and the
/// other for `perf_event_paranoid`.
/// Whether this process can read a model-specific register.
///
/// Three answers, and they send a reader to three different places: the `msr`
/// module is not loaded (`modprobe msr`), the device is there and this user may
/// not read it (root, or `CAP_SYS_RAWIO`), or it can be read. The register's
/// own name is not checked against anything -- there is no enumeration of
/// which MSRs a cpu implements, and reading an absent one answers zero rather
/// than failing, which is exactly why a collector must never guess at one.
fn probe_msr(requirement: &str) -> Probe {
    let path = PathBuf::from("/dev/cpu/0/msr");
    match access(&path) {
        Access::Readable => Probe::Available,
        Access::Restricted => Probe::Missing(format!(
            "{} is present and not readable by uid {}: an MSR read needs root or \
             CAP_SYS_RAWIO, which is more than this project asks for elsewhere",
            path.display(),
            current_uid()
        )),
        Access::Absent => Probe::Missing(format!(
            "{} does not exist, so `{requirement}` cannot be read: the `msr` module is not \
             loaded, or this is not an x86 machine",
            path.display()
        )),
    }
}

/// Whether a perf event is only ever raised from kernel code, so that a count
/// excluding kernel mode is zero whatever the task did.
///
/// A context switch and a migration are both counted by the scheduler, with
/// the scheduler's registers: `perf_exclude_event` drops every one of them for
/// an event that sets `exclude_kernel`. A page fault is not on this list -- it
/// is raised with the faulting task's registers, which are user mode for a
/// fault in user code, so a user-mode count of it is the program's own faults.
pub fn counted_only_in_kernel(kind: u32, config: u64) -> bool {
    use perf_event_open_sys::bindings as perf;
    kind == perf::PERF_TYPE_SOFTWARE
        && (config == perf::PERF_COUNT_SW_CONTEXT_SWITCHES as u64
            || config == perf::PERF_COUNT_SW_CPU_MIGRATIONS as u64)
}

fn probe_pmu(event: &str) -> Probe {
    // A PMU that is not the cpu's -- RAPL, the uncore, the memory controller --
    // counts for a whole package and refuses to be opened against a process.
    // Asking it the per-process question answers "cannot open" for an event the
    // machine offers, which is how RAPL read as missing on a machine where it
    // works.
    let system_wide = event.contains('/') && !event.starts_with("cpu/");
    let Some((kind, config)) = named_pmu_event(event)
        .or_else(|| cache_event(event))
        .or_else(|| sysfs_event(event))
    else {
        return match sysfs_device_state(event) {
            // A unit that is there may still refuse this process: AMD's op
            // sampler read available under a sandbox that opens no perf event
            // at all, and every run of `mem.latency` was refused at the attach.
            // It is opened the way its collector opens it.
            Some(Probe::Available) if event == crate::perf::IbsOp::PMU => probe_ibs_op(),
            Some(probe) => probe,
            None => Probe::Unknown(format!(
                "`pmu:{event}` needs an event encoding this tool does not build yet"
            )),
        };
    };

    // An event the kernel raises only from its own code -- the scheduler's
    // switch and migration counters -- counts nothing once kernel mode is
    // excluded, and excluding it is exactly what perf_event_paranoid 2 lets an
    // unprivileged process do. So it is asked for with the kernel included:
    // "you may open it for user mode" would be a yes for a counter that then
    // reads zero, which is what the CI's paranoid-2 cell measured (0 switches
    // against the kernel's 50).
    let kernel_only = counted_only_in_kernel(kind, config);

    let mut attr: perf_event_open_sys::bindings::perf_event_attr = unsafe { std::mem::zeroed() };
    attr.type_ = kind;
    attr.size = std::mem::size_of::<perf_event_open_sys::bindings::perf_event_attr>() as u32;
    attr.config = config;
    attr.set_disabled(1);
    if !system_wide && !kernel_only {
        // Meaningless for a package counter, and rejected: RAPL answers EINVAL
        // to an event that asks it to exclude kernel mode, because it does not
        // have modes to exclude.
        attr.set_exclude_kernel(1);
        attr.set_exclude_hv(1);
    }

    // A cpu event is opened against this process, the weakest thing that can be
    // asked for: if that is refused, nothing wider will be allowed either. A
    // package-wide one has to be opened the only way it exists, against every
    // process on one cpu.
    let (pid, cpu) = if system_wide { (-1, 0) } else { (0, -1) };
    let fd = unsafe {
        perf_event_open_sys::perf_event_open(
            &mut attr,
            pid,
            cpu,
            -1,
            perf_event_open_sys::bindings::PERF_FLAG_FD_CLOEXEC as u64,
        )
    };
    if fd >= 0 {
        unsafe { libc::close(fd) };
        return Probe::Available;
    }

    let error = std::io::Error::last_os_error();
    let paranoid =
        read_trimmed("/proc/sys/kernel/perf_event_paranoid").unwrap_or_else(|| "unknown".into());
    match error.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) if kernel_only => Probe::Missing(format!(
            "`{event}` is raised by the kernel's own code, so a count of user mode only reads \
             zero, and perf_event_paranoid {paranoid} allows this process nothing else: it needs \
             perf_event_paranoid at 1 or below, or CAP_PERFMON on this binary"
        )),
        Some(libc::EPERM) if !system_wide && seccomp_refusal().is_some() => {
            Probe::Missing(format!(
                "perf_event_open was refused for `{event}`: {}",
                seccomp_refusal().unwrap_or_default()
            ))
        }
        Some(libc::EACCES) | Some(libc::EPERM) => Probe::Missing(format!(
            "perf_event_open was refused for `{event}`: perf_event_paranoid is {paranoid}{}",
            paranoid_beyond_capabilities()
        )),
        Some(libc::ENOENT) | Some(libc::EOPNOTSUPP) => {
            Probe::Missing(format!("`{event}` is not available on this cpu"))
        }
        _ => Probe::Missing(format!("perf_event_open failed for `{event}`: {error}")),
    }
}

/// Opens AMD's op sampler the way `mem.latency`'s collector does: one event
/// following this process on one cpu, excluding the kernel only where the unit
/// can (`swfilt`), since without it the unit refuses every `exclude_*` bit --
/// which is why it needs `perf_event_paranoid` at 1 or below, or CAP_PERFMON.
fn probe_ibs_op() -> Probe {
    use perf_event_open_sys::bindings as perf;
    let Some(unit) = crate::perf::IbsOp::probe() else {
        return Probe::Missing(format!(
            "this machine publishes no readable `{}` type under {PMU_DEVICES}",
            crate::perf::IbsOp::PMU
        ));
    };
    // The unit's own minimum is sixteen ops; a period this long never fires
    // in the microseconds the event is open.
    let spec = unit.spec(crate::perf::Rate::Period(1 << 20), true, false);
    let mut attr: perf::perf_event_attr = unsafe { std::mem::zeroed() };
    attr.type_ = spec.kind;
    attr.size = std::mem::size_of::<perf::perf_event_attr>() as u32;
    attr.config = spec.config;
    attr.__bindgen_anon_4.config2 = spec.config2;
    attr.__bindgen_anon_1.sample_period = 1 << 20;
    attr.set_disabled(1);
    if spec.exclude_kernel {
        attr.set_exclude_kernel(1);
        attr.set_exclude_hv(1);
    }
    let cpu = online_cpus()
        .ok()
        .and_then(|cpus| cpus.first().copied())
        .unwrap_or(0) as i32;
    // SAFETY: a zeroed attr with its size set, opened on this process.
    let fd = unsafe {
        perf_event_open_sys::perf_event_open(
            &mut attr,
            0,
            cpu,
            -1,
            perf::PERF_FLAG_FD_CLOEXEC as u64,
        )
    };
    if fd >= 0 {
        // SAFETY: closing a descriptor just opened here.
        unsafe { libc::close(fd) };
        return Probe::Available;
    }
    let error = std::io::Error::last_os_error();
    let paranoid =
        read_trimmed("/proc/sys/kernel/perf_event_paranoid").unwrap_or_else(|| "unknown".into());
    match error.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => Probe::Missing(format!(
            "perf_event_open was refused for `{}` on cpu {cpu}: following a process's ops needs \
             perf_event_paranoid at 1 or below (it is {paranoid}) or CAP_PERFMON on this \
             binary{}",
            crate::perf::IbsOp::PMU,
            if unit.can_exclude_kernel() {
                ""
            } else {
                ", and this unit has no `swfilt` to leave the kernel's ops out"
            }
        )),
        _ => Probe::Missing(format!(
            "perf_event_open failed for `{}` on cpu {cpu}: {error}",
            crate::perf::IbsOp::PMU
        )),
    }
}

/// The netlink protocols the catalog names, and their kernel numbers.
fn netlink_protocol(requirement: &str) -> Option<i32> {
    match requirement {
        // NETLINK_SOCK_DIAG
        "sock_diag" => Some(4),
        // NETLINK_ROUTE.
        "rtnetlink" | "netlink" => Some(0),
        // `ethtool` is deliberately absent: it is a generic-netlink family,
        // resolved by name through CTRL_CMD_GETFAMILY, not a protocol number.
        // Answering it as NETLINK_ROUTE would report a different question's
        // answer, and reporting it unverified is the honest state until the
        // family lookup exists.
        _ => None,
    }
}

/// Asks whether a netlink protocol can be opened, rather than assuming a kernel
/// built with it.
fn probe_netlink(requirement: &str, protocol: i32) -> Probe {
    // SAFETY: creating a socket; the arguments are constants.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            protocol,
        )
    };
    if fd >= 0 {
        // SAFETY: closing a descriptor just created here.
        unsafe { libc::close(fd) };
        return Probe::Available;
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EPROTONOSUPPORT) => Probe::Missing(format!(
            "this kernel has no netlink protocol for `{requirement}`"
        )),
        Some(libc::EPERM) | Some(libc::EACCES) => Probe::Missing(format!(
            "opening netlink for `{requirement}` was refused: {error}"
        )),
        _ => Probe::Missing(format!("cannot open netlink for `{requirement}`: {error}")),
    }
}

/// Whether this process may open a packet socket.
///
/// Opening one is the only way to know: the capability can come from the
/// process, from a file capability on the binary, or from a user namespace, and
/// none of those are visible by reading a file.
fn probe_af_packet(requirement: &str) -> Probe {
    // SAFETY: creating a socket; the arguments are constants.
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0) };
    if fd >= 0 {
        // SAFETY: closing a descriptor just created here.
        unsafe { libc::close(fd) };
        return Probe::Available;
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) => Probe::Missing(format!(
            "`{requirement}` needs CAP_NET_RAW, which uid {} does not have; this is a privilege to grant, not a kernel to change -- `make CAPTURE=1` grants it, and only when asked, since it can send frames as well as read them",
            current_uid()
        )),
        Some(libc::EAFNOSUPPORT) => {
            Probe::Missing("this kernel was built without AF_PACKET".into())
        }
        _ => Probe::Missing(format!("cannot open a packet socket: {error}")),
    }
}

fn current_uid() -> u32 {
    // SAFETY: `getuid` reads process state and cannot fail.
    unsafe { libc::getuid() }
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

/// Picks the `[selected]` entry out of a sysfs enumeration such as
/// `always [madvise] never`.
fn selected_bracket(line: &str) -> Option<String> {
    line.split_whitespace()
        .find_map(|word| {
            word.strip_prefix('[')
                .and_then(|word| word.strip_suffix(']'))
        })
        .map(str::to_owned)
        .or_else(|| Some(line.to_owned()))
}

fn mitigations() -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu/vulnerabilities") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Ok(value) = std::fs::read_to_string(entry.path()) {
            found.insert(name, value.trim().to_owned());
        }
    }
    found
}

/// Enumerates the graphics devices through `/sys/class/drm`.
///
/// Only the `cardN` entries are devices. The same directory also holds one
/// entry per connector -- `card1-DP-1`, `card1-HDMI-A-1` -- and a render node,
/// and counting those would report six GPUs on a machine with one.
///
/// A card with no PCI ids is skipped rather than reported with blanks: a
/// virtual or platform device that cannot be identified is not a device two
/// runs can be compared on, and an entry naming nothing would suggest
/// otherwise.
fn gpus() -> Vec<GpuDevice> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut found: Vec<GpuDevice> = Vec::new();
    for entry in entries.flatten() {
        let Some(card) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_card(&card) {
            continue;
        }
        let device = entry.path().join("device");
        let (Some(vendor), Some(id)) = (
            read_trimmed(&device.join("vendor").to_string_lossy()),
            read_trimmed(&device.join("device").to_string_lossy()),
        ) else {
            continue;
        };
        let driver = std::fs::read_link(device.join("driver"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            });
        let driver_version = driver
            .as_deref()
            .and_then(|driver| read_trimmed(&format!("/sys/module/{driver}/version")));
        found.push(GpuDevice {
            card,
            pci_id: format!("{vendor}:{id}"),
            driver,
            driver_version,
        });
    }
    found.sort_by(|a, b| a.card.cmp(&b.card));
    found
}

/// `card1` is a device; `card1-DP-1` is one of its connectors.
fn is_card(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

fn arch() -> String {
    let mut info: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `uname` fills a struct we own and never fails for a valid pointer.
    if unsafe { libc::uname(&mut info) } != 0 {
        return "unknown".into();
    }
    let bytes: Vec<u8> = info
        .machine
        .iter()
        .take_while(|byte| **byte != 0)
        // `c_char` is `i8` on x86-64 and `u8` on aarch64: the same bits
        // either way, and the cast a no-op on one of the two.
        .map(|byte| u8::from_ne_bytes(byte.to_ne_bytes()))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn available_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0)
}

/// Where the kernel lists the cpus it currently has.
pub const ONLINE_CPUS: &str = "/sys/devices/system/cpu/online";

/// The cpus this machine has right now, as the kernel numbers them.
///
/// Read rather than derived from a core count: the numbering has holes on a
/// machine where a cpu was taken offline, and `nproc` would hand back a count
/// that indexes cpus which do not exist. A per-cpu event opened on one of those
/// fails with `ENODEV`, which reads like a permission problem and is not.
pub fn online_cpus() -> std::io::Result<Vec<u32>> {
    let list = std::fs::read_to_string(ONLINE_CPUS)?;
    parse_cpu_list(&list).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{ONLINE_CPUS} does not read as a cpu list: `{}`",
                list.trim()
            ),
        )
    })
}

/// `0-3,8,10-11`, which is how the kernel writes every cpu list it publishes.
pub fn parse_cpu_list(list: &str) -> Option<Vec<u32>> {
    let mut cpus = Vec::new();
    for part in list
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        match part.split_once('-') {
            Some((first, last)) => {
                let first: u32 = first.parse().ok()?;
                let last: u32 = last.parse().ok()?;
                if last < first {
                    return None;
                }
                cpus.extend(first..=last);
            }
            None => cpus.push(part.parse().ok()?),
        }
    }
    Some(cpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seccomp filter is named where the level would have allowed the
    /// event, and only there: at 3 the level refuses it whatever the filter.
    #[test]
    fn a_seccomp_filter_is_named_where_the_level_allows_the_event() {
        assert!(seccomp_explanation(Some(2), Some(2))
            .is_some_and(|why| why.contains("seccomp filter") && why.contains("CAP_PERFMON")));
        assert_eq!(seccomp_explanation(Some(2), Some(3)), None);
        assert_eq!(seccomp_explanation(Some(0), Some(2)), None);
        assert_eq!(seccomp_explanation(None, Some(1)), None);
    }

    /// A perf kprobe is let in by `CAP_PERFMON`, `CAP_SYS_ADMIN` or root, and
    /// refused with that reason otherwise: the `kprobe` PMU existing is not
    /// enough, which is how `mem.swap` was listed usable on the nocaps cells.
    /// Above 2 the level is Debian's and Ubuntu's, and the refusal says that
    /// CAP_PERFMON does not lift it; at 2 and below it says nothing more.
    #[test]
    fn a_paranoid_level_above_two_is_said_to_need_cap_sys_admin() {
        let said = super::paranoid_explanation(Some(3));
        assert!(
            said.contains("CAP_SYS_ADMIN") && said.contains("CAP_PERFMON included"),
            "{said}"
        );
        assert_eq!(super::paranoid_explanation(Some(2)), "");
        assert_eq!(super::paranoid_explanation(Some(-1)), "");
        assert_eq!(super::paranoid_explanation(None), "");
    }

    #[test]
    fn a_kprobe_is_missing_without_perfmon_whatever_the_pmu_says() {
        let holding = |uid: u32, effective: &[&str]| Privileges {
            uid,
            effective: effective.iter().map(|name| (*name).to_owned()).collect(),
            no_new_privs: false,
            withheld: None,
        };
        let refused = kprobe_not_permitted(Some(&holding(1000, &["CAP_BPF"])))
            .expect("CAP_BPF alone does not open a kprobe");
        assert!(refused.contains("CAP_PERFMON"), "{refused}");
        assert_eq!(
            kprobe_not_permitted(Some(&holding(1000, &["CAP_PERFMON"]))),
            None
        );
        assert_eq!(
            kprobe_not_permitted(Some(&holding(1000, &["CAP_SYS_ADMIN"]))),
            None
        );
        assert_eq!(kprobe_not_permitted(Some(&holding(0, &[]))), None);
        assert_eq!(kprobe_not_permitted(None), None);
    }

    /// `writable:` asks the question a control file is used by. This process's
    /// own `clear_refs` is write-only for its owner, and is what the probe was
    /// written for; a file nobody but root may write, and one that is not
    /// there, are each missing, with which of the two it is.
    #[test]
    fn a_writable_requirement_asks_whether_the_file_may_be_written() {
        let capabilities = super::Capabilities::detect();
        let own = std::process::id() as i32;
        assert_eq!(
            capabilities.probe("writable:/proc/[pid]/clear_refs", Some(own)),
            super::Probe::Available,
            "this process may write its own clear_refs"
        );
        // Read-only by construction for everyone but root, which a test binary
        // is not: the kernel's own version string.
        if unsafe { libc::geteuid() } != 0 {
            match capabilities.probe("writable:/proc/version", None) {
                super::Probe::Missing(reason) => {
                    assert!(reason.contains("not writable"), "{reason}")
                }
                other => panic!("/proc/version answered {other:?}"),
            }
        }
        match capabilities.probe("writable:/proc/omn-no-such-file", None) {
            super::Probe::Missing(reason) => assert!(reason.contains("does not exist"), "{reason}"),
            other => panic!("an absent file answered {other:?}"),
        }
    }

    /// This thread's own switches, counted by the kernel in
    /// `/proc/thread-self/status`: voluntary plus involuntary.
    fn switches_of_this_thread() -> u64 {
        let status = std::fs::read_to_string("/proc/thread-self/status").expect("status");
        status
            .lines()
            .filter(|line| line.contains("ctxt_switches:"))
            .filter_map(|line| line.split_whitespace().last()?.parse::<u64>().ok())
            .sum()
    }

    /// The scheduler's counters are raised from kernel code, so excluding
    /// kernel mode leaves them at zero: the probe has to ask with the kernel
    /// included, or it says yes to a counter that reads nothing.
    ///
    /// Ground truth is the kernel's own per-thread account in
    /// `/proc/thread-self/status`, over a workload that sleeps a chosen number
    /// of times on this thread alone (the counter is opened on this thread,
    /// not the process, because `cargo test` runs its tests as threads of one).
    #[test]
    fn a_switch_counter_that_excludes_the_kernel_counts_none_and_is_not_offered() {
        use perf_event_open_sys::bindings as perf;
        const SLEEPS: u64 = 30;
        let count = |exclude_kernel: bool| -> Result<(u64, u64), std::io::Error> {
            let mut attr: perf::perf_event_attr = unsafe { std::mem::zeroed() };
            attr.type_ = perf::PERF_TYPE_SOFTWARE;
            attr.size = std::mem::size_of::<perf::perf_event_attr>() as u32;
            attr.config = perf::PERF_COUNT_SW_CONTEXT_SWITCHES as u64;
            // Opened disabled, and enabled only once the first status read has
            // returned: the counted window then lies inside the kernel's
            // bracket, so every switch the counter sees the kernel saw too.
            attr.set_disabled(1);
            if exclude_kernel {
                attr.set_exclude_kernel(1);
                attr.set_exclude_hv(1);
            }
            let fd = unsafe {
                perf_event_open_sys::perf_event_open(
                    &mut attr,
                    0,
                    -1,
                    -1,
                    perf::PERF_FLAG_FD_CLOEXEC as u64,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let before = switches_of_this_thread();
            assert_eq!(unsafe { perf_event_open_sys::ioctls::ENABLE(fd, 0) }, 0);
            for _ in 0..SLEEPS {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert_eq!(unsafe { perf_event_open_sys::ioctls::DISABLE(fd, 0) }, 0);
            let after = switches_of_this_thread();
            let mut value = 0u64;
            let read = unsafe { libc::read(fd, (&mut value as *mut u64).cast(), 8) };
            unsafe { libc::close(fd) };
            assert_eq!(read, 8, "the counter reads");
            Ok((value, after - before))
        };

        // Opening user mode only is what paranoid 2 allows everyone; above it
        // (Ubuntu's 4) nothing opens, and the probe has to say so.
        let (user_only, kernel_saw) = match count(true) {
            Ok(counted) => counted,
            Err(error) => {
                match super::probe_pmu("PERF_COUNT_SW_CONTEXT_SWITCHES") {
                    super::Probe::Missing(reason) => {
                        assert!(reason.contains("perf_event_paranoid"), "{reason}")
                    }
                    other => {
                        panic!("no perf event opens here ({error}), and the probe said {other:?}")
                    }
                }
                eprintln!(
                    "skipped: this machine opens no perf event for this process ({error}), \
                     perf_event_paranoid {}",
                    super::read_trimmed("/proc/sys/kernel/perf_event_paranoid")
                        .unwrap_or_else(|| "unknown".into())
                );
                return;
            }
        };
        assert!(
            kernel_saw >= SLEEPS,
            "{SLEEPS} sleeps, and the kernel saw {kernel_saw} switches"
        );
        assert_eq!(
            user_only, 0,
            "a user-mode switch counter read {user_only} over {kernel_saw} switches"
        );

        let probe = super::probe_pmu("PERF_COUNT_SW_CONTEXT_SWITCHES");
        match count(false) {
            Ok((counted, kernel_saw)) => {
                // The counter is enabled after the first status read and
                // disabled before the second, so its window lies inside the
                // kernel's bracket: a switch in the gaps (the thread preempted
                // around an ioctl, at this wave's load) is in the kernel's
                // account and not the counter's, never the other way round.
                // Each of the thirty sleeps is a switch inside the window.
                assert!(
                    counted >= SLEEPS && counted <= kernel_saw,
                    "the counter read {counted} and the kernel {kernel_saw} for {SLEEPS} sleeps"
                );
                assert_eq!(probe, super::Probe::Available);
            }
            Err(error) => {
                eprintln!(
                    "this machine will not count kernel mode for this process ({error}), so \
                     the probe must say so rather than offer a counter that reads zero"
                );
                match probe {
                    super::Probe::Missing(reason) => {
                        assert!(reason.contains("reads zero"), "{reason}");
                        assert!(reason.contains("CAP_PERFMON"), "{reason}");
                    }
                    other => panic!("a switch counter that can only read zero answered {other:?}"),
                }
            }
        }
    }

    /// `pmu:ibs_op` is answered by opening AMD's op sampler, not by finding it
    /// in sysfs. Ground truth is an open made without the probe's code: the
    /// unit's type number read straight from sysfs, no config bit and no
    /// `exclude_*` bit, following this thread. That open asks for more than the
    /// probe's does (the kernel's ops included), so where it succeeds the probe
    /// must say available; where it is refused, missing, naming the sysctl.
    ///
    /// A refusal is then made here, whatever this machine allows: a seccomp
    /// filter on one thread of its own makes `perf_event_open` answer EACCES,
    /// which is what a sandbox or a paranoid kernel answers, and the probe run
    /// on that thread has to read missing with `perf_event_paranoid` named.
    #[test]
    fn the_op_sampler_is_available_exactly_when_it_opens() {
        use perf_event_open_sys::bindings as perf;
        let Some(kind) = super::read_trimmed(&format!(
            "{}/{}/type",
            super::PMU_DEVICES,
            crate::perf::IbsOp::PMU
        ))
        .and_then(|kind| kind.parse::<u32>().ok()) else {
            eprintln!(
                "skipped: this machine publishes no `{}` unit under {}, so there is no op \
                 sampler to open",
                crate::perf::IbsOp::PMU,
                super::PMU_DEVICES
            );
            return;
        };

        // Opened as `mem.latency`'s collector opens it: the kernel's ops left
        // out where the unit can (`swfilt`), which perf_event_paranoid 2
        // allows an unprivileged process; an event that keeps them it does
        // not.
        let spec = crate::perf::IbsOp::probe()
            .expect("the unit's type was just read")
            .spec(crate::perf::Rate::Period(1 << 20), true, false);
        let reference = {
            let mut attr: perf::perf_event_attr = unsafe { std::mem::zeroed() };
            attr.type_ = kind;
            attr.size = std::mem::size_of::<perf::perf_event_attr>() as u32;
            attr.config = spec.config;
            attr.__bindgen_anon_4.config2 = spec.config2;
            attr.__bindgen_anon_1.sample_period = 1 << 20;
            attr.set_disabled(1);
            if spec.exclude_kernel {
                attr.set_exclude_kernel(1);
                attr.set_exclude_hv(1);
            }
            // SAFETY: a zeroed attr with its size set, opened on this thread.
            let fd = unsafe {
                perf_event_open_sys::perf_event_open(
                    &mut attr,
                    0,
                    -1,
                    -1,
                    perf::PERF_FLAG_FD_CLOEXEC as u64,
                )
            };
            if fd >= 0 {
                // SAFETY: closing a descriptor just opened here.
                unsafe { libc::close(fd) };
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        };
        let probe = super::probe_pmu(crate::perf::IbsOp::PMU);
        match &reference {
            Ok(()) => assert_eq!(
                probe,
                super::Probe::Available,
                "the op sampler opens on this thread, and the probe said otherwise"
            ),
            Err(error) if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM)) => {
                match &probe {
                    super::Probe::Missing(reason) => {
                        assert!(reason.contains("perf_event_paranoid"), "{reason}")
                    }
                    other => panic!(
                        "the op sampler is refused here ({error}), and the probe said {other:?}"
                    ),
                }
            }
            Err(error) => {
                assert!(
                    matches!(probe, super::Probe::Missing(_)),
                    "the op sampler does not open here ({error}), and the probe said {probe:?}"
                );
            }
        }

        // The refusal, made on a thread of its own: a seccomp filter binds the
        // thread that installs it and nothing else in this process.
        let refused = std::thread::spawn(|| {
            const LOAD_SYSCALL_NR: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
            const JUMP_IF_EQUAL: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
            const RETURN: u16 = 0x06; // BPF_RET | BPF_K
            const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
            const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
            let filter = [
                libc::sock_filter {
                    code: LOAD_SYSCALL_NR,
                    jt: 0,
                    jf: 0,
                    k: 0,
                },
                libc::sock_filter {
                    code: JUMP_IF_EQUAL,
                    jt: 0,
                    jf: 1,
                    k: libc::SYS_perf_event_open as u32,
                },
                libc::sock_filter {
                    code: RETURN,
                    jt: 0,
                    jf: 0,
                    k: SECCOMP_RET_ERRNO | libc::EACCES as u32,
                },
                libc::sock_filter {
                    code: RETURN,
                    jt: 0,
                    jf: 0,
                    k: SECCOMP_RET_ALLOW,
                },
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_ptr() as *mut libc::sock_filter,
            };
            // SAFETY: both calls affect only this thread, which ends with the
            // closure; the program outlives the prctl that copies it.
            unsafe {
                assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
                assert_eq!(
                    libc::prctl(
                        libc::PR_SET_SECCOMP,
                        libc::SECCOMP_MODE_FILTER,
                        &program as *const libc::sock_fprog
                    ),
                    0,
                    "installing the filter: {}",
                    std::io::Error::last_os_error()
                );
            }
            super::probe_pmu(crate::perf::IbsOp::PMU)
        })
        .join()
        .expect("the filtered thread");
        match refused {
            super::Probe::Missing(reason) => {
                assert!(reason.contains("refused"), "{reason}");
                assert!(reason.contains("perf_event_paranoid"), "{reason}");
                assert!(reason.contains("CAP_PERFMON"), "{reason}");
            }
            other => panic!("an op sampler whose open is refused answered {other:?}"),
        }
    }

    /// A qualified `pmu:unit/event` whose unit or event this machine does not
    /// publish is missing, naming which -- never unknown, which the resolver
    /// would have walked past as "assumed" onto a start that fails.
    #[test]
    fn a_qualified_pmu_event_that_is_not_published_is_missing_by_name() {
        match super::probe_pmu("omnnounit/energy-pkg/") {
            super::Probe::Missing(reason) => {
                assert!(
                    reason.contains("no `omnnounit` performance unit"),
                    "{reason}"
                )
            }
            other => panic!("an absent unit answered {other:?}"),
        }
        if !std::path::Path::new(super::PMU_DEVICES)
            .join("cpu")
            .exists()
        {
            eprintln!("skipped: this machine publishes no `cpu` performance unit to ask about an event of");
            return;
        }
        match super::probe_pmu("cpu/omn-no-such-event") {
            super::Probe::Missing(reason) => assert!(
                reason.contains("`cpu` performance unit publishes no `omn-no-such-event`"),
                "{reason}"
            ),
            other => panic!("an absent event of a present unit answered {other:?}"),
        }
    }

    /// Why `omn` makes itself dumpable again at startup, proved without needing
    /// the capabilities that cause the problem.
    ///
    /// A binary carrying file capabilities is marked non-dumpable at exec, and
    /// the kernel then reassigns its `/proc/<pid>` entries to root -- so the
    /// tool loses the ability to read its own procfs, and the heap sources that
    /// need no privileges at all stop resolving the moment the eBPF ones are
    /// granted theirs.
    ///
    /// `PR_SET_DUMPABLE` is the same switch by a different route, so the whole
    /// effect is reproducible by an ordinary process. It runs in a forked child
    /// because dumpability is process-wide and the rest of the suite is running
    /// in the other threads of this one.
    #[test]
    fn a_non_dumpable_process_cannot_read_its_own_procfs() {
        const UNREADABLE_WHILE_HIDDEN: i32 = 0;
        const READABLE_THROUGHOUT: i32 = 1;
        const NEVER_READABLE: i32 = 2;

        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            let path = format!("/proc/{}/pagemap\0", std::process::id());
            let readable = || unsafe {
                libc::faccessat(
                    libc::AT_FDCWD,
                    path.as_ptr().cast(),
                    libc::R_OK,
                    libc::AT_EACCESS,
                ) == 0
            };
            let code = if !readable() {
                NEVER_READABLE
            } else {
                unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
                let hidden = !readable();
                unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) };
                if hidden && readable() {
                    UNREADABLE_WHILE_HIDDEN
                } else {
                    READABLE_THROUGHOUT
                }
            };
            unsafe { libc::_exit(code) };
        }

        let mut status = 0;
        assert!(
            unsafe { libc::waitpid(child, &mut status, 0) } == child,
            "waiting for the child"
        );
        assert!(libc::WIFEXITED(status), "the child did not exit normally");
        match libc::WEXITSTATUS(status) {
            NEVER_READABLE => {
                eprintln!("skipped: this kernel does not expose a readable pagemap at all")
            }
            UNREADABLE_WHILE_HIDDEN => {}
            READABLE_THROUGHOUT => panic!(
                "a non-dumpable process could still read its own pagemap, so \
                 `restore_own_dumpability` in main.rs is guarding against nothing on this \
                 kernel and the comment there is wrong"
            ),
            other => panic!("unexpected exit status {other}"),
        }
    }

    #[test]
    fn kernel_versions_parse_from_a_release_string() {
        assert_eq!(
            KernelVersion::parse("6.12.4-arch1-1"),
            Some(KernelVersion {
                major: 6,
                minor: 12
            })
        );
        assert!(KernelVersion::parse("6.1") < KernelVersion::parse("6.12"));
    }

    /// A file that exists and cannot be read must probe as restricted.
    ///
    /// This is the rule the whole `Access` type exists for, and it was broken:
    /// the probe asked `stat`, which answers a different question. Every
    /// requirement naming a root-only file -- the RAPL energy counters, the MSR
    /// devices, several sysfs paths across `power`, `cpu` and `gpu` -- was
    /// reported as available and then failed at open.
    #[test]
    fn a_file_that_exists_and_cannot_be_read_is_restricted_not_available() {
        // Any root-only file under a readable directory will do; RAPL is the
        // one that caused this, and the test skips where it is absent.
        let candidates = [
            "/sys/class/powercap/intel-rapl:0/energy_uj",
            "/proc/1/environ",
            "/proc/kcore",
        ];
        let Some(path) = candidates
            .iter()
            .map(Path::new)
            .find(|path| path.exists() && std::fs::read_to_string(path).is_err())
        else {
            eprintln!(
                "skipped: every candidate path is readable here, so there is nothing to check"
            );
            return;
        };

        assert_eq!(
            access(path),
            Access::Restricted,
            "{} cannot be read, so it must not probe as available",
            path.display()
        );
        let probe = Capabilities::detect().probe(&path.display().to_string(), None);
        assert!(probe.is_missing(), "{probe}");
        assert!(
            probe.to_string().contains("not readable"),
            "the reason must send the reader to privileges, not to a missing file: {probe}"
        );
    }

    #[test]
    fn a_restricted_path_is_not_reported_as_absent() {
        // Whatever this machine's tracefs is, the two states must not be
        // confused: one calls for privileges, the other for a different kernel.
        let capabilities = Capabilities::detect();
        if capabilities.environment().tracefs == Access::Restricted {
            let probe = capabilities.probe("tracefs", None);
            assert!(probe.to_string().contains("not readable"), "{probe}");
            assert!(!probe.to_string().contains("not mounted"), "{probe}");
        }
    }

    #[test]
    fn a_missing_file_is_reported_as_missing_not_unknown() {
        let capabilities = Capabilities::detect();
        let probe = capabilities.probe("/proc/definitely-not-here", None);
        assert!(probe.is_missing(), "{probe}");
    }

    /// A family answers the requirement classes it introduces, and the probe
    /// walks through this module to reach it. Losing that hookup would not
    /// break a build: every `gpu` source would quietly go back to reading `?`
    /// on a machine where the answer is known.
    #[test]
    fn a_requirement_a_family_answers_is_not_left_unknown() {
        let capabilities = Capabilities::detect();
        let probe = capabilities.probe("nvml", None);
        assert!(
            !probe.is_unknown(),
            "the gpu family answers `nvml`, so this must be checked, not assumed: {probe}"
        );
        let hwmon = capabilities.probe("hwmon:definitely-not-a-sensor", None);
        assert!(hwmon.is_missing(), "{hwmon}");
    }

    #[test]
    fn a_requirement_with_no_probe_is_unknown_rather_than_assumed() {
        let capabilities = Capabilities::detect();
        // A userspace static probe in a library nobody has enumerated: still
        // genuinely unchecked, so the answer must be "not checked", never
        // "available". (`pmu:uncore_imc` used to serve here and is now
        // answered, which is the improvement that moved this test.)
        let probe = capabilities.probe("usdt:libc", None);
        assert!(probe.is_unknown(), "{probe}");
        assert!(
            !probe.is_available(),
            "an unwritten probe must never read as available"
        );
    }

    /// Ground truth: a temperature read from sysfs directly, as an integer in
    /// millidegrees, by this test and not through the probe.
    #[test]
    fn hwmon_is_available_exactly_when_a_temperature_can_be_read() {
        let probe = Capabilities::detect().probe("hwmon", None);
        assert!(!probe.is_unknown(), "{probe}");
        let readable = std::fs::read_dir("/sys/class/hwmon")
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|chip| {
                std::fs::read_dir(chip.path())
                    .into_iter()
                    .flatten()
                    .flatten()
            })
            .filter(|file| {
                let name = file.file_name().to_string_lossy().into_owned();
                name.starts_with("temp") && name.ends_with("_input")
            })
            .any(|file| {
                std::fs::read_to_string(file.path())
                    .is_ok_and(|text| text.trim().parse::<i64>().is_ok())
            });
        assert_eq!(probe.is_available(), readable, "{probe}");
        let nowhere = probe_hwmon(Path::new("/nonexistent/hwmon"));
        assert!(nowhere.to_string().contains("absent"), "{nowhere}");
    }

    /// Every preload source requires `dynamic-linking`, and it read as
    /// unverified on every run of any of them until it was answered.
    #[test]
    fn dynamic_linking_is_answered_by_looking_for_the_shim() {
        let probe = Capabilities::detect().probe("dynamic-linking", None);
        assert!(!probe.is_unknown(), "{probe}");
        if let Probe::Missing(reason) = &probe {
            assert!(
                reason.contains("libomn_preload.so") || reason.contains("OMN_PRELOAD_LIB"),
                "a missing shim must name where it was looked for: {reason}"
            );
        }
    }

    /// Against the vendors' documentation, not against this code: Intel's
    /// `0x1a4` with bits 0 and 2 set has the L2 hardware prefetcher and the
    /// DCU streamer off and the other two on; AMD's `PrefetchControl` at zero
    /// has everything on.
    #[test]
    fn a_set_bit_disables_its_prefetcher() {
        let intel = decode_prefetchers(&INTEL_PREFETCH, 0b0101);
        assert!(!intel["l2_hardware"]);
        assert!(intel["l2_adjacent_line"]);
        assert!(!intel["dcu"]);
        assert!(intel["dcu_ip"]);

        let amd = decode_prefetchers(&AMD_PREFETCH, 0);
        assert_eq!(amd.len(), 5);
        assert!(amd.values().all(|on| *on));
        assert!(!decode_prefetchers(&AMD_PREFETCH, 1 << 5)["l2_up_down"]);
    }

    #[test]
    fn the_register_is_chosen_by_vendor_and_family_and_zen_two_has_none() {
        assert_eq!(prefetch_register("GenuineIntel", 6), Ok(INTEL_PREFETCH));
        assert_eq!(prefetch_register("AuthenticAMD", 0x19), Ok(AMD_PREFETCH));
        assert_eq!(prefetch_register("AuthenticAMD", 0x1a), Ok(AMD_PREFETCH));
        let zen2 = prefetch_register("AuthenticAMD", 0x17).unwrap_err();
        assert!(zen2.contains("0x17"), "{zen2}");
        assert!(prefetch_register("HygonGenuine", 0x18).is_err());
    }

    #[test]
    fn the_cpu_identity_is_read_from_the_first_block_only() {
        let cpuinfo = "processor\t: 0\nvendor_id\t: AuthenticAMD\ncpu family\t: 23\n\n\
                       processor\t: 1\nvendor_id\t: GenuineIntel\ncpu family\t: 6\n";
        assert_eq!(
            cpu_identity(cpuinfo.as_bytes()),
            Some(("AuthenticAMD".to_owned(), 23))
        );
        assert_eq!(cpu_identity("processor\t: 0\n".as_bytes()), None);
    }

    /// Ground truth: the kernel's own answer to whether the register can be
    /// read, taken through `faccessat` on the device rather than through the
    /// capture. A state that could not be read must never come back as a
    /// reading, and a reading must name the register this cpu has.
    #[test]
    fn a_prefetcher_state_that_cannot_be_read_is_never_reported_as_read() {
        let captured = Capabilities::detect()
            .environment()
            .prefetchers
            .clone()
            .expect("captured on every detect");
        let identity = cpu_identity(std::io::BufReader::new(
            std::fs::File::open("/proc/cpuinfo").unwrap(),
        ));
        let known = identity
            .as_ref()
            .and_then(|(vendor, family)| prefetch_register(vendor, *family).ok());
        let readable = access(Path::new("/dev/cpu/0/msr")).is_readable();
        match (&captured, known, readable) {
            (Prefetchers::Unknown { .. }, None, _) => {}
            (Prefetchers::Unreadable { register, .. }, Some(known), false) => {
                assert_eq!(register, &format!("{:#x}", known.msr))
            }
            (
                Prefetchers::Read {
                    register, enabled, ..
                },
                Some(known),
                true,
            ) => {
                assert_eq!(register, &format!("{:#x}", known.msr));
                assert_eq!(enabled.len(), known.bits.len());
            }
            other => panic!("the capture disagrees with the machine: {other:?}"),
        }
    }

    #[test]
    fn a_named_perf_event_is_answered_by_asking_the_kernel() {
        let capabilities = Capabilities::detect();
        // Software page faults exist on every kernel, so only permission can
        // stand in the way -- and then the answer names the reason.
        let probe = capabilities.probe("pmu:PERF_COUNT_SW_PAGE_FAULTS", None);
        assert!(!probe.is_unknown(), "{probe}");
        if probe.is_missing() {
            assert!(
                probe.to_string().contains("paranoid") || probe.to_string().contains("cpu"),
                "{probe}"
            );
        }
    }

    #[test]
    fn a_missing_capability_is_not_reported_as_a_missing_kernel_feature() {
        let capabilities = Capabilities::detect();
        let probe = capabilities.probe("af_packet:tpacket_v3", None);
        assert!(!probe.is_unknown(), "{probe}");
        if probe.is_missing() {
            let message = probe.to_string();
            // Whichever it is, the reader must be told where to go: a
            // capability to grant, or a kernel that cannot do it at all.
            assert!(
                message.contains("CAP_NET_RAW") || message.contains("without AF_PACKET"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_netlink_protocol_is_answered_by_opening_it() {
        let capabilities = Capabilities::detect();
        // Every kernel this tool targets has sock_diag; only a stripped build
        // or a sandbox refuses it, and then the answer says which.
        let probe = capabilities.probe("sock_diag", None);
        assert!(!probe.is_unknown(), "{probe}");
    }

    /// A key of `memory.stat` is answered from the file the kernel writes for
    /// this process's own cgroup: a key read independently out of that file is
    /// available, and one no kernel writes is missing and says which file was
    /// read.
    #[test]
    fn a_cgroup_key_is_answered_from_the_file_the_kernel_writes() {
        let capabilities = Capabilities::detect();
        if capabilities.environment().cgroup2 == Access::Absent {
            eprintln!("skipped: no cgroup v2 hierarchy here");
            return;
        }
        let own = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let own = own
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .unwrap_or("/");
        let mut directory = Path::new("/sys/fs/cgroup").join(own.trim_start_matches('/'));
        let text = loop {
            if let Ok(text) = std::fs::read_to_string(directory.join("memory.stat")) {
                break Some(text);
            }
            if !directory.pop() || !directory.starts_with("/sys/fs/cgroup") {
                break None;
            }
        };
        let absent = capabilities.probe("cgroup2:memory.stat:definitely_not_a_key", None);
        let Some(text) = text else {
            assert!(absent.is_unknown(), "{absent}");
            return;
        };
        let first = text
            .split_whitespace()
            .next()
            .expect("memory.stat is not empty");
        let present = capabilities.probe(&format!("cgroup2:memory.stat:{first}"), None);
        assert!(
            present.is_available(),
            "`{first}` is in the file: {present}"
        );
        assert!(absent.is_missing(), "{absent}");
        assert!(absent.to_string().contains("memory.stat"), "{absent}");
    }

    #[test]
    fn a_cgroup_file_absent_from_the_root_is_judged_by_its_controller() {
        let capabilities = Capabilities::detect();
        if capabilities.environment().cgroup2 == Access::Absent {
            return;
        }
        // The root cgroup has no `memory.current`, yet every child does, so a
        // machine with the memory controller enabled must not read as missing.
        let probe = capabilities.probe("cgroup2:memory.current", None);
        let controllers =
            std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers").unwrap_or_default();
        if controllers.split_whitespace().any(|name| name == "memory") {
            assert!(probe.is_available(), "{probe}");
        }
    }

    #[test]
    fn this_process_own_procfs_files_are_found() {
        let capabilities = Capabilities::detect();
        assert!(capabilities.probe("/proc/[pid]/statm", None).is_available());
    }

    /// The card entries under `/sys/class/drm` are the devices; everything
    /// else there is a connector or a render node. Counting those would report
    /// six GPUs on a machine that has one, which is exactly the kind of number
    /// nobody checks.
    #[test]
    fn a_connector_is_not_mistaken_for_a_card() {
        assert!(is_card("card0"));
        assert!(is_card("card12"));
        assert!(!is_card("card1-DP-1"));
        assert!(!is_card("card1-HDMI-A-1"));
        assert!(!is_card("renderD128"));
        assert!(!is_card("version"));
        assert!(!is_card("card"));
    }

    /// Ground truth: the kernel's own description of its graphics devices.
    ///
    /// The capture is checked against the files it did not read through --
    /// the directory listing, the two PCI id files and the driver's own
    /// version -- rather than against itself. On a machine with no `/sys/class
    /// /drm` at all the expectation is an empty list, which is still a check:
    /// a machine with no GPU must report none, not fail to look.
    #[test]
    fn every_graphics_device_the_kernel_lists_is_captured_with_what_the_kernel_says() {
        let captured = Capabilities::detect().environment().gpus.clone();

        let mut expected: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let digits = name
                    .strip_prefix("card")
                    .filter(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
                if digits.is_some() && entry.path().join("device/vendor").exists() {
                    expected.push(name);
                }
            }
        }
        expected.sort();

        let cards: Vec<String> = captured.iter().map(|gpu| gpu.card.clone()).collect();
        assert_eq!(
            cards, expected,
            "the capture must hold exactly the cards the kernel lists"
        );

        for gpu in &captured {
            let device = PathBuf::from("/sys/class/drm")
                .join(&gpu.card)
                .join("device");
            let vendor = std::fs::read_to_string(device.join("vendor")).unwrap();
            let id = std::fs::read_to_string(device.join("device")).unwrap();
            assert_eq!(
                gpu.pci_id,
                format!("{}:{}", vendor.trim(), id.trim()),
                "the ids must be the kernel's, not a reformatting of them"
            );
            let Some(driver) = &gpu.driver else { continue };
            assert!(
                std::fs::read_link(device.join("driver"))
                    .unwrap()
                    .ends_with(driver),
                "the driver must be the one actually bound to {}",
                gpu.card
            );
            assert_eq!(
                gpu.driver_version,
                std::fs::read_to_string(format!("/sys/module/{driver}/version"))
                    .ok()
                    .map(|version| version.trim().to_owned()),
                "`{driver}` publishes a version or it does not; inventing one is worse than \
                 leaving it out"
            );
        }
    }

    /// The reason this capture reads sysfs instead of calling `nvmlInit`.
    ///
    /// `Capabilities::detect` runs on every command, including `omn list` and
    /// every refusal, and `nvmlInit` was measured at 3.3 to 4.9 ms on this
    /// machine. A capture that costs a millisecond of every invocation is one
    /// that will be made conditional, and then forgotten for the requests it
    /// was made conditional on. The bound is the fastest of twenty runs,
    /// because a mean on a loaded machine measures the scheduler.
    #[test]
    fn capturing_the_graphics_devices_is_cheap_enough_to_do_unconditionally() {
        let fastest = (0..20)
            .map(|_| {
                let started = std::time::Instant::now();
                std::hint::black_box(gpus());
                started.elapsed()
            })
            .min()
            .expect("twenty runs");
        assert!(
            fastest < std::time::Duration::from_millis(1),
            "the sysfs walk took {fastest:?}, which is the order of the nvmlInit call it exists \
             to avoid"
        );
    }

    /// Whether a path's mount is `nosuid` is read from `mountinfo`, and the
    /// kernel answers the same question through another call: `statvfs`'s
    /// `ST_NOSUID`. The two agree for every mount a binary could be on here.
    /// The bpf layer's advice already says why the grant did not reach this
    /// process; the probe's rewrite must not say it a second time.
    #[test]
    fn a_reason_that_already_says_why_the_grant_was_withheld_is_left_as_it_is() {
        let withheld = "The binary /mnt/omn carries CAP_BPF and CAP_PERFMON, but this process \
                        was not given them: the filesystem at /mnt is mounted nosuid";
        let said = format!("loading a tracing program needs CAP_BPF and CAP_PERFMON. {withheld}.");
        assert_eq!(rewrite_withheld(said.clone(), withheld), said);

        let granted = "x needs CAP_BPF and CAP_PERFMON. Grant them to the binary with `sudo \
                       setcap cap_bpf,cap_perfmon+ep $(which omn)`, or run it under sudo. This \
                       is the privilege model.";
        let rewritten = rewrite_withheld(granted.to_owned(), withheld);
        assert_eq!(rewritten.matches(withheld).count(), 1, "{rewritten}");
        assert!(!rewritten.contains("setcap"), "{rewritten}");

        let bare = rewrite_withheld("bpf: CAP_BPF missing".to_owned(), withheld);
        assert_eq!(bare, format!("bpf: CAP_BPF missing; {withheld}"));
    }

    #[test]
    fn a_mount_is_nosuid_exactly_when_statvfs_says_so() {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        let mut paths: Vec<PathBuf> = ["/", "/proc", "/sys", "/dev", "/dev/shm", "/tmp", "/run"]
            .iter()
            .map(PathBuf::from)
            .collect();
        paths.push(std::env::current_exe().unwrap());
        paths.push(std::env::temp_dir());
        let mut nosuid = 0;
        for path in paths.iter().filter(|path| path.exists()) {
            let c_path =
                std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes()).unwrap();
            // SAFETY: a NUL-terminated path and a struct for the kernel to fill.
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) }, 0);
            let kernel = stat.f_flag & libc::ST_NOSUID != 0;
            let (point, read) = mount_of(&mountinfo, path).expect("every path is on a mount");
            assert_eq!(
                read,
                kernel,
                "{} is on {}: mountinfo says nosuid {read}, statvfs {kernel}",
                path.display(),
                point.display()
            );
            nosuid += usize::from(kernel);
        }
        assert!(
            nosuid > 0,
            "/proc and /sys are nosuid everywhere, and none was found so"
        );
    }

    /// The file's attribute is decoded as `getcap` prints it, for the binary
    /// `make` granted: `cap_bpf` and `cap_perfmon`, effective.
    #[test]
    fn a_binary_s_file_capabilities_are_read_as_getcap_prints_them() {
        let omn = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/omn");
        let Ok(output) = std::process::Command::new("getcap").arg(&omn).output() else {
            eprintln!("skipped: no `getcap` on this machine to compare with");
            return;
        };
        let printed = String::from_utf8_lossy(&output.stdout).to_string();
        let decoded = file_capabilities(&omn);
        if !printed.contains("cap_bpf") {
            assert!(
                decoded.is_none_or(|(permitted, _)| permitted >> 39 & 1 == 0),
                "getcap prints {printed:?} and the attribute decodes to {decoded:?}"
            );
            eprintln!(
                "skipped the positive half: {} carries no capabilities (run `make`)",
                omn.display()
            );
            return;
        }
        let (permitted, effective) = decoded.expect("getcap printed capabilities");
        for (bit, name) in GRANTED_TO_THE_BINARY {
            assert_eq!(
                permitted >> bit & 1 == 1,
                printed.contains(&name.to_lowercase()),
                "{name}: getcap prints {printed:?}, decoded {permitted:#x}"
            );
        }
        assert_eq!(
            effective,
            printed.contains("=ep") || printed.contains("+ep"),
            "{printed}"
        );
    }

    /// A file that carries the two capabilities, and a process that holds
    /// neither -- this test's own, by the kernel's `/proc/self/status` -- is
    /// told which mount withholds them, and never to run `setcap`.
    #[test]
    fn a_capability_the_file_carries_and_the_process_lacks_is_explained_by_its_mount() {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        if capability_set(&status, "CapEff").unwrap_or(0) >> 39 & 1 == 1 {
            eprintln!("skipped: this test process holds CAP_BPF, so nothing is withheld from it");
            return;
        }
        let exe = Path::new("/opt/omn/bin/omn");
        let file = Some((1u64 << 38 | 1u64 << 39, true));
        let nosuid = "22 1 0:21 / / rw,relatime - ext4 /dev/root rw\n\
                      40 22 0:40 / /opt/omn rw,nosuid,nodev - tmpfs tmpfs rw\n";
        let said = withheld_reason(exe, file, &status, nosuid).expect("both are withheld");
        assert!(
            said.contains("CAP_BPF and CAP_PERFMON")
                && said.contains("/opt/omn is mounted nosuid")
                && !said.contains("setcap"),
            "{said}"
        );
        let plain = "22 1 0:21 / / rw,relatime - ext4 /dev/root rw\n";
        let said = withheld_reason(exe, Some((1u64 << 39, false)), &status, plain).unwrap();
        assert!(said.contains("without `e`"), "{said}");
        assert_eq!(withheld_reason(exe, None, &status, nosuid), None);
    }

    /// The effective set recorded is the one `capget` returns, asked of the
    /// kernel directly rather than through `/proc`.
    #[test]
    fn the_privileges_recorded_are_the_ones_capget_returns() {
        #[repr(C)]
        struct Header {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        struct Data {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        let mut header = Header {
            version: 0x2008_0522, // _LINUX_CAPABILITY_VERSION_3
            pid: 0,
        };
        let mut data = [Data::default(); 2];
        // SAFETY: a version-3 header and the two words it asks for.
        let result = unsafe {
            libc::syscall(
                libc::SYS_capget,
                &mut header as *mut Header,
                data.as_mut_ptr(),
            )
        };
        assert_eq!(result, 0, "capget: {}", std::io::Error::last_os_error());
        let effective = u64::from(data[0].effective) | u64::from(data[1].effective) << 32;
        let recorded = privileges().expect("/proc/self/status is readable");
        for (bit, name) in CAPABILITIES_ASKED {
            assert_eq!(
                effective >> bit & 1 == 1,
                recorded.effective.iter().any(|held| held == name),
                "{name}: capget {effective:#x}, recorded {:?}",
                recorded.effective
            );
        }
        // SAFETY: a read-only query.
        assert_eq!(recorded.uid, unsafe { libc::geteuid() });
        // SAFETY: a read-only query of this thread's flag.
        let flag = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
        assert_eq!(recorded.no_new_privs, flag == 1);
    }

    /// A guest is told from a host the way systemd tells them apart.
    #[test]
    fn a_virtual_machine_is_recorded_as_one_where_systemd_detects_one() {
        let Ok(output) = std::process::Command::new("systemd-detect-virt")
            .arg("--vm")
            .output()
        else {
            eprintln!("skipped: no `systemd-detect-virt` on this machine to compare with");
            return;
        };
        let systemd = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let recorded = Capabilities::detect()
            .environment()
            .virtualization
            .clone()
            .expect("/proc/cpuinfo is readable");
        assert_eq!(
            systemd == "none",
            recorded == "none",
            "systemd-detect-virt says {systemd:?}, the record {recorded:?}"
        );
    }

    #[test]
    fn the_environment_captures_what_makes_runs_comparable() {
        let environment = Capabilities::detect().environment().clone();
        assert!(!environment.kernel_release.is_empty());
        assert!(environment.cpus > 0);
        assert!(
            !environment.not_captured.is_empty(),
            "what is not captured must be stated"
        );
    }
}
