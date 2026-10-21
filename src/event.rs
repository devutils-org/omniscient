//! The event model.
//!
//! An `Event` is append-only: produced once by a collector, never modified
//! afterwards. Everything downstream -- joins, aggregation, emit -- derives new
//! values rather than editing events in place, so a stream can be replayed and
//! give the same answer.
//!
//! The fields are deliberately few. `ts_ns` and `ts_err_ns` give the time and
//! its uncertainty, `keys` give what the event can be joined on, `attrs` give
//! what it measured, and `collector_id` plus `seq` say where it came from and in
//! what order it was produced. Anything specific to a subsystem lives in
//! `attrs`, which is what keeps the correlator from growing a case per
//! subsystem.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::request::KeyKind;

/// Which collector produced an event, so a stream can be split back apart and
/// losses attributed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CollectorId(String);

impl CollectorId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id of the collector built for a source through a backend: the
    /// registry's key, `<source>/<backend>`, which the runner stamps on every
    /// event a collection hands over (`collect::stamp`).
    pub fn of(source: &str, backend: &str) -> Self {
        Self(format!("{source}/{backend}"))
    }

    /// What a collector writes on the events it builds. The collector does not
    /// name itself: the runner knows which source and backend it was built
    /// for, and stamps that ([`CollectorId::of`]) before anything reads the
    /// event, so the name can never disagree with the registry.
    pub fn unstamped() -> Self {
        Self(String::new())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CollectorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// `CLOCK_MONOTONIC`, nanoseconds. One domain for every collector.
    pub ts_ns: u64,
    /// How far off `ts_ns` may be. Zero when the timestamp was taken in this
    /// domain; non-zero when it was converted from a device's own clock.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ts_err_ns: u32,
    /// What happened, named as the catalog names it: `mem.rss`, `io.latency`.
    pub kind: String,
    /// What this event can be joined on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<KeyValue>,
    /// What it measured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attrs: Vec<Attr>,
    pub collector_id: CollectorId,
    /// Per-collector sequence number, so a gap is visible even when timestamps
    /// are not.
    pub seq: u64,
}

impl Event {
    pub fn attr(&self, name: &str) -> Option<&AttrValue> {
        self.attrs
            .iter()
            .find(|attr| attr.name == name)
            .map(|attr| &attr.value)
    }

