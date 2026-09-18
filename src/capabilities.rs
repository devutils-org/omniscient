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
    /// What is deliberately not captured yet, so its absence is not read as
    /// "this machine has none of it".
    pub not_captured: Vec<String>,
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
            not_captured: vec![
                "microarchitecture".into(),
                "NUMA topology".into(),
                "virtualization and steal time".into(),
                "vendor SDK versions".into(),
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
        let root = PathBuf::from("/sys/fs/cgroup");
        if access(&root.join(file)).is_readable() {
            return Probe::Available;
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
                Access::Readable => {
                    let Some(root) = tracefs_root() else {
                        return Probe::Missing("tracefs is not mounted".into());
                    };
                    probe_path(&root.join("events").join(event.replace(':', "/")))
                }
            };
        }

        if let Some(event) = requirement.strip_prefix("pmu:") {
            return probe_pmu(event);
        }

        // A raw tracepoint is looked for in the kernel's own BTF rather than in
        // tracefs, because that is how this tool attaches to one: through
        // `BPF_RAW_TRACEPOINT_OPEN`, which takes a name and needs no tracefs.
        if let Some(event) = requirement.strip_prefix("raw_tracepoint:") {
            return Probe::from(crate::bpf::raw_tracepoint_support(event));
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

        if requirement == "preload:shim" {
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

const TRACEFS_ROOTS: [&str; 2] = ["/sys/kernel/tracing", "/sys/kernel/debug/tracing"];

fn tracefs_root() -> Option<PathBuf> {
    TRACEFS_ROOTS
        .iter()
        .map(PathBuf::from)
        .find(|path| access(&path.join("events")).is_readable())
}

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

/// Asks the kernel whether an event can be opened, rather than inferring it
/// from the cpu vendor or the kernel version.
///
/// The answer distinguishes "this cpu has no such counter" from "this machine
/// will not let you read it", because one calls for different hardware and the
/// other for `perf_event_paranoid`.
fn probe_pmu(event: &str) -> Probe {
    let Some((kind, config)) = named_pmu_event(event) else {
        return Probe::Unknown(format!(
            "`pmu:{event}` needs an event encoding this tool does not build yet"
        ));
    };

    let mut attr: perf_event_open_sys::bindings::perf_event_attr = unsafe { std::mem::zeroed() };
    attr.type_ = kind;
    attr.size = std::mem::size_of::<perf_event_open_sys::bindings::perf_event_attr>() as u32;
    attr.config = config;
    attr.set_disabled(1);
    attr.set_exclude_kernel(1);
    attr.set_exclude_hv(1);

    // Opened against this process, which is the weakest thing that can be
    // asked for: if this is refused, nothing wider will be allowed either.
    let fd = unsafe {
        perf_event_open_sys::perf_event_open(
            &mut attr,
            0,
            -1,
            -1,
            perf_event_open_sys::bindings::PERF_FLAG_FD_CLOEXEC as u64,
        )
    };
    if fd >= 0 {
        unsafe { libc::close(fd) };
        return Probe::Available;
    }

    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => Probe::Missing(format!(
            "perf_event_open was refused for `{event}`: perf_event_paranoid is {}",
            read_trimmed("/proc/sys/kernel/perf_event_paranoid")
                .unwrap_or_else(|| "unknown".into())
        )),
        Some(libc::ENOENT) | Some(libc::EOPNOTSUPP) => {
            Probe::Missing(format!("`{event}` is not available on this cpu"))
        }
        _ => Probe::Missing(format!("perf_event_open failed for `{event}`: {error}")),
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
            "`{requirement}` needs CAP_NET_RAW, which uid {} does not have; this is a privilege to grant, not a kernel to change",
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
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn available_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Uncore boxes need an event encoding this tool does not build, so the
        // answer must be "not checked", never "available".
        let probe = capabilities.probe("pmu:uncore_imc", None);
        assert!(probe.is_unknown(), "{probe}");
        assert!(
            !probe.is_available(),
            "an unwritten probe must never read as available"
        );
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
