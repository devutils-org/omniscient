//! `net.bytes` and `net.packets` over `/proc/net/dev`.
//!
//! The file holds counters that only ever grow, one line per interface. A
//! counter is not a measurement: what a run wants to know is how much moved
//! during it, so this collector reports the increment between two reads and not
//! the running total.
//!
//! That has a consequence worth stating rather than hiding: the first read of a
//! run produces no event. It establishes the baseline, and a baseline is not a
//! measurement of anything. A run shorter than one poll interval therefore
//! measures nothing, and says so, instead of reporting the traffic since boot
//! as though it had just happened.

use std::collections::BTreeMap;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Target;

const PATH: &str = "/proc/net/dev";

/// Which of the two sources this collector is serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counted {
    Bytes,
    Packets,
}

impl Counted {
    fn source(self) -> &'static str {
        match self {
            Counted::Bytes => "net.bytes",
            Counted::Packets => "net.packets",
        }
    }

    fn value_attr(self) -> &'static str {
        match self {
            Counted::Bytes => "bytes",
            Counted::Packets => "packets",
        }
    }

    /// Column offsets within a `/proc/net/dev` line, receive then transmit.
    ///
    /// The line is eight receive fields then eight transmit fields; bytes and
    /// packets are the first two of each.
    fn columns(self) -> (usize, usize) {
        match self {
            Counted::Bytes => (0, 8),
            Counted::Packets => (1, 9),
        }
    }
}

