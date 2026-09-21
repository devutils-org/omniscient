//! `net.drops` over a raw tracepoint: every packet the kernel threw away.
//!
//! This is the source that makes the rest of the family honest. `net.iface`
//! reports the frames a *device* lost and can see nothing that happens after
//! the driver hands a packet up; `net.qdisc` reports what a queueing discipline
//! held or dropped on the way out. Everything in between -- a checksum that did
//! not match, a socket that was not listening, a buffer that was full, a
//! filter that said no -- is `kfree_skb`, and since 5.17 it says which.
//!
//! The program is in `src/bpf/net_drops.rs`. This file is the part that belongs
//! to the family: what an event looks like, what is refused and what is counted
//! as lost.
//!
//! **The reason is a number here, and that is deliberate.** `enum
//! skb_drop_reason` has its names in the kernel's own BTF, which this build
//! cannot yet read an enum out of. A table of names written into this file
//! would be a constant no run could check against the kernel it is running on,
//! and a mislabelled reason is worse than a number: it sends a reader to fix
//! the wrong thing. The request that would close it is under `## Needs from
//! another stream` in `docs/open-findings.md`; until then the number is the
//! kernel's own and `docs/scenarios/net.md` says which one a closed port gives.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use crate::bpf::{DropProgram, PacketDrop};
use crate::collect::{not_built, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained.
///
/// Drops are bursty in exactly the way that fills a ring: a port scan, a flood,
/// a link that starts erroring produce thousands in a millisecond. Draining
/// often is cheap, and what it cannot absorb is counted rather than hidden.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// The ring the program writes into. Each record is 48 bytes plus an 8-byte
/// header, so this holds about nine thousand drops between drains.
const RING_BYTES: usize = 512 * 1024;

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// The request's predicate, which this collector applies itself: the
    /// resolver credits every `bpf` backend with filtering in the kernel and
    /// then leaves its events alone.
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot observe drops: a drop is an instant, and sampling one in a \
                 hundred of them would report a fraction of the packets the machine lost as \
                 though it were all of them",
                self.mode
            ));
        }
        if !matches!(target, Target::System) {
            return Err(not_built(format!(
                "a {} target cannot be served here: a dropped packet is dropped because nothing \
                 took delivery of it, so most drops belong to no process at all -- use --system",
                target.kind()
            )));
        }
        Ok(Box::new(DropCollector::new(self.mode, self.filter)))
    }
}

pub struct DropCollector {
    id: CollectorId,
    /// `trace` is one event per drop; `count` is the single measurement of one
    /// window, totalled per series.
    mode: Mode,
    program: Option<DropProgram>,
    losses: Losses,
    notes: Vec<String>,
    /// The program's counter is cumulative, so what is new is the difference.
    dropped: u64,
    /// The totals a `count` run accumulates, by what keys a series.
    counted: BTreeMap<(u64, u32), Counted>,
    /// Drops seen with no socket, which is what makes `protocol` unanswerable.
    without_socket: u64,
    seen: u64,
    /// Whether this kernel's tracepoint says which socket a received packet
    /// would have gone to.
    receive_socket: bool,
    predicate: Option<FilterExpr>,
    filtered_in: u64,
    filtered_out: u64,
}

/// One `(socket, reason)` pair's drops over a counted window.
struct Counted {
    drops: u64,
    bytes: u64,
    last: PacketDrop,
}

