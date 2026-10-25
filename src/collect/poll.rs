//! Cumulative counters read at intervals: what every file-reading collector
//! of a counter does the same way, in one place.
//!
//! A counter the kernel keeps since boot says nothing about a run until it is
//! measured against an earlier reading. Doing that well has four rules, each
//! of which a collector got wrong once on its own:
//!
//! - **The baseline is taken at the collector's start**, before the observed
//!   program is released, so what it does from its first instruction is in
//!   the first interval.
//! - **A polled run's first tick is the window's opening, not an interval.**
//!   The runner takes it as soon as the program is released, and an interval
//!   of microseconds would put a meaningless ratio at the head of the series.
//!   It produces nothing; a baseline older than one interval -- taken while
//!   the rest of a live `omn top` was still loading its programs -- is
//!   replaced by it, since its increase is from before the window and would
//!   arrive as one spike at offset zero.
//! - **A counted run is one total**, from the baseline to the stop, and reads
//!   nothing in between.
//! - **A counter that appeared, or went backwards, is no interval.** One
//!   that appeared has nothing to be measured against; one that went
//!   backwards was replaced (a device removed and re-added, a namespace's
//!   counters reset). Both are handed back by key, for the collector to count
//!   and say in its own words, never folded into a zero.
//!
//! A collector supplies the reading -- [`Reading`], keyed counters of any
//! shape that knows its own increase ([`Cumulative`]) -- and turns an
//! [`Interval`] into events. What a snapshot or a gauge reports is the
//! collector's: neither is an increase.

use std::collections::BTreeMap;

use crate::collect::SampleError;
use crate::request::Mode;

/// A counter, or a set of counters read together, that can say how much it
/// grew since an earlier reading of the same thing.
pub trait Cumulative: Clone {
    /// The increase since `earlier`, or `None` when any part of it went
    /// backwards.
    fn since(&self, earlier: &Self) -> Option<Self>;
}

impl Cumulative for u64 {
    fn since(&self, earlier: &Self) -> Option<Self> {
        self.checked_sub(*earlier)
    }
}

impl Cumulative for i64 {
    fn since(&self, earlier: &Self) -> Option<Self> {
        (*self >= *earlier).then(|| *self - *earlier)
    }
}

/// Counters read together, such as a time and a count: grown only when both
/// grew, or stood still.
impl<A: Cumulative, B: Cumulative> Cumulative for (A, B) {
    fn since(&self, earlier: &Self) -> Option<Self> {
        Some((self.0.since(&earlier.0)?, self.1.since(&earlier.1)?))
    }
}

/// One reading: every counter, by key, and when it was read.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading<K, C> {
    pub ts_ns: u64,
    pub counters: BTreeMap<K, C>,
}

/// What moved between two readings.
#[derive(Debug, Clone, PartialEq)]
pub struct Interval<K, C> {
    /// When the later reading was taken.
    pub ts_ns: u64,
    /// How long the interval is.
    pub interval_ns: u64,
    /// Each counter present at both ends, with its increase, in key order.
    pub moved: Vec<(K, C)>,
    /// Counters present only at the end: no interval to report.
    pub appeared: Vec<K>,
    /// Counters that went backwards: replaced, no interval to report.
    pub went_back: Vec<K>,
    /// Counters present only at the start: unread at the end.
    pub vanished: Vec<K>,
    /// The later reading whole, for what a collector reports as it stands --
    /// a gauge among the counters.
    pub after: Reading<K, C>,
}

impl<K: Ord + Clone, C: Cumulative> Interval<K, C> {
    fn between(before: &Reading<K, C>, after: Reading<K, C>) -> Self {
        let mut moved = Vec::new();
        let mut appeared = Vec::new();
        let mut went_back = Vec::new();
        for (key, counter) in &after.counters {
            match before.counters.get(key) {
                None => appeared.push(key.clone()),
                Some(earlier) => match counter.since(earlier) {
                    Some(increase) => moved.push((key.clone(), increase)),
                    None => went_back.push(key.clone()),
                },
            }
        }
        let vanished = before
            .counters
            .keys()
            .filter(|key| !after.counters.contains_key(*key))
            .cloned()
            .collect();
        Interval {
            ts_ns: after.ts_ns,
            interval_ns: after.ts_ns.saturating_sub(before.ts_ns),
            moved,
            appeared,
            went_back,
            vanished,
            after,
        }
    }
}

/// The baseline and the interval of one run of one counter collector.
#[derive(Debug, Clone)]
pub struct CounterPoll<K, C> {
    mode: Mode,
    /// The reading the next interval is measured against.
    previous: Option<Reading<K, C>>,
    /// Set from the start until the first tick has opened the window.
    opening: bool,
}

