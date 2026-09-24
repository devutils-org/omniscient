//! `net.capture` over `AF_PACKET`.
//!
//! Every other source in this family reports what the stack concluded about a
//! packet. This one reports the packet, which is the only way to answer "did it
//! arrive at all" -- and the only source in the tool whose raw material is
//! somebody else's data.
//!
//! So the shape of it is decided by what must *not* happen. The kernel copies
//! ninety-six bytes of each frame and no more, which is the headers; a payload
//! cannot be leaked by a tool that never receives one. What the events carry is
//! what the catalog declares filterable on this source and nothing beyond it.
//! Both facts reach the user with the results, because a promise nobody is told
//! about is indistinguishable from no promise.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use super::afpacket::{self, Capture, Stats, SNAPLEN};
use super::foreign_clock::{Calibration, MAX_SLEW_PPM};
use super::interfaces;
use super::packet;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Target;

/// How often the ring is drained.
///
/// The kernel hands a block over after five milliseconds whether or not it is
/// full, so draining four times slower than that keeps at most a handful of
/// blocks outstanding while asking the ring far less often than a packet
/// arrives.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(20);

pub struct Prepare;

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let (ifindex, name) = match target {
            // A packet socket bound to interface zero sees every interface,
            // which is what a machine-wide capture is.
            Target::System => (0, None),
            Target::Iface(name) => (interfaces::index_of(name)?, Some(name.clone())),
            other => {
                return Err(format!(
                    "a capture reads frames off an interface, and a {} target names none; use \
                     --iface or --system",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(CaptureCollector {
            id: CollectorId::new("net.capture/af_packet"),
            ifindex,
            bound_to: name,
            names: BTreeMap::new(),
            capture: None,
            calibration: None,
            stepped: 0,
            step_allowance_ns: 0,
            totals: Stats::default(),
            losses: Losses::default(),
        }))
    }
}

pub struct CaptureCollector {
    id: CollectorId,
    ifindex: u32,
    /// The interface the request named, when it named one.
    bound_to: Option<String>,
    /// Names already resolved from indices, so a machine-wide capture does not
    /// ask the kernel once per packet.
    names: BTreeMap<u32, String>,
    capture: Option<Capture>,
    /// The last pairing of the realtime and monotonic clocks.
    calibration: Option<Calibration>,
    /// How many times the realtime clock moved by more than it could have been
    /// slewed, which means it was set.
    stepped: u64,
    /// How much a step moved it by, carried into the error of every timestamp
    /// converted afterwards.
    step_allowance_ns: u64,
    totals: Stats,
    losses: Losses,
}

impl CaptureCollector {
    /// Re-pairs the two clocks, and notices if the realtime one was set rather
    /// than slewed since the last time.
    fn recalibrate(&mut self) -> Result<Calibration, SampleError> {
        let taken = Calibration::take(libc::CLOCK_REALTIME).map_err(|error| {
            SampleError::Transient(format!("the realtime clock could not be read: {error}"))
        })?;
        if let Some(previous) = self.calibration {
            if !previous.is_continuous_with(&taken, MAX_SLEW_PPM) {
                let moved = (taken.offset_ns() - previous.offset_ns()).unsigned_abs();
                self.stepped += 1;
                self.step_allowance_ns = self
                    .step_allowance_ns
                    .max(moved.min(u64::MAX as u128) as u64);
                self.losses.note(format!(
                    "the realtime clock was set, not slewed, by {moved} ns during the run; \
                     the frames stamped around that moment carry it in their ts_err_ns"
                ));
            }
        }
        self.calibration = Some(taken);
        Ok(taken)
    }

    /// Reads the kernel's own accounting and folds it into the run's losses.
    ///
    /// `PACKET_STATISTICS` reports since the previous read and resets, so this
    /// accumulates rather than overwriting.
    fn absorb_statistics(&mut self) {
        let Some(capture) = self.capture.as_ref() else {
            return;
        };
        match capture.statistics() {
            Ok(stats) => {
                self.totals.packets += stats.packets;
                self.totals.drops += stats.drops;
                self.totals.queue_freezes += stats.queue_freezes;
                if stats.drops > 0 {
                    self.losses.dropped += stats.drops as u64;
                    self.losses.note(
                        "the capture ring was full and the kernel dropped frames it had nowhere \
                         to put"
                            .to_owned(),
                    );
                }
                if stats.queue_freezes > 0 {
                    self.losses.note(
                        "the capture ring ran out of blocks and the kernel had to wait for one, \
                         which is the state immediately before dropping"
                            .to_owned(),
                    );
                }
            }
            Err(error) => {
                self.losses.failed_reads += 1;
                self.losses.note(format!(
                    "the capture's own drop counter could not be read: {error}"
                ));
            }
        }
    }

    fn name_of(&mut self, index: u32) -> String {
        if let Some(name) = &self.bound_to {
            return name.clone();
        }
        if let Some(name) = self.names.get(&index) {
            return name.clone();
        }
        // An interface that went away between the frame and this lookup has no
        // name left to find; its index is still an identity.
        let name = interfaces::name_of(index).unwrap_or_else(|| format!("if{index}"));
        self.names.insert(index, name.clone());
        name
    }
}

impl Collector for CaptureCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    fn start(&mut self) -> Result<(), String> {
        let capture = Capture::open(self.ifindex).map_err(|error| match error.raw_os_error() {
            Some(libc::EPERM) | Some(libc::EACCES) => format!(
                "a packet capture needs CAP_NET_RAW, which this process does not have \
                     ({error}); this is a privilege to grant, not a kernel to change: `make \
                     CAPTURE=1` grants it with the other two, and only when asked, because it \
                     is the one capability here that can send frames as well as read them"
            ),
            Some(libc::ENODEV) => format!(
                "interface {} went away before the capture could bind to it",
                self.bound_to.as_deref().unwrap_or("?")
            ),
            _ => format!("the capture ring could not be set up: {error}"),
        })?;
        self.capture = Some(capture);
        // A first pairing of the clocks, before any frame arrives, so the
        // earliest frames are converted against a calibration of their own age
        // rather than one taken at the end of the first tick.
        match self.recalibrate() {
            Ok(_) => Ok(()),
            Err(SampleError::Transient(reason) | SampleError::TargetGone(reason)) => Err(reason),
        }
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if self.capture.is_none() {
            return Err(SampleError::Transient("the capture is closed".into()));
        }
        let calibration = self.recalibrate()?;
        let step_allowance_ns = self.step_allowance_ns;
        let id = self.id.clone();

        // Drained into a buffer rather than straight into events: a frame
        // borrows the ring block it lives in, and the block has to be given
        // back to the kernel quickly. Everything kept here is a number or a
        // short string the reader already decided to carry.
        let mut captured = Vec::new();
        let mut capture = self.capture.take().expect("checked above");
        capture.drain(|frame| {
            let headers = packet::read(frame.bytes, frame.network_offset);
            let (ts_ns, error_ns) = calibration.convert(frame.realtime_ns, MAX_SLEW_PPM);
            captured.push((
                ts_ns,
                error_ns.saturating_add(step_allowance_ns.min(u32::MAX as u64) as u32),
                frame.wire_len,
                frame.captured_len,
                frame.ifindex,
                frame.packet_type,
                headers,
            ));
        });
        self.capture = Some(capture);
        self.absorb_statistics();

        let mut events = Vec::with_capacity(captured.len());
        for (index, (ts_ns, ts_err_ns, wire_len, captured_len, ifindex, packet_type, headers)) in
            (seq..).zip(captured)
        {
            let name = self.name_of(ifindex);
            let mut attrs = vec![
                // The length on the wire, which is not the length that was
                // copied: the snaplen caps the copy, never the measurement.
                Attr::uint("bytes", wire_len as u64),
                Attr::uint("captured_bytes", captured_len as u64),
                Attr::text("iface", name.clone()),
                Attr::text("direction", afpacket::packet_type_name(packet_type)),
            ];
            if let Some(protocol) = headers.protocol {
                attrs.push(Attr::text("protocol", packet::protocol_name(protocol)));
            }
            if let Some(version) = headers.ip_version {
                attrs.push(Attr::uint("ip_version", version as u64));
            }
            if let Some(address) = headers.source {
                attrs.push(Attr::text("saddr", address));
            }
            if let Some(address) = headers.destination {
                attrs.push(Attr::text("daddr", address));
            }
            if let Some(port) = headers.source_port {
                attrs.push(Attr::uint("sport", port as u64));
            }
            if let Some(port) = headers.destination_port {
                attrs.push(Attr::uint("dport", port as u64));
            }

            events.push(Event {
                ts_ns,
                // Never zero: the kernel stamps a frame from the realtime
                // clock, and this is what the conversion out of it cost.
                ts_err_ns,
                kind: "net.capture".to_owned(),
                keys: vec![KeyValue::iface(ifindex, name)],
                attrs,
                collector_id: id.clone(),
                seq: index,
            });
        }
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        // A ring is drained one last time, unlike a polled counter: what is in
        // it now was captured inside the window, and leaving it there would
        // lose the run's final milliseconds.
        let events = self.sample(u64::MAX / 2).unwrap_or_default();
        self.capture = None;
        events
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = vec![
            format!(
                "the kernel copied at most {SNAPLEN} bytes of each frame, which is the headers: \
                 no payload entered this process, and `bytes` is still the length on the wire"
            ),
            "frames are stamped by the kernel from CLOCK_REALTIME, so every timestamp here was \
             converted into CLOCK_MONOTONIC and carries what that conversion could be off by in \
             its ts_err_ns"
                .to_owned(),
        ];
        if self.totals.packets > 0 || self.totals.drops > 0 {
            notes.push(format!(
                "the capture socket saw {} frames and dropped {}",
                self.totals.packets, self.totals.drops
            ));
        }
        if self.stepped > 0 {
            notes.push(format!(
                "the realtime clock was set {} time(s) during this run, so the timestamps are \
                 no better than {} ns",
                self.stepped, self.step_allowance_ns
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_that_names_no_interface_is_refused_with_what_to_use_instead() {
        let Err(error) = Box::new(Prepare).attach(&Target::Pid(vec![1])) else {
            panic!("a capture reads frames off an interface, and a process is not one");
        };
        assert!(error.contains("--iface"), "{error}");
    }

    #[test]
    fn an_interface_that_does_not_exist_is_refused_before_any_socket_is_opened() {
        let Err(error) = Box::new(Prepare).attach(&Target::Iface("nosuchif0".to_owned())) else {
            panic!("this machine has no interface called nosuchif0");
        };
        assert!(error.contains("nosuchif0"), "{error}");
    }

    #[test]
    fn the_notes_say_what_was_not_captured_before_any_frame_is() {
        let collector = Box::new(Prepare)
            .attach(&Target::Iface("lo".to_owned()))
            .expect("loopback exists");
        let notes = collector.notes().join(" ");
        assert!(
            notes.contains("no payload"),
            "the privacy boundary reaches the user with the results: {notes}"
        );
        assert!(
            notes.contains("CLOCK_REALTIME"),
            "so does the clock domain the timestamps came from: {notes}"
        );
    }

    #[test]
    fn a_capture_this_machine_will_not_allow_says_which_privilege_it_needs() {
        let mut collector = Box::new(Prepare)
            .attach(&Target::Iface("lo".to_owned()))
            .expect("loopback exists");
        match collector.start() {
            Ok(()) => {
                // The capture works here, so the drain has a ring to read and
                // the ground truth in `tests/family_net.rs` is what has
                // something to say. All that is asserted in process is that
                // the first tick does not fail on its own.
                assert!(collector.sample(0).is_ok());
            }
            Err(reason) => assert!(
                reason.contains("CAP_NET_RAW"),
                "the refusal must name the privilege, not the kernel: {reason}"
            ),
        }
    }
}
