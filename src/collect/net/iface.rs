//! `net.iface` over `/proc/net/dev`.
//!
//! Errors, drops and collisions per interface. These are the counters that say
//! the link itself is the problem, which is the one explanation a latency
//! measurement further up the stack can never offer.
//!
//! The file carries the counters every driver keeps. Driver-specific ones --
//! the long list `ethtool -S` prints -- need the ethtool netlink family, and
//! the catalog says so rather than pretending this is the whole picture.

use super::procfs::{self, Deltas};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

/// Column offsets in `/proc/net/dev`, receive then transmit.
const RX_ERRORS: usize = 2;
const RX_DROPPED: usize = 3;
const RX_FIFO: usize = 4;
const RX_MULTICAST: usize = 7;
const TX_ERRORS: usize = 10;
const TX_DROPPED: usize = 11;
const TX_FIFO: usize = 12;
const COLLISIONS: usize = 13;
const CARRIER: usize = 14;
/// Not a column of the file: `rx_missed_errors`, read from sysfs and appended
/// to the interface's columns (`with_missed`). `/proc/net/dev`'s receive drop
/// column already adds it in, so `missed` is the part of `drops` the card
/// counted for want of a buffer, apart, as `ip -s link` prints it.
const MISSED: usize = 16;

/// The kinds of counter a series is kept for, each a different failure, with
/// the receive and transmit columns that make it up. `collisions` and
/// `carrier` are transmit-side only: the file has no receive column for them;
/// `mcast` (multicast frames received, which is not a failure but is what
/// `ip -s link` prints beside them) and `missed` are receive-side only.
const KINDS: [(&str, Option<usize>, Option<usize>); 7] = [
    ("errors", Some(RX_ERRORS), Some(TX_ERRORS)),
    ("drops", Some(RX_DROPPED), Some(TX_DROPPED)),
    ("fifo", Some(RX_FIFO), Some(TX_FIFO)),
    ("collisions", None, Some(COLLISIONS)),
    ("carrier", None, Some(CARRIER)),
    ("mcast", Some(RX_MULTICAST), None),
    ("missed", Some(MISSED), None),
];

/// The kinds an interface has a reading for: `missed` only when sysfs gave
/// one, since a counter that could not be read is not a zero.
fn kinds_of(
    interface: &procfs::Interface,
) -> impl Iterator<Item = (&'static str, Option<usize>, Option<usize>)> + '_ {
    KINDS
        .into_iter()
        .filter(|(kind, _, _)| *kind != "missed" || interface.columns.len() > MISSED)
}

/// Appends `rx_missed_errors` from sysfs as column `MISSED`, or leaves the
/// interface without it, noted, when sysfs has no such file for it.
fn with_missed(
    mut interfaces: Vec<procfs::Interface>,
    losses: &mut Losses,
) -> Vec<procfs::Interface> {
    for interface in &mut interfaces {
        let path = format!(
            "/sys/class/net/{}/statistics/rx_missed_errors",
            interface.name
        );
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
        {
            Some(missed) if interface.columns.len() == MISSED => interface.columns.push(missed),
            Some(_) => {}
            None => losses.note(format!(
                "{path} could not be read, so `{}` has no `missed` series",
                interface.name
            )),
        }
    }
    interfaces
}

/// The series one counter of one interface is kept under.
///
/// A series is a set of keys, and `iface` is the only key the entry declares,
/// so the counter kind goes in the key's name: `veth0/drops`. An interface
/// name cannot contain `/` -- the kernel refuses it -- so the split cannot be
/// read two ways, and the bare name travels as the `iface` attribute, which is
/// what the filter field matches.
fn series(interface: &procfs::Interface, kind: &str) -> KeyValue {
    KeyValue::iface(interface.index, format!("{}/{kind}", interface.name))
}

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
            pass_first_tick: false,
            losses: Losses::default(),
        }))
    }
}

