//! `io.writeback` over the flusher's own two tracepoints.
//!
//! Deferred writeback is the half of a write that no process is waiting for.
//! `write` returns when the page is dirty; the pages reach the device later, in
//! a kworker, and the time that takes is in nobody's accounting. This source
//! measures one turn of the flusher's loop: how long it ran, and how many pages
//! it moved.
//!
//! `src/bpf/io_writeback.rs` holds the programs. What belongs here is what the
//! events carry and, more importantly, **what they do not**: the catalog lists
//! `file` among this source's keys, and these two tracepoints cannot produce
//! it. A round walks a superblock's dirty inodes and the tracepoints see the
//! work item, not the inodes, so the finest thing available is the filesystem.
//! The collector says that in its own output rather than emitting events with
//! an empty key and letting a join attribute nothing.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use crate::bpf::{Writeback, WritebackProgram};
use crate::clock::monotonic_ns;
use crate::collect::{not_built, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained. A writeback round is a rare event -- tens a
/// second on a machine doing nothing but writing -- so this is about latency to
/// the output rather than about overflow.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(50);

/// Each record is 48 bytes plus a header; a round is rare enough that this is
/// several minutes of them.
const RING_BYTES: usize = 64 * 1024;

pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match self.mode {
            Mode::Trace | Mode::Count => {}
            other => {
                return Err(format!(
                    "`{other}` mode cannot measure writeback: a round is the gap between the \
                     flusher starting and the flusher finishing, and a sample that catches one \
                     of them without the other has no duration to report"
                ))
            }
        }
        match target {
            Target::System => {}
            Target::Cgroup(_) | Target::Container(_) => {
                return Err(not_built(
                    "`io.writeback` can be scoped to a cgroup -- the catalog says so, and the \
                     flusher carries a `memcg_css` that would answer it -- but this build reads \
                     no cgroup from it; the request is written down in `docs/open-findings.md`",
                ))
            }
            other => {
                return Err(format!(
                    "a {} target names a task, and deferred writeback has none: it runs in a \
                     kworker minutes after the process that dirtied the pages returned from \
                     `write`, and attributing it to that process would be a guess. Use --system",
                    other.kind()
                ))
            }
        }
        Ok(Box::new(WritebackCollector::new(self.mode, self.filter)))
    }
}

pub struct WritebackCollector {
    id: CollectorId,
    mode: Mode,
    program: Option<WritebackProgram>,
    losses: Losses,
    dropped_open: u64,
    dropped_ring: u64,
    unpaired: u64,
    still_running: usize,
    names: BTreeMap<(u32, u32), String>,
    /// Rounds that named no filesystem, which background writeback over a whole
    /// backing device does not.
    no_super_block: u64,
    counted: u64,
    counted_total: u64,
    counted_pages: i64,
    opened_ns: u64,
    predicate: Option<FilterExpr>,
    filtered_out: u64,
    filtered_in: u64,
}