pub struct Prepare(pub Counted);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        // An interface target narrows the file to one line; a system target
        // reports every interface as its own series. Anything else names a
        // process, and a process does not have interface counters.
        let only = match target {
            Target::System => None,
            Target::Iface(name) => Some(name.clone()),
            other => {
                return Err(format!(
                    "interface counters are read per interface or machine-wide, and a {} target \
                     names neither; use --iface or --system",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(DevCollector {
            id: CollectorId::new(format!("{}/fs:proc", self.0.source())),
            counted: self.0,
            only,
            previous: BTreeMap::new(),
            losses: Losses::default(),
        }))
    }
}

/// Reads interface counters and reports what moved between reads.
pub struct DevCollector {
    id: CollectorId,
    counted: Counted,
    only: Option<String>,
    /// Last value seen per interface, for the delta.
    previous: BTreeMap<String, (u64, u64)>,
    losses: Losses,
}

/// One interface's line, reduced to the two columns being counted.
struct Line {
    name: String,
    received: u64,
    transmitted: u64,
}

impl DevCollector {
    fn read(&mut self) -> Result<Vec<Line>, SampleError> {
        let text = std::fs::read_to_string(PATH)
            .map_err(|error| SampleError::Transient(format!("{PATH}: {error}")))?;
        let (receive_column, transmit_column) = self.counted.columns();

        let mut lines = Vec::new();
        // The first two lines are the two-row header.
        for line in text.lines().skip(2) {
            let Some((name, counters)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim().to_owned();
            if self.only.as_ref().is_some_and(|wanted| *wanted != name) {
                continue;
            }
            let fields: Vec<u64> = counters
                .split_whitespace()
                .map(|field| field.parse().unwrap_or(0))
                .collect();
            let (Some(received), Some(transmitted)) =
                (fields.get(receive_column), fields.get(transmit_column))
            else {
                self.losses.undecoded += 1;
                self.losses.note(format!(
                    "{PATH}: a line for `{name}` had fewer columns than expected"
                ));
                continue;
            };
            lines.push(Line {
                name,
                received: *received,
                transmitted: *transmitted,
            });
        }

        if lines.is_empty() {
            if let Some(wanted) = &self.only {
                return Err(SampleError::TargetGone(format!(
                    "interface `{wanted}` is not in {PATH}"
                )));
            }
        }
        Ok(lines)
    }
}

impl Collector for DevCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        self.counted.value_attr()
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let lines = self.read()?;
        let ts_ns = monotonic_ns();
        let mut events = Vec::new();
        let mut index = seq;

        for line in lines {
            let current = (line.received, line.transmitted);
            let Some(previous) = self.previous.insert(line.name.clone(), current) else {
                // First sight of this interface: a baseline, not a measurement.
                continue;
            };

            // A counter that went backwards means the interface was removed and
            // re-added, or the kernel reset it. There is no delta to report and
            // pretending otherwise would put a spike in the series.
            if current.0 < previous.0 || current.1 < previous.1 {
                self.losses.failed_reads += 1;
                self.losses.note(format!(
                    "the counters for `{}` went backwards, so the interface was replaced; \
                     that interval is not measured",
                    line.name
                ));
                continue;
            }

            let received = current.0 - previous.0;
            let transmitted = current.1 - previous.1;
            events.push(Event {
                ts_ns,
                ts_err_ns: 0,
                kind: self.counted.source().to_owned(),
                keys: vec![KeyValue::iface(0, line.name.clone())],
                attrs: vec![
                    Attr::uint(self.counted.value_attr(), received + transmitted),
                    Attr::uint("rx", received),
                    Attr::uint("tx", transmitted),
                    Attr::text("iface", line.name),
                ],
                collector_id: self.id.clone(),
                seq: index,
            });
            index += 1;
        }
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        // Without a final read the last interval of every run is silently
        // dropped, so a run systematically under-reports by up to one tick.
        // A counter source has to close its own window.
        self.sample(u64::MAX / 2).unwrap_or_default()
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collector(counted: Counted) -> Box<dyn Collector> {
        Box::new(Prepare(counted))
            .attach(&Target::System)
            .expect("a system target is readable")
    }

    #[test]
    fn the_last_interval_of_a_run_is_measured_rather_than_dropped() {
        let mut collector = collector(Counted::Bytes);
        collector.sample(0).unwrap();
        // Whatever moved between the last tick and the stop still belongs to
        // the run.
        assert!(
            !collector.stop().is_empty(),
            "stopping must close the window with a final read"
        );
    }

    #[test]
    fn the_first_read_is_a_baseline_and_not_a_measurement() {
        let mut collector = collector(Counted::Bytes);
        assert!(
            collector.sample(0).unwrap().is_empty(),
            "the first read has nothing to compare against"
        );
        // The second read has a delta for every interface, even if it is zero.
        let second = collector.sample(0).unwrap();
        assert!(
            !second.is_empty(),
            "the machine has at least a loopback interface"
        );
    }

    #[test]
    fn every_interface_is_its_own_series() {
        let mut collector = collector(Counted::Packets);
        collector.sample(0).unwrap();
        let events = collector.sample(0).unwrap();
        let names: Vec<String> = events
            .iter()
            .filter_map(|event| event.attr("iface").map(|value| value.to_string()))
            .collect();
        assert!(
            names.contains(&"lo".to_owned()),
            "loopback should be listed: {names:?}"
        );
        assert_eq!(
            names.len(),
            events.len(),
            "one event per interface, so one series per interface"
        );
    }

    #[test]
    fn an_interface_target_narrows_to_that_interface() {
        let mut collector = Box::new(Prepare(Counted::Bytes))
            .attach(&Target::Iface("lo".to_owned()))
            .expect("loopback exists");
        collector.sample(0).unwrap();
        let events = collector.sample(0).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attr("iface").unwrap().to_string(), "lo");
    }

    #[test]
    fn a_process_target_is_refused_with_the_reason() {
        let Err(error) = Box::new(Prepare(Counted::Bytes)).attach(&Target::Pid(vec![1])) else {
            panic!("a process target has no interface counters and must be refused");
        };
        assert!(error.contains("--iface"), "{error}");
    }

    #[test]
    fn an_interface_that_does_not_exist_ends_the_run() {
        let mut collector = Box::new(Prepare(Counted::Bytes))
            .attach(&Target::Iface("definitely-not-an-interface".to_owned()))
            .expect("attaching does not read the file");
        match collector.sample(0) {
            Err(SampleError::TargetGone(reason)) => assert!(reason.contains("not in"), "{reason}"),
            other => panic!("expected the run to end, got {other:?}"),
        }
    }
}
