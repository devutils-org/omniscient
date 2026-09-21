//! `net.tcp.lifetime` over a raw tracepoint: one event per closed connection.
//!
//! The measurement is the gap between a socket entering `TCP_ESTABLISHED` and
//! the same socket entering `TCP_CLOSE`, with what crossed it: payload bytes
//! each way and segments each way, read off the socket at the moment it closes.
//! The program is in `src/bpf/net_lifetime.rs`.
//!
//! **A connection has no process**, and that is the family's own rule rather
//! than a shortcoming here. Both ends of a TCP connection reach
//! `TCP_ESTABLISHED` in softirq context -- the passive end when the final ACK
//! arrives, the active end when the SYN-ACK does -- so the task that is current
//! at that moment is whatever the interrupt happened to land on. The task this
//! collector reports is the one current at the *close*, which is the owning
//! process when an application closed the connection and is not when the kernel
//! closed it from a timer. That is stated on the event and in the run rather
//! than left to be assumed, and it is why a process target is refused here.
//!
//! **The cookie is the join key.** `net.tcp.rtt` keys its series on the same
//! number, read through `sock_diag`, so a `sock:cookie` join between the two
//! needs no translation. The kernel generates a cookie the first time something
//! asks for one, so a connection that opened and closed without anything ever
//! dumping it has none: those events carry the socket's inode and no socket
//! key, because a key of zero would join every anonymous connection to every
//! other.

use std::time::Duration as StdDuration;

use crate::bpf::{TcpLifetime, TcpLifetimeProgram};
use crate::collect::{not_built, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained. Connections close in bursts -- a load
/// generator finishing, a pool being recycled -- and draining often is cheap.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(20);

/// The ring the program writes into. Each record is 104 bytes plus an 8-byte
/// header, so this holds about two thousand connections between drains.
const RING_BYTES: usize = 256 * 1024;

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
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot measure a connection's lifetime: it is the gap between two \
                 state changes, and a sample that catches one of them without the other has no \
                 duration to report",
                self.mode
            ));
        }
        if !matches!(target, Target::System) {
            return Err(not_built(format!(
                "a {} target cannot be served here: a connection becomes established in softirq \
                 context at both ends, so the task current at that moment is whichever one the \
                 interrupt landed on and is not the connection's owner -- attributing a \
                 connection to a process needs a socket-to-process map this backend does not \
                 build. Use --system.",
                target.kind()
            )));
        }
        Ok(Box::new(LifetimeCollector::new(self.filter)))
    }
}

pub struct LifetimeCollector {
    id: CollectorId,
    program: Option<TcpLifetimeProgram>,
    losses: Losses,
    notes: Vec<String>,
    /// The program's counters are cumulative, so what is new is the difference.
    dropped_open: u64,
    dropped_ring: u64,
    /// Connections seen, and how many of them the kernel had never given a
    /// cookie -- which is what decides whether they can be joined at all.
    closed: u64,
    without_cookie: u64,
    without_inode: u64,
    /// Connections still established when the run ended.
    still_open: usize,
    predicate: Option<FilterExpr>,
    filtered_in: u64,
    filtered_out: u64,
}

impl LifetimeCollector {
    fn new(predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("net.tcp.lifetime/bpf:raw_tp"),
            program: None,
            losses: Losses::default(),
            notes: Vec::new(),
            dropped_open: 0,
            dropped_ring: 0,
            closed: 0,
            without_cookie: 0,
            without_inode: 0,
            still_open: 0,
            predicate,
            filtered_in: 0,
            filtered_out: 0,
        }
    }

    fn event(&self, closed: &TcpLifetime, seq: u64) -> Event {
        Event {
            ts_ns: closed.ts_ns,
            ts_err_ns: 0,
            kind: "net.tcp.lifetime".to_owned(),
            keys: match closed.cookie {
                0 => Vec::new(),
                cookie => vec![KeyValue::Sock { cookie }],
            },
            attrs: attributes(closed),
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
        program.drain(|closed| batch.push(closed), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the connection ring was shorter than one connection");
        }

        let mut index = 0;
        for closed in batch {
            let candidate = self.event(&closed, seq + index);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &candidate) {
                    self.filtered_out += 1;
                    continue;
                }
                self.filtered_in += 1;
            }
            self.closed += 1;
            if closed.cookie == 0 {
                self.without_cookie += 1;
            }
            if closed.inode == 0 {
                self.without_inode += 1;
            }
            events.push(candidate);
            index += 1;
        }
    }

    /// Reads the program's own counters, which are the only place a connection
    /// it could not record is counted.
    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let (open, ring) = program.dropped();
        if open > self.dropped_open {
            self.losses.dropped += open - self.dropped_open;
            self.losses.note(
                "more connections were established at once than the open map holds, so their \
                 lifetimes were never timed",
            );
            self.dropped_open = open;
        }
        if ring > self.dropped_ring {
            self.losses.dropped += ring - self.dropped_ring;
            self.losses.note(
                "the connection ring was full: connections closed faster than they were drained",
            );
            self.dropped_ring = ring;
        }
    }
}

