//! What a backend needs of the machine, read from the catalog's `requires:`
//! strings once, at load.
//!
//! The catalog stays data: a requirement is still written as the string
//! `docs/CATALOG.md` documents, and `omn list` still shows that string. What moves
//! here is the reading. A string no rule below reads is refused when the
//! catalog loads, not answered `unknown` on the machine that runs it, and the
//! probe (`Capabilities::probe_need`) is one `match` over [`Need`] that the
//! compiler holds complete: a kind added here is a probe someone must write,
//! or must say is not written.

use std::fmt;
use std::str::FromStr;

use crate::capabilities::KernelVersion;

/// One requirement of one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// `kernel>=X.Y`: a release, where no feature can be asked instead.
    KernelAtLeast(KernelVersion),
    /// `/path`, `[pid]` substituted for the target's: a file that must be
    /// readable.
    Path(String),
    /// `writable:/path`: a control file used by writing it.
    Writable(String),
    /// `cgroup2:<file>` or `cgroup2:<file>:<key>`.
    Cgroup2(String),
    /// `tracepoint:<category>:<name>`: a classic tracepoint, read from tracefs.
    Tracepoint(String),
    /// `raw_tracepoint:<name>` or `raw_tracepoint:<name>(<proto>)|(<proto>)`:
    /// looked for in the kernel's BTF, its prototype compared when one is
    /// written. Checked well formed at load.
    RawTracepoint(String),
    /// `fentry:<function>` (or `fentry:a or b`).
    Fentry(String),
    /// `kprobe:<function>`: a kernel symbol a kprobe lands on, and one a call
    /// reaches (`bpf::fentry::kprobe_support`).
    KprobeOn(String),
    /// `btf_member:<struct>.<member>`.
    BtfMember(String),
    /// `pmu:<event>`.
    Pmu(String),
    /// `msr:<register>`.
    Msr(String),
    /// `af_packet:<version>`.
    AfPacket(String),
    /// `bpf_map:<type>`: a map type this kernel creates, such as `ringbuf`.
    BpfMap(BpfMap),
    /// `symbols:<name>`: a function the C library exports.
    Symbols(String),
    /// `usdt:<provider>`: USDT probes an object on this machine carries.
    Usdt(String),
    /// `rdt:<feature>`: a resctrl monitoring feature, `mbm` for memory
    /// bandwidth.
    Rdt(String),
    /// One of the words the probe answers whole.
    Word(Word),
    /// A vendor library or device the gpu family answers for
    /// (`collect::gpu::probe`): `nvml`, `cupti`, `rocprofiler`, `level-zero`,
    /// `nvidia:profiling`, `hwmon:<driver>`.
    Gpu(String),
    /// A kind the catalog declares and no probe answers yet. Listed by name,
    /// so a misspelt requirement is still refused at load; answered `unknown`,
    /// which never counts as available.
    Unprobed(String),
}

/// The map types a requirement can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BpfMap {
    /// `BPF_MAP_TYPE_RINGBUF`, which the programs publish their events
    /// through: Linux 5.8, and backported to some older releases (RHEL 8).
    Ringbuf,
}

/// The requirements that are one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Word {
    Bpf,
    Btf,
    Tracefs,
    Callchain,
    Hwmon,
    Kprobe,
    Uprobe,
    UprobeMulti,
    Phc,
    Ethtool,
    /// `netlink` or `rtnetlink`: NETLINK_ROUTE.
    Rtnetlink,
    /// `sock_diag`: NETLINK_SOCK_DIAG.
    SockDiag,
    /// `preload:shim`.
    PreloadShim,
    /// `dynamic-linking`.
    DynamicLinking,
    /// `damon`: the kernel's data access monitor, through its sysfs.
    Damon,
    /// `so_timestamping`: the socket option that stamps packets.
    SoTimestamping,
    /// `cpufreq:transitions`: a cpufreq driver that sets the frequency, and
    /// so announces each change on `power:cpu_frequency`.
    CpufreqTransitions,
}

