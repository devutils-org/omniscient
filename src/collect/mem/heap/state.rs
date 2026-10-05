//! What the record stream adds up to.
//!
//! The shim reports events; these two structures are the state those events
//! imply. `LiveSet` holds what has been allocated and not freed, which is the
//! whole of `mem.heap.live`. `FreedRanges` holds the address ranges the program
//! gave back, which is where `mem.heap.retained` goes looking for pages the
//! allocator kept.

use std::collections::{BTreeMap, HashMap, VecDeque};

/// Allocations that have not been freed.
#[derive(Debug, Default)]
pub struct LiveSet {
    sizes: HashMap<u64, u64>,
    bytes: u64,
    /// Frees of pointers no allocation was seen for. The shim suppresses
    /// reporting for anything allocated inside its own hooks -- the dynamic
    /// loader's bootstrap, mostly -- and those blocks are freed normally later.
    untracked_frees: u64,
    /// The most that was live at once, in bytes and in blocks, over every
    /// record applied. Exact where a poll is not: every call is a record, so a
    /// peak shorter than any poll period is still a state this set was in.
    peak_bytes: u64,
    peak_count: usize,
    /// When each live block was allocated, for the lifetime its free closes.
    /// Filled only through [`LiveSet::allocated_at`].
    born: HashMap<u64, u64>,
}

impl LiveSet {
    /// Records an allocation, returning the range it covers.
    pub fn allocated(&mut self, ptr: u64, size: u64) -> (u64, u64) {
        if let Some(previous) = self.sizes.insert(ptr, size) {
            // An address handed out twice without a free in between means a
            // record was lost. The newer size is the truth from here on.
            self.bytes = self.bytes.saturating_sub(previous);
        }
        self.bytes += size;
        self.peak_bytes = self.peak_bytes.max(self.bytes);
        self.peak_count = self.peak_count.max(self.sizes.len());
        (ptr, ptr.saturating_add(size))
    }

    /// The same, remembering when, so that the free can say how long the
    /// block lived.
    pub fn allocated_at(&mut self, ptr: u64, size: u64, ts_ns: u64) -> (u64, u64) {
        self.born.insert(ptr, ts_ns);
        self.allocated(ptr, size)
    }

    /// Records a free at `ts_ns`, answering the range it gave back and how
    /// long the block lived, when its allocation was seen with a time.
    pub fn freed_at(&mut self, ptr: u64, ts_ns: u64) -> Option<((u64, u64), Option<u64>)> {
        let born = self.born.remove(&ptr);
        let range = self.freed(ptr)?;
        Some((range, born.map(|born| ts_ns.saturating_sub(born))))
    }

    pub fn peak_bytes(&self) -> u64 {
        self.peak_bytes
    }

    pub fn peak_count(&self) -> usize {
        self.peak_count
    }

