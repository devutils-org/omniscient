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
    /// A network interface, by kernel index and by name. Both are carried: the
    /// index is what the kernel uses, the name is what a person reads, and an
    /// interface that gets renamed is still the same interface.
    Iface {
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
}

impl KeyValue {
    pub fn iface(index: u32, name: impl Into<String>) -> Self {
        KeyValue::Iface {
            index,
            name: name.into(),
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

    pub fn task(tid: i32) -> Self {
        KeyValue::Task { tid }
    }

    pub fn proc(pidns: u64, pid: i32) -> Self {
        KeyValue::Proc { pidns, pid }
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
            KeyValue::Iface { name, .. } => write!(f, "iface:{name}"),
            KeyValue::Dev { major, minor } => write!(f, "dev:{major}+{minor}"),
            KeyValue::Cpu { cpu } => write!(f, "cpu:{cpu}"),
            KeyValue::Node { node } => write!(f, "node:{node}"),
        }
    }
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
