//! `net.sockstat` over `/proc/net/sockstat` and `sockstat6`: the machine's
//! sockets by transport, what `ss -s` prints.
//!
//! One event per transport per read, keyed `counter` (`TCP`, `UDP`, `RAW`,
//! `FRAG`, and `sockets`, every socket of every family), its value the
//! sockets in use over IPv4 and IPv6 added up -- `ss -s`'s Total column --
//! with `ipv4` and `ipv6` beside it. The TCP row also carries the kernel's
//! `orphan`, `tw` (time-wait), `alloc` and `mem` (pages of buffers); `FRAG`
//! carries the reassembly queue's bytes as `memory`.
//!
//! These are levels, read as they stand: no baseline, no increment.

use std::collections::BTreeMap;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::request::{Mode, Target};

const SOCKSTAT: &str = "/proc/net/sockstat";
const SOCKSTAT6: &str = "/proc/net/sockstat6";

/// One line, `NAME: key value key value ...`, as its pairs.
fn lines(text: &str) -> BTreeMap<String, Vec<(String, u64)>> {
    text.lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let words: Vec<&str> = rest.split_whitespace().collect();
            let pairs = words
                .chunks(2)
                .filter_map(|pair| Some((pair.first()?.to_string(), pair.get(1)?.parse().ok()?)))
                .collect();
            Some((name.trim().to_owned(), pairs))
        })
        .collect()
}

fn field(pairs: Option<&Vec<(String, u64)>>, name: &str) -> Option<u64> {
    pairs?
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| *value)
}

/// The rows of one read: `(counter, attributes)`, the value first.
fn rows(v4: &str, v6: &str) -> Vec<(String, Vec<Attr>)> {
    let v4 = lines(v4);
    let v6 = lines(v6);
    let mut rows = Vec::new();
    if let Some(used) = field(v4.get("sockets"), "used") {
        rows.push(("sockets".to_owned(), vec![Attr::uint("inuse", used)]));
    }
    for transport in ["TCP", "UDP", "UDPLITE", "RAW", "FRAG"] {
        let four = v4.get(transport);
        let six = v6.get(&format!("{transport}6"));
        if four.is_none() && six.is_none() {
            continue;
        }
        let ipv4 = field(four, "inuse").unwrap_or(0);
        let ipv6 = field(six, "inuse").unwrap_or(0);
        let mut attrs = vec![
            Attr::uint("inuse", ipv4 + ipv6),
            Attr::uint("ipv4", ipv4),
            Attr::uint("ipv6", ipv6),
        ];
        for extra in ["orphan", "tw", "alloc", "mem"] {
            if let Some(value) = field(four, extra) {
                attrs.push(Attr::uint(extra, value));
            }
        }
        if let Some(memory) = field(four, "memory") {
            attrs.push(Attr::uint(
                "memory",
                memory + field(six, "memory").unwrap_or(0),
            ));
        }
        rows.push((transport.to_owned(), attrs));
    }
    rows
}

pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(target, Target::System) {
            return Err(format!(
                "{SOCKSTAT} counts the machine's sockets, and a {} target is not the machine: a \
                 process's sockets are `net.sockets` with that target",
                target.kind()
            ));
        }
        match self.0 {
            Mode::Poll { .. } | Mode::Snapshot => {}
            other => {
                return Err(format!(
                    "{SOCKSTAT} is a set of levels, and `{}` cannot be served from it: poll it, \
                     or take one snapshot",
                    other.kind()
                ))
            }
        }
        Ok(Box::new(SockStat {}))
    }
}

pub struct SockStat {}

impl Collector for SockStat {
    fn value_attr(&self) -> &'static str {
        "inuse"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let v4 = std::fs::read_to_string(SOCKSTAT)
            .map_err(|error| SampleError::Transient(format!("{SOCKSTAT}: {error}")))?;
        // No IPv6 in this kernel is no IPv6 sockets, not a failed read.
        let v6 = std::fs::read_to_string(SOCKSTAT6).unwrap_or_default();
        let ts_ns = monotonic_ns();
        Ok(rows(&v4, &v6)
            .into_iter()
            .enumerate()
            .map(|(index, (counter, mut attrs))| {
                attrs.push(Attr::text("counter", counter));
                Event {
                    ts_ns,
                    ts_err_ns: 0,
                    kind: "net.sockstat".to_owned(),
                    keys: Vec::new(),
                    attrs,
                    collector_id: CollectorId::unstamped(),
                    seq: seq + index as u64,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V4: &str = "sockets: used 284
TCP: inuse 5 orphan 1 tw 26 alloc 12 mem 663
UDP: inuse 2 mem 1279
RAW: inuse 0
FRAG: inuse 0 memory 0
";
    const V6: &str = "TCP6: inuse 6
UDP6: inuse 2
RAW6: inuse 1
FRAG6: inuse 0 memory 0
";

    fn get(rows: &[(String, Vec<Attr>)], counter: &str, name: &str) -> Option<f64> {
        rows.iter()
            .find(|(row, _)| row == counter)?
            .1
            .iter()
            .find(|attr| attr.name == name)?
            .value
            .as_f64()
    }

    /// The same files `ss -s` read on this machine at the same instant:
    /// `RAW 1 0 1`, `UDP 4 2 2`, `TCP 11 5 6`, `Total: 284`.
    #[test]
    fn the_rows_are_ss_s_transport_table() {
        let rows = rows(V4, V6);
        assert_eq!(get(&rows, "TCP", "inuse"), Some(11.0));
        assert_eq!(get(&rows, "TCP", "ipv4"), Some(5.0));
        assert_eq!(get(&rows, "TCP", "ipv6"), Some(6.0));
        assert_eq!(get(&rows, "UDP", "inuse"), Some(4.0));
        assert_eq!(get(&rows, "RAW", "inuse"), Some(1.0));
        assert_eq!(get(&rows, "TCP", "tw"), Some(26.0));
        assert_eq!(get(&rows, "sockets", "inuse"), Some(284.0));
        assert_eq!(get(&rows, "UDP", "tw"), None);
    }

    #[test]
    fn this_machine_s_files_read() {
        let mut collector = Box::new(Prepare(Mode::Snapshot))
            .attach(&Target::System)
            .expect("the machine");
        let events = collector.sample(0).expect("readable");
        assert!(events
            .iter()
            .any(|event| event.attr("counter").map(|v| v.to_string()) == Some("TCP".into())));
    }

    #[test]
    fn every_declared_filter_field_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the catalog");
        let entry = catalog
            .get(&"net.sockstat".parse().expect("an id"))
            .expect("net.sockstat");
        let mut attrs = rows(V4, V6).remove(1).1;
        attrs.push(Attr::text("counter", "TCP"));
        for field in &entry.filter_fields {
            assert!(attrs.iter().any(|attr| attr.name == *field), "{field}");
        }
    }
}