impl WritebackCollector {
    pub fn new(mode: Mode, predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("io.writeback/bpf:raw_tp"),
            mode,
            program: None,
            losses: Losses::default(),
            dropped_open: 0,
            dropped_ring: 0,
            unpaired: 0,
            still_running: 0,
            names: BTreeMap::new(),
            no_super_block: 0,
            counted: 0,
            counted_total: 0,
            counted_pages: 0,
            opened_ns: 0,
            predicate,
            filtered_out: 0,
            filtered_in: 0,
        }
    }

    fn device_name(&mut self, major: u32, minor: u32) -> String {
        if let Some(name) = self.names.get(&(major, minor)) {
            return name.clone();
        }
        self.names = super::device_names();
        self.names
            .get(&(major, minor))
            .cloned()
            .unwrap_or_else(|| format!("{major}:{minor}"))
    }

    fn event(&mut self, record: &Writeback, seq: u64) -> Event {
        let (major, minor) = (record.major(), record.minor());
        let name = self.device_name(major, minor);
        Event {
            ts_ns: record.ts_ns,
            ts_err_ns: 0,
            kind: "io.writeback".to_owned(),
            // The filesystem the round flushed is the series. A round that
            // named none is its own series rather than being folded into a
            // neighbour's.
            keys: vec![KeyValue::dev(major, minor)],
            attrs: vec![
                Attr::uint("writeback_ns", record.duration_ns),
                Attr::uint("count", 1),
                Attr::int("pages", record.pages),
                Attr::text("dev", name),
                Attr::uint("dev_major", u64::from(major)),
                Attr::uint("dev_minor", u64::from(minor)),
                // Two values that have not changed since writeback existed, so
                // naming them is safe where naming `reason` is not.
                Attr::text(
                    "sync_mode",
                    if record.sync_mode == 0 { "none" } else { "all" },
                ),
                // `enum wb_reason` is carried as the kernel's own number and
                // deliberately not translated: the enum has gained and lost
                // members between releases -- `WB_REASON_LAPTOP_TIMER` is gone
                // from 6.x -- so a name from a table built against one kernel
                // is a wrong answer that reads like a right one on another.
                // The request for a BTF enum reader is in
                // `docs/open-findings.md`.
                Attr::uint("reason", u64::from(record.reason)),
                Attr::uint("bdi_id", record.bdi_id),
                Attr::uint("cpu", u64::from(record.cpu)),
            ],
            collector_id: self.id.clone(),
            seq,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let mut undecoded = 0;
        let mut records = Vec::new();
        program.drain(|record| records.push(record), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the writeback ring was shorter than one round");
        }
        for (index, record) in records.into_iter().enumerate() {
            if record.dev == 0 {
                self.no_super_block += 1;
            }
            let event = self.event(&record, seq + index as u64);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += 1;
                    continue;
                }
                self.filtered_in += 1;
            }
            if self.mode == Mode::Count {
                self.counted += 1;
                self.counted_total += record.duration_ns;
                self.counted_pages += record.pages;
                continue;
            }
            events.push(event);
        }
    }

    fn absorb_counters(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let (open, ring) = program.dropped();
        if open > self.dropped_open {
            self.losses.dropped += open - self.dropped_open;
            self.losses.note(
                "more writeback rounds were running at once than the round map holds, so they \
                 were never timed",
            );
            self.dropped_open = open;
        }
        if ring > self.dropped_ring {
            self.losses.dropped += ring - self.dropped_ring;
            self.losses
                .note("the writeback ring was full: rounds ended faster than they were drained");
            self.dropped_ring = ring;
        }
        self.unpaired = program.unpaired();
    }
}