    pub fn key(&self, kind: KeyKind) -> Option<&KeyValue> {
        self.keys.iter().find(|key| key.kind() == kind)
    }
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn is_zero_long(value: &u64) -> bool {
    *value == 0
}

/// A typed correlation key carried by an event.
///
/// The set is closed, and each variant names the boundary it crosses. A join
/// rule matches on these and nothing else, which is what keeps the join engine
/// from knowing about `bio` or `malloc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyValue {
    /// A propagated request identity.
    Req {
        id: String,
    },
    /// A thread.
    Task {
        tid: i32,
    },
    /// A process, unambiguous across containers.
    Proc {
        pidns: u64,
        pid: i32,
    },
    /// A file, by device and inode.
    File {
        dev: u64,
        ino: u64,
    },
    /// A socket, by cookie: the key that survives softirq processing.
    Sock {
        cookie: u64,
    },
    /// A block request. Scoped by open timestamp because pointers get recycled.
    Blk {
        ptr: u64,
        open_ts: u64,
    },
    /// A memory range, matched by interval.
    Mem {
        base: u64,
        len: u64,
    },
    /// A GPU fence.
    Gpu {
        fence_ctx: u64,
        seqno: u64,
    },
    Cgroup {
        id: u64,
    },
    /// A network interface, by network namespace, kernel index and name. The
    /// index is what the kernel uses, the name is what a person reads, and an
    /// interface that gets renamed is still the same interface -- a join
    /// compares the namespace and the index ([`KeyValue::iface_identity`]),
    /// and the name in place of the index only when no index was known at the
    /// read.
    ///
    /// An index is only an index inside one network namespace: `lo` is 1 in
    /// every one. `netns` is that namespace's inode, as `/proc/<pid>/ns/net`
    /// names it -- the kernel's `net->ns.inum` for a producer that reads the
    /// packet's device, this process's own for one that reads a name in its
    /// own namespace. Zero is "not known": a key written by hand or read back
    /// from an older file, which meets only another such key.
    ///
    /// The name may carry a sub-series after a `/` (`lo/drops`, `net.iface`'s
    /// counter kinds): no Linux interface name holds a `/`, so everything
    /// before it is the interface ([`KeyValue::iface_name`]).
    Iface {
        #[serde(default, skip_serializing_if = "is_zero_long")]
        netns: u64,
        index: u32,
        name: String,
    },
    /// A block device, by the major and minor the kernel writes in
    /// `/proc/diskstats`. The numbers rather than the name because a name is a
    /// symlink away from meaning something else, and `dm-0` and `nvme0n1p2`
    /// both resolve to a pair.
    Dev {
        major: u32,
        minor: u32,
    },
    /// A logical cpu, as the kernel numbers them.
    Cpu {
        cpu: u32,
    },
    /// A memory node, as the kernel numbers them.
    Node {
        node: u32,
    },
    /// The direction a byte or an operation went: one series per direction
    /// of a counter that counts both, so every aggregating emit splits them
    /// without `raw` beside it.
    Dir {
        dir: Direction,
    },
    /// `syscall:<name>`, the system call an event counted or timed: what a
    /// `strace -c` table is one row per, so one `proc.syscall` run keyed on
    /// it answers that table rather than one run per syscall. The name is
    /// this build's table for the architecture (`syscall_<n>` where it has
    /// none).
    Syscall {
        name: String,
    },
    /// `comm:<name>`, the task's name as the kernel held it when the event
    /// happened: what bpftrace's and bcc's tools aggregate by, where `task`
    /// keeps one series per thread.
    Comm {
        comm: String,
    },
    /// An energy domain (`package`, `cores`, `dram`): domains nest -- the
    /// cores are inside their package -- so a sum across them counts the same
    /// joules twice, and each is its own series.
    Domain {
        name: String,
    },
}

/// Which way a counted byte or operation went. A closed set: a block device
/// reads, writes, discards and flushes; a link receives and transmits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Read,
    Write,
    Discard,
    Flush,
    Rx,
    Tx,
}

