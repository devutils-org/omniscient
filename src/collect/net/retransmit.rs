//! `net.tcp.retransmit` over a raw tracepoint: one event per resent segment.
//!
//! A retransmission is the stack's own admission that something was lost. It is
//! the cheapest evidence there is that a path is bad, and it is invisible to
//! every byte counter: the bytes were already counted once.
//!
//! The program is in `src/bpf/net_retransmit.rs`. This file is the part that
//! belongs to the family: what an event looks like, what is refused and what is
//! counted as lost.
//!
//! **The connection state is what makes the count readable.** A retransmission
//! from `SYN_SENT` is a handshake nobody answered, one from `ESTABLISHED` is
//! data that did not arrive, and one from `FIN_WAIT1` is a close that did not
//! land. The kernel's own state number is on every event, with the name beside
//! it, because "twelve retransmissions" is three different faults.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use crate::bpf::{Retransmit, RetransmitProgram};
use crate::collect::{not_built, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained. Retransmissions arrive in bursts -- a link
/// that drops one segment usually drops the next -- and draining often is
/// cheap.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(20);

/// The ring the program writes into. Each record is 56 bytes plus an 8-byte
/// header, so this holds about four thousand retransmissions between drains.
const RING_BYTES: usize = 256 * 1024;

/// The TCP states, in the kernel's own frozen numbering -- the one
/// `/proc/net/tcp` prints and `ss` decodes.
const STATE_NAMES: [&str; 13] = [
    "UNKNOWN",
    "ESTABLISHED",
    "SYN_SENT",
    "SYN_RECV",
    "FIN_WAIT1",
    "FIN_WAIT2",
    "TIME_WAIT",
    "CLOSE",
    "CLOSE_WAIT",
    "LAST_ACK",
    "LISTEN",
    "CLOSING",
    "NEW_SYN_RECV",
];

fn state_name(state: u32) -> String {
    STATE_NAMES
        .get(state as usize)
        .map(|name| (*name).to_owned())
        .unwrap_or_else(|| format!("state {state}"))
}

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
                "`{}` mode cannot count retransmissions: a retransmission is an instant, and \
                 sampling instants at a frequency counts the samples rather than the losses",
                self.mode
            ));
        }
        if !matches!(target, Target::System) {
            return Err(not_built(format!(
                "a {} target cannot be served here: a retransmission is issued by a timer or \
                 from softirq context, so the task current at that moment is not the \
                 connection's owner -- attributing one to a process needs a socket-to-process \
                 map this backend does not build. Use --system.",
                target.kind()
            )));
        }
        Ok(Box::new(RetransmitCollector::new(self.mode, self.filter)))
    }
}

pub struct RetransmitCollector {
    id: CollectorId,
    /// `trace` is one event per retransmission; `count` is the single
    /// measurement of one window, totalled per series.
    mode: Mode,
    program: Option<RetransmitProgram>,
    /// Runs of the programs the kernel skipped, as last read.
    skipped: u64,
    losses: Losses,
    /// The program's counter is cumulative, so what is new is the difference.
    dropped: u64,
    /// The totals a `count` run accumulates, by what keys a series.
    counted: BTreeMap<(u64, u32, u32), Counted>,
    seen: u64,
    without_cookie: u64,
    /// Whether this kernel's tracepoint says what the send returned.
    has_error: bool,
    predicate: Option<FilterExpr>,
    filtered_in: u64,
    filtered_out: u64,
}

/// One series' retransmissions over a counted window.
struct Counted {
    retransmits: u64,
    last: Retransmit,
}

impl RetransmitCollector {
    fn new(mode: Mode, predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("net.tcp.retransmit/bpf:raw_tp"),
            mode,
            program: None,
            losses: Losses::default(),
            dropped: 0,
            skipped: 0,
            counted: BTreeMap::new(),
            seen: 0,
            without_cookie: 0,
            has_error: true,
            predicate,
            filtered_in: 0,
            filtered_out: 0,
        }
    }

    fn counting(&self) -> bool {
        self.mode == Mode::Count
    }

    fn event(&self, again: &Retransmit, retransmits: u64, seq: u64) -> Event {
        Event {
            ts_ns: again.ts_ns,
            ts_err_ns: 0,
            kind: "net.tcp.retransmit".to_owned(),
            keys: match again.cookie {
                0 => Vec::new(),
                cookie => vec![KeyValue::Sock { cookie }],
            },
            attrs: attributes(again, retransmits, self.has_error),
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
        program.drain(|again| batch.push(again), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the retransmission ring was shorter than one retransmission");
        }

        let mut index = 0;
        for again in batch {
            let candidate = self.event(&again, 1, seq + index);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &candidate) {
                    self.filtered_out += 1;
                    continue;
                }
                self.filtered_in += 1;
            }
            self.seen += 1;
            if again.cookie == 0 {
                self.without_cookie += 1;
            }
            if self.counting() {
                let held = self
                    .counted
                    .entry((again.cookie, u32::from(again.local_port()), again.state))
                    .or_insert(Counted {
                        retransmits: 0,
                        last: again,
                    });
                held.retransmits += 1;
                held.last = again;
                continue;
            }
            events.push(candidate);
            index += 1;
        }
    }

    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note(
                "the kernel skipped a run of these programs because one was already running on \
                 its cpu, so a retransmission went unseen; the count is the machine's, and an upper bound \
                 on this target's",
            );
            self.skipped = skipped;
        }
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note(
                "the retransmission ring was full: segments were resent faster than they were \
                 drained, so this run's count of them is short",
            );
            self.dropped = dropped;
        }
    }
}

