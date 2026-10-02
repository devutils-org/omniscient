//! Shared reading of the kernel's network counter files.
//!
//! Two files, both of the same shape: names in one place, ever-growing totals
//! in another. Several sources read the same lines for different columns, so
//! the parsing lives here and each source says which columns it means.
//!
//! Everything here reports *increments*. A total since boot is not a
//! measurement of a run, and the discipline that follows from that -- a first
//! read that is a baseline and produces nothing, a final read at stop, and a
//! counter that went backwards treated as a replaced interface rather than a
//! spike -- is the same for every source that reads a counter.

use std::collections::BTreeMap;

use crate::collect::{Losses, SampleError};

pub const DEV: &str = "/proc/net/dev";
pub const NETSTAT: &str = "/proc/net/netstat";

/// One interface's line from `/proc/net/dev`, as raw columns.
///
/// Receive is columns 0-7 and transmit is 8-15: bytes, packets, errs, drop,
/// fifo, then frame/colls, compressed/carrier, multicast/compressed.
pub struct Interface {
    pub name: String,
    /// The kernel's index for it, asked at the read (`if_nametoindex`), so
    /// that a key from these counters is the same interface as one from a
    /// source that reads the index itself. 0 for one removed between the read
    /// and the question, which is what an unknown index has always been.
    pub index: u32,
    pub columns: Vec<u64>,
}

impl Interface {
    pub fn column(&self, index: usize) -> u64 {
        self.columns.get(index).copied().unwrap_or(0)
    }
}

/// Reads `/proc/net/dev`, optionally narrowed to one interface.
pub fn interfaces(only: Option<&str>, losses: &mut Losses) -> Result<Vec<Interface>, SampleError> {
    let text = std::fs::read_to_string(DEV)
        .map_err(|error| SampleError::Transient(format!("{DEV}: {error}")))?;

    let mut interfaces = Vec::new();
    // The first two lines are the two-row header.
    for line in text.lines().skip(2) {
        let Some((name, counters)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_owned();
        if only.is_some_and(|wanted| wanted != name) {
            continue;
        }
        let columns: Vec<u64> = counters
            .split_whitespace()
            .map(|field| field.parse().unwrap_or(0))
            .collect();
        if columns.len() < 16 {
            losses.undecoded += 1;
            losses.note(format!(
                "{DEV}: the line for `{name}` had {} columns, not 16",
                columns.len()
            ));
            continue;
        }
        let index = super::interfaces::index_of(&name).unwrap_or(0);
        interfaces.push(Interface {
            name,
            index,
            columns,
        });
    }

    if interfaces.is_empty() {
        if let Some(wanted) = only {
            return Err(SampleError::TargetGone(format!(
                "interface `{wanted}` is not in {DEV}"
            )));
        }
    }
    Ok(interfaces)
}

/// Reads the named counters out of `/proc/net/netstat` and `/proc/net/snmp`.
///
/// Both files pair a line of names with a line of values under a section
/// prefix, so a counter is addressed as `TcpExt:ListenDrops`.
pub fn named_counters(path: &str) -> Result<BTreeMap<String, u64>, SampleError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| SampleError::Transient(format!("{path}: {error}")))?;

    let mut counters = BTreeMap::new();
    let mut lines = text.lines();
    while let (Some(names), Some(values)) = (lines.next(), lines.next()) {
        let Some((section, names)) = names.split_once(':') else {
            continue;
        };
        let Some((value_section, values)) = values.split_once(':') else {
            continue;
        };
        if section != value_section {
            // The two lines of a pair must belong to the same section, or the
            // file is not the shape this parser assumes.
            continue;
        }
        for (name, value) in names.split_whitespace().zip(values.split_whitespace()) {
            if let Ok(value) = value.parse::<u64>() {
                counters.insert(format!("{section}:{name}"), value);
            }
        }
    }
    Ok(counters)
}

/// Tracks the previous reading of a set of counters and yields the increments.
///
/// A counter that goes backwards means what it was counting was replaced -- an
/// interface removed and re-added, a namespace swapped. There is no increment
/// to report, and reporting one would put a spike in the series.
#[derive(Default)]
pub struct Deltas {
    previous: BTreeMap<String, u64>,
}

impl Deltas {
    /// Returns the increment since the last reading, or nothing when this is
    /// the first sight of the counter or when it went backwards.
    pub fn since(&mut self, key: &str, current: u64, losses: &mut Losses) -> Option<u64> {
        match self.previous.insert(key.to_owned(), current) {
            // First sight: a baseline, not a measurement.
            None => None,
            Some(previous) if current < previous => {
                losses.failed_reads += 1;
                losses.note(format!(
                    "`{key}` went backwards, so what it counts was replaced; that interval is \
                     not measured"
                ));
                None
            }
            Some(previous) => Some(current - previous),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_reading_of_a_counter_is_a_baseline() {
        let mut deltas = Deltas::default();
        let mut losses = Losses::default();
        assert_eq!(deltas.since("lo:rx", 100, &mut losses), None);
        assert_eq!(deltas.since("lo:rx", 140, &mut losses), Some(40));
        assert!(losses.is_clean());
    }

    #[test]
    fn a_counter_that_goes_backwards_is_a_loss_and_not_a_spike() {
        let mut deltas = Deltas::default();
        let mut losses = Losses::default();
        deltas.since("lo:rx", 100, &mut losses);
        assert_eq!(deltas.since("lo:rx", 10, &mut losses), None);
        assert_eq!(losses.failed_reads, 1);
        // The new value becomes the baseline, so the next interval measures.
        assert_eq!(deltas.since("lo:rx", 30, &mut losses), Some(20));
    }

    #[test]
    fn loopback_is_present_with_all_sixteen_columns() {
        let mut losses = Losses::default();
        let interfaces = interfaces(None, &mut losses).expect("/proc/net/dev is readable");
        let loopback = interfaces
            .iter()
            .find(|interface| interface.name == "lo")
            .expect("every machine has a loopback");
        assert_eq!(loopback.columns.len(), 16);
    }

    #[test]
    fn netstat_counters_are_addressed_by_section_and_name() {
        let counters = named_counters(NETSTAT).expect("/proc/net/netstat is readable");
        assert!(
            counters.keys().any(|key| key.starts_with("TcpExt:")),
            "netstat should carry a TcpExt section"
        );
        assert!(
            counters.contains_key("TcpExt:ListenDrops"),
            "the accept queue counters should be present"
        );
    }
}