/// Builds a requirement from what follows its prefix.
type Make = fn(String) -> Need;

/// The prefixes of requirements no probe answers yet: declared, so the
/// listing can say what a backend waits on, and refused nowhere else.
const UNPROBED_PREFIXES: [&str; 1] = ["allocator-stats:"];

/// The single words no probe answers yet.
const UNPROBED_WORDS: [&str; 1] = ["xdp"];

/// The gpu family's single words.
const GPU_WORDS: [&str; 6] = [
    "nvml",
    "cupti",
    "rocprofiler",
    "rocprofv3",
    "level-zero",
    "nvidia:profiling",
];

impl Need {
    /// Whether the kernel's answer depends on the process asking with the
    /// target's pid substituted in (`/proc/[pid]/...`).
    pub fn names_the_target(&self) -> bool {
        matches!(self, Need::Path(path) | Need::Writable(path) if path.contains("[pid]"))
    }
}

impl Word {
    fn of(text: &str) -> Option<Self> {
        Some(match text {
            "bpf" => Word::Bpf,
            "btf" => Word::Btf,
            "tracefs" => Word::Tracefs,
            "callchain" => Word::Callchain,
            "hwmon" => Word::Hwmon,
            "kprobe" => Word::Kprobe,
            "uprobe" => Word::Uprobe,
            "uprobe_multi" => Word::UprobeMulti,
            "phc" => Word::Phc,
            "ethtool" => Word::Ethtool,
            "netlink" | "rtnetlink" => Word::Rtnetlink,
            "sock_diag" => Word::SockDiag,
            "preload:shim" => Word::PreloadShim,
            "dynamic-linking" => Word::DynamicLinking,
            "damon" => Word::Damon,
            "so_timestamping" => Word::SoTimestamping,
            "cpufreq:transitions" => Word::CpufreqTransitions,
            _ => return None,
        })
    }
}

impl FromStr for Need {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        if text.starts_with('/') {
            return Ok(Need::Path(text.to_owned()));
        }
        if let Some(version) = text.strip_prefix("kernel>=") {
            return KernelVersion::parse(version)
                .map(Need::KernelAtLeast)
                .ok_or_else(|| format!("`{text}` names no kernel release"));
        }
        let prefixed: [(&str, Make); 14] = [
            ("symbols:", Need::Symbols),
            ("usdt:", Need::Usdt),
            ("rdt:", Need::Rdt),
            ("writable:", Need::Writable),
            ("cgroup2:", Need::Cgroup2),
            ("tracepoint:", Need::Tracepoint),
            ("raw_tracepoint:", Need::RawTracepoint),
            ("fentry:", Need::Fentry),
            ("kprobe:", Need::KprobeOn),
            ("btf_member:", Need::BtfMember),
            ("pmu:", Need::Pmu),
            ("msr:", Need::Msr),
            ("af_packet:", Need::AfPacket),
            ("hwmon:", |driver| Need::Gpu(format!("hwmon:{driver}"))),
        ];
        if let Some(word) = Word::of(text) {
            return Ok(Need::Word(word));
        }
        if let Some(map) = text.strip_prefix("bpf_map:") {
            return match map {
                "ringbuf" => Ok(Need::BpfMap(BpfMap::Ringbuf)),
                other => Err(format!(
                    "`bpf_map:{other}` is not a map type this build asks"
                )),
            };
        }
        if GPU_WORDS.contains(&text) {
            return Ok(Need::Gpu(text.to_owned()));
        }
        for (prefix, make) in prefixed {
            if let Some(rest) = text.strip_prefix(prefix) {
                if rest.is_empty() {
                    return Err(format!("`{text}` names nothing after `{prefix}`"));
                }
                if prefix == "raw_tracepoint:" {
                    crate::bpf::parse_raw_tracepoint(rest)
                        .map_err(|reason| format!("`{text}`: {reason}"))?;
                }
                return Ok(make(rest.to_owned()));
            }
        }
        if UNPROBED_WORDS.contains(&text)
            || UNPROBED_PREFIXES
                .iter()
                .any(|prefix| text.starts_with(prefix))
        {
            return Ok(Need::Unprobed(text.to_owned()));
        }
        Err(format!(
            "`{text}` is not a requirement this build can read: `docs/CATALOG.md`, \"Requirements\", \
             lists the forms"
        ))
    }
}