impl Direction {
    pub const ALL: [Direction; 6] = [
        Direction::Read,
        Direction::Write,
        Direction::Discard,
        Direction::Flush,
        Direction::Rx,
        Direction::Tx,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Read => "read",
            Direction::Write => "write",
            Direction::Discard => "discard",
            Direction::Flush => "flush",
            Direction::Rx => "rx",
            Direction::Tx => "tx",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl KeyValue {
    /// An interface key in this process's network namespace: what a producer
    /// that reads a name or an index in its own namespace writes
    /// (`/proc/net/dev`, `if_nametoindex`, a socket this process opened). An
    /// index of 0 is resolved from the name at the read, while the interface
    /// exists, so every producer's key of one interface carries its index; it
    /// stays 0 when the kernel knows no interface by that name here.
    pub fn iface(index: u32, name: impl Into<String>) -> Self {
        let name = name.into();
        let index = if index == 0 {
            index_of(iface_base(&name))
        } else {
            index
        };
        KeyValue::Iface {
            netns: own_netns(),
            index,
            name,
        }
    }

    /// An interface key in the network namespace `netns` (its inode): what a
    /// producer that reads the device in the kernel writes, since the device
    /// may be in any namespace. Nothing is looked up: an index of another
    /// namespace means nothing in this one.
    pub fn iface_in(netns: u64, index: u32, name: impl Into<String>) -> Self {
        KeyValue::Iface {
            netns,
            index,
            name: name.into(),
        }
    }

    /// An interface key exactly as given, with no lookup and no namespace:
    /// what a test, or an event read back from a file, writes.
    pub fn iface_as_given(index: u32, name: impl Into<String>) -> Self {
        KeyValue::Iface {
            netns: 0,
            index,
            name: name.into(),
        }
    }

    /// The network namespace an `iface` key's index belongs to, by inode;
    /// `None` for every other key, and for one whose namespace is not known.
    pub fn iface_netns(&self) -> Option<u64> {
        match self {
            KeyValue::Iface { netns, .. } if *netns != 0 => Some(*netns),
            _ => None,
        }
    }

    /// The interface an `iface` key names, without the sub-series a producer
    /// may have written after a `/`. `None` for every other key.
    pub fn iface_name(&self) -> Option<&str> {
        match self {
            KeyValue::Iface { name, .. } => Some(iface_base(name)),
            _ => None,
        }
    }

    /// What two `iface` keys are compared by: the network namespace and the
    /// kernel's index when the read knew it (`iface#<netns>:<index>`), so a
    /// rename is still one interface, `lo/drops` is `lo`, and another
    /// namespace's `lo` is another interface; the namespace and the name
    /// otherwise (`iface:<netns>:<name>`). `None` for every other key.
    pub fn iface_identity(&self) -> Option<String> {
        match self {
            KeyValue::Iface { netns, index, .. } if *index != 0 => {
                Some(format!("iface#{netns}:{index}"))
            }
            KeyValue::Iface { netns, name, .. } => {
                Some(format!("iface:{netns}:{}", iface_base(name)))
            }
            _ => None,
        }
    }

    pub fn dev(major: u32, minor: u32) -> Self {
        KeyValue::Dev { major, minor }
    }

    pub fn cpu(cpu: u32) -> Self {
        KeyValue::Cpu { cpu }
    }

    pub fn node(node: u32) -> Self {
        KeyValue::Node { node }
    }

    pub fn dir(dir: Direction) -> Self {
        KeyValue::Dir { dir }
    }

    pub fn domain(name: impl Into<String>) -> Self {
        KeyValue::Domain { name: name.into() }
    }

    pub fn task(tid: i32) -> Self {
        KeyValue::Task { tid }
    }

    pub fn syscall(name: impl Into<String>) -> Self {
        KeyValue::Syscall { name: name.into() }
    }

    pub fn comm(comm: impl Into<String>) -> Self {
        KeyValue::Comm { comm: comm.into() }
    }

    pub fn proc(pidns: u64, pid: i32) -> Self {
        KeyValue::Proc { pidns, pid }
    }

    /// The `proc` key of a process this tool sees as `pid`: the pid namespace
    /// it lives in (the inode `/proc/<pid>/ns/pid` names) and its pid inside
    /// that namespace (the last `NSpid` of `/proc/<pid>/status`), so two
    /// containers' pid 1 are two keys and a process is never its main thread.
    /// `None` when either is unreadable: the process is gone, or the namespace
    /// link is closed to this user (it needs the access `ptrace` read does).
    pub fn proc_of(pid: i32) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let pidns = std::fs::metadata(format!("/proc/{pid}/ns/pid")).ok()?.ino();
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let inner = status
            .lines()
            .find_map(|line| line.strip_prefix("NSpid:"))?
            .split_whitespace()
            .last()?
            .parse()
            .ok()?;
        Some(KeyValue::Proc { pidns, pid: inner })
    }

    pub fn kind(&self) -> KeyKind {
        match self {
            KeyValue::Req { .. } => KeyKind::Req,
            KeyValue::Task { .. } => KeyKind::Task,
            KeyValue::Proc { .. } => KeyKind::Proc,
            KeyValue::File { .. } => KeyKind::File,
            KeyValue::Sock { .. } => KeyKind::Sock,
            KeyValue::Blk { .. } => KeyKind::Blk,
            KeyValue::Mem { .. } => KeyKind::Mem,
            KeyValue::Gpu { .. } => KeyKind::Gpu,
            KeyValue::Cgroup { .. } => KeyKind::Cgroup,
            KeyValue::Iface { .. } => KeyKind::Iface,
            KeyValue::Dev { .. } => KeyKind::Dev,
            KeyValue::Cpu { .. } => KeyKind::Cpu,
            KeyValue::Node { .. } => KeyKind::Node,
            KeyValue::Dir { .. } => KeyKind::Dir,
            KeyValue::Syscall { .. } => KeyKind::Syscall,
            KeyValue::Comm { .. } => KeyKind::Comm,
            KeyValue::Domain { .. } => KeyKind::Domain,
        }
    }
}

impl fmt::Display for KeyValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyValue::Req { id } => write!(f, "req:{id}"),
            KeyValue::Task { tid } => write!(f, "task:{tid}"),
            KeyValue::Proc { pidns, pid } => write!(f, "proc:{pidns}+{pid}"),
            KeyValue::File { dev, ino } => write!(f, "file:{dev}+{ino}"),
            KeyValue::Sock { cookie } => write!(f, "sock:{cookie}"),
            KeyValue::Blk { ptr, open_ts } => write!(f, "blk:{ptr:#x}@{open_ts}"),
            KeyValue::Mem { base, len } => write!(f, "mem:[{base:#x},{:#x})", base + len),
            KeyValue::Gpu { fence_ctx, seqno } => write!(f, "gpu:{fence_ctx}+{seqno}"),
            KeyValue::Cgroup { id } => write!(f, "cgroup:{id}"),
            // The namespace is printed only when it is not this process's:
            // another namespace's `lo` is another interface, and a reader
            // who saw two series called `iface:lo` would add them up.
            // "This process" is the one printing, not the one that recorded:
            // a recording rendered from another namespace prints `@netns` on
            // every interface, the recorder's own included. The identity
            // (`iface_identity`) carries the namespace either way, so only the
            // label changes, and it errs towards saying more.
            KeyValue::Iface { netns, name, .. } if *netns != 0 && *netns != own_netns() => {
                write!(f, "iface:{name}@netns:{netns}")
            }
            KeyValue::Iface { name, .. } => write!(f, "iface:{name}"),
            KeyValue::Dev { major, minor } => write!(f, "dev:{major}+{minor}"),
            KeyValue::Cpu { cpu } => write!(f, "cpu:{cpu}"),
            KeyValue::Node { node } => write!(f, "node:{node}"),
            KeyValue::Dir { dir } => write!(f, "dir:{dir}"),
            KeyValue::Syscall { name } => write!(f, "syscall:{name}"),
            KeyValue::Comm { comm } => write!(f, "comm:{comm}"),
            KeyValue::Domain { name } => write!(f, "domain:{name}"),
        }
    }
}

