//! `net.iface` over `/proc/net/dev`.
//!
//! Errors, drops and collisions per interface. These are the counters that say
//! the link itself is the problem, which is the one explanation a latency
//! measurement further up the stack can never offer.
//!
//! The file carries the counters every driver keeps. Driver-specific ones --
//! the long list `ethtool -S` prints -- need the ethtool netlink family, and
//! the catalog says so rather than pretending this is the whole picture.

use std::collections::BTreeMap;

use super::procfs::{self, Deltas};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

/// Column offsets in `/proc/net/dev`, receive then transmit.
const RX_ERRORS: usize = 2;
const RX_DROPPED: usize = 3;
const RX_FIFO: usize = 4;
const TX_ERRORS: usize = 10;
const TX_DROPPED: usize = 11;
const TX_FIFO: usize = 12;
const COLLISIONS: usize = 13;
const CARRIER: usize = 14;

pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let only = match target {
            Target::System => None,
            Target::Iface(name) => Some(name.clone()),
            other => {
                return Err(format!(
                    "link counters belong to an interface, and a {} target names none; use \
                     --iface or --system",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(IfaceCollector {
            id: CollectorId::new("net.iface/fs:proc"),
            mode: self.0,
            only,
            deltas: Deltas::default(),
            losses: Losses::default(),
            hidden: BTreeMap::new(),
        }))
    }
}

pub struct IfaceCollector {
    id: CollectorId,
    /// A poll and a count measure an interval; a snapshot cannot, and reports
    /// the counters as they stand instead.
    mode: Mode,
    only: Option<String>,
    deltas: Deltas,
    losses: Losses,
    /// Per interface, the frames lost to something other than the measured
    /// value, accumulated over the run.
    ///
    /// The series is errors, deliberately -- a bad frame and a full queue are
    /// different failures and summing them would hide which happened -- and the
    /// cost is that every emit but `raw` answers zero for a link that is losing
    /// everything given to it. Until a series can be split by which counter it
    /// came from, the total is carried here and stated in `notes`, so that a
    /// rate of zero over a thousand dropped frames is a stated omission rather
    /// than a silent one.
    hidden: BTreeMap<String, Hidden>,
}

/// What an interface lost to counters the series does not carry.
#[derive(Default, Clone, Copy)]
struct Hidden {
    drops: u64,
    fifo: u64,
    collisions: u64,
    carrier: u64,
}

impl Hidden {
    fn total(self) -> u64 {
        self.drops + self.fifo + self.collisions + self.carrier
    }
}

impl Collector for IfaceCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    /// A poll and a count measure errors over a window; a snapshot can only
    /// report what the counter stands at. Different numbers, different names.
    fn value_attr(&self) -> &'static str {
        match self.mode {
            Mode::Snapshot => "errors_since_boot",
            _ => "errors",
        }
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let interfaces = procfs::interfaces(self.only.as_deref(), &mut self.losses)?;
        let ts_ns = monotonic_ns();
        let mut events = Vec::new();
        let mut index = seq;

        for interface in interfaces {
            if self.mode == Mode::Snapshot {
                events.push(self.standing_event(&interface, ts_ns, index));
                index += 1;
                continue;
            }

            let mut delta = |column: usize, name: &str| {
                self.deltas.since(
                    &format!("{}:{name}", interface.name),
                    interface.column(column),
                    &mut self.losses,
                )
            };

            // Every counter of an interface is read in the same tick, so if one
            // has no increment to report none of them do: this is the first
            // sight of the interface.
            let Some(rx_errors) = delta(RX_ERRORS, "rx_errors") else {
                let _ = delta(RX_DROPPED, "rx_dropped");
                let _ = delta(RX_FIFO, "rx_fifo");
                let _ = delta(TX_ERRORS, "tx_errors");
                let _ = delta(TX_DROPPED, "tx_dropped");
                let _ = delta(TX_FIFO, "tx_fifo");
                let _ = delta(COLLISIONS, "collisions");
                let _ = delta(CARRIER, "carrier");
                continue;
            };
            let rx_dropped = delta(RX_DROPPED, "rx_dropped").unwrap_or(0);
            let rx_fifo = delta(RX_FIFO, "rx_fifo").unwrap_or(0);
            let tx_errors = delta(TX_ERRORS, "tx_errors").unwrap_or(0);
            let tx_dropped = delta(TX_DROPPED, "tx_dropped").unwrap_or(0);
            let tx_fifo = delta(TX_FIFO, "tx_fifo").unwrap_or(0);
            let collisions = delta(COLLISIONS, "collisions").unwrap_or(0);
            let carrier = delta(CARRIER, "carrier").unwrap_or(0);

            // What this interval lost to counters the series does not carry,
            // kept so that `notes` can say what a rate of zero is leaving out.
            let hidden = self.hidden.entry(interface.name.clone()).or_default();
            hidden.drops += rx_dropped + tx_dropped;
            hidden.fifo += rx_fifo + tx_fifo;
            hidden.collisions += collisions;
            hidden.carrier += carrier;

            events.push(Event {
                ts_ns,
                ts_err_ns: 0,
                kind: "net.iface".to_owned(),
                keys: vec![KeyValue::iface(0, interface.name.clone())],
                attrs: vec![
                    // The measured value is errors. Drops are a different
                    // failure -- a full queue rather than a bad frame -- and
                    // summing them would hide which one happened.
                    Attr::uint("errors", rx_errors + tx_errors),
                    Attr::uint("rx_errors", rx_errors),
                    Attr::uint("tx_errors", tx_errors),
                    Attr::uint("drops", rx_dropped + tx_dropped),
                    Attr::uint("rx_dropped", rx_dropped),
                    Attr::uint("tx_dropped", tx_dropped),
                    Attr::uint("fifo", rx_fifo + tx_fifo),
                    Attr::uint("collisions", collisions),
                    Attr::uint("carrier", carrier),
                    Attr::text("iface", interface.name),
                ],
                collector_id: self.id.clone(),
                seq: index,
            });
            index += 1;
        }
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        // A counter source closes its own window, or the last interval of every
        // run is dropped. A snapshot has no window to close: its one read has
        // already happened, and a second one here would replace the counters it
        // reported with an increment over a few hundred microseconds.
        if self.mode == Mode::Snapshot {
            return Vec::new();
        }
        self.sample(u64::MAX / 2).unwrap_or_default()
    }

    /// What the series leaves out, named rather than left at zero.
    ///
    /// The measured value of this source is errors, so `rate`, `timeline` and
    /// `summary` all answer about errors and a link losing every frame given to
    /// it reads `0 count/s`. The frames are counted -- they are on every event,
    /// and `raw` shows them -- but no emit that aggregates can reach them. A
    /// run where that gap is non-empty says so here, which is the one place
    /// every emit and every format carries.
    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        for (iface, hidden) in &self.hidden {
            if hidden.total() == 0 {
                continue;
            }
            let mut kinds = Vec::new();
            for (name, count) in [
                ("drops", hidden.drops),
                ("fifo overruns", hidden.fifo),
                ("collisions", hidden.collisions),
                ("carrier losses", hidden.carrier),
            ] {
                if count > 0 {
                    kinds.push(format!("{count} {name}"));
                }
            }
            notes.push(format!(
                "`{iface}` also reported {} over this run; the measured value of `net.iface` is \
                 errors, so every emit but `raw` answers about errors alone and these are not in \
                 it",
                kinds.join(", ")
            ));
        }
        notes
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