impl fmt::Display for Need {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Need::KernelAtLeast(version) => write!(f, "kernel>={version}"),
            Need::Path(path) => f.write_str(path),
            Need::Writable(path) => write!(f, "writable:{path}"),
            Need::Cgroup2(file) => write!(f, "cgroup2:{file}"),
            Need::Tracepoint(event) => write!(f, "tracepoint:{event}"),
            Need::RawTracepoint(event) => write!(f, "raw_tracepoint:{event}"),
            Need::Fentry(function) => write!(f, "fentry:{function}"),
            Need::KprobeOn(function) => write!(f, "kprobe:{function}"),
            Need::BtfMember(member) => write!(f, "btf_member:{member}"),
            Need::Pmu(event) => write!(f, "pmu:{event}"),
            Need::Msr(register) => write!(f, "msr:{register}"),
            Need::AfPacket(version) => write!(f, "af_packet:{version}"),
            Need::BpfMap(BpfMap::Ringbuf) => f.write_str("bpf_map:ringbuf"),
            Need::Symbols(name) => write!(f, "symbols:{name}"),
            Need::Usdt(provider) => write!(f, "usdt:{provider}"),
            Need::Rdt(feature) => write!(f, "rdt:{feature}"),
            Need::Word(word) => f.write_str(match word {
                Word::Bpf => "bpf",
                Word::Btf => "btf",
                Word::Tracefs => "tracefs",
                Word::Callchain => "callchain",
                Word::Hwmon => "hwmon",
                Word::Kprobe => "kprobe",
                Word::Uprobe => "uprobe",
                Word::UprobeMulti => "uprobe_multi",
                Word::Phc => "phc",
                Word::Ethtool => "ethtool",
                Word::Rtnetlink => "rtnetlink",
                Word::SockDiag => "sock_diag",
                Word::PreloadShim => "preload:shim",
                Word::DynamicLinking => "dynamic-linking",
                Word::Damon => "damon",
                Word::SoTimestamping => "so_timestamping",
                Word::CpufreqTransitions => "cpufreq:transitions",
            }),
            Need::Gpu(text) | Need::Unprobed(text) => f.write_str(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every requirement the shipped catalog writes is read by a rule, and
    /// reads back as itself -- but for `netlink`, the one alias.
    #[test]
    fn every_shipped_requirement_reads_and_reads_back() {
        let catalog = crate::catalog::Catalog::embedded().expect("the embedded catalog loads");
        for entry in catalog.entries() {
            for backend in &entry.backends {
                for text in &backend.requires {
                    let need: Need = text
                        .parse()
                        .unwrap_or_else(|reason| panic!("{}: {reason}", entry.id));
                    if text != "netlink" {
                        assert_eq!(need.to_string(), *text);
                    }
                }
            }
        }
    }

    #[test]
    fn a_misspelt_requirement_is_refused_by_name() {
        let refused = "kprobes".parse::<Need>().unwrap_err();
        assert!(refused.contains("`kprobes`"), "{refused}");
        assert!("raw_tracepoint:x(task_struct *".parse::<Need>().is_err());
        assert!("bpf_map:hash".parse::<Need>().is_err());
        assert!("pmu:".parse::<Need>().is_err());
    }

    #[test]
    fn a_declared_kind_with_no_probe_reads_as_unprobed() {
        assert_eq!("xdp".parse::<Need>().unwrap(), Need::Unprobed("xdp".into()));
        assert_eq!(
            "bpf_map:ringbuf".parse::<Need>().unwrap(),
            Need::BpfMap(BpfMap::Ringbuf)
        );
    }
}