/// The interface part of an `iface` key's name: everything before a `/`,
/// which the kernel refuses in an interface name.
fn iface_base(name: &str) -> &str {
    name.split_once('/').map_or(name, |(base, _)| base)
}

/// This process's network namespace, by the inode `/proc/self/ns/net` names;
/// 0 where the kernel has no network namespaces to name. Read once: a thread
/// that `setns`es into another is not a thing this tool does.
pub fn own_netns() -> u64 {
    use std::os::unix::fs::MetadataExt;
    static OWN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *OWN.get_or_init(|| {
        std::fs::metadata("/proc/self/ns/net")
            .map(|meta| meta.ino())
            .unwrap_or(0)
    })
}

/// The kernel's index for an interface name in this network namespace, 0 when
/// it has none.
fn index_of(name: &str) -> u32 {
    let Ok(name) = std::ffi::CString::new(name) else {
        return 0;
    };
    // SAFETY: a NUL-terminated string we own, read and not kept.
    unsafe { libc::if_nametoindex(name.as_ptr()) }
}

/// A named measurement carried by an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attr {
    pub name: String,
    pub value: AttrValue,
}

impl Attr {
    pub fn uint(name: impl Into<String>, value: u64) -> Self {
        Self {
            name: name.into(),
            value: AttrValue::Uint(value),
        }
    }

    /// A signed integer: an errno, a return value, a difference. Stored in
    /// its canonical form ([`AttrValue::int`]), so a non-negative one is a
    /// `Uint` and reads back as the value it was written as.
    pub fn int(name: impl Into<String>, value: i64) -> Self {
        Self {
            name: name.into(),
            value: AttrValue::int(value),
        }
    }