impl IfaceCollector {
    /// The counters as they stand, which is what a single read can honestly
    /// report.
    ///
    /// A snapshot observes no window: there is no earlier reading to measure
    /// against, and the increment between a snapshot's read and the run's close
    /// is an increment over a few hundred microseconds, which on anything short
    /// of a saturated link is zero. The totals since boot are a different number
    /// and carry different names, so a reader cannot mistake one for the other.
    fn standing_event(&self, interface: &procfs::Interface, ts_ns: u64, seq: u64) -> Event {
        let rx_errors = interface.column(RX_ERRORS);
        let tx_errors = interface.column(TX_ERRORS);
        let rx_dropped = interface.column(RX_DROPPED);
        let tx_dropped = interface.column(TX_DROPPED);
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "net.iface".to_owned(),
            keys: vec![KeyValue::iface(0, interface.name.clone())],
            attrs: vec![
                Attr::uint("errors_since_boot", rx_errors + tx_errors),
                Attr::uint("rx_errors_since_boot", rx_errors),
                Attr::uint("tx_errors_since_boot", tx_errors),
                Attr::uint("drops_since_boot", rx_dropped + tx_dropped),
                Attr::uint("rx_dropped_since_boot", rx_dropped),
                Attr::uint("tx_dropped_since_boot", tx_dropped),
                Attr::uint(
                    "fifo_since_boot",
                    interface.column(RX_FIFO) + interface.column(TX_FIFO),
                ),
                Attr::uint("collisions_since_boot", interface.column(COLLISIONS)),
                Attr::uint("carrier_since_boot", interface.column(CARRIER)),
                Attr::text("iface", interface.name.clone()),
                Attr::text(
                    "basis",
                    "the counters as they stand: a snapshot observes no window of its own",
                ),
            ],
            collector_id: self.id.clone(),
            seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polling() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    #[test]
    fn a_healthy_loopback_reports_zero_of_everything() {
        let mut collector = Box::new(Prepare(polling()))
            .attach(&Target::Iface("lo".to_owned()))
            .expect("loopback exists");
        collector.sample(0).unwrap();
        let events = collector.sample(0).unwrap();
        assert_eq!(events.len(), 1);
        // Loopback cannot have link errors: there is no link.
        for name in ["errors", "drops", "collisions", "carrier"] {
            assert_eq!(
                events[0].attr(name).and_then(|value| value.as_f64()),
                Some(0.0),
                "loopback reported a non-zero `{name}`"
            );
        }
    }

    #[test]
    fn errors_and_drops_are_kept_apart() {
        let mut collector = Box::new(Prepare(polling()))
            .attach(&Target::System)
            .expect("system");
        collector.sample(0).unwrap();
        let events = collector.sample(0).unwrap();
        let event = events.first().expect("at least the loopback");
        assert!(event.attr("errors").is_some(), "a bad frame");
        assert!(event.attr("drops").is_some(), "a full queue");
    }

    /// A snapshot of a counter source reports the counter, not an increment
    /// over the microseconds between its read and the run's close.
    ///
    /// Loopback is the ground truth here: it has carried traffic since boot on
    /// any machine that has run anything, so its receive counters are not zero
    /// -- and a snapshot that answered with an increment would report zero for
    /// all of them, which is the defect this replaces.
    #[test]
    fn a_snapshot_reports_the_counters_as_they_stand() {
        let mut collector = Box::new(Prepare(Mode::Snapshot))
            .attach(&Target::Iface("lo".to_owned()))
            .expect("loopback exists");
        let events = collector.sample(0).expect("one read");
        assert_eq!(events.len(), 1, "one interface, one reading");
        // The first read is the reading: there is no baseline to wait for.
        assert!(
            events[0].attr("errors_since_boot").is_some(),
            "a snapshot names what it reports: {:?}",
            events[0].attrs
        );
        assert!(
            events[0].attr("errors").is_none(),
            "an interval's name on a number that is not one would be the lie this fixes"
        );
        assert!(
            collector.stop().is_empty(),
            "a snapshot has no window to close, and a closing read would replace its answer"
        );

        // And the reading is the file's, byte for byte.
        let mut losses = Losses::default();
        let file = procfs::interfaces(Some("lo"), &mut losses).expect("/proc/net/dev");
        let standing = file[0].column(RX_ERRORS) + file[0].column(TX_ERRORS);
        assert_eq!(
            events[0]
                .attr("errors_since_boot")
                .and_then(|value| value.as_f64()),
            Some(standing as f64)
        );
    }
}
