//! `io.pagecache` and `io.readahead` over the page cache's own tracepoints,
//! counted in the kernel.
//!
//! One program pair, two readings. `src/bpf/io_pagecache.rs` counts, per task
//! and file, the pages reads looked up and the pages the cache had to fill, and
//! tells a page a write created at the end of a file from one a read filled
//! inside it. What the two sources make of those counts is different:
//!
//! - **`io.pagecache`** answers "did the cache have what was read". Its value
//!   is `misses`: pages the cache had to fill for this task and file. Beside it
//!   travel `lookups`, the pages the reads looked up, so a reader has the other
//!   half of the ratio on the same event. A page read ahead is a miss here --
//!   the cache did not have it -- and the lookup that later finds it is not.
//! - **`io.readahead`** answers "did the cache fill ahead of what was asked".
//!   Its value is `ahead`: pages filled minus pages looked up, signed. Positive
//!   is readahead running in front of the reader, zero is a cache filled
//!   exactly as fast as it was read, and negative is a reader served from pages
//!   that were already there.
//!
//! **Why a difference and not a ratio.** A ratio of two window counts is not
//! additive: a window in which readahead filled a thousand pages and the next
//! in which the reader consumed them have ratios whose mean means nothing. A
//! difference is: the sum of every window's `ahead` is the run's, so `rate`
//! over a series is exact however the drains fell. It is signed for the same
//! reason, and a histogram has no bucket for a negative number, which is why
//! the catalog offers `io.readahead` no `hist`.
//!
//! **What neither sees.** A page brought in by a fault on a mapping is filled
//! here and looked up nowhere -- `filemap_fault` does not pass through
//! `filemap_get_pages` -- so a program that reads its files through `mmap`
//! shows misses with no lookups. And a whole-page overwrite of a page that was
//! not cached is a fill with no read behind it. Both are named in the run's
//! notes, and in the catalog's caveats.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use super::vfs::{narrowing, Mounts};
use crate::bpf::attach::io_vfs;
use crate::bpf::{PageCacheNarrowing, PageCacheProgram, PageCacheRow};
use crate::clock::monotonic_ns;
use crate::collect::{cannot_mean_anything, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the rows are drained. One syscall per row that saw a page, not
/// per page, and a tenth of a second keeps even a machine-wide `find | xargs
/// cat` far below the map's capacity.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Which question a collector answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// `io.pagecache`: pages missed, against pages looked up.
    Misses,
    /// `io.readahead`: pages filled ahead of what was looked up.
    Ahead,
}

impl Reading {
    fn kind(self) -> &'static str {
        match self {
            Reading::Misses => "io.pagecache",
            Reading::Ahead => "io.readahead",
        }
    }

    fn collector(self) -> &'static str {
        match self {
            Reading::Misses => "io.pagecache/bpf:raw_tp",
            Reading::Ahead => "io.readahead/bpf:raw_tp",
        }
    }

    fn value_attr(self) -> &'static str {
        match self {
            Reading::Misses => "misses",
            Reading::Ahead => "ahead",
        }
    }
}

pub struct Prepare {
    pub reading: Reading,
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let source = self.reading.kind();
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(cannot_mean_anything(format!(
                "`{}` mode cannot count pages entering the cache: a fill is an instant, and \
                 sampling instants at a frequency counts how often the sampler looked",
                self.mode
            )));
        }
        let narrowed = narrowing(source, target)?;
        Ok(Box::new(PageCacheCollector::new(
            self.reading,
            self.mode,
            PageCacheNarrowing {
                tgid: narrowed.tgid,
                tid: narrowed.tid,
            },
            self.filter,
        )))
    }
}

pub struct PageCacheCollector {
    id: CollectorId,
    reading: Reading,
    mode: Mode,
    narrowing: PageCacheNarrowing,
    program: Option<PageCacheProgram>,
    mounts: Mounts,
    losses: Losses,
    /// The program's counters are cumulative, so what is new is the difference.
    dropped: u64,
    skipped: u64,
    unnamed: u64,
    untracked: u64,
    continued: u64,
    window_opened_ns: u64,
    run_opened_ns: u64,
    /// A `count` run's totals, by the row's whole key.
    counted: BTreeMap<(u32, u32, u32, u64), PageCacheRow>,
    rows: u64,
    /// Every page this run saw, before the filter: what the notes report.
    totals: Totals,
    predicate: Option<FilterExpr>,
    filtered_out: u64,
}

/// The run's pages, summed over every row.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Totals {
    asked: u64,
    lookups: u64,
    filled: u64,
    created: u64,
}