impl DropCollector {
    fn new(mode: Mode, predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("net.drops/bpf:raw_tp"),
            mode,
            program: None,
            losses: Losses::default(),
            notes: Vec::new(),
            dropped: 0,
            counted: BTreeMap::new(),
            without_socket: 0,
            seen: 0,
            receive_socket: true,
            predicate,
            filtered_in: 0,
            filtered_out: 0,
        }
    }

    fn counting(&self) -> bool {
        self.mode == Mode::Count
    }

    fn event(&self, dropped: &PacketDrop, drops: u64, bytes: u64, seq: u64) -> Event {
        Event {
            ts_ns: dropped.ts_ns,
            ts_err_ns: 0,
            kind: "net.drops".to_owned(),
            // The socket is the series when there is one. A cookie of zero is
            // not a socket: the kernel generates cookies lazily, so a socket
            // nothing has ever dumped has none, and keying such an event on
            // zero would join every anonymous drop to every other.
            keys: match dropped.cookie {
                0 => Vec::new(),
                cookie => vec![KeyValue::Sock { cookie }],
            },
            attrs: attributes(dropped, drops, bytes),
            collector_id: self.id.clone(),
            seq,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let mut undecoded = 0;
        let mut batch = Vec::new();
        program.drain(|dropped| batch.push(dropped), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the drop ring was shorter than one drop");
        }

        let mut index = 0;
        for dropped in batch {
            let candidate = self.event(&dropped, 1, u64::from(dropped.len), seq + index);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &candidate) {
                    self.filtered_out += 1;
                    continue;
                }
                self.filtered_in += 1;
            }
            self.seen += 1;
            if dropped.protocol().is_none() {
                self.without_socket += 1;
            }
            if self.counting() {
                let held = self
                    .counted
                    .entry((dropped.cookie, dropped.reason))
                    .or_insert(Counted {
                        drops: 0,
                        bytes: 0,
                        last: dropped,
                    });
                held.drops += 1;
                held.bytes += u64::from(dropped.len);
                held.last = dropped;
                continue;
            }
            events.push(candidate);
            index += 1;
        }
    }

    /// Reads the program's own drop counter, which is the only place a packet
    /// the ring could not hold is recorded.
    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note(
                "the drop ring was full: the machine lost packets faster than they were drained, \
                 so this run's own count of them is short",
            );
            self.dropped = dropped;
        }
    }
}

/// The IANA protocol numbers this build names.
///
/// Unlike a kernel enum these are an assigned-numbers registry: 6 has been TCP
/// since 1981 and cannot be renumbered, so naming them here is not the kind of
/// constant that goes stale. Anything else is reported as its number.
fn protocol_name(protocol: Option<u32>) -> String {
    match protocol {
        // Not a protocol this run could not identify -- a drop that had no
        // socket to read one from, which is most receive-side drops and every
        // drop of a packet nothing was listening for. Named rather than left
        // out so that a predicate over `protocol` can select exactly them.
        None => "no-socket".to_owned(),
        Some(1) => "icmp".to_owned(),
        Some(6) => "tcp".to_owned(),
        Some(17) => "udp".to_owned(),
        Some(58) => "icmpv6".to_owned(),
        Some(132) => "sctp".to_owned(),
        Some(other) => format!("proto {other}"),
    }
}

/// What an event carries beside its value.
///
/// `reason` and `protocol` are here because the catalog entry declares them as
/// filter fields: a predicate over a field no event carries is false
/// everywhere, which empties the series and looks exactly like a machine that
/// dropped nothing.
fn attributes(dropped: &PacketDrop, drops: u64, bytes: u64) -> Vec<Attr> {
    let mut attrs = vec![
        Attr::uint("drops", drops),
        // The kernel's own `enum skb_drop_reason` value. A number, because the
        // names live in BTF this build cannot yet read.
        Attr::uint("reason", u64::from(dropped.reason)),
        Attr::text("protocol", protocol_name(dropped.protocol())),
        Attr::uint("bytes", bytes),
        // Where in the kernel the packet died. Not symbolised: this is a kernel
        // address and resolving it needs `/proc/kallsyms`, which is a different
        // privilege and a different question.
        Attr::uint("location", dropped.location),
    ];
    if dropped.family != 0 {
        attrs.push(Attr::uint("family", u64::from(dropped.family)));
    }
    if dropped.cookie != 0 {
        attrs.push(Attr::uint("cookie", dropped.cookie));
    }
    if dropped.inode != 0 {
        // The socket's inode, which is what `/proc/<pid>/fd` shows as
        // `socket:[n]`. Carried beside the cookie because a socket has an inode
        // from the moment it exists and gets a cookie only when something asks.
        attrs.push(Attr::uint("inode", dropped.inode));
    }
    attrs
}