    /// A value that is genuinely fractional, such as a ratio of two counters.
    /// Counts stay integers: rounding one into a float loses the exactness that
    /// makes a counter worth reading.
    pub fn float(name: impl Into<String>, value: f64) -> Self {
        Self {
            name: name.into(),
            value: AttrValue::Float(value),
        }
    }

    pub fn text(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: AttrValue::Text(value.into()),
        }
    }
}

/// A measured value.
///
/// Untagged on the wire, so a number is written as a number and every consumer
/// of the output reads it without knowing this type. What that costs is the
/// integer's signedness: `5` does not say whether it was written as a `Uint`
/// or an `Int`, and the untagged reader answers `Uint`. So an integer has one
/// canonical form -- `Uint` when it is non-negative, `Int` only when it is
/// negative -- which [`AttrValue::int`] and [`Attr::int`] produce and which is
/// what a round trip reads back. An `Int` built directly with a non-negative
/// value is still equal to the `Uint` of the same integer: two spellings of one
/// number are one value, and equality says so rather than depending on how a
/// collector happened to build it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttrValue {
    Uint(u64),
    Int(i64),
    Float(f64),
    Text(String),
}

impl PartialEq for AttrValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AttrValue::Uint(left), AttrValue::Uint(right)) => left == right,
            (AttrValue::Int(left), AttrValue::Int(right)) => left == right,
            (AttrValue::Uint(unsigned), AttrValue::Int(signed))
            | (AttrValue::Int(signed), AttrValue::Uint(unsigned)) => {
                u64::try_from(*signed).is_ok_and(|signed| signed == *unsigned)
            }
            (AttrValue::Float(left), AttrValue::Float(right)) => left == right,
            (AttrValue::Text(left), AttrValue::Text(right)) => left == right,
            _ => false,
        }
    }
}

impl AttrValue {
    /// A signed integer in its canonical form: `Uint` when it is
    /// non-negative, which is what the untagged reader makes of it.
    pub fn int(value: i64) -> Self {
        match u64::try_from(value) {
            Ok(unsigned) => AttrValue::Uint(unsigned),
            Err(_) => AttrValue::Int(value),
        }
    }

    /// The numeric value, for aggregation. Text attributes have none.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            AttrValue::Uint(value) => Some(*value as f64),
            AttrValue::Int(value) => Some(*value as f64),
            AttrValue::Float(value) => Some(*value),
            AttrValue::Text(_) => None,
        }
    }
}

