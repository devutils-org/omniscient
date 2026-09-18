//! `mem.heap.live`: what has been allocated and not freed.
//!
//! The shim reports both sides of every allocation, so the live total is exact
//! -- not sampled, not inferred from a footprint. It is also not RSS and cannot
//! be compared to it: `malloc` returns an address, and the pages behind it
//! become resident only when the program writes to them. A program can hold a
//! gigabyte of live heap it has never touched, and a program that freed
//! everything can still have a large resident size. Those are two different
//! facts, and `mem.heap.retained` is the one that measures the gap.

use std::time::Duration;

use super::super::cadence::Cadence;
use super::{state::LiveSet, Stream, OP_ALLOC};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};

pub struct LiveCollector {
    id: CollectorId,
    stream: Stream,
    live: LiveSet,
    /// When a value is published, as opposed to how often the pipe is drained.
    cadence: Cadence,
    emitted: u64,
}

impl LiveCollector {
    pub fn new(stream: Stream, interval: Duration) -> Self {
        Self {
            id: CollectorId::new("mem.heap.live/preload"),
            stream,
            live: LiveSet::default(),
            cadence: Cadence::new(interval),
            emitted: 0,
        }
    }

    /// Applies everything the shim has written since the last call.
    fn catch_up(&mut self) {
        for record in self.stream.drain() {
            if record.op == OP_ALLOC {
                self.live.allocated(record.ptr, record.size);
            } else {
                self.live.freed(record.ptr);
            }
        }
    }

    fn event(&mut self, ts_ns: u64) -> Event {
        let event = Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.heap.live".to_owned(),
            keys: vec![KeyValue::task(self.stream.pid())],
            attrs: vec![
                Attr::uint("bytes", self.live.bytes()),
                Attr::uint("allocations", self.live.count() as u64),
                // Frees of blocks allocated before the shim could report them:
                // they are not in the total, and saying how many there were is
                // how a reader knows whether that matters here.
                Attr::uint("untracked_frees", self.live.untracked_frees()),
            ],
            collector_id: self.id.clone(),
            seq: self.emitted,
        };
        self.emitted += 1;
        event
    }
}

impl Collector for LiveCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
        self.catch_up();
        if !self.cadence.due(monotonic_ns()) {
            return Ok(Vec::new());
        }
        Ok(vec![self.event(monotonic_ns())])
    }

    fn stop(&mut self) -> Vec<Event> {
        // The last point is the one that matters most: the live heap at exit is
        // what the program never gave back.
        self.catch_up();
        let final_event = self.event(monotonic_ns());
        self.stream.close();
        vec![final_event]
    }

    fn tick(&self) -> Option<Duration> {
        Some(super::DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.stream.losses()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{piped, record, write};
    use super::super::OP_FREE;
    use super::*;

    #[test]
    fn the_live_total_is_what_was_allocated_minus_what_was_freed() {
        let (writer, stream) = piped();
        let mut collector = LiveCollector::new(stream, Duration::ZERO);

        write(writer, &record(OP_ALLOC, 0x1000, 4096));
        write(writer, &record(OP_ALLOC, 0x8000, 1024));
        let events = collector.sample(0).unwrap();
        assert_eq!(events[0].attr("bytes").unwrap().as_f64(), Some(5120.0));
        assert_eq!(events[0].attr("allocations").unwrap().as_f64(), Some(2.0));

        write(writer, &record(OP_FREE, 0x1000, 0));
        let events = collector.sample(1).unwrap();
        assert_eq!(events[0].attr("bytes").unwrap().as_f64(), Some(1024.0));
    }

    /// `realloc` is reported as a free and an allocation, because that is what
    /// it is. The live total has to follow the pointer to its new address
    /// rather than counting the block twice or losing it.
    #[test]
    fn a_block_that_moved_is_counted_once_at_its_new_address() {
        let (writer, stream) = piped();
        let mut collector = LiveCollector::new(stream, Duration::ZERO);

        write(writer, &record(OP_ALLOC, 0x1000, 1024));
        write(writer, &record(OP_FREE, 0x1000, 0));
        write(writer, &record(OP_ALLOC, 0x5000, 4096));

        let events = collector.sample(0).unwrap();
        assert_eq!(events[0].attr("bytes").unwrap().as_f64(), Some(4096.0));
        assert_eq!(events[0].attr("allocations").unwrap().as_f64(), Some(1.0));
    }

    /// Blocks allocated before the shim could report them are freed normally
    /// later, and those frees have nothing to subtract from. The count travels
    /// with the value, because it is what says whether the total is missing a
    /// handful of blocks or an entire allocator.
    #[test]
    fn frees_with_no_allocation_behind_them_are_reported_beside_the_total() {
        let (writer, stream) = piped();
        let mut collector = LiveCollector::new(stream, Duration::ZERO);

        write(writer, &record(OP_ALLOC, 0x1000, 1024));
        write(writer, &record(OP_FREE, 0xdead_0000, 0));

        let events = collector.sample(0).unwrap();
        assert_eq!(events[0].attr("bytes").unwrap().as_f64(), Some(1024.0));
        assert_eq!(
            events[0].attr("untracked_frees").unwrap().as_f64(),
            Some(1.0)
        );
    }

    /// The live heap at exit is the one point of the series that cannot be
    /// missed: it is what the program never gave back.
    #[test]
    fn the_final_point_is_published_whatever_the_grid_says() {
        let (writer, stream) = piped();
        let mut collector = LiveCollector::new(stream, Duration::from_secs(3600));
        assert_eq!(collector.sample(0).unwrap().len(), 1, "the first point");

        write(writer, &record(OP_ALLOC, 0x1000, 8192));
        assert!(collector.sample(1).unwrap().is_empty());

        let last = collector.stop();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].attr("bytes").unwrap().as_f64(), Some(8192.0));
    }

    /// The pipe has to be drained far more often than the series is published,
    /// or a burst of allocations overflows it. Both cadences exist, and the
    /// value is emitted on the slower one.
    #[test]
    fn records_are_drained_between_the_points_that_are_published() {
        let (writer, stream) = piped();
        let mut collector = LiveCollector::new(stream, Duration::from_secs(3600));

        write(writer, &record(OP_ALLOC, 0x1000, 4096));
        // The first sample publishes: nothing has been emitted yet.
        assert_eq!(collector.sample(0).unwrap().len(), 1);

        write(writer, &record(OP_ALLOC, 0x8000, 1024));
        assert!(
            collector.sample(1).unwrap().is_empty(),
            "an hour has not passed, so nothing is published"
        );

        // But the record was read, and the final point proves it.
        let last = collector.stop();
        assert_eq!(last[0].attr("bytes").unwrap().as_f64(), Some(5120.0));
    }
}
