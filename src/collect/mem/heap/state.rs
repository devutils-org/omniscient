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
        (ptr, ptr.saturating_add(size))
    }

    /// Records a free, returning the range it gave back when the allocation is
    /// one this set knows the size of.
    pub fn freed(&mut self, ptr: u64) -> Option<(u64, u64)> {
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
