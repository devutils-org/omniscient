//! `net.bytes` and `net.packets` over `/proc/net/dev`.
//!
//! The file holds counters that only ever grow, one line per interface. A
//! counter is not a measurement: what a run wants to know is how much moved
//! during it, so this collector reports the increment between two reads and not
//! the running total.
//!
//! That has a consequence worth stating rather than hiding: the first read of a
//! run produces no event. It establishes the baseline, and a baseline is not a
//! measurement of anything. It is taken at `start`, before the observed program
//! is released, so the program's first bytes are in the first interval rather
//! than before a baseline; the traffic since boot is never reported as though
//! it had just happened.

use std::collections::BTreeMap;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Direction, Event, KeyValue};
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

/// What to count, and whether the request is keyed on `dir`: then each
/// interface's increase is one event per direction rather than one with both.
pub struct Prepare(pub Counted, pub bool);

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
            by_direction: self.1,
            only,
            previous: BTreeMap::new(),
            pass_first_tick: false,
            losses: Losses::default(),
        }))
    }
}

/// Reads interface counters and reports what moved between reads.
pub struct DevCollector {
    id: CollectorId,
    counted: Counted,
    /// One event per direction, keyed `dir:rx` and `dir:tx`.
    by_direction: bool,
    only: Option<String>,
    /// Last value seen per interface, for the delta.
    previous: BTreeMap<String, (u64, u64)>,
    /// The first tick after a baseline taken at `start`, which the runner
    /// takes as soon as the program is released: its interval would be
    /// microseconds long, so it is passed over and the next tick measures
    /// from the baseline.
    pass_first_tick: bool,
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

    /// The baseline, taken before the observed program is released, so what
    /// it sends from its first instruction is in the first interval rather
    /// than before a baseline taken at the first tick (`io.bytes` does the
    /// same, `docs/open-findings.md`, "`io.bytes`' polled baseline").
    ///
    /// The first tick then produces no event and moves nothing: its interval
    /// since this read is the few microseconds since the release, and the
    /// bytes in it are in the next interval rather than lost.
    fn start(&mut self) -> Result<(), String> {
        let lines = match self.read() {
            Ok(lines) => lines,
            // An interface that is not there yet is the first tick's to say.
            Err(SampleError::TargetGone(_)) => return Ok(()),
            Err(error) => return Err(format!("cannot read {PATH}: {error:?}")),
        };
        for line in lines {
            self.previous
                .insert(line.name, (line.received, line.transmitted));
        }
        self.pass_first_tick = !self.previous.is_empty();
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if std::mem::take(&mut self.pass_first_tick) {
            return Ok(Vec::new());
        }
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
            let iface = KeyValue::iface(
                super::interfaces::index_of(&line.name).unwrap_or(0),
                line.name.clone(),
            );
            if self.by_direction {
                for (direction, value) in [(Direction::Rx, received), (Direction::Tx, transmitted)]
                {
                    events.push(Event {
                        ts_ns,
                        ts_err_ns: 0,
                        kind: self.counted.source().to_owned(),
                        keys: vec![iface.clone(), KeyValue::dir(direction)],
                        attrs: vec![
                            Attr::uint(self.counted.value_attr(), value),
                            Attr::text("iface", line.name.clone()),
                        ],
                        collector_id: self.id.clone(),
                        seq: index,
                    });
                    index += 1;
                }
                continue;
            }
            events.push(Event {
                ts_ns,
                ts_err_ns: 0,
                kind: self.counted.source().to_owned(),
                // The index asked at the read, as `procfs::interfaces` does, so
                // this key names the interface the index-reading sources name.
                keys: vec![iface],
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

    /// Reads nothing: the runner closes the window (`DECISIONS.md`, "the
    /// runner closes the window"). Its last round of `sample` calls lands at
    /// the moment the stop condition is met, so the last interval is already
    /// measured; a read here covered the few microseconds since, and put a
    /// zero-delta point at the previous one's offset into every series. `io`
    /// and `power` stopped reading here for that reason, and this was the
    /// one counter source left that did.
    fn stop(&mut self) -> Vec<Event> {
        Vec::new()
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collector(counted: Counted) -> Box<dyn Collector> {
        Box::new(Prepare(counted, false))
            .attach(&Target::System)
            .expect("a system target is readable")
    }

    /// The runner's last round of reads is the closing read. A read at `stop`
    /// covered the microseconds since it and put a zero-delta point beside
    /// the previous one, which is what this test used to require.
    #[test]
    fn stopping_reads_nothing_because_the_runner_closed_the_window() {
        let mut collector = collector(Counted::Bytes);
        collector.sample(0).unwrap();
        assert!(collector.stop().is_empty());
    }

    /// Keyed on `dir`, an interface's increase is two events, `dir:rx` and
    /// `dir:tx`, under the interface; together they are its one event's bytes.
    #[test]
    fn a_request_keyed_on_direction_gets_one_series_per_direction() {
        let mut collector = Box::new(Prepare(Counted::Bytes, true))
            .attach(&Target::Iface("lo".to_owned()))
            .expect("loopback is readable");
        assert!(collector.sample(0).unwrap().is_empty());
        let events = collector.sample(1).unwrap();
        let directions: Vec<String> = events
            .iter()
            .map(|event| {
                assert_eq!(event.attr("iface").unwrap().to_string(), "lo");
                event.keys[1].to_string()
            })
            .collect();
        assert_eq!(directions, ["dir:rx", "dir:tx"]);
    }

    /// Pushes `bytes` through loopback, end to end, and returns once the
    /// peer has read them all.
    fn push_over_loopback(bytes: usize) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let reader = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let mut sink = Vec::new();
            peer.read_to_end(&mut sink).unwrap();
            sink.len()
        });
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.write_all(&vec![7u8; bytes]).unwrap();
        drop(stream);
        assert_eq!(reader.join().unwrap(), bytes);
    }

    /// The baseline is taken at `start`, before the program is released, so
    /// what it sends before the runner's first tick is in the first
    /// interval. Ground truth: a known volume pushed between the two; the
    /// first tick passes over and the second has at least that volume on
    /// `lo`, in each direction.
    #[test]
    fn what_moves_before_the_first_tick_is_measured_from_the_start() {
        const VOLUME: usize = 4 << 20;
        let mut collector = Box::new(Prepare(Counted::Bytes, false))
            .attach(&Target::Iface("lo".to_owned()))
            .expect("loopback is readable");
        collector.start().expect("the baseline is readable");
        push_over_loopback(VOLUME);
        assert!(
            collector.sample(0).unwrap().is_empty(),
            "the first tick, microseconds after the release, is no interval"
        );
        let events = collector.sample(1).unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        for direction in ["rx", "tx"] {
            let moved: u64 = events[0].attr(direction).unwrap().to_string().parse().unwrap();
            assert!(
                moved >= VOLUME as u64,
                "{direction}: {moved} bytes, {VOLUME} pushed before the first tick"
            );
        }
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
        let mut collector = Box::new(Prepare(Counted::Bytes, false))
            .attach(&Target::Iface("lo".to_owned()))
            .expect("loopback exists");
        collector.sample(0).unwrap();
        let events = collector.sample(0).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attr("iface").unwrap().to_string(), "lo");
    }