/// What an event carries beside its value: both ends, the state and the error.
fn attributes(again: &Retransmit, retransmits: u64, has_error: bool) -> Vec<Attr> {
    let mut attrs = vec![
        Attr::uint("retransmits", retransmits),
        // The kernel's own state number, and the name beside it: a
        // retransmission from SYN_SENT and one from ESTABLISHED are different
        // faults with the same count.
        Attr::uint("state", u64::from(again.state)),
        Attr::text("state_name", state_name(again.state)),
        Attr::uint("family", u64::from(again.family)),
        Attr::uint("sport", u64::from(again.local_port())),
    ];
    if let Some(address) = again.local_address() {
        attrs.push(Attr::text("saddr", address));
    }
    if let Some(address) = again.remote_address() {
        attrs.push(Attr::text("daddr", address));
        attrs.push(Attr::uint("dport", u64::from(again.remote_port())));
    }
    if again.cookie != 0 {
        attrs.push(Attr::uint("cookie", again.cookie));
    }
    if again.inode != 0 {
        attrs.push(Attr::uint("inode", again.inode));
    }
    if has_error {
        // Only when the kernel says: without the argument every record's error
        // would be zero, which reads as "the resend succeeded".
        attrs.push(Attr::int("error", i64::from(again.error)));
    }
    attrs
}

impl Collector for RetransmitCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "retransmits"
    }

    fn start(&mut self) -> Result<(), String> {
        let program = RetransmitProgram::load(RING_BYTES)?;
        self.has_error = program.has_error();
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
                trailing.push(self.event(&held.last, held.retransmits, seq + index as u64));
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
        let mut notes = Vec::new();
        if !self.has_error {
            notes.push(
                "this kernel's `tcp_retransmit_skb` does not carry what the resend returned, so \
                 no event here has an `error`"
                    .to_owned(),
            );
        }
        if self.without_cookie > 0 {
            notes.push(format!(
                "{} of {} retransmissions carry no socket key: the kernel generates a socket \
                 cookie the first time something asks for one, and nothing ever asked for \
                 theirs",
                self.without_cookie, self.seen,
            ));
        }
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} retransmissions and removed {}; it ran here, over the \
                 drained records, not in the kernel",
                self.filtered_in, self.filtered_out,
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn again() -> Retransmit {
        let mut bytes = [0u8; 80];
        bytes[0..8].copy_from_slice(&1_000u64.to_ne_bytes());
        bytes[8..16].copy_from_slice(&4242u64.to_ne_bytes());
        bytes[16..24].copy_from_slice(&909u64.to_ne_bytes());
        bytes[24..28].copy_from_slice(&[127, 0, 0, 1]);
        bytes[40..44].copy_from_slice(&2u32.to_ne_bytes());
        bytes[44..48].copy_from_slice(&u32::from(443u16.to_be()).to_ne_bytes());
        bytes[48..52].copy_from_slice(&1u32.to_ne_bytes());
        bytes[56..60].copy_from_slice(&[10, 0, 0, 7]);
        bytes[72..76].copy_from_slice(&u32::from(51000u16.to_be()).to_ne_bytes());
        Retransmit::from_bytes(&bytes).expect("a full record")
    }

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
        assert!(error.contains("cannot count retransmissions"), "{error}");
    }

    /// A process target is refused with what the tracepoint cannot do, rather
    /// than with a filter that would attribute a timer's work to whoever it
    /// interrupted.
    #[test]
    fn a_process_target_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("a retransmission is issued by a timer");
        };
        assert!(error.contains("timer"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// The state is the thing that makes a retransmission count readable, and
    /// the name comes from the kernel's own frozen numbering.
    #[test]
    fn the_connection_state_is_on_the_event_by_number_and_by_name() {
        let attributes = attributes(&again(), 1, true);
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .map(|attr| attr.value.to_string())
                .unwrap_or_else(|| panic!("no `{name}` attribute"))
        };
        assert_eq!(of("state"), "1");
        assert_eq!(of("state_name"), "ESTABLISHED");
        assert_eq!(state_name(2), "SYN_SENT");
        assert_eq!(state_name(99), "state 99");
    }

    /// Both halves of the tuple are on the event, each from its own bytes, and
    /// every filter field the catalog declares is among them.
    #[test]
    fn both_ends_are_on_the_event_and_every_declared_filter_field_with_them() {
        let attributes = attributes(&again(), 1, true);
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .map(|attr| attr.value.to_string())
                .unwrap_or_else(|| panic!("no `{name}` attribute"))
        };
        assert_eq!(of("saddr"), "127.0.0.1");
        assert_eq!(of("sport"), "443");
        assert_eq!(of("daddr"), "10.0.0.7");
        assert_eq!(of("dport"), "51000");
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        for field in &catalog
            .get(&"net.tcp.retransmit".parse().expect("a source id"))
            .expect("in the catalog")
            .filter_fields
        {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// A kernel that does not report the resend's result must not have every
    /// event claim the resend succeeded.
    #[test]
    fn an_error_is_absent_when_the_kernel_does_not_report_one() {
        assert!(!attributes(&again(), 1, false)
            .iter()
            .any(|attr| attr.name == "error"));
        assert!(attributes(&again(), 1, true)
            .iter()
            .any(|attr| attr.name == "error"));
    }
}
