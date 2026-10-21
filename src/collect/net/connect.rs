//! `net.tcp.connect` over fentry: one event per handshake, from the caller's
//! side.
//!
//! `src/bpf/net_connect.rs` has the programs and where each moment is. What
//! this file decides is the event: the socket is the series, by the cookie
//! `SO_COOKIE` and `sock_diag` report, and the task is the one that called
//! `connect` -- carried from the stamp, because the handshake closes in softirq
//! context and the task current there is whoever was interrupted.

use std::time::Duration as StdDuration;

use crate::bpf::net_connect::{Connect, ConnectProgram, Outcome};
use crate::bpf::Whose;
use crate::collect::io::vfs::narrowing;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
    /// The `inet_sock_set_state` road (`bpf:raw_tp`) rather than fentry.
    pub state: bool,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot measure how long a handshake took: each one is the gap between \
                 a SYN leaving and an answer arriving, reported one at a time",
                self.mode
            ));
        }
        Ok(Box::new(ConnectCollector {
            state: self.state,
            whose: whose(target)?,
            program: None,
            losses: Losses::default(),
            predicate: self.filter,
            dropped_pending: 0,
            dropped_ring: 0,
            missed: 0,
            by_outcome: [0; 3],
            filtered_out: 0,
            still_connecting: 0,
            v6: true,
        }))
    }
}

/// Whose handshakes the program keeps. A cgroup is compared in the program,
/// against the task that called `connect`; every other target is narrowed as
/// the other fentry sources narrow it, and refused where they refuse it.
fn whose(target: &Target) -> Result<Whose, String> {
    match target {
        Target::Cgroup(_) => Whose::of(target),
        _ => narrowing("net.tcp.connect", target).map(|narrowing| {
            match (narrowing.tgid, narrowing.tid) {
                (Some(tgid), _) => Whose::Process(tgid),
                (None, Some(tid)) => Whose::Thread(tid),
                (None, None) => Whose::Any,
            }
        }),
    }
}

pub struct ConnectCollector {
    /// Over `inet_sock_set_state`: no `failed`, and the cookie as it stands.
    state: bool,
    whose: Whose,
    program: Option<ConnectProgram>,
    losses: Losses,
    predicate: Option<FilterExpr>,
    dropped_pending: u64,
    dropped_ring: u64,
    missed: u64,
    by_outcome: [u64; 3],
    filtered_out: u64,
    still_connecting: usize,
    v6: bool,
}

/// What one handshake says.
fn event(record: &Connect, seq: u64) -> Event {
    let outcome = record.outcome.map(Outcome::name).unwrap_or("unknown");
    let mut attrs = vec![
        Attr::uint("lat_ns", record.duration_ns),
        Attr::text("outcome", outcome),
        Attr::uint("family", u64::from(record.family)),
        Attr::uint("sport", u64::from(record.sport)),
        Attr::uint("dport", u64::from(record.dport)),
        Attr::uint("tid", u64::from(record.tid)),
        Attr::uint("pid", u64::from(record.tgid)),
        Attr::text("comm", record.comm.clone()),
    ];
    if let Some(address) = record.local_address() {
        attrs.push(Attr::text("saddr", address));
    }
    if let Some(address) = record.peer_address() {
        attrs.push(Attr::text("daddr", address));
    }
    if record.errno != 0 {
        attrs.push(Attr::int("errno", i64::from(record.errno)));
    }
    let task = KeyValue::task(record.tid as i32);
    let keys = if record.cookie == 0 {
        vec![task]
    } else {
        attrs.push(Attr::uint("cookie", record.cookie));
        vec![
            KeyValue::Sock {
                cookie: record.cookie,
            },
            task,
        ]
    };
    Event {
        ts_ns: record.ts_ns,
        ts_err_ns: 0,
        kind: "net.tcp.connect".to_owned(),
        keys,
        attrs,
        collector_id: CollectorId::unstamped(),
        seq,
    }
}

impl ConnectCollector {
    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let mut records = Vec::new();
        let mut undecoded = 0;
        program.drain(|record| records.push(record), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the connect ring was shorter than one handshake");
        }
        for (index, record) in records.iter().enumerate() {
            if let Some(outcome) = record.outcome {
                self.by_outcome[outcome as usize] += 1;
            }
            let event = event(record, seq + index as u64);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += 1;
                    continue;
                }
            }
            events.push(event);
        }
    }

    fn absorb_counters(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let (pending, ring) = program.dropped();
        if pending > self.dropped_pending {
            self.losses.dropped += pending - self.dropped_pending;
            self.losses.note_times(
                "more handshakes were in flight at once than the pending map holds, so they were \
                 never timed",
                pending - self.dropped_pending,
            );
            self.dropped_pending = pending;
        }
        if ring > self.dropped_ring {
            self.losses.dropped += ring - self.dropped_ring;
            self.losses
                .note("the connect ring was full: handshakes ended faster than they were drained");
            self.dropped_ring = ring;
        }
        let missed = program.missed();
        if missed > self.missed {
            self.losses.dropped += missed - self.missed;
            self.losses.note_times(
                "the kernel skipped a run of these programs because one was already running on \
                 that cpu, preempted halfway, so a handshake went untimed",
                missed - self.missed,
            );
            self.missed = missed;
        }
    }
}

impl Collector for ConnectCollector {
    fn value_attr(&self) -> &'static str {
        "lat_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        let program = if self.state {
            ConnectProgram::load_state(self.whose)?
        } else {
            ConnectProgram::load(self.whose)?
        };
        self.v6 = program.v6;
        self.program = Some(program);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_counters();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        self.absorb_counters();
        if let Some(program) = self.program.as_ref() {
            self.still_connecting = program.still_connecting();
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
        let [established, refused, failed] = self.by_outcome;
        let mut notes = vec![format!(
            "{established} established, {refused} refused or timed out, {failed} failed before a \
             SYN left: the latency of each is to its own outcome, and a refusal is not a slow \
             connection"
        )];
        if !self.v6 {
            notes.push(
                "this kernel has no tcp_v6_connect in its BTF, so IPv6 handshakes are not timed"
                    .to_owned(),
            );
        }
        if self.still_connecting > 0 {
            notes.push(format!(
                "{} handshake{} still in flight when the run ended, or abandoned by a program \
                 that closed the socket before an answer came; in no event",
                self.still_connecting,
                if self.still_connecting == 1 {
                    " was"
                } else {
                    "s were"
                }
            ));
        }
        if self.predicate.is_some() {
            notes.push(format!(
                "the filter removed {} handshake{}; it ran here, over the drained records",
                self.filtered_out,
                if self.filtered_out == 1 { "" } else { "s" }
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let mut bytes = [0u8; 104];
        bytes[56..60].copy_from_slice(&2u32.to_ne_bytes());
        bytes[16..24].copy_from_slice(&42u64.to_ne_bytes());
        let record = Connect::from_bytes(&bytes).expect("a record");
        let event = event(&record, 0);
        assert_eq!(event.keys[0].to_string(), "sock:42");
        for field in &catalog
            .get(&"net.tcp.connect".parse().expect("a source id"))
            .expect("net.tcp.connect is in the catalog")
            .filter_fields
        {
            assert!(
                event.attrs.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }
}