/// What an event carries beside its value.
///
/// The catalog declares `comm`, `daddr`, `dport`, `saddr`, `sport` and `family`
/// as filter fields. Three of them are here; `daddr` and `dport` are the peer's
/// and are not reachable through named BTF members, which
/// `src/bpf/net_lifetime.rs` explains and `docs/families/net.md` records. A
/// predicate over one of those is false everywhere until the shared layer can
/// walk an anonymous union, which is worse than a refusal and is why it is
/// written down in both places.
fn attributes(closed: &TcpLifetime) -> Vec<Attr> {
    let mut attrs = vec![
        Attr::uint("duration_ns", closed.duration_ns),
        // Payload bytes, which is what `bytes_sent` counts in the kernel: not
        // segments, not wire bytes, and not retransmissions.
        Attr::uint("bytes_sent", closed.bytes_sent),
        Attr::uint("bytes_received", closed.bytes_received),
        Attr::uint("segs_out", u64::from(closed.segs_out)),
        Attr::uint("segs_in", u64::from(closed.segs_in)),
        Attr::uint("family", u64::from(closed.family)),
        Attr::uint("sport", u64::from(closed.local_port())),
        // The task current at the close. Named `comm` because that is the
        // filter field the catalog offers, and qualified in the run's notes
        // because it is not the connection's owner.
        Attr::text("comm", closed.comm()),
        Attr::uint("pid", u64::from(closed.pid)),
        Attr::uint("tid", u64::from(closed.tid)),
    ];
    if closed.inode != 0 {
        // Zero is not an inode. A socket accepted from a listen queue has none
        // until `accept()` gives it a file, which is after it is established,
        // so the passive end of a connection usually has no inode here.
        attrs.push(Attr::uint("inode", closed.inode));
    }
    if let Some(address) = closed.local_address() {
        attrs.push(Attr::text("saddr", address));
    }
    if closed.cookie != 0 {
        attrs.push(Attr::uint("cookie", closed.cookie));
    }
    attrs
}