impl PageCacheCollector {
    fn new(
        reading: Reading,
        mode: Mode,
        narrowing: PageCacheNarrowing,
        predicate: Option<FilterExpr>,
    ) -> Self {
        Self {
            id: CollectorId::new(reading.collector()),
            reading,
            mode,
            narrowing,
            program: None,
            mounts: Mounts::read(),
            losses: Losses::default(),
            dropped: 0,
            skipped: 0,
            unnamed: 0,
            untracked: 0,
            continued: 0,
            window_opened_ns: 0,
            run_opened_ns: 0,
            counted: BTreeMap::new(),
            rows: 0,
            totals: Totals::default(),
            predicate,
            filtered_out: 0,
        }
    }

    fn event(&mut self, row: &PageCacheRow, ts_ns: u64, window_ns: u64, seq: u64) -> Event {
        let dev = io_vfs::major_minor(row.dev);
        let (fs, _, device) = self.mounts.lookup(dev);
        let file = KeyValue::File {
            dev: io_vfs::stat_dev(row.dev),
            ino: row.ino,
        };
        let mut attrs = match self.reading {
            Reading::Misses => vec![Attr::uint("misses", row.filled)],
            Reading::Ahead => vec![Attr::int("ahead", ahead(row))],
        };
        attrs.extend([
            // Both halves on every event, whichever the value is, so either
            // source's output holds the other's numbers too.
            Attr::uint("lookups", row.asked),
            Attr::uint("lookup_calls", row.lookups),
            Attr::uint("filled", row.filled),
            Attr::uint("folios", row.folios),
            Attr::uint("created", row.created),
            Attr::uint("window_ns", window_ns),
            Attr::uint("tid", u64::from(row.tid)),
            Attr::uint("pid", u64::from(row.tgid)),
            Attr::text("comm", row.comm.clone()),
            Attr::uint("ino", row.ino),
            Attr::text("dev", device),
            Attr::uint("sb_dev", io_vfs::stat_dev(row.dev)),
            Attr::text("fs", fs),
        ]);
        // The devices a fill read from, as `io.bytes` keys them.
        if let Some(under) = self.mounts.block_devices(dev) {
            attrs.push(Attr::text("blk_devs", under));
        }
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: self.reading.kind().to_owned(),
            // The file first: both questions are about a file, and the same
            // file read by two processes is one series.
            keys: vec![file, KeyValue::task(row.tid as i32)],
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.window_opened_ns);
        let (rows, drain_losses) = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                // The rows are still in the map and arrive with the next drain;
                // what is lost is which window they belonged to.
                self.losses.failed_reads += 1;
                self.losses.note(reason);
                return;
            }
        };
        self.window_opened_ns = closed_ns;
        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("a page cache row was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the page cache rows were more than one drain can walk");
        }
        let mut index = 0;
        for row in rows {
            // Stamped where the window's first event of this key happened, not
            // at the drain: finding 44.
            let event = self.event(&row, row.first_ns, window_ns, seq + index);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += 1;
                    continue;
                }
            }
            self.totals.asked += row.asked;
            self.totals.lookups += row.lookups;
            self.totals.filled += row.filled;
            self.totals.created += row.created;
            if self.mode == Mode::Count {
                self.counted
                    .entry((row.tid, row.tgid, row.dev, row.ino))
                    .and_modify(|held| held.absorb(&row))
                    .or_insert(row);
                continue;
            }
            self.rows += 1;
            index += 1;
            events.push(event);
        }
    }

    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note(format!(
                "more distinct (task, file) rows appeared in one interval than the {} the map \
                 holds, so those lookups and fills were never counted and every total here is \
                 a lower bound",
                program.capacity()
            ));
            self.dropped = dropped;
        }
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note(
                "the kernel skipped a run of these programs because one was already running on \
                 that cpu, so a lookup or a fill was never counted and every total here is a \
                 lower bound",
            );
            self.skipped = skipped;
        }
        self.unnamed = program.unnamed();
        self.continued = program.continued();
        let untracked = program.untracked();
        if untracked > self.untracked {
            self.losses.note(
                "the kernel could not allocate a reading thread's slot for its previous lookup, \
                 so a read of that thread larger than one batch of folios may have had each \
                 batch counted as a lookup of its own: `lookups` is an upper bound",
            );
            self.untracked = untracked;
        }
    }
}

/// Pages filled ahead of what was looked up, which is negative when reads were
/// served from pages already there.
fn ahead(row: &PageCacheRow) -> i64 {
    row.filled as i64 - row.asked as i64
}

