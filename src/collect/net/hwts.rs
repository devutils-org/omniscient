//! `net.hwts` over the `ethtool` netlink family and the PTP character device.
//!
//! This source answers the question every other latency measurement in the tool
//! quietly assumes away: *whose word is the timestamp*. A packet the kernel says
//! arrived at some moment arrived then only as far as the kernel noticed it --
//! the interrupt, the softirq, the queue. A card that stamps in hardware says
//! when the frame was on the wire, and the difference between the two is the
//! part of a latency that belongs to this machine rather than to the network.
//!
//! A card that stamps in hardware stamps from its own clock, and that clock is
//! not `CLOCK_MONOTONIC`. So the measurement here is the offset between the two,
//! read as a sandwich: monotonic, the card's clock, monotonic again. Polled, the
//! offset becomes a series and the rate it moves at is the rate the two
//! oscillators disagree at -- which is the calibration the catalog's caveat
//! calls mandatory, made rather than assumed.
//!
//! **An interface with no hardware clock is refused, by name.** Its packets are
//! stamped from `CLOCK_REALTIME` like everything else the kernel stamps in
//! software, there is no second clock to be offset from, and answering with a
//! zero would be a number where there is no measurement. The refusal carries
//! what the interface *can* do, because that is the other half of what a reader
//! came here for.
//!
//! What this backend is **not** is a timestamp per packet. That needs a socket
//! the tool owns or a program in the kernel; the catalog declares it as the
//! `so_timestamping` backend and this one says so as its degradation.

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd};

use super::ethtool::{Ethtool, TimestampInfo};
use super::foreign_clock::Calibration;
use super::interfaces;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Target;

/// A PTP hardware clock is free-running unless something disciplines it, so
/// nothing bounds how fast it moves away from `CLOCK_MONOTONIC` the way the
/// kernel's slew ceiling bounds the realtime clock. Nothing is assumed: each
/// offset carries the width of the read that produced it, and how fast it moves
/// is what polling this source measures.
const NO_ASSUMED_DRIFT_PPM: u64 = 0;