impl Collector for WritebackCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "writeback_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(WritebackProgram::load(RING_BYTES)?);
        self.names = super::device_names();
        self.opened_ns = monotonic_ns();
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
            self.still_running = program.still_running();
        }
        if self.mode == Mode::Count {
            trailing.push(Event {
                ts_ns: monotonic_ns(),
                ts_err_ns: 0,
                kind: "io.writeback".to_owned(),
                keys: Vec::new(),
                attrs: vec![
                    Attr::uint("writeback_ns", self.counted_total),
                    Attr::uint("rounds", self.counted),
                    Attr::int("pages", self.counted_pages),
                    Attr::uint("window_ns", monotonic_ns().saturating_sub(self.opened_ns)),
                ],
                collector_id: self.id.clone(),
                seq: u64::MAX / 2,
            });
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
        let mut notes = vec![
            // Said on every run, because it is what separates this source from
            // the one a reader probably wanted. `io.writeback` is the flusher's
            // time, not a file's.
            "a round flushes a superblock's dirty inodes and these two tracepoints see the work \
             item rather than the inodes, so the finest attribution here is the filesystem: the \
             `file` key the catalog lists for this source has no producer, and no event carries \
             one"
            .to_owned(),
        ];
        if self.mode == Mode::Count {
            notes.push(format!(
                "{} rounds, {} ns and {} pages in total: a counted run reports how long the \
                 flusher ran altogether, which is not the duration of a round",
                self.counted, self.counted_total, self.counted_pages
            ));
        }
        if self.no_super_block > 0 {
            notes.push(format!(
                "{} of the rounds named no filesystem, which writeback over a whole backing \
                 device does not, and are reported under device 0:0 with their `bdi_id`",
                self.no_super_block
            ));
        }
        if self.unpaired > 0 {
            notes.push(format!(
                "{} rounds ended that had begun before these programs attached and were not \
                 timed; nothing was dropped -- there was nothing to subtract from",
                self.unpaired
            ));
        }
        if self.still_running > 0 {
            notes.push(format!(
                "{} writeback round{} still running when the run ended, and in no event",
                self.still_running,
                if self.still_running == 1 { "" } else { "s" }
            ));
        }
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} rounds and removed {}; it ran here, over the drained \
                 records, not in the kernel",
                self.filtered_in, self.filtered_out
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Writeback {
        let mut bytes = [0u8; 48];
        bytes[0..8].copy_from_slice(&5_000_000u64.to_ne_bytes());
        bytes[8..16].copy_from_slice(&1_250_000u64.to_ne_bytes());
        bytes[16..24].copy_from_slice(&1024i64.to_ne_bytes());
        bytes[24..28].copy_from_slice(&((259u32 << 20) | 6).to_ne_bytes());
        bytes[28..32].copy_from_slice(&2u32.to_ne_bytes());
        bytes[32..36].copy_from_slice(&1u32.to_ne_bytes());
        Writeback::from_bytes(&bytes).expect("a full record")
    }

    fn collector() -> WritebackCollector {
        WritebackCollector::new(Mode::Trace, None)
    }

    /// Every filter field the catalog offers must be on the events, or a
    /// predicate over it is false everywhere and empties the series.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let event = collector().event(&record(), 0);
        let declared = &catalog
            .get(&"io.writeback".parse().expect("a source id"))
            .expect("io.writeback is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty());
        for field in declared {
            assert!(
                event.attrs.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// The key the catalog lists that this source cannot produce has to be
    /// missing *and* said to be missing. A reader who joins on `file` and gets
    /// nothing must be told why rather than left with a zero.
    #[test]
    fn the_file_key_is_absent_and_the_run_says_so() {
        let event = collector().event(&record(), 0);
        assert!(event.key(crate::request::KeyKind::File).is_none());
        let notes = collector().notes().join(" ");
        assert!(notes.contains("`file` key"), "{notes}");
        assert!(notes.contains("no producer"), "{notes}");
    }

    /// `reason` travels as the kernel's own number. The enum has gained and
    /// lost members between releases, so a name from a table is a wrong answer
    /// that reads like a right one.
    #[test]
    fn the_reason_is_a_number_and_the_sync_mode_is_a_name() {
        let event = collector().event(&record(), 0);
        assert_eq!(
            event.attr("reason").and_then(|value| value.as_f64()),
            Some(2.0)
        );
        assert_eq!(
            event.attr("sync_mode").map(ToString::to_string),
            Some("all".to_owned())
        );
    }

    /// A task target is refused with the reason, not served with a guess: the
    /// flusher runs minutes after the process that dirtied the pages returned.
    #[test]
    fn a_task_target_is_refused_because_writeback_has_no_task() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("a pid target must be refused");
        };
        assert!(error.contains("kworker"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// The modes served and the modes declared have to be the same set.
    #[test]
    fn the_modes_served_are_the_modes_the_catalog_declares() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"io.writeback".parse().expect("a source id"))
            .expect("io.writeback is in the catalog")
            .modes;
        for (mode, kind) in [
            (Mode::Trace, crate::request::ModeKind::Trace),
            (Mode::Count, crate::request::ModeKind::Count),
            (
                Mode::Poll {
                    interval: "100ms".parse().expect("a duration"),
                },
                crate::request::ModeKind::Poll,
            ),
        ] {
            let served = Box::new(Prepare { mode, filter: None })
                .attach(&Target::System)
                .is_ok();
            assert_eq!(
                served,
                declared.contains(&kind),
                "`io.writeback` and its catalog entry disagree about `{kind:?}`"
            );
        }
    }
}