    /// Every series carries the kernel's index for its interface, as the
    /// sources that read the index themselves do. Ground truth from another
    /// file than the counters': sysfs's `ifindex`.
    #[test]
    fn every_series_carries_the_index_sysfs_gives_its_interface() {
        let mut collector = collector(Counted::Bytes);
        collector.sample(0).unwrap();
        let events = collector.sample(1).unwrap();
        assert!(!events.is_empty(), "the machine has at least loopback");
        for event in &events {
            let KeyValue::Iface { netns, index, name } = &event.keys[0] else {
                panic!("not an interface key: {:?}", event.keys[0]);
            };
            // `/proc/net/dev` is this process's namespace's, and the key says so.
            assert_eq!(*netns, crate::event::own_netns(), "`{name}`");
            let Ok(text) = std::fs::read_to_string(format!("/sys/class/net/{name}/ifindex")) else {
                continue; // removed since the read
            };
            assert_eq!(index.to_string(), text.trim(), "`{name}`");
        }
    }

    #[test]
    fn a_process_target_is_refused_with_the_reason() {
        let Err(error) = Box::new(Prepare(Counted::Bytes, false)).attach(&Target::Pid(vec![1]))
        else {
            panic!("a process target has no interface counters and must be refused");
        };
        assert!(error.contains("--iface"), "{error}");
    }

    #[test]
    fn an_interface_that_does_not_exist_ends_the_run() {
        let mut collector = Box::new(Prepare(Counted::Bytes, false))
            .attach(&Target::Iface("definitely-not-an-interface".to_owned()))
            .expect("attaching does not read the file");
        match collector.sample(0) {
            Err(SampleError::TargetGone(reason)) => assert!(reason.contains("not in"), "{reason}"),
            other => panic!("expected the run to end, got {other:?}"),
        }
    }
}