/// One event per interface and per counter kind.
///
/// It used to be one event per interface whose value was errors, with drops,
/// fifo overruns, collisions and carrier losses as attributes beside it -- so
/// `rate`, `timeline` and `summary` answered about errors alone, and a link
/// that had just dropped a thousand frames read `0 count/s` (net 5 in
/// `docs/scenarios/net.md`). Kept apart, as the catalog requires, and every
/// one of them reachable by every emit.
pub struct IfaceCollector {
    id: CollectorId,
    /// A poll and a count measure an interval; a snapshot cannot, and reports
    /// the counters as they stand instead.
    mode: Mode,
    only: Option<String>,
    deltas: Deltas,
    /// The first tick after the baseline taken at `start`, passed over: its
    /// interval is the microseconds since the program's release.
    pass_first_tick: bool,
    losses: Losses,
}

impl Collector for IfaceCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    /// A poll and a count measure frames over a window; a snapshot can only
    /// report what the counter stands at. Different numbers, different names.
    fn value_attr(&self) -> &'static str {
        match self.mode {
            Mode::Snapshot => "frames_since_boot",
            _ => "frames",
        }
    }

    /// The baseline, before the observed program is released, so its first
    /// frames are in the first interval; the runner's first tick, which
    /// follows the release by microseconds, is then passed over.
    fn start(&mut self) -> Result<(), String> {
        if self.mode == Mode::Snapshot {
            return Ok(());
        }
        match self.sample(0) {
            Ok(_) => {}
            // What is not there yet is the first tick's to say.
            Err(SampleError::TargetGone(_)) => return Ok(()),
            Err(error) => return Err(format!("the baseline read failed: {error:?}")),
        }
        self.pass_first_tick = self.mode != Mode::Count;
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if std::mem::take(&mut self.pass_first_tick) {
            return Ok(Vec::new());
        }
        let interfaces = procfs::interfaces(self.only.as_deref(), &mut self.losses)?;
        let interfaces = with_missed(interfaces, &mut self.losses);
        let ts_ns = monotonic_ns();
        let mut events = Vec::new();
        let mut index = seq;

        for interface in interfaces {
            if self.mode == Mode::Snapshot {
                for event in self.standing_events(&interface, ts_ns, index) {
                    events.push(event);
                    index += 1;
                }
                continue;
            }

            let mut delta = |column: usize, side: &str, kind: &str| {
                self.deltas.since(
                    &format!("{}:{side}_{kind}", interface.name),
                    interface.column(column),
                    &mut self.losses,
                )
            };
            // Every counter of an interface is read in the same tick, so if one
            // has no increment to report none of them do: this is the first
            // sight of the interface, and every column becomes its baseline.
            let mut increments = Vec::new();
            let mut first_sight = false;
            for (kind, rx, tx) in kinds_of(&interface) {
                let rx = rx.map(|column| delta(column, "rx", kind));
                let tx = tx.map(|column| delta(column, "tx", kind));
                if rx == Some(None) || tx == Some(None) {
                    first_sight = true;
                }
                increments.push((kind, rx.flatten(), tx.flatten()));
            }
            if first_sight {
                continue;
            }

            for (kind, rx, tx) in increments {
                let mut attrs = vec![Attr::uint("frames", rx.unwrap_or(0) + tx.unwrap_or(0))];
                if let Some(rx) = rx {
                    attrs.push(Attr::uint("rx", rx));
                }
                if let Some(tx) = tx {
                    attrs.push(Attr::uint("tx", tx));
                }
                attrs.push(Attr::text("counter", kind));
                attrs.push(Attr::text("iface", interface.name.clone()));
                events.push(Event {
                    ts_ns,
                    ts_err_ns: 0,
                    kind: "net.iface".to_owned(),
                    keys: vec![series(&interface, kind)],
                    attrs,
                    collector_id: self.id.clone(),
                    seq: index,
                });
                index += 1;
            }
        }
        Ok(events)
    }

    /// A count's closing read. A poll's last interval is the runner's last
    /// tick (`DECISIONS.md`, "the runner closes the window"): a read here
    /// covered the microseconds since and put a zero point at the previous
    /// one's offset into every series. A snapshot has no window to close.
    fn stop(&mut self) -> Vec<Event> {
        if self.mode != Mode::Count {
            return Vec::new();
        }
        self.sample(u64::MAX / 2).unwrap_or_default()
    }

    fn notes(&self) -> Vec<String> {
        vec![
            "each interface is one series per counter kind -- `<iface>/errors`, `/drops`, \
             `/fifo`, `/collisions`, `/carrier`, `/mcast`, `/missed` -- because a bad frame and a full queue are \
             different failures; `iface` on each event is the interface alone"
                .to_owned(),
        ]
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
    fn standing_events(&self, interface: &procfs::Interface, ts_ns: u64, seq: u64) -> Vec<Event> {
        kinds_of(interface)
            .enumerate()
            .map(|(offset, (kind, rx, tx))| {
                let rx = rx.map(|column| interface.column(column));
                let tx = tx.map(|column| interface.column(column));
                let mut attrs = vec![Attr::uint(
                    "frames_since_boot",
                    rx.unwrap_or(0) + tx.unwrap_or(0),
                )];
                if let Some(rx) = rx {
                    attrs.push(Attr::uint("rx_since_boot", rx));
                }
                if let Some(tx) = tx {
                    attrs.push(Attr::uint("tx_since_boot", tx));
                }
                attrs.push(Attr::text("counter", kind));
                attrs.push(Attr::text("iface", interface.name.clone()));
                attrs.push(Attr::text(
                    "basis",
                    "the counters as they stand: a snapshot observes no window of its own",
                ));
                Event {
                    ts_ns,
                    ts_err_ns: 0,
                    kind: "net.iface".to_owned(),
                    keys: vec![series(interface, kind)],
                    attrs,
                    collector_id: self.id.clone(),
                    seq: seq + offset as u64,
                }
            })
            .collect()
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

    fn of<'a>(events: &'a [Event], kind: &str) -> &'a Event {
        events
            .iter()
            .find(|event| {
                event
                    .attr("counter")
                    .is_some_and(|value| value.to_string() == kind)
            })
            .unwrap_or_else(|| panic!("no `{kind}` series"))
    }

    /// The baseline is read at `start`; the runner's first tick passes
    /// over, the second measures, and a poll reads nothing at `stop`, whose
    /// interval would be the microseconds since the last tick.
    #[test]
    fn a_poll_measures_from_the_start_and_reads_nothing_at_the_close() {
        let mut collector = Box::new(Prepare(polling()))
            .attach(&Target::Iface("lo".to_owned()))
            .expect("readable");
        collector.start().expect("the baseline");
        assert!(
            collector.sample(0).unwrap().is_empty(),
            "the first tick passes over"
        );
        assert!(
            !collector.sample(1).unwrap().is_empty(),
            "the second measures"
        );
        assert!(collector.stop().is_empty());
    }

    #[test]
    fn a_healthy_loopback_reports_zero_of_everything() {
        let mut collector = Box::new(Prepare(polling()))
            .attach(&Target::Iface("lo".to_owned()))
            .expect("loopback exists");
        collector.sample(0).unwrap();
        let events = collector.sample(0).unwrap();
        assert_eq!(events.len(), KINDS.len(), "one series per counter kind");
        // Loopback cannot have link errors: there is no link.
        for (kind, _, _) in KINDS {
            assert_eq!(
                of(&events, kind)
                    .attr("frames")
                    .and_then(|value| value.as_f64()),
                Some(0.0),
                "loopback reported a non-zero `{kind}`"
            );
        }
    }

    /// Every series carries the kernel's index for its interface, so a join on
    /// `iface` can meet a source that reads the index itself. Ground truth from
    /// another file than the one the counters come from: sysfs's `ifindex`.
    #[test]
    fn every_series_carries_the_index_sysfs_gives_its_interface() {
        let mut collector = Box::new(Prepare(polling()))
            .attach(&Target::System)
            .expect("the machine is readable");
        collector.sample(0).unwrap();
        let events = collector.sample(1).unwrap();
        assert!(!events.is_empty(), "the machine has at least loopback");
        for event in &events {
            let KeyValue::Iface { index, .. } = &event.keys[0] else {
                panic!("not an interface key: {:?}", event.keys[0]);
            };
            let name = event.attr("iface").expect("the interface").to_string();
            let Ok(text) = std::fs::read_to_string(format!("/sys/class/net/{name}/ifindex")) else {
                continue; // removed since the read
            };
            assert_eq!(index.to_string(), text.trim(), "`{name}`");
        }
    }

    /// Each counter kind is its own series, so every emit that aggregates by
    /// series reaches each of them -- and the key cannot be read two ways,
    /// because an interface name cannot hold a `/`.
    #[test]
    fn each_counter_kind_is_its_own_series() {
        let mut collector = Box::new(Prepare(polling()))
            .attach(&Target::Iface("lo".to_owned()))
            .expect("loopback exists");
        collector.sample(0).unwrap();
        let events = collector.sample(1).unwrap();
        let keys: std::collections::BTreeSet<String> = events
            .iter()
            .map(|event| event.keys[0].to_string())
            .collect();
        assert_eq!(keys.len(), KINDS.len(), "{keys:?}");
        assert!(keys.contains("iface:lo/drops"), "{keys:?}");
        for event in &events {
            assert_eq!(
                event
                    .attr("iface")
                    .map(|value| value.to_string())
                    .as_deref(),
                Some("lo")
            );
        }
        assert!(
            of(&events, "collisions").attr("rx").is_none(),
            "the file has no receive column for collisions, and zero would claim one"
        );
    }

    /// `mcast` and `missed` are what `ip -s link` prints beside the errors;
    /// a snapshot gives them as they stand. Ground truth from another file
    /// than the one `mcast` is read from: sysfs's statistics, for every
    /// interface of the machine.
    #[test]
    fn multicast_and_missed_frames_are_the_counters_sysfs_reports() {
        let mut collector = Box::new(Prepare(Mode::Snapshot))
            .attach(&Target::System)
            .expect("the machine is readable");
        let events = collector.sample(0).expect("one read");
        let mut checked = 0;
        for (kind, file) in [("mcast", "multicast"), ("missed", "rx_missed_errors")] {
            for event in events
                .iter()
                .filter(|event| event.attr("counter").unwrap().to_string() == kind)
            {
                let name = event.attr("iface").unwrap().to_string();
                let Ok(text) =
                    std::fs::read_to_string(format!("/sys/class/net/{name}/statistics/{file}"))
                else {
                    continue;
                };
                let truth: f64 = text.trim().parse().unwrap();
                let read = event.attr("rx_since_boot").unwrap().as_f64().unwrap();
                // A busy interface moves between the two reads.
                assert!(
                    read <= truth && truth - read <= truth * 0.01 + 16.0,
                    "`{name}` {kind}: {read} against sysfs's {truth}"
                );
                assert!(event.attr("tx_since_boot").is_none(), "receive only");
                checked += 1;
            }
        }
        assert!(checked >= 2, "loopback has both: {checked}");
    }

    /// A snapshot of a counter source reports the counter, not an increment
    /// over the microseconds between its read and the run's close.
    ///
    /// Loopback is the ground truth here: the reading has to be the file's,
    /// byte for byte, and a snapshot that answered with an increment would
    /// report zero, which is the defect this replaced.
    #[test]
    fn a_snapshot_reports_the_counters_as_they_stand() {
        let mut collector = Box::new(Prepare(Mode::Snapshot))
            .attach(&Target::Iface("lo".to_owned()))
            .expect("loopback exists");
        let events = collector.sample(0).expect("one read");
        assert_eq!(
            events.len(),
            KINDS.len(),
            "one interface, one reading per kind"
        );
        let errors = of(&events, "errors");
        assert!(
            errors.attr("frames").is_none(),
            "an interval's name on a number that is not one would be the lie this fixes"
        );
        assert!(
            collector.stop().is_empty(),
            "a snapshot has no window to close, and a closing read would replace its answer"
        );

        let mut losses = Losses::default();
        let file = procfs::interfaces(Some("lo"), &mut losses).expect("/proc/net/dev");
        let standing = file[0].column(RX_ERRORS) + file[0].column(TX_ERRORS);
        assert_eq!(
            errors
                .attr("frames_since_boot")
                .and_then(|value| value.as_f64()),
            Some(standing as f64)
        );
    }
}