impl Collector for PageCacheCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        self.reading.value_attr()
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(PageCacheProgram::load(self.narrowing)?);
        self.run_opened_ns = monotonic_ns();
        self.window_opened_ns = self.run_opened_ns;
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
        if self.mode == Mode::Count {
            let window_ns = monotonic_ns().saturating_sub(self.run_opened_ns);
            let counted = std::mem::take(&mut self.counted);
            self.rows += counted.len() as u64;
            for (index, row) in counted.into_values().enumerate() {
                let event = self.event(&row, row.first_ns, window_ns, u64::MAX / 2 + index as u64);
                trailing.push(event);
            }
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
        let mut notes = notes(
            self.totals,
            self.rows,
            self.unnamed,
            self.filtered_out,
            self.predicate.is_some(),
        );
        if self.continued > 0 {
            notes.push(format!(
                "{} call{} of filemap_get_pages continued a read already counted -- the kernel \
                 reports a read's whole remaining range once per batch of folios -- and added \
                 nothing",
                self.continued,
                if self.continued == 1 { "" } else { "s" }
            ));
        }
        notes
    }
}

fn notes(
    totals: Totals,
    rows: u64,
    unnamed: u64,
    filtered_out: u64,
    filtered: bool,
) -> Vec<String> {
    let plural = |count: u64| if count == 1 { "" } else { "s" };
    let mut notes = vec![format!(
        "{} page{} looked up in {} lookup{}, {} filled inside a file and {} created at the end \
         of one, counted in the kernel and reported as {} row{}: an event is a task's pages of \
         one file over one drain, not a page",
        totals.asked,
        plural(totals.asked),
        totals.lookups,
        plural(totals.lookups),
        totals.filled,
        totals.created,
        rows,
        plural(rows),
    )];
    if totals.created > 0 {
        notes.push(format!(
            "{} page{} added at or past a file's end are counted as created rather than \
             missed: a write extends the file only after its page is in the cache, and no \
             read can have asked for them",
            totals.created,
            plural(totals.created)
        ));
    }
    if unnamed > 0 {
        notes.push(format!(
            "{unnamed} event{} named no file -- a mapping with no inode, or an inode with no \
             superblock -- and are in no row",
            plural(unnamed)
        ));
    }
    if filtered {
        notes.push(format!(
            "the filter ran here, over the drained rows, and removed {filtered_out} row{}: the \
             kernel still counted them, so the cost is the unfiltered one",
            plural(filtered_out)
        ));
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ahead` is signed, and it is the fill minus the lookup, not the other way
    /// round: a reader served from the cache is negative.
    #[test]
    fn ahead_is_the_fill_minus_the_lookup_and_goes_below_zero() {
        assert_eq!(ahead(&PageCacheRow::for_tests(1, 2, 100, 132, 0)), 32);
        assert_eq!(ahead(&PageCacheRow::for_tests(1, 2, 100, 0, 0)), -100);
        assert_eq!(ahead(&PageCacheRow::for_tests(1, 2, 64, 64, 0)), 0);
    }

    /// A created page is not a filled one: the row keeps them apart and so do
    /// the event's attributes.
    #[test]
    fn a_page_a_write_created_is_not_counted_as_a_miss() {
        let mut collector = PageCacheCollector::new(
            Reading::Misses,
            Mode::Trace,
            PageCacheNarrowing::default(),
            None,
        );
        let event = collector.event(&PageCacheRow::for_tests(7, 9, 0, 0, 256), 5, 10, 0);
        assert_eq!(event.attr("misses").and_then(|v| v.as_f64()), Some(0.0));
        assert_eq!(event.attr("created").and_then(|v| v.as_f64()), Some(256.0));
        assert!(matches!(event.keys[0], KeyValue::File { ino: 9, .. }));
    }

    /// Both readings carry both halves, and differ only in which is the value.
    #[test]
    fn both_readings_carry_the_lookups_and_the_fills() {
        for reading in [Reading::Misses, Reading::Ahead] {
            let mut collector =
                PageCacheCollector::new(reading, Mode::Trace, PageCacheNarrowing::default(), None);
            let event = collector.event(&PageCacheRow::for_tests(7, 9, 40, 72, 0), 5, 10, 0);
            assert!(event.attr(reading.value_attr()).is_some(), "{reading:?}");
            assert_eq!(event.attr("lookups").and_then(|v| v.as_f64()), Some(40.0));
            assert_eq!(event.attr("filled").and_then(|v| v.as_f64()), Some(72.0));
            assert_eq!(event.ts_ns, 5, "stamped at the row's first event");
        }
    }

    #[test]
    fn two_drains_of_one_key_add_up_in_a_counted_run() {
        let mut first = PageCacheRow::for_tests(7, 9, 10, 20, 1);
        first.first_ns = 50;
        let mut second = PageCacheRow::for_tests(7, 9, 5, 6, 2);
        second.first_ns = 40;
        first.absorb(&second);
        assert_eq!(
            (first.asked, first.filled, first.created, first.first_ns),
            (15, 26, 3, 40)
        );
    }
}
