//! `mem.heap.retained`: memory the program gave back and the process still has.
//!
//! `free()` returns nothing to the operating system. The allocator keeps the
//! pages, ready for the next allocation, and a process that has freed
//! everything can sit on hundreds of megabytes indefinitely. That gap is what
//! ends runs in an OOM killer while every allocation-site profile says the
//! program is fine, and almost nobody measures it.
//!
//! The obvious way to measure it is `RSS - live heap`, and that subtraction is
//! wrong: RSS also holds the executable, the stacks, every file mapping and
//! every anonymous mapping the program made for itself, while live heap counts
//! bytes the program asked for, which are not resident until it writes to them.
//! The difference is a number made of four unrelated things.
//!
//! So this measures it directly instead. The shim says which ranges were
//! allocated and then freed; `/proc/[pid]/pagemap` says which pages of those
//! ranges are still in memory. Retained memory is the intersection: addresses
//! the program gave back, pages the process still holds. RSS is reported beside
//! it for scale, and is not part of the number.

use std::time::Duration;

use super::super::cadence::Cadence;
use super::state::{FreedRanges, LiveSet};
use super::{pagemap::Pagemap, Feed, Stream, OP_ALLOC};
use crate::clock::monotonic_ns;
use crate::collect::task::Comm;
use crate::collect::{Collector, Losses, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};

/// How many freed ranges are held at once.
///
/// Ranges that touch are merged, so this is a bound on fragmentation rather
/// than on the number of frees: a program that frees a million blocks next to
/// each other holds one range. A program that scatters a million holes across
/// its address space is a program whose fragmentation cannot be summarised
/// anyway, and the table says what it dropped.
const RANGE_CAPACITY: usize = 128 * 1024;

pub struct RetainedCollector<F: Feed = Stream> {
    stream: F,
    live: LiveSet,
    freed: FreedRanges,
    pagemap: Pagemap,
    /// When a value is published, as opposed to how often the pipe is drained.
    cadence: Cadence,
    /// `comm` is a declared filter field on this source.
    comm: Comm,
    emitted: u64,
    losses: Losses,
    /// Evictions already counted, so the loss is reported once per range and
    /// not once per poll.
    evictions_reported: u64,
}

impl<F: Feed> RetainedCollector<F> {
    pub fn new(stream: F, interval: Duration) -> Result<Box<Self>, String> {
        Self::with_capacity(stream, interval, RANGE_CAPACITY)
    }

    fn with_capacity(stream: F, interval: Duration, capacity: usize) -> Result<Box<Self>, String> {
        let pagemap = Pagemap::open(stream.pid()).map_err(|error| {
            format!(
                "cannot read /proc/{}/pagemap: {error}; without it there is no way to tell \
                 which of the freed ranges are still in memory",
                stream.pid()
            )
        })?;
        let comm = Comm::of(stream.pid());
        Ok(Box::new(Self {
            stream,
            live: LiveSet::default(),
            freed: FreedRanges::new(capacity),
            pagemap,
            cadence: Cadence::new(interval),
            comm,
            emitted: 0,
            losses: Losses::default(),
            evictions_reported: 0,
        }))
    }

    /// Applies everything the shim has written since the last call.
    ///
    /// An allocation removes its range from the freed table: an address handed
    /// out again is not retained any more, and leaving it there would credit
    /// fragmentation with the pages of a live allocation.
    fn catch_up(&mut self) {
        for record in self.stream.drain() {
            if record.op == OP_ALLOC {
                let (base, end) = self.live.allocated(record.ptr, record.size);
                self.freed.remove(base, end);
            } else if let Some((base, end)) = self.live.freed(record.ptr) {
                self.freed.insert(base, end);
            }
        }

        let (evicted, evicted_bytes) = self.freed.evicted();
        if evicted > self.evictions_reported {
            self.losses.dropped += evicted - self.evictions_reported;
            self.losses.note_times(
                format!(
                    "the freed-range table holds {RANGE_CAPACITY} ranges and dropped the oldest \
                 ones, {evicted_bytes} bytes of address space in all: whatever is still \
                 resident in those is missing from the retained total"
                ),
                evicted - self.evictions_reported,
            );
            self.evictions_reported = evicted;
        }
    }