    /// Records a free, returning the range it gave back when the allocation is
    /// one this set knows the size of.
    pub fn freed(&mut self, ptr: u64) -> Option<(u64, u64)> {
        self.born.remove(&ptr);
        match self.sizes.remove(&ptr) {
            Some(size) => {
                self.bytes = self.bytes.saturating_sub(size);
                Some((ptr, ptr.saturating_add(size)))
            }
            None => {
                self.untracked_frees += 1;
                None
            }
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn count(&self) -> usize {
        self.sizes.len()
    }

    pub fn untracked_frees(&self) -> u64 {
        self.untracked_frees
    }
}

/// Address ranges that were allocated, then freed, and not handed out again.
///
/// Ranges that touch are merged, because that is what the allocator does to the
/// chunks behind them: two adjacent freed chunks become one free block, and
/// keeping them apart here would invent a boundary the allocator does not have.
///
/// The table is bounded. A program that frees without ever reusing the address
/// would otherwise grow it without limit, and an observer that runs out of
/// memory before the observed program does is not an observer. What it drops is
/// counted, because retained memory missing from the table looks exactly like
/// retained memory that does not exist.
#[derive(Debug)]
pub struct FreedRanges {
    ranges: BTreeMap<u64, Freed>,
    /// Insertion order, for eviction. Entries go stale when a range is merged
    /// or carved up, and are skipped by comparing the sequence number.
    order: VecDeque<(u64, u64)>,
    next_seq: u64,
    capacity: usize,
    evicted: u64,
    evicted_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct Freed {
    end: u64,
    seq: u64,
}

impl FreedRanges {
    pub fn new(capacity: usize) -> Self {
        Self {
            ranges: BTreeMap::new(),
            order: VecDeque::new(),
            next_seq: 0,
            capacity,
            evicted: 0,
            evicted_bytes: 0,
        }
    }

    /// Adds a range the program gave back, merging it with anything it touches.
    pub fn insert(&mut self, base: u64, end: u64) {
        if end <= base {
            return;
        }
        let (mut base, mut end) = (base, end);

        // The ranges are disjoint and sorted, so their ends increase with their
        // bases: walking back from the new range's end and stopping at the
        // first one that ends before it starts finds every neighbour.
        let touching: Vec<u64> = self
            .ranges
            .range(..=end)
            .rev()
            .take_while(|(_, freed)| freed.end >= base)
            .map(|(&start, _)| start)
            .collect();
        for start in touching {
            let freed = self.ranges.remove(&start).expect("just listed");
            base = base.min(start);
            end = end.max(freed.end);
        }

        let seq = self.next_seq;
        self.next_seq += 1;
        self.ranges.insert(base, Freed { end, seq });
        self.order.push_back((seq, base));
        self.evict_while_over_capacity();
    }

    /// Removes a range that has been handed out again.
    ///
    /// The allocator reusing an address is the normal case, not an exception:
    /// memory that came back is not retained any more, and leaving it in the
    /// table would credit the live allocation's pages to fragmentation.
    pub fn remove(&mut self, base: u64, end: u64) {
        if end <= base {
            return;
        }
        let overlapping: Vec<u64> = self
            .ranges
            .range(..end)
            .rev()
            .take_while(|(_, freed)| freed.end > base)
            .map(|(&start, _)| start)
            .collect();

        for start in overlapping {
            let freed = self.ranges.remove(&start).expect("just listed");
            // What is left of the range on either side of the reused part
            // stays freed.
            if start < base {
                self.ranges.insert(
                    start,
                    Freed {
                        end: base,
                        seq: freed.seq,
                    },
                );
            }
            if freed.end > end {
                let seq = self.next_seq;
                self.next_seq += 1;
                self.ranges.insert(
                    end,
                    Freed {
                        end: freed.end,
                        seq,
                    },
                );
                self.order.push_back((seq, end));
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.ranges.iter().map(|(&base, freed)| (base, freed.end))
    }

    /// How much address space is held, which is an upper bound on how much of
    /// it is resident.
    pub fn bytes(&self) -> u64 {
        self.ranges
            .iter()
            .map(|(&base, freed)| freed.end - base)
            .sum()
    }

    pub fn count(&self) -> usize {
        self.ranges.len()
    }

    pub fn evicted(&self) -> (u64, u64) {
        (self.evicted, self.evicted_bytes)
    }

    fn evict_while_over_capacity(&mut self) {
        while self.ranges.len() > self.capacity {
            let Some((seq, base)) = self.order.pop_front() else {
                return;
            };
            // A stale entry: that range was merged into another, or carved up.
            if self.ranges.get(&base).is_none_or(|freed| freed.seq != seq) {
                continue;
            }
            let freed = self.ranges.remove(&base).expect("checked above");
            self.evicted += 1;
            self.evicted_bytes += freed.end - base;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_peak_is_the_most_ever_live_even_after_it_is_freed() {
        let mut live = LiveSet::default();
        live.allocated_at(0x1000, 100, 10);
        live.allocated_at(0x2000, 300, 20);
        let (_, lifetime) = live.freed_at(0x1000, 50).expect("known");
        assert_eq!(lifetime, Some(40));
        live.allocated(0x3000, 50);
        assert_eq!(live.bytes(), 350);
        assert_eq!(live.peak_bytes(), 400);
        assert_eq!(live.peak_count(), 2);
        let (_, lifetime) = live.freed_at(0x3000, 60).expect("known");
        assert_eq!(lifetime, None, "allocated without a time");
        assert!(live.freed_at(0x9000, 70).is_none());
    }

    #[test]
    fn freeing_an_allocation_gives_back_the_range_it_covered() {
        let mut live = LiveSet::default();
        live.allocated(0x1000, 4096);
        live.allocated(0x8000, 128);
        assert_eq!(live.bytes(), 4096 + 128);
        assert_eq!(live.count(), 2);

        assert_eq!(live.freed(0x1000), Some((0x1000, 0x2000)));
        assert_eq!(live.bytes(), 128);
        assert_eq!(live.freed(0x1000), None, "freeing twice frees nothing");
        assert_eq!(live.untracked_frees(), 1);
    }

    #[test]
    fn adjacent_freed_ranges_become_one() {
        let mut freed = FreedRanges::new(16);
        freed.insert(0x1000, 0x2000);
        freed.insert(0x2000, 0x3000);
        freed.insert(0x2800, 0x4000);
        assert_eq!(freed.count(), 1);
        assert_eq!(freed.iter().collect::<Vec<_>>(), vec![(0x1000, 0x4000)]);
        assert_eq!(freed.bytes(), 0x3000);
    }

    #[test]
    fn a_reused_address_stops_counting_as_freed() {
        let mut freed = FreedRanges::new(16);
        freed.insert(0x1000, 0x4000);

        // The allocator hands the middle of the block back out.
        freed.remove(0x2000, 0x3000);
        assert_eq!(
            freed.iter().collect::<Vec<_>>(),
            vec![(0x1000, 0x2000), (0x3000, 0x4000)]
        );
        assert_eq!(freed.bytes(), 0x2000);

        // And then the rest of it.
        freed.remove(0x1000, 0x4000);
        assert_eq!(freed.count(), 0);
    }

    /// Freeing a block and then handing parts of it back out leaves the table
    /// with what the program still has not taken back, and nothing else.
    #[test]
    fn a_reused_address_that_overlaps_an_edge_carves_only_that_edge() {
        let mut freed = FreedRanges::new(16);
        freed.insert(0x2000, 0x4000);

        // An allocation that starts before the freed range and ends inside it.
        freed.remove(0x1000, 0x2800);
        assert_eq!(freed.iter().collect::<Vec<_>>(), vec![(0x2800, 0x4000)]);

        // And one that starts inside it and runs past its end.
        freed.remove(0x3800, 0x9000);
        assert_eq!(freed.iter().collect::<Vec<_>>(), vec![(0x2800, 0x3800)]);
        assert_eq!(freed.bytes(), 0x1000);
    }

    #[test]
    fn a_range_that_covers_nothing_is_not_a_range() {
        let mut freed = FreedRanges::new(16);
        freed.insert(0x1000, 0x1000);
        freed.insert(0x2000, 0x1000);
        assert_eq!(freed.count(), 0);

        freed.insert(0x1000, 0x2000);
        freed.remove(0x1000, 0x1000);
        assert_eq!(freed.count(), 1, "removing nothing removes nothing");
    }

    /// Merging leaves entries in the eviction queue pointing at ranges that no
    /// longer exist. Eviction has to walk past those rather than count them as
    /// dropped, or the table reports losses it never had and stops evicting the
    /// ranges it does hold.
    #[test]
    fn eviction_walks_past_the_ranges_that_merging_replaced() {
        let mut freed = FreedRanges::new(2);

        // Three ranges that merge into one: two queue entries go stale.
        freed.insert(0x1000, 0x2000);
        freed.insert(0x2000, 0x3000);
        freed.insert(0x3000, 0x4000);
        assert_eq!(freed.count(), 1);
        assert_eq!(freed.evicted(), (0, 0), "merging is not eviction");

        // Now past the capacity, with ranges that cannot merge.
        freed.insert(0x10_0000, 0x10_1000);
        freed.insert(0x20_0000, 0x20_1000);
        assert_eq!(freed.count(), 2);

        // The merged range was the oldest, so it went first, and what it cost
        // is the size it had after merging.
        let (count, bytes) = freed.evicted();
        assert_eq!(count, 1);
        assert_eq!(bytes, 0x3000);
        assert_eq!(
            freed.iter().collect::<Vec<_>>(),
            vec![(0x10_0000, 0x10_1000), (0x20_0000, 0x20_1000)]
        );
    }

    /// An allocation the collector never saw cannot be subtracted from a total
    /// it was never added to. Counting those frees is what tells a reader
    /// whether the live total is missing a whole allocator's worth of activity
    /// or a few blocks from before the shim was loaded.
    #[test]
    fn a_free_of_an_unknown_pointer_is_counted_and_changes_nothing() {
        let mut live = LiveSet::default();
        live.allocated(0x1000, 4096);
        assert_eq!(live.freed(0x9999), None);
        assert_eq!(live.bytes(), 4096);
        assert_eq!(live.count(), 1);
        assert_eq!(live.untracked_frees(), 1);
    }

    /// The same address handed out twice without a free in between means a
    /// record was lost. The newer size is what the program is holding.
    #[test]
    fn an_address_allocated_twice_counts_once() {
        let mut live = LiveSet::default();
        live.allocated(0x1000, 4096);
        live.allocated(0x1000, 64);
        assert_eq!(live.bytes(), 64);
        assert_eq!(live.count(), 1);
        assert_eq!(live.freed(0x1000), Some((0x1000, 0x1040)));
        assert_eq!(live.bytes(), 0);
    }

    #[test]
    fn the_table_is_bounded_and_says_what_it_dropped() {
        let mut freed = FreedRanges::new(4);
        // Ranges with a gap between them, so nothing merges.
        for index in 0..8u64 {
            let base = 0x10000 + index * 0x2000;
            freed.insert(base, base + 0x1000);
        }
        assert_eq!(freed.count(), 4);
        let (count, bytes) = freed.evicted();
        assert_eq!(count, 4);
        assert_eq!(bytes, 4 * 0x1000);

        // The oldest went first, so the newest four are what is left.
        let (first, _) = freed.iter().next().expect("four ranges");
        assert_eq!(first, 0x10000 + 4 * 0x2000);
    }
}
