//! Collectors for the `mem` family.

mod footprint;
pub mod heap;
mod page_faults;

use super::Factory;
use footprint::{Metric, Source};

/// The collectors this family provides.
///
/// Adding a source means adding a line here and a module beside it. No file
/// outside this directory changes, which is what lets the families be worked on
/// independently.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("mem.rss", "fs:proc:smaps_rollup", |_| {
            Ok(Box::new(footprint::Prepare(
                Metric::Rss,
                Source::SmapsRollup,
            )))
        }),
        Factory::new("mem.rss", "fs:proc:statm", |_| {
            Ok(Box::new(footprint::Prepare(Metric::Rss, Source::Statm)))
        }),
        Factory::new("mem.pss", "fs:proc:smaps_rollup", |_| {
            Ok(Box::new(footprint::Prepare(
                Metric::Pss,
                Source::SmapsRollup,
            )))
        }),
        Factory::new("mem.pss", "fs:proc:smaps", |_| {
            Ok(Box::new(footprint::Prepare(Metric::Pss, Source::Smaps)))
        }),
        Factory::new("mem.uss", "fs:proc:smaps_rollup", |_| {
            Ok(Box::new(footprint::Prepare(
                Metric::Uss,
                Source::SmapsRollup,
            )))
        }),
        Factory::new("mem.uss", "fs:proc:smaps", |_| {
            Ok(Box::new(footprint::Prepare(Metric::Uss, Source::Smaps)))
        }),
        Factory::new("mem.faults", "pmu:sw", |resolved| {
            Ok(Box::new(page_faults::Prepare(resolved.request.mode)))
        }),
        Factory::new("mem.heap.alloc", "preload", |_| {
            Ok(Box::new(heap::HeapPrepared::new()?))
        }),
    ]
}