    fn measure(&mut self) -> Result<Event, SampleError> {
        let ranges: Vec<(u64, u64)> = self.freed.iter().collect();
        let residency = self.pagemap.scan(ranges.into_iter()).map_err(|error| {
            let gone = error.kind() == std::io::ErrorKind::NotFound
                || error.kind() == std::io::ErrorKind::UnexpectedEof
                || error.raw_os_error() == Some(libc::ESRCH);
            if gone {
                SampleError::TargetGone(format!("process {} exited", self.stream.pid()))
            } else {
                SampleError::Transient(format!("{}: {error}", self.pagemap.path().display()))
            }
        })?;

        let ts_ns = monotonic_ns();
        let (evicted, _) = self.freed.evicted();

        let mut attrs = vec![
            Attr::uint("bytes", residency.resident),
            // Paged out is not given back: the process still owns those pages
            // and touching them costs a major fault.
            Attr::uint("swapped", residency.swapped),
            Attr::uint("live", self.live.bytes()),
            // Address space that was freed, an upper bound on what can be
            // retained: the rest of it was never written to or has been
            // reclaimed.
            Attr::uint("freed_span", self.freed.bytes()),
            Attr::uint("ranges", self.freed.count() as u64),
            Attr::uint("evicted_ranges", evicted),
            // What the scan cost, in pages looked at.
            Attr::uint("pages_examined", residency.pages_examined),
        ];
        // For scale only. The retained number is not derived from it.
        if let Some(rss) = super::super::footprint::resident_bytes(self.stream.pid()) {
            attrs.push(Attr::uint("rss", rss));
        }
        if let Some(comm) = self.comm.get() {
            attrs.push(Attr::text("comm", comm));
        }

        let event = Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.heap.retained".to_owned(),
            keys: vec![KeyValue::task(self.stream.pid())],
            attrs,
            collector_id: CollectorId::unstamped(),
            seq: self.emitted,
        };
        self.emitted += 1;
        Ok(event)
    }
}