impl fmt::Display for AttrValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttrValue::Uint(value) => write!(f, "{value}"),
            AttrValue::Int(value) => write!(f, "{value}"),
            AttrValue::Float(value) => write!(f, "{value}"),
            AttrValue::Text(value) => f.write_str(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process's `proc` key is its pid namespace and its pid inside it, as
    /// the kernel names both: the namespace's inode in the link's own text
    /// (`pid:[<inode>]`), and, in the namespace this test runs in, the pid
    /// the kernel gave it.
    #[test]
    fn a_process_is_keyed_by_its_pid_namespace_and_its_pid_inside_it() {
        let me = std::process::id() as i32;
        let link = std::fs::read_link("/proc/self/ns/pid").expect("a pid namespace");
        let named: u64 = link
            .to_string_lossy()
            .trim_start_matches("pid:[")
            .trim_end_matches(']')
            .parse()
            .expect("an inode");
        let Some(KeyValue::Proc { pidns, pid }) = KeyValue::proc_of(me) else {
            panic!("this process can read its own namespace");
        };
        assert_eq!(pidns, named);
        let status = std::fs::read_to_string("/proc/self/status").expect("readable");
        let depth = status
            .lines()
            .find_map(|line| line.strip_prefix("NSpid:"))
            .map_or(1, |pids| pids.split_whitespace().count());
        if depth == 1 {
            assert_eq!(pid, me, "in one pid namespace the pid inside it is the pid");
        }
        assert!(KeyValue::proc_of(i32::MAX).is_none(), "no such process");
    }

    /// A producer that reads only a name gets the kernel's index at the read:
    /// `lo` is 1 in every network namespace Linux makes, and the sub-series
    /// after a `/` is not part of the name looked up.
    #[test]
    fn an_interface_named_without_its_index_is_given_the_kernels() {
        let expected: u32 = std::fs::read_to_string("/sys/class/net/lo/ifindex")
            .expect("every Linux machine has lo")
            .trim()
            .parse()
            .expect("an index");
        assert_eq!(expected, 1);
        for name in ["lo", "lo/drops"] {
            let key = KeyValue::iface(0, name);
            assert!(
                matches!(key, KeyValue::Iface { index, .. } if index == expected),
                "{key:?}"
            );
            assert_eq!(key.iface_name(), Some("lo"));
            let here = own_netns();
            assert_ne!(here, 0, "this kernel names its network namespaces");
            assert_eq!(
                key.iface_identity(),
                Some(format!("iface#{here}:1")),
                "{key:?}"
            );
        }
        let gone = KeyValue::iface(0, "omn-no-such-if/drops");
        assert!(matches!(gone, KeyValue::Iface { index: 0, .. }), "{gone:?}");
        assert_eq!(
            gone.iface_identity(),
            Some(format!("iface:{}:omn-no-such-if", own_netns()))
        );
        assert_eq!(KeyValue::task(1).iface_identity(), None);
    }

    /// Another namespace's `lo` is another interface: the same index, a
    /// different identity, and a name that says which namespace it is in.
    /// This namespace's is printed as it always was.
    #[test]
    fn another_namespaces_interface_of_the_same_index_is_another_interface() {
        let here = own_netns();
        let ours = KeyValue::iface(1, "lo");
        let theirs = KeyValue::iface_in(here + 1, 1, "lo");
        assert_ne!(ours.iface_identity(), theirs.iface_identity());
        assert_eq!(
            ours.iface_identity(),
            KeyValue::iface_in(here, 1, "lo").iface_identity()
        );
        assert_eq!(ours.to_string(), "iface:lo");
        assert_eq!(theirs.to_string(), format!("iface:lo@netns:{}", here + 1));
        assert_eq!(theirs.iface_name(), Some("lo"));
        assert_eq!(theirs.iface_netns(), Some(here + 1));
        assert_eq!(KeyValue::iface_as_given(1, "lo").iface_netns(), None);
    }

    fn sample() -> Event {
        Event {
            ts_ns: 1_234_567_890,
            ts_err_ns: 0,
            kind: "mem.rss".to_owned(),
            keys: vec![KeyValue::task(4242), KeyValue::proc(4_026_531_836, 4242)],
            attrs: vec![
                Attr::uint("bytes", 12_582_912),
                Attr::text("source", "smaps_rollup"),
            ],
            collector_id: CollectorId::new("mem.rss/fs:proc"),
            seq: 7,
        }
    }

    /// One of every key and every kind of value, at the edges of their
    /// ranges, with a timestamp converted from another clock.
    fn every_variant() -> Event {
        Event {
            ts_ns: u64::MAX - 1,
            ts_err_ns: 1_250,
            kind: "gpu.kernel".to_owned(),
            keys: vec![
                KeyValue::Req {
                    id: "req-7f3a".to_owned(),
                },
                KeyValue::task(-1),
                KeyValue::proc(4_026_531_836, i32::MAX),
                KeyValue::File {
                    dev: 0x10302,
                    ino: u64::MAX,
                },
                KeyValue::Sock { cookie: 1 << 40 },
                KeyValue::Blk {
                    ptr: 0xffff_8881_0a2b_c000,
                    open_ts: 0,
                },
                KeyValue::Mem {
                    base: 0x7f00_0000_0000,
                    len: 4096,
                },
                KeyValue::Gpu {
                    fence_ctx: 3,
                    seqno: u64::MAX,
                },
                KeyValue::Cgroup { id: 0 },
                KeyValue::iface(u32::MAX, "enp3s0"),
                KeyValue::dev(259, 0),
                KeyValue::cpu(0),
                KeyValue::node(u32::MAX),
            ],
            attrs: vec![
                Attr::uint("zero", 0),
                Attr::uint("large", u64::MAX),
                Attr::int("negative", -1),
                Attr::int("most_negative", i64::MIN),
                Attr::int("int_zero", 0),
                Attr::int("int_positive", 42),
                Attr::int("int_largest", i64::MAX),
                Attr::float("ratio", 0.25),
                Attr::float("negative_ratio", -1.5e-9),
                Attr::float("large_ratio", 1.0e300),
                Attr::float("whole_float", 2.0),
                Attr::text("digits", "5"),
                Attr::text("empty", ""),
                Attr::text("yaml_word", "yes"),
            ],
            collector_id: CollectorId::new("gpu.kernel/bpf:uprobe"),
            seq: u64::MAX,
        }
    }

    /// The round trip is held structurally, variant by variant, and not only
    /// through `PartialEq`, which equates two spellings of one integer.
    fn assert_identical(back: &Event, event: &Event) {
        assert_eq!(back, event);
        assert_eq!(format!("{back:?}"), format!("{event:?}"));
    }

    #[test]
    fn an_event_survives_a_round_trip_through_json_and_yaml() {
        for event in [sample(), every_variant()] {
            let json = serde_json::to_string(&event).unwrap();
            assert_identical(&serde_json::from_str::<Event>(&json).unwrap(), &event);
            let yaml = serde_norway::to_string(&event).unwrap();
            assert_identical(&serde_norway::from_str::<Event>(&yaml).unwrap(), &event);
        }
    }

    /// The defect the fourteenth wave's audit found: an `Int` holding a
    /// non-negative value was read back as a `Uint`. The canonical form is
    /// what is written, so what is read back is what was written; and the
    /// number on the wire is the number, whatever its variant.
    #[test]
    fn a_non_negative_signed_integer_reads_back_as_what_was_written() {
        for value in [0i64, 1, 42, i64::MAX] {
            let attr = Attr::int("ret", value);
            let json = serde_json::to_string(&attr).unwrap();
            assert_eq!(json, format!("{{\"name\":\"ret\",\"value\":{value}}}"));
            let back: Attr = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{back:?}"), format!("{attr:?}"));
            assert_eq!(back.value.as_f64(), Some(value as f64));
        }
        for value in [-1i64, i64::MIN] {
            let attr = Attr::int("errno", value);
            assert!(matches!(attr.value, AttrValue::Int(v) if v == value));
            let back: Attr = serde_json::from_str(&serde_json::to_string(&attr).unwrap()).unwrap();
            assert!(matches!(back.value, AttrValue::Int(v) if v == value));
        }
        // An `Int` spelled directly is still the integer it holds.
        assert_eq!(AttrValue::Int(7), AttrValue::Uint(7));
        assert_eq!(AttrValue::Uint(7), AttrValue::Int(7));
        assert_ne!(AttrValue::Int(-1), AttrValue::Uint(u64::MAX));
        assert_ne!(AttrValue::Int(7), AttrValue::Float(7.0));
        let back: AttrValue =
            serde_json::from_str(&serde_json::to_string(&AttrValue::Int(7)).unwrap()).unwrap();
        assert_eq!(back, AttrValue::Int(7));
    }

    #[test]
    fn keys_render_the_way_the_catalog_writes_them() {
        assert_eq!(KeyValue::Task { tid: 42 }.to_string(), "task:42");
        assert_eq!(
            KeyValue::Blk {
                ptr: 0xffff,
                open_ts: 99
            }
            .to_string(),
            "blk:0xffff@99"
        );
        assert_eq!(
            KeyValue::Mem {
                base: 0x1000,
                len: 0x1000
            }
            .to_string(),
            "mem:[0x1000,0x2000)"
        );
    }

    #[test]
    fn an_event_finds_its_own_keys_and_attributes() {
        let event = sample();
        assert_eq!(
            event.key(KeyKind::Task),
            Some(&KeyValue::Task { tid: 4242 })
        );
        assert_eq!(
            event.attr("bytes").and_then(AttrValue::as_f64),
            Some(12_582_912.0)
        );
        assert_eq!(event.attr("absent"), None);
    }
}