pub struct Prepare;

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let only = match target {
            Target::System => None,
            Target::Iface(name) => {
                // Refused here rather than at the first read, so a typo does
                // not look like an interface that answers nothing.
                interfaces::index_of(name)?;
                Some(name.clone())
            }
            other => {
                return Err(format!(
                    "timestamping belongs to an interface, and a {} target names none; a \
                     per-socket answer is the `so_timestamping` backend, which is not built",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(HwtsCollector {
            only,
            ethtool: None,
            clocked: Vec::new(),
            unclocked: Vec::new(),
            clocks: BTreeMap::new(),
            losses: Losses::default(),
            notes: Vec::new(),
        }))
    }
}

/// The probe behind the `phc` requirement: whether some interface on this
/// machine keeps a PTP hardware clock this process can read.
///
/// It asks exactly what `start` asks -- every interface's timestamping
/// capabilities over ethtool, and an open of each clock they name -- because
/// available means it attaches. Until the fourteenth wave `phc` had no probe,
/// the netlink backend did not require it, and the CI's runner listed
/// `net.hwts` usable and refused it at start: its one interface keeps
/// `/dev/ptp1`, which only root may read.
pub fn clock_support() -> Result<(), String> {
    let mut collector = Box::new(Prepare).attach(&Target::System)?;
    collector.start()
}

/// An open PTP hardware clock, and the clock id `clock_gettime` reads it by.
struct Phc {
    index: u32,
    /// Held for the whole run: the clock id is derived from the descriptor, so
    /// closing it would take the clock with it.
    fd: OwnedFd,
}

impl Phc {
    fn open(index: u32) -> std::io::Result<Self> {
        let fd: OwnedFd = std::fs::File::open(format!("/dev/ptp{index}"))?.into();
        Ok(Self { index, fd })
    }

    /// The dynamic POSIX clock id of an open PTP device.
    ///
    /// `FD_TO_CLOCKID` in `linux/posix-clock.h`: the descriptor number,
    /// complemented, shifted up three and tagged. There is no libc wrapper
    /// because it is not a constant -- it is an encoding of a descriptor.
    fn clock_id(&self) -> libc::clockid_t {
        (!(self.fd.as_raw_fd() as libc::clockid_t) << 3) | 3
    }

    fn device(&self) -> String {
        format!("/dev/ptp{}", self.index)
    }
}

/// One interface that keeps a clock, as the run found it.
struct Clocked {
    name: String,
    ifindex: u32,
    phc_index: u32,
    info: TimestampInfo,
}

pub struct HwtsCollector {
    /// The interface the request named; every interface when it named none.
    only: Option<String>,
    ethtool: Option<Ethtool>,
    /// The interfaces this run measures, settled once at start.
    clocked: Vec<Clocked>,
    /// The interfaces that were looked at and have no clock. Not events, and
    /// not silence either: they are named in the notes.
    unclocked: Vec<String>,
    /// Clocks opened, by PTP index. Two interfaces of one card share a clock.
    clocks: BTreeMap<u32, Phc>,
    losses: Losses,
    notes: Vec<String>,
}

impl HwtsCollector {
    fn interfaces(&self) -> Result<Vec<String>, String> {
        match &self.only {
            Some(name) => Ok(vec![name.clone()]),
            None => interfaces::all(),
        }
    }

    /// Asks every interface in scope what it can do, and opens the clocks they
    /// name.
    ///
    /// Done once, at start, because the answer is a property of the driver
    /// rather than of the moment -- and because a run that can measure nothing
    /// should say so before it observes anything, not after.
    fn survey(&mut self) -> Result<(), String> {
        let names = self.interfaces()?;
        let mut ethtool = self.ethtool.take().expect("opened by the caller");
        for name in names {
            let Ok(ifindex) = interfaces::index_of(&name) else {
                continue;
            };
            let info = match ethtool.timestamping(&name) {
                Ok(info) => info,
                // A driver with no timestamping support at all answers this
                // way, and that is an answer: it cannot stamp in hardware.
                Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => {
                    TimestampInfo::default()
                }
                Err(error) => {
                    self.losses.failed_reads += 1;
                    self.losses.note_times(
                        format!("`{name}` was asked what it can timestamp and answered: {error}"),
                        1,
                    );
                    continue;
                }
            };
            match info.phc_index {
                Some(phc_index) => {
                    if let std::collections::btree_map::Entry::Vacant(slot) =
                        self.clocks.entry(phc_index)
                    {
                        match Phc::open(phc_index) {
                            Ok(clock) => {
                                slot.insert(clock);
                            }
                            Err(error) => {
                                // Out of reach is not absent: this one is a
                                // permission to grant, and saying which.
                                self.notes.push(format!(
                                    "`{name}` keeps PTP hardware clock {phc_index} and \
                                     /dev/ptp{phc_index} could not be read ({error}), so its \
                                     offset is not in these results"
                                ));
                                self.unclocked.push(name.clone());
                                continue;
                            }
                        }
                    }
                    self.clocked.push(Clocked {
                        name,
                        ifindex,
                        phc_index,
                        info,
                    });
                }
                None => self.unclocked.push(name),
            }
        }
        self.ethtool = Some(ethtool);
        Ok(())
    }

    /// The refusal for a run that has no clock to measure against.
    ///
    /// It carries what was found rather than only what was not, because "`lo`
    /// cannot do this" sends a reader looking for a different flag and "`lo`
    /// stamps in software, from CLOCK_REALTIME" answers the actual question.
    fn nothing_to_measure(&self) -> String {
        let (who, whose) = match &self.only {
            Some(name) => (format!("`{name}` keeps no"), format!("`{name}`")),
            None => (
                "no interface on this machine keeps a".to_owned(),
                "them".to_owned(),
            ),
        };
        let mut reason = format!(
            "{who} readable PTP hardware clock, so there is no second clock domain to measure an \
             offset to: packets on {whose} are stamped by the kernel from CLOCK_REALTIME, like \
             every other software timestamp"
        );
        if !self.notes.is_empty() {
            reason.push_str(&format!(" ({})", self.notes.join("; ")));
        }
        reason
    }

    fn event(&self, clocked: &Clocked, seq: u64) -> Result<Event, SampleError> {
        let clock = self
            .clocks
            .get(&clocked.phc_index)
            .expect("every clocked interface had its clock opened at start");
        let calibration = Calibration::take(clock.clock_id()).map_err(|error| {
            SampleError::Transient(format!(
                "PTP hardware clock {} could not be read: {error}",
                clocked.phc_index
            ))
        })?;
        let (_, offset_err_ns) = calibration.convert(calibration.foreign_ns, NO_ASSUMED_DRIFT_PPM);

        let info = &clocked.info;
        let mut attrs = vec![
            // The measurement: how far the clock that would stamp this
            // interface's packets is from the one every event here is stamped
            // with. Signed, because it can be either way round, and large,
            // because a disciplined PHC counts from 1970 and CLOCK_MONOTONIC
            // from boot.
            Attr::int(
                "offset_ns",
                calibration
                    .offset_ns()
                    .clamp(i64::MIN as i128, i64::MAX as i128) as i64,
            ),
            Attr::uint("offset_err_ns", offset_err_ns as u64),
            Attr::text("iface", clocked.name.clone()),
            Attr::text("phc", clock.device()),
            Attr::uint("phc_index", clocked.phc_index as u64),
            // A card can keep a clock and still not stamp packets with it --
            // `hardware-raw-clock` with neither direction set is exactly that
            // -- so having a clock and using one are two questions.
            Attr::uint("hardware", u64::from(info.stamps_in_hardware())),
            // Kept apart, because a card is allowed to stamp one direction and
            // not the other, and a latency built from both is only as good as
            // its weaker half.
            Attr::uint("hardware_receive", u64::from(info.has("hardware-receive"))),
            Attr::uint(
                "hardware_transmit",
                u64::from(info.has("hardware-transmit")),
            ),
            Attr::text("capabilities", info.capabilities.join(",")),
        ];
        if !info.tx_types.is_empty() {
            attrs.push(Attr::text("tx_types", info.tx_types.join(",")));
        }
        if !info.rx_filters.is_empty() {
            attrs.push(Attr::text("rx_filters", info.rx_filters.join(",")));
        }

        Ok(Event {
            // The moment of the reading is the midpoint of the sandwich, and it
            // is known only to within the sandwich's own width. Never zero:
            // this event is about another clock domain, and the invariant is
            // that such an event says how far off it may be.
            ts_ns: calibration.monotonic_ns,
            ts_err_ns: (calibration.window_ns / 2).max(1).min(u32::MAX as u64) as u32,
            kind: "net.hwts".to_owned(),
            // This process's network namespace: the `ethtool` netlink socket
            // answers for the devices of the namespace it was opened in.
            keys: vec![KeyValue::iface(clocked.ifindex, clocked.name.clone())],
            attrs,
            collector_id: CollectorId::unstamped(),
            seq,
        })
    }
}

impl Collector for HwtsCollector {
    fn value_attr(&self) -> &'static str {
        "offset_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.ethtool = Some(Ethtool::open().map_err(|error| {
            format!(
                "the `ethtool` netlink family could not be reached ({error}); it is how this \
                 kernel is asked what an interface can timestamp"
            )
        })?);
        self.survey()?;
        if self.clocked.is_empty() {
            return Err(self.nothing_to_measure());
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::with_capacity(self.clocked.len());
        let mut index = seq;
        // Indices rather than a borrow of `self.clocked`: the event builder
        // needs the collector, and the list it walks belongs to it.
        for position in 0..self.clocked.len() {
            match self.event(&self.clocked[position], index) {
                Ok(event) => {
                    events.push(event);
                    index += 1;
                }
                Err(SampleError::Transient(reason)) => {
                    self.losses.failed_reads += 1;
                    self.losses.note_times(reason, 1);
                }
                Err(other) => return Err(other),
            }
        }
        Ok(events)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        if !self.unclocked.is_empty() {
            notes.push(format!(
                "these interfaces keep no hardware clock, so their packets are stamped from \
                 CLOCK_REALTIME and they are not in these results: {}",
                self.unclocked.join(", ")
            ));
        }
        notes.push(
            "this backend reports how far an interface's own clock is from CLOCK_MONOTONIC; a \
             timestamp per packet is the `so_timestamping` backend, which is not built"
                .to_owned(),
        );
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Attaches and starts, or says why this machine has nothing to measure.
    ///
    /// A machine with no readable PTP hardware clock cannot serve this source
    /// at all, which is a different answer from a test that means nothing --
    /// so it is reported here and the caller decides, rather than every test
    /// deciding to fall silent.
    fn started(target: Target) -> Result<Box<dyn Collector>, String> {
        let mut collector = Box::new(Prepare).attach(&target)?;
        collector.start()?;
        Ok(collector)
    }

    #[test]
    fn a_process_target_is_refused_and_names_the_backend_that_would_answer() {
        let Err(error) = Box::new(Prepare).attach(&Target::Pid(vec![1])) else {
            panic!("timestamping belongs to an interface");
        };
        assert!(error.contains("so_timestamping"), "{error}");
    }

    #[test]
    fn loopback_is_refused_because_it_has_no_second_clock_to_be_offset_from() {
        // Ground truth from outside this code: loopback is ARPHRD_LOOPBACK, a
        // software interface with no card, so it cannot have a card's clock.
        let kind =
            std::fs::read_to_string("/sys/class/net/lo/type").expect("every machine has one");
        assert_eq!(kind.trim(), "772", "ARPHRD_LOOPBACK");

        let Err(reason) = started(Target::Iface("lo".to_owned())) else {
            panic!("loopback has no hardware clock and cannot report an offset to one");
        };
        assert!(
            reason.contains("CLOCK_REALTIME"),
            "the refusal should say where loopback's timestamps do come from: {reason}"
        );
    }

    #[test]
    fn every_interface_that_reports_an_offset_names_a_clock_this_machine_has() {
        // The cross-check: ethtool says an interface is wired to PTP clock N,
        // and sysfs -- a different subsystem entirely -- must agree that clock
        // N exists. A backend that invented an index would pass every test that
        // only read its own answer back.
        let mut collector = match started(Target::System) {
            Ok(collector) => collector,
            Err(reason) => {
                eprintln!("this machine cannot serve net.hwts: {reason}");
                return;
            }
        };
        let events = collector.sample(0).expect("the machine answers");
        assert!(
            !events.is_empty(),
            "the run started, so at least one interface has a clock"
        );
        for event in &events {
            let index = event
                .attr("phc_index")
                .and_then(|value| value.as_f64())
                .expect("an interface that reported an offset names its clock")
                as u32;
            assert!(
                std::path::Path::new(&format!("/sys/class/ptp/ptp{index}")).exists(),
                "an interface claims PTP clock {index} and sysfs has no such clock"
            );
            assert!(
                event
                    .attr("offset_ns")
                    .and_then(|value| value.as_f64())
                    .is_some(),
                "the measured value is on every event this source produces"
            );
            assert!(
                event.ts_err_ns > 0,
                "a reading of another clock domain never claims to be exact"
            );
        }
    }

    #[test]
    fn a_hardware_clock_advances_at_very_nearly_our_own_rate() {
        // Two independent clocks and a known interval between two readings of
        // both. If the offset between them moved by anything like the interval
        // itself, one of the two is not a clock and the reading is not a
        // measurement. The bound is loose on purpose: a free-running PHC is
        // allowed to drift, it is not allowed to be fiction.
        let mut collector = match started(Target::System) {
            Ok(collector) => collector,
            Err(reason) => {
                eprintln!("this machine cannot serve net.hwts: {reason}");
                return;
            }
        };

        let offsets = |events: &[Event]| -> BTreeMap<String, i64> {
            events
                .iter()
                .filter_map(|event| {
                    Some((
                        event.attr("iface")?.to_string(),
                        event.attr("offset_ns")?.as_f64()? as i64,
                    ))
                })
                .collect()
        };

        let before = offsets(&collector.sample(0).expect("the machine answers"));
        let elapsed = std::time::Duration::from_millis(200);
        std::thread::sleep(elapsed);
        let after = offsets(&collector.sample(0).expect("the machine answers again"));

        assert!(!before.is_empty(), "the run started, so something answered");
        for (name, first) in &before {
            let Some(second) = after.get(name) else {
                continue;
            };
            let moved = first.abs_diff(*second);
            assert!(
                moved < elapsed.as_nanos() as u64 / 100,
                "over {elapsed:?} the offset to `{name}`'s clock moved {moved} ns, which is more \
                 than one part in a hundred: that clock is not ticking at our rate"
            );
        }
    }
}
