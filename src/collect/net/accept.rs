//! `net.tcp.accept` over fexit: one event per connection a server accepted.
//!
//! `src/bpf/net_accept.rs` has the program. The socket is the series, by the
//! cookie `sock_diag` reports, so an accepted connection is the same key in
//! `net.tcp.lifetime`, `net.tcp.rtt` and `net.sockets`; the task is the one
//! that called `accept`, which is current at the exit of `inet_csk_accept`.

use std::time::Duration as StdDuration;

use crate::bpf::attach::fentry::Narrowing;
use crate::bpf::attach::net_connect::accept::{Accept, AcceptProgram};
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
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot see an accept: each one is an event, traced or counted",
                self.mode
            ));
        }
        Ok(Box::new(AcceptCollector {
            id: CollectorId::new("net.tcp.accept/bpf:fentry"),
            narrowing: narrowing("net.tcp.accept", target)?,
            program: None,
            losses: Losses::default(),
            predicate: self.filter,
            dropped_ring: 0,
            missed: 0,
            accepted: 0,
            filtered_out: 0,
        }))
    }
}

pub struct AcceptCollector {
    id: CollectorId,
    narrowing: Narrowing,
    program: Option<AcceptProgram>,
    losses: Losses,
    predicate: Option<FilterExpr>,
    dropped_ring: u64,
    missed: u64,
    accepted: u64,
    filtered_out: u64,
}

/// What one accept says.
fn event(id: &CollectorId, record: &Accept, seq: u64) -> Event {
    let mut attrs = vec![
        Attr::uint("accepts", 1),
        Attr::uint("backlog", u64::from(record.backlog)),
        Attr::uint("max_backlog", u64::from(record.max_backlog)),
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
        kind: "net.tcp.accept".to_owned(),
        keys,
        attrs,
        collector_id: id.clone(),
        seq,
    }
}

impl AcceptCollector {
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
                .note("a record in the accept ring was shorter than one accept");
        }
        for (index, record) in records.iter().enumerate() {
            self.accepted += 1;
            let event = event(&self.id, record, seq + index as u64);
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
        let ring = program.dropped();
        if ring > self.dropped_ring {
            self.losses.dropped += ring - self.dropped_ring;
            self.losses
                .note("the accept ring was full: accepts came faster than they were drained");
            self.dropped_ring = ring;
        }
        let missed = program.missed();
        if missed > self.missed {
            self.losses.dropped += missed - self.missed;
            self.losses.note(
                "the kernel skipped a run of the accept program because one was already running \
                 on that cpu, so an accept went unseen",
            );
            self.missed = missed;
        }
    }
}

impl Collector for AcceptCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "accepts"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(AcceptProgram::load(self.narrowing)?);
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
        let mut notes = vec![format!(
            "{} accepts; `backlog` is the listener's queue after each took one off it, \
             `max_backlog` its bound",
            self.accepted
        )];
        if self.predicate.is_some() {
            notes.push(format!(
                "the filter removed {} accept{}; it ran here, over the drained records",
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
        let mut bytes = [0u8; 96];
        bytes[48..52].copy_from_slice(&2u32.to_ne_bytes());
        bytes[8..16].copy_from_slice(&42u64.to_ne_bytes());
        let record = Accept::from_bytes(&bytes).expect("a record");
        let event = event(&CollectorId::new("t"), &record, 0);
        assert_eq!(event.keys[0].to_string(), "sock:42");
        for field in &catalog
            .get(&"net.tcp.accept".parse().expect("a source id"))
            .expect("net.tcp.accept is in the catalog")
            .filter_fields
        {
            assert!(
                event.attrs.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    #[test]
    fn a_polled_mode_is_refused_with_the_reason() {
        let Err(error) = Box::new(Prepare {
            mode: Mode::Snapshot,
            filter: None,
        })
        .attach(&Target::System) else {
            panic!("an accept is an event");
        };
        assert!(error.contains("traced or counted"), "{error}");
    }
}
