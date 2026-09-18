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
use crate::request::Target;

/// Column offsets in `/proc/net/dev`, receive then transmit.
const RX_ERRORS: usize = 2;
const RX_DROPPED: usize = 3;
const RX_FIFO: usize = 4;
const TX_ERRORS: usize = 10;
const TX_DROPPED: usize = 11;
const TX_FIFO: usize = 12;
const COLLISIONS: usize = 13;
const CARRIER: usize = 14;

pub struct Prepare;

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
            only,
            deltas: Deltas::default(),
            losses: Losses::default(),
        }))
    }
}

pub struct IfaceCollector {
    id: CollectorId,
    only: Option<String>,
    deltas: Deltas,
    losses: Losses,
}

impl Collector for IfaceCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "errors"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let interfaces = procfs::interfaces(self.only.as_deref(), &mut self.losses)?;
        let ts_ns = monotonic_ns();
        let mut events = Vec::new();
        let mut index = seq;

        for interface in interfaces {
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
        // run is dropped.
        self.sample(u64::MAX / 2).unwrap_or_default()
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_healthy_loopback_reports_zero_of_everything() {
        let mut collector = Box::new(Prepare)
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
        let mut collector = Box::new(Prepare).attach(&Target::System).expect("system");
        collector.sample(0).unwrap();
        let events = collector.sample(0).unwrap();
        let event = events.first().expect("at least the loopback");
        assert!(event.attr("errors").is_some(), "a bad frame");
        assert!(event.attr("drops").is_some(), "a full queue");
    }
}