impl Collector for LifetimeCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "duration_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(TcpLifetimeProgram::load(RING_BYTES)?);
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
        if let Some(program) = self.program.as_ref() {
            self.still_open = program.still_open();
        }
        self.program = None;
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
        notes.push(
            "the address and port on each event are the local end of the connection; the peer's \
             live in anonymous unions of `sock_common` that this build cannot resolve by name, \
             so a filter on `daddr` or `dport` matches nothing"
                .to_owned(),
        );
        notes.push(
            "`comm` and `pid` are the task current when the connection reached TCP_CLOSE, which \
             is the closing process when an application closed it and is not the connection's \
             owner when the kernel closed it from a timer or on a reset"
                .to_owned(),
        );
        if self.without_inode > 0 {
            notes.push(format!(
                "{} of {} connections carry no inode: a socket taken off a listen queue has \
                 no file, and therefore no inode, until `accept()` gives it one -- which is \
                 after it is established and is where this reads",
                self.without_inode, self.closed,
            ));
        }
        if self.without_cookie > 0 {
            notes.push(format!(
                "{} of {} connections carry no socket key: the kernel generates a socket cookie \
                 the first time something asks for one, and nothing ever asked for theirs, so \
                 they cannot be joined to `net.tcp.rtt` -- their inode is on the event instead",
                self.without_cookie, self.closed,
            ));
        }
        if self.still_open > 0 {
            // Not a loss. A connection open when the run ended has a real
            // lifetime with no end yet, so it is in no event.
            notes.push(format!(
                "{} connection{} still established when the run ended; their lifetimes have no \
                 end yet and are in no event",
                self.still_open,
                if self.still_open == 1 {
                    " was"
                } else {
                    "s were"
                },
            ));
        }
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} connections and removed {}; it ran here, over the drained \
                 records, not in the kernel",
                self.filtered_in, self.filtered_out,
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed() -> TcpLifetime {
        let mut bytes = [0u8; 104];
        bytes[0..8].copy_from_slice(&1_000_000u64.to_ne_bytes());
        bytes[8..16].copy_from_slice(&300_000_000u64.to_ne_bytes());
        bytes[16..24].copy_from_slice(&4242u64.to_ne_bytes());
        bytes[24..32].copy_from_slice(&909u64.to_ne_bytes());
        bytes[32..40].copy_from_slice(&4096u64.to_ne_bytes());
        bytes[40..48].copy_from_slice(&8192u64.to_ne_bytes());
        bytes[48..52].copy_from_slice(&7u32.to_ne_bytes());
        bytes[52..56].copy_from_slice(&9u32.to_ne_bytes());
        bytes[56..60].copy_from_slice(&[127, 0, 0, 1]);
        bytes[72..76].copy_from_slice(&2u32.to_ne_bytes());
        bytes[76..80].copy_from_slice(&u32::from(8080u16.to_be()).to_ne_bytes());
        bytes[80..84].copy_from_slice(&11u32.to_ne_bytes());
        bytes[84..88].copy_from_slice(&12u32.to_ne_bytes());
        bytes[88..92].copy_from_slice(b"curl");
        TcpLifetime::from_bytes(&bytes).expect("a full record")
    }

    /// A mode that cannot mean anything here is refused with the reason.
    #[test]
    fn a_sampled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Count,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a counted mode must be refused");
        };
        assert!(error.contains("gap between two"), "{error}");
    }

    /// A process target is refused, and the refusal says what the tracepoint
    /// cannot do rather than pretending the filter would be meaningful.
    #[test]
    fn a_process_target_is_refused_because_a_connection_has_no_process() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("a connection is established in softirq context");
        };
        assert!(error.contains("softirq context"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// The three filter fields this backend can serve are on the event, and the
    /// run says which two it cannot -- because a predicate over a field no
    /// event carries is false everywhere and empties the series silently.
    #[test]
    fn the_filter_fields_it_can_serve_are_on_the_event_and_the_rest_are_named() {
        let attributes = attributes(&closed());
        for field in ["comm", "saddr", "sport", "family"] {
            assert!(
                attributes.iter().any(|attr| attr.name == field),
                "no event carries `{field}`"
            );
        }
        let notes = LifetimeCollector::new(None).notes().join(" ");
        for field in ["daddr", "dport"] {
            assert!(
                !attributes.iter().any(|attr| attr.name == field),
                "`{field}` is not reachable and must not be invented"
            );
            assert!(
                notes.contains(field),
                "the run must say that a filter on `{field}` matches nothing: {notes}"
            );
        }
    }

    /// The record's numbers reach the event unchanged, and the port is in the
    /// order a person reads.
    #[test]
    fn the_events_numbers_are_the_records_numbers() {
        let attributes = attributes(&closed());
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .map(|attr| attr.value.clone())
                .unwrap_or_else(|| panic!("no `{name}` attribute"))
        };
        assert_eq!(of("duration_ns").as_f64(), Some(300_000_000.0));
        assert_eq!(of("bytes_sent").as_f64(), Some(4096.0));
        assert_eq!(of("bytes_received").as_f64(), Some(8192.0));
        assert_eq!(of("sport").as_f64(), Some(8080.0));
        assert_eq!(of("saddr").to_string(), "127.0.0.1");
        assert_eq!(of("comm").to_string(), "curl");
    }

    /// A cookie of zero is not a socket: the kernel assigns one lazily, so a
    /// connection nothing ever dumped has none, and a key of zero would join
    /// every such connection to every other.
    #[test]
    fn a_connection_the_kernel_never_named_carries_no_socket_key() {
        let collector = LifetimeCollector::new(None);
        let mut anonymous = closed();
        anonymous.cookie = 0;
        assert!(collector.event(&anonymous, 0).keys.is_empty());
        assert_eq!(
            collector.event(&closed(), 0).keys,
            vec![KeyValue::Sock { cookie: 4242 }]
        );
        assert!(
            !attributes(&anonymous)
                .iter()
                .any(|attr| attr.name == "cookie"),
            "a cookie of zero must not be reported as a cookie"
        );
    }

    /// A connection still open at the end of the run is a fact about the
    /// window, not a loss: its lifetime has no end yet.
    #[test]
    fn a_connection_still_open_at_the_end_is_a_note_and_not_a_loss() {
        let mut collector = LifetimeCollector::new(None);
        collector.still_open = 3;
        let notes = collector.notes().join(" ");
        assert!(
            notes.contains("3 connections were still established"),
            "{notes}"
        );
        assert!(collector.losses().is_clean());
    }
}
