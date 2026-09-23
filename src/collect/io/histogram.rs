//! The collector half of a kernel-built latency histogram.
//!
//! `io.syscall`, `io.fs` and `net.socket` are one program shape --
//! `bpf::attach::fentry::HistogramProgram`, a timed pair counting into rows of
//! `(task, op, object, log2 bucket)` -- and so one collector shape: drain the
//! rows on a tick, turn each into one event that stands for as many calls as it
//! counted, and account for what the programs could not record. What differs
//! per source is what a row's `object`, `dev` and `aux` mean, and that is the
//! one thing each source hands in.
//!
//! It lives in `io` because two of its three users do, and `net` reaches it as
//! a sibling. The alternative was a second copy in `net`, which is the shape
//! this project has spent three waves removing from `src/bpf/`.
//!
//! **An event is a row, and a row is not one call.** Each event carries
//! `count` and `emit::WEIGHT_ATTR`, and `lat_ns` is the low bound of the row's
//! bucket, so a histogram the emit layer builds from these events puts every
//! call back in the bucket the kernel put it in. The exact total travels as
//! `total_ns`, because a log2 bucket is a factor of two wide and a mean built
//! from its bound would be wrong by up to that.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use crate::bpf::attach::fentry::{Drained, Row};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::Mode;

/// How often the rows are drained.
///
/// A drain costs one syscall per row that saw a call in the interval, not per
/// call. A tenth of a second keeps the map far from its capacity on anything
/// short of the loss scenario, and keeps the drain's own reads -- which a
/// machine-wide `io.syscall` counts -- to a few per second.
pub const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// The loaded programs, whichever source they belong to.
pub trait Rows {
    /// The rows of the interval before the last one; with `last`, of both.
    fn drain(&self, last: bool) -> Result<Drained, String>;
    /// Stamps left at the end, as `(in flight, orphaned)`.
    fn still_pending(&self) -> (usize, usize);
    /// `(pending, rows, orphaned)`: calls lost because a thread could not be
    /// stamped, because their row could not be created, and because the
    /// kernel skipped their exit half.
    fn dropped(&self) -> (u64, u64, u64);
    /// Runs of the programs the kernel skipped, which lost the call outright.
    fn missed(&self) -> u64;
}

/// What one source makes of a row: the keys its event carries, and the
/// attributes beyond the ones every histogram event has.
pub type Describe = Box<dyn FnMut(&Row) -> (Vec<KeyValue>, Vec<Attr>)>;

/// Loads the programs once the target is known and the run is starting.
pub type Load = Box<dyn FnOnce() -> Result<Box<dyn Rows>, String>>;

/// What a source tells the shared collector about itself.
pub struct Source {
    /// The catalog id, which is also each event's kind.
    pub kind: &'static str,
    pub collector: &'static str,
    /// What one call is, for a note a reader has to understand: "a read or a
    /// write", "a send or a receive".
    pub call: &'static str,
    pub load: Load,
    pub describe: Describe,
    /// Notes the source knows before the run starts: what its functions do not
    /// cover, which is part of what the number means.
    pub notes: Vec<String>,
}

/// A counted run's rows, by the whole kernel key.
type Key = (u32, u32, u32, u32, u64, u32, u32);

pub struct HistogramCollector {
    id: CollectorId,
    kind: &'static str,
    call: &'static str,
    mode: Mode,
    load: Option<Load>,
    describe: Describe,
    program: Option<Box<dyn Rows>>,
    losses: Losses,
    notes: Vec<String>,
    predicate: Option<FilterExpr>,
    dropped_pending: u64,
    dropped_rows: u64,
    orphaned: u64,
    missed: u64,
    window_opened_ns: u64,
    /// Calls this run bucketed, which is what the event count is not.
    calls: u64,
    /// Rows those calls were reported as, which is the event count.
    rows: u64,
    filtered_out: u64,
    totals: BTreeMap<Key, Row>,
}

