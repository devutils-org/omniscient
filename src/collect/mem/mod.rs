//! Collectors for the `mem` family.

pub mod heap;
mod page_faults;
mod rss;

use super::Factory;

/// The collectors this family provides.
///
/// Adding a source means adding a line here and a module beside it. No file
/// outside this directory changes, which is what lets the families be worked on
/// independently.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("mem.rss", "fs:proc:smaps_rollup", |_| {
            Ok(Box::new(rss::Prepare(rss::RssSource::SmapsRollup)))
        }),
        Factory::new("mem.rss", "fs:proc:statm", |_| {
            Ok(Box::new(rss::Prepare(rss::RssSource::Statm)))
        }),
        Factory::new("mem.faults", "pmu:sw", |resolved| {
            Ok(Box::new(page_faults::Prepare(resolved.request.mode)))
        }),
        Factory::new("mem.heap.alloc", "preload", |_| {
            Ok(Box::new(heap::HeapPrepared::new()?))
        }),
    ]
}
