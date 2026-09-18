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
}

impl KeyValue {
    pub fn iface(index: u32, name: impl Into<String>) -> Self {
        KeyValue::Iface {
            index,
            name: name.into(),
        }
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

    pub fn int(name: impl Into<String>, value: i64) -> Self {
        Self {
            name: name.into(),
            value: AttrValue::Int(value),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttrValue {
    Uint(u64),
    Int(i64),
    Float(f64),
    Text(String),
}

impl AttrValue {
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

    #[test]
    fn an_event_survives_a_round_trip_through_json_and_yaml() {
        let event = sample();
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        let yaml = serde_norway::to_string(&event).unwrap();
        assert_eq!(serde_norway::from_str::<Event>(&yaml).unwrap(), event);
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