impl HistogramCollector {
    pub fn new(source: Source, mode: Mode, predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new(source.collector),
            kind: source.kind,
            call: source.call,
            mode,
            load: Some(source.load),
            describe: source.describe,
            program: None,
            losses: Losses::default(),
            notes: source.notes,
            predicate,
            dropped_pending: 0,
            dropped_rows: 0,
            orphaned: 0,
            missed: 0,
            window_opened_ns: 0,
            calls: 0,
            rows: 0,
            filtered_out: 0,
            totals: BTreeMap::new(),
        }
    }

    fn event(&mut self, row: &Row, window_ns: u64, ts_ns: u64, seq: u64) -> Event {
        let (keys, mut attrs) = (self.describe)(row);
        attrs.splice(0..0, common_attributes(row, window_ns));
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: self.kind.to_owned(),
            keys,
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64, last: bool) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let Drained {
            rows,
            losses: drain_losses,
            opened_ns,
            closed_ns,
        } = match program.drain(last) {
            Ok(drained) => drained,
            Err(reason) => {
                // The rows are still in the map and arrive with the next drain;
                // what is lost is which window they belonged to.
                self.losses.failed_reads += 1;
                self.losses.note(reason);
                return;
            }
        };
        // The interval the rows were counted over, which the program knows:
        // they were written one drain ago, not since the last one.
        let window_ns = closed_ns.saturating_sub(opened_ns);
        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("a histogram row was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the histogram held more rows than one drain can walk");
        }
        for (index, row) in rows.into_iter().enumerate() {
            let event = self.event(&row, window_ns, closed_ns, seq + index as u64);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += row.count;
                    continue;
                }
            }
            self.calls += row.count;
            if self.mode == Mode::Count {
                let key = (
                    row.tid, row.tgid, row.op, row.bucket, row.object, row.dev, row.aux,
                );
                match self.totals.get_mut(&key) {
                    Some(total) => {
                        total.count += row.count;
                        total.total_ns += row.total_ns;
                        total.bytes += row.bytes;
                        total.errors += row.errors;
                    }
                    None => {
                        self.totals.insert(key, row);
                    }
                }
                continue;
            }
            self.rows += 1;
            events.push(event);
        }
    }

    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let (pending, rows, orphaned) = program.dropped();
        if pending > self.dropped_pending {
            self.losses.dropped += pending - self.dropped_pending;
            self.losses.note(format!(
                "more threads were inside {} at once than the pending map holds, so their \
                 calls were never timed",
                self.call
            ));
            self.dropped_pending = pending;
        }
        if rows > self.dropped_rows {
            self.losses.dropped += rows - self.dropped_rows;
            self.losses.note(
                "more distinct rows appeared in one drain interval than the histogram holds, \
                 so those calls are in no bucket and every count here is a lower bound",
            );
            self.dropped_rows = rows;
        }
        if orphaned > self.orphaned {
            self.losses.dropped += orphaned - self.orphaned;
            self.losses.note(ORPHANED);
            self.orphaned = orphaned;
        }
        let missed = program.missed();
        if missed > self.missed {
            self.losses.dropped += missed - self.missed;
            self.losses.note(format!(
                "the kernel skipped a run of these programs because one was already running on \
                 that cpu, preempted halfway, so {} was never timed and every count here is a \
                 lower bound",
                self.call
            ));
            self.missed = missed;
        }
    }
}

/// Why a call whose exit half never ran is a loss, in the words a reader sees.
pub const ORPHANED: &str = "the exit half of a call never ran: another tracer attached to or \
    detached from the same kernel function while the call was in flight, and the kernel \
    rebuilds a function's trampoline so that calls in flight skip their exit programs -- the \
    longest calls are the likeliest to be lost this way";

/// What every event of a kernel-built histogram carries, whatever the source.
fn common_attributes(row: &Row, window_ns: u64) -> Vec<Attr> {
    vec![
        // The low bound of the bucket: the one value that the emit layer's own
        // log2 bucketing maps back to exactly this bucket.
        Attr::uint("lat_ns", row.bucket_low_ns()),
        Attr::uint("count", row.count),
        Attr::uint(crate::emit::WEIGHT_ATTR, row.count),
        Attr::uint("total_ns", row.total_ns),
        Attr::uint("bucket", u64::from(row.bucket)),
        // Summed over the calls, and only over those that returned a count.
        Attr::uint("bytes", row.bytes),
        Attr::uint("errors", row.errors),
        // The interval the row was drained over. Not a denominator: a call is
        // counted where it ended, so one longer than the tick reports time the
        // window does not contain.
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", u64::from(row.tid)),
        Attr::uint("pid", u64::from(row.tgid)),
        Attr::text("comm", row.comm.clone()),
    ]
}

impl Collector for HistogramCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "lat_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        let load = self
            .load
            .take()
            .ok_or_else(|| "the histogram was started twice".to_owned())?;
        self.program = Some(load()?);
        self.window_opened_ns = monotonic_ns();
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq, false);
        self.absorb_drops();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2, true);
        self.absorb_drops();
        if let Some(program) = self.program.as_ref() {
            let (open, orphaned) = program.still_pending();
            if orphaned > 0 {
                self.losses.dropped += orphaned as u64;
                self.losses.note(ORPHANED);
            }
            if open > 0 {
                self.notes.push(format!(
                    "{open} thread{} inside {} when the run ended; that call has no end yet and \
                     is in no bucket",
                    if open == 1 { " was" } else { "s were" },
                    self.call
                ));
            }
        }
        if self.predicate.is_some() {
            self.notes.push(format!(
                "the filter ran here, over the drained rows, and removed {} call{}: the kernel \
                 still timed them, so the cost is the unfiltered one",
                self.filtered_out,
                if self.filtered_out == 1 { "" } else { "s" }
            ));
        }
        if self.mode == Mode::Count {
            let window_ns = monotonic_ns().saturating_sub(self.window_opened_ns);
            let closed_ns = monotonic_ns();
            let totals = std::mem::take(&mut self.totals);
            for (index, row) in totals.values().enumerate() {
                let event = self.event(row, window_ns, closed_ns, index as u64);
                self.rows += 1;
                trailing.push(event);
            }
        }
        self.notes.push(format!(
            "{} call{} bucketed in the kernel, reported as {} row{}",
            self.calls,
            if self.calls == 1 { "" } else { "s" },
            self.rows,
            if self.rows == 1 { "" } else { "s" },
        ));
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        self.notes.clone()
    }
}