impl Collector for DropCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "drops"
    }

    fn start(&mut self) -> Result<(), String> {
        let program = DropProgram::load(RING_BYTES)?;
        self.receive_socket = program.has_receive_socket();
        self.program = Some(program);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_drops();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        self.absorb_drops();
        self.program = None;
        if self.counting() {
            let seq = u64::MAX / 2;
            let counted = std::mem::take(&mut self.counted);
            for (index, held) in counted.into_values().enumerate() {
                trailing.push(self.event(&held.last, held.drops, held.bytes, seq + index as u64));
            }
        }
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        if !self.receive_socket {
            notes.push(
                "this kernel's `kfree_skb` does not say which socket a received packet would \
                 have gone to, so a receive-side drop here carries no socket at all"
                    .to_owned(),
            );
        }
        if self.without_socket > 0 {
            notes.push(format!(
                "{} of {} drops had no socket to read a protocol from and are reported as \
                 `no-socket`; a packet nothing was listening for is the commonest case and is \
                 exactly the one a protocol cannot be recovered for without reading the packet \
                 itself",
                self.without_socket, self.seen,
            ));
        }
        notes.push(
            "the reason is this kernel's own `enum skb_drop_reason` value; the names are in its \
             BTF and this build does not read them yet"
                .to_owned(),
        );
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} drops and removed {}; it ran here, over the drained records, \
                 not in the kernel, so the removed ones were counted before they were dropped",
                self.filtered_in, self.filtered_out,
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dropped() -> PacketDrop {
        // Built through the decoder rather than by naming private fields, which
        // is the same path a real record takes.
        let mut bytes = [0u8; 48];
        bytes[0..8].copy_from_slice(&1_000u64.to_ne_bytes());
        bytes[8..12].copy_from_slice(&4u32.to_ne_bytes());
        bytes[12..16].copy_from_slice(&17u32.to_ne_bytes());
        bytes[16..20].copy_from_slice(&142u32.to_ne_bytes());
        bytes[20..24].copy_from_slice(&2u32.to_ne_bytes());
        bytes[24..32].copy_from_slice(&4242u64.to_ne_bytes());
        bytes[32..40].copy_from_slice(&99u64.to_ne_bytes());
        bytes[40..48].copy_from_slice(&0xffff_ffff_8000_0000u64.to_ne_bytes());
        PacketDrop::from_bytes(&bytes).expect("a full record")
    }

    /// A mode that cannot mean anything here is refused with the reason.
    #[test]
    fn a_sampled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Sample {
                freq: "997hz".parse().expect("a frequency"),
            },
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a sampled mode must be refused");
        };
        assert!(error.contains("cannot observe drops"), "{error}");
    }

    /// A process target is refused, and the refusal says why rather than
    /// filtering to a process that would see almost nothing: a packet is
    /// dropped precisely because no process took delivery of it.
    #[test]
    fn a_process_target_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("a pid names nothing most drops belong to");
        };
        assert!(error.contains("belong to no process"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// Every filter field the catalog offers must be on the events.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&dropped(), 1, 142);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"net.drops".parse().expect("a source id"))
            .expect("net.drops is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty(), "the entry declares filter fields");
        for field in declared {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// A drop with no socket says so under the `protocol` field rather than
    /// carrying no field at all, so that a predicate can select exactly those
    /// and a predicate over a real protocol excludes them.
    #[test]
    fn a_drop_with_no_socket_is_selectable_by_the_protocol_field() {
        assert_eq!(protocol_name(None), "no-socket");
        assert_eq!(protocol_name(Some(6)), "tcp");
        assert_eq!(protocol_name(Some(17)), "udp");
        assert_eq!(protocol_name(Some(41)), "proto 41");
    }

    /// A cookie of zero is not a socket, and keying an event on it would join
    /// every anonymous drop on the machine to every other.
    #[test]
    fn a_drop_whose_socket_has_no_cookie_carries_no_socket_key() {
        let collector = DropCollector::new(Mode::Trace, None);
        let mut anonymous = dropped();
        anonymous.cookie = 0;
        assert!(collector.event(&anonymous, 1, 0, 0).keys.is_empty());
        assert_eq!(
            collector.event(&dropped(), 1, 0, 0).keys,
            vec![KeyValue::Sock { cookie: 4242 }]
        );
    }

    /// The run says the reason is a number and why, because a reader who sees
    /// `reason 4` and no name will otherwise go looking for the name in this
    /// tool rather than in the kernel's headers.
    #[test]
    fn the_run_says_the_reason_is_the_kernels_own_number() {
        let collector = DropCollector::new(Mode::Trace, None);
        let notes = collector.notes().join(" ");
        assert!(notes.contains("skb_drop_reason"), "{notes}");
    }
}