impl<K: Ord + Clone, C: Cumulative> CounterPoll<K, C> {
    /// For a run in `mode`. A snapshot measures no increase, and this keeps no
    /// state for it: every call answers nothing.
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            previous: None,
            opening: false,
        }
    }

    fn measures(&self) -> bool {
        matches!(self.mode, Mode::Count | Mode::Poll { .. })
    }

    /// Takes the baseline, at the collector's start.
    pub fn start(
        &mut self,
        read: impl FnOnce() -> Result<Reading<K, C>, SampleError>,
    ) -> Result<(), SampleError> {
        if !self.measures() {
            return Ok(());
        }
        self.previous = Some(read()?);
        self.opening = matches!(self.mode, Mode::Poll { .. });
        Ok(())
    }

    /// At a tick: the interval since the last one, or nothing -- for a
    /// counted run, which reads only at its stop, and at a polled run's first
    /// tick, which opens the window. `read` is called only when a reading is
    /// needed.
    pub fn tick(
        &mut self,
        read: impl FnOnce() -> Result<Reading<K, C>, SampleError>,
    ) -> Result<Option<Interval<K, C>>, SampleError> {
        let Mode::Poll { interval } = self.mode else {
            return Ok(None);
        };
        if std::mem::take(&mut self.opening) {
            let since = self.previous.as_ref().map(|previous| previous.ts_ns);
            if let Some(since) = since {
                let now = read()?;
                if now.ts_ns.saturating_sub(since) > interval.as_nanos() {
                    self.previous = Some(now);
                }
            }
            return Ok(None);
        }
        let after = read()?;
        let before = self.previous.replace(after.clone());
        Ok(before.map(|before| Interval::between(&before, after)))
    }

    /// At the stop: a counted run's one total, from the baseline. A polled
    /// run reports nothing here: the runner samples every collector and then
    /// decides to stop, so a closing read would be an interval of microseconds.
    pub fn stop(
        &mut self,
        read: impl FnOnce() -> Result<Reading<K, C>, SampleError>,
    ) -> Result<Option<Interval<K, C>>, SampleError> {
        if self.mode != Mode::Count {
            return Ok(None);
        }
        let Some(before) = self.previous.take() else {
            return Ok(None);
        };
        Ok(Some(Interval::between(&before, read()?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scalar::Duration;

    fn reading(ts_ns: u64, counters: &[(&'static str, u64)]) -> Reading<&'static str, u64> {
        Reading {
            ts_ns,
            counters: counters.iter().copied().collect(),
        }
    }

    fn poll(ms: u64) -> Mode {
        Mode::Poll {
            interval: Duration::from_nanos(ms * 1_000_000),
        }
    }

    #[test]
    fn a_polled_run_measures_each_interval_from_the_one_before() {
        let mut poll = CounterPoll::new(poll(100));
        poll.start(|| Ok(reading(1_000, &[("a", 10)]))).unwrap();
        // The opening tick, just after the release: nothing, and the young
        // baseline is kept, so the bytes since the release are not lost.
        let opening = poll.tick(|| Ok(reading(2_000, &[("a", 11)]))).unwrap();
        assert_eq!(opening, None);
        let first = poll
            .tick(|| Ok(reading(100_002_000, &[("a", 15)])))
            .unwrap()
            .expect("an interval");
        assert_eq!(first.moved, vec![("a", 5)]);
        assert_eq!(first.interval_ns, 100_001_000);
        let second = poll
            .tick(|| Ok(reading(200_002_000, &[("a", 18)])))
            .unwrap()
            .expect("an interval");
        assert_eq!(second.moved, vec![("a", 3)]);
        assert_eq!(poll.stop(|| unreachable!()).unwrap(), None);
    }

    #[test]
    fn a_baseline_older_than_an_interval_is_replaced_at_the_opening() {
        let mut poll = CounterPoll::new(poll(100));
        poll.start(|| Ok(reading(0, &[("a", 10)]))).unwrap();
        // Three seconds of attaching later, the window opens.
        poll.tick(|| Ok(reading(3_000_000_000, &[("a", 900)])))
            .unwrap();
        let first = poll
            .tick(|| Ok(reading(3_100_000_000, &[("a", 905)])))
            .unwrap()
            .expect("an interval");
        assert_eq!(
            first.moved,
            vec![("a", 5)],
            "no spike from before the window"
        );
    }

    #[test]
    fn a_counted_run_is_one_total_read_at_the_stop() {
        let mut poll = CounterPoll::new(Mode::Count);
        poll.start(|| Ok(reading(0, &[("a", 10), ("b", 1)])))
            .unwrap();
        assert_eq!(poll.tick(|| unreachable!()).unwrap(), None);
        let total = poll
            .stop(|| Ok(reading(5, &[("a", 40), ("b", 1)])))
            .unwrap()
            .expect("the total");
        assert_eq!(total.moved, vec![("a", 30), ("b", 0)]);
    }

    #[test]
    fn a_counter_that_appeared_or_went_back_is_handed_back_not_zeroed() {
        let mut poll = CounterPoll::new(Mode::Count);
        poll.start(|| Ok(reading(0, &[("kept", 1), ("reset", 50)])))
            .unwrap();
        let total = poll
            .stop(|| Ok(reading(5, &[("kept", 4), ("new", 7), ("reset", 3)])))
            .unwrap()
            .expect("the total");
        assert_eq!(total.moved, vec![("kept", 3)]);
        assert_eq!(total.appeared, vec!["new"]);
        assert_eq!(total.went_back, vec!["reset"]);
        assert!(total.vanished.is_empty());
    }

    #[test]
    fn a_counter_unread_at_the_end_is_handed_back_as_vanished() {
        let mut poll = CounterPoll::new(Mode::Count);
        poll.start(|| Ok(reading(0, &[("kept", 1), ("gone", 5)])))
            .unwrap();
        let total = poll
            .stop(|| Ok(reading(5, &[("kept", 4)])))
            .unwrap()
            .expect("the total");
        assert_eq!(total.moved, vec![("kept", 3)]);
        assert_eq!(total.vanished, vec!["gone"]);
    }

    #[test]
    fn a_snapshot_measures_no_increase_and_reads_nothing() {
        let mut poll: CounterPoll<&str, u64> = CounterPoll::new(Mode::Snapshot);
        poll.start(|| unreachable!()).unwrap();
        assert_eq!(poll.tick(|| unreachable!()).unwrap(), None);
        assert_eq!(poll.stop(|| unreachable!()).unwrap(), None);
    }
}