impl<F: Feed> Collector for RetainedCollector<F> {
    fn start(&mut self) -> Result<(), String> {
        self.stream.start()
    }

    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
        self.catch_up();
        if !self.cadence.due(monotonic_ns()) {
            return Ok(Vec::new());
        }
        Ok(vec![self.measure()?])
    }

    fn stop(&mut self) -> Vec<Event> {
        self.catch_up();
        // The program is usually gone by now, and its pages with it: the last
        // usable measurement is the one taken while it was still running. A
        // scan of a dead process reads as zero retained memory, which is the
        // one answer this source must never invent, so it is dropped rather
        // than published.
        let last = self.measure().ok();
        self.stream.close();
        last.into_iter().collect()
    }

    fn tick(&self) -> Option<Duration> {
        Some(super::DRAIN_INTERVAL)
    }

    fn notes(&self) -> Vec<String> {
        self.stream.notes()
    }

    fn losses(&self) -> Losses {
        let mut losses = self.stream.losses();
        losses.dropped += self.losses.dropped;
        for (reason, count) in &self.losses.reasons {
            *losses.reasons.entry(reason.clone()).or_insert(0) += count;
        }
        losses
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{piped, record, write};
    use super::super::OP_FREE;
    use super::*;

    /// Ground truth at the collector's own level.
    ///
    /// This test writes to a megabyte of memory, so those pages are resident by
    /// definition, and then tells the collector that range was allocated and
    /// freed. Every one of those pages is memory the program gave back and the
    /// process still holds, which is exactly what this source claims to count.
    #[test]
    fn pages_of_a_freed_range_that_are_still_in_memory_are_retained() {
        const SIZE: usize = 1 << 20;
        let (writer, stream) = piped();
        let mut collector = RetainedCollector::new(stream, Duration::ZERO).expect("our pagemap");

        let mut touched = vec![0u8; SIZE];
        for page in touched.chunks_mut(4096) {
            page[0] = 1;
        }
        let base = touched.as_ptr() as u64;
        write(writer, &record(OP_ALLOC, base, SIZE as u64));
        write(writer, &record(OP_FREE, base, 0));

        let events = collector.sample(0).unwrap();
        let retained = events[0].attr("bytes").unwrap().as_f64().unwrap();
        std::hint::black_box(&touched);

        assert!(
            retained >= 0.9 * SIZE as f64,
            "a megabyte that was written to and then freed should be retained, got {retained}"
        );
        assert_eq!(events[0].attr("live").unwrap().as_f64(), Some(0.0));
        assert_eq!(
            events[0].attr("freed_span").unwrap().as_f64(),
            Some(SIZE as f64)
        );
    }

    /// The other half of the same claim: memory that was never written to has
    /// no pages behind it, so freeing it retains nothing. A collector that
    /// counted address space rather than residency would pass the test above
    /// and fail this one.
    #[test]
    fn an_untouched_allocation_retains_nothing_when_it_is_freed() {
        const SIZE: usize = 1 << 20;
        let (writer, stream) = piped();
        let mut collector = RetainedCollector::new(stream, Duration::ZERO).expect("our pagemap");

        // Mapped directly rather than allocated: a block from the allocator's
        // arena can land on pages another test in this binary already made
        // resident, and this test is about pages that were never written to.
        // SAFETY: an anonymous mapping of our own, unmapped below.
        let untouched = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(untouched, libc::MAP_FAILED, "the mapping must succeed");
        let base = untouched as u64;
        write(writer, &record(OP_ALLOC, base, SIZE as u64));
        write(writer, &record(OP_FREE, base, 0));

        let events = collector.sample(0).unwrap();
        let retained = events[0].attr("bytes").unwrap().as_f64().unwrap();
        // SAFETY: unmapping what was mapped above.
        unsafe { libc::munmap(untouched, SIZE) };

        assert!(
            retained <= 0.1 * SIZE as f64,
            "memory that was never written to cannot be retained, got {retained}"
        );
        assert_eq!(
            events[0].attr("freed_span").unwrap().as_f64(),
            Some(SIZE as f64),
            "the address space was still given back"
        );
    }

    /// A table that dropped a range is a table whose total is short by whatever
    /// was resident in it, and the run has to say so. An attribution missing
    /// because its range was evicted looks exactly like one that never
    /// happened.
    #[test]
    fn ranges_dropped_for_space_are_counted_as_a_loss() {
        let (writer, stream) = piped();
        let mut collector =
            RetainedCollector::with_capacity(stream, Duration::ZERO, 2).expect("our pagemap");

        // Four blocks, far enough apart that none of them merge.
        for index in 0..4u64 {
            let base = 0x1_0000_0000 + index * 0x1000_0000;
            write(writer, &record(OP_ALLOC, base, 0x4000));
            write(writer, &record(OP_FREE, base, 0));
        }

        let events = collector.sample(0).unwrap();
        assert_eq!(events[0].attr("ranges").unwrap().as_f64(), Some(2.0));
        assert_eq!(
            events[0].attr("evicted_ranges").unwrap().as_f64(),
            Some(2.0)
        );

        let losses = collector.losses();
        assert_eq!(losses.dropped, 2);
        assert!(!losses.is_clean());
        assert!(
            losses
                .reasons
                .keys()
                .any(|reason| reason.contains("freed-range table")),
            "the reason must say what was dropped and why: {:?}",
            losses.reasons
        );
    }

    /// The same eviction reported twice would double the loss. It is counted
    /// once per range, not once per poll.
    #[test]
    fn an_eviction_is_reported_once_and_not_on_every_poll() {
        let (writer, stream) = piped();
        let mut collector =
            RetainedCollector::with_capacity(stream, Duration::ZERO, 1).expect("our pagemap");

        for index in 0..3u64 {
            let base = 0x1_0000_0000 + index * 0x1000_0000;
            write(writer, &record(OP_ALLOC, base, 0x4000));
            write(writer, &record(OP_FREE, base, 0));
        }
        collector.sample(0).unwrap();
        assert_eq!(collector.losses().dropped, 2);

        collector.sample(1).unwrap();
        collector.sample(2).unwrap();
        assert_eq!(collector.losses().dropped, 2, "nothing new was dropped");
    }

    /// A program that has exited holds nothing, and a collector that said so
    /// would be reporting the one number this source must never invent. The run
    /// ends instead, and the last value stands.
    #[test]
    fn a_target_that_has_exited_ends_the_run_rather_than_reading_as_empty() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("sh should run");
        let pid = child.id() as i32;

        // The pipe is never written to: this is about the target, not the shim.
        let stream = Stream::new(-1, pid);
        let mut collector = match RetainedCollector::new(stream, Duration::ZERO) {
            Ok(collector) => collector,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("skipped: a child's pagemap cannot be read: {error}");
                return;
            }
        };
        child.kill().expect("our own child");
        child.wait().expect("reaped");

        match collector.sample(0) {
            Err(SampleError::TargetGone(reason)) => assert!(reason.contains("exited"), "{reason}"),
            other => panic!("expected the run to end, got {other:?}"),
        }
        assert!(
            collector.stop().is_empty(),
            "a scan of a dead process publishes nothing"
        );
    }

    #[test]
    fn a_live_allocation_is_not_retained_memory() {
        let (writer, stream) = piped();
        let mut collector = RetainedCollector::new(stream, Duration::ZERO).expect("our pagemap");
        write(writer, &record(OP_ALLOC, 0x10_0000, 4096));

        let events = collector.sample(0).unwrap();
        assert_eq!(events[0].attr("live").unwrap().as_f64(), Some(4096.0));
        assert_eq!(events[0].attr("bytes").unwrap().as_f64(), Some(0.0));
        assert_eq!(events[0].attr("freed_span").unwrap().as_f64(), Some(0.0));
    }

    /// An address handed out again stops being retained the moment it is: the
    /// pages behind it belong to the new allocation.
    #[test]
    fn reusing_a_freed_address_takes_it_out_of_the_retained_total() {
        const SIZE: usize = 1 << 20;
        let (writer, stream) = piped();
        let mut collector = RetainedCollector::new(stream, Duration::ZERO).expect("our pagemap");

        let mut touched = vec![0u8; SIZE];
        for page in touched.chunks_mut(4096) {
            page[0] = 1;
        }
        let base = touched.as_ptr() as u64;
        write(writer, &record(OP_ALLOC, base, SIZE as u64));
        write(writer, &record(OP_FREE, base, 0));
        assert!(
            collector.sample(0).unwrap()[0]
                .attr("bytes")
                .unwrap()
                .as_f64()
                > Some(0.0)
        );

        write(writer, &record(OP_ALLOC, base, SIZE as u64));
        let events = collector.sample(1).unwrap();
        std::hint::black_box(&touched);
        assert_eq!(events[0].attr("bytes").unwrap().as_f64(), Some(0.0));
        assert_eq!(events[0].attr("live").unwrap().as_f64(), Some(SIZE as f64));
    }
}
