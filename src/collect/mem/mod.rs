//! Collectors for the `mem` family.

mod cadence;
mod footprint;
pub mod heap;
mod page_faults;

use super::Factory;
use footprint::{Metric, Source};
use heap::Which;

/// The collectors this family provides.
///
/// Adding a source means adding a line here and a module beside it. No file
/// outside this directory changes, which is what lets the families be worked on
/// independently.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("mem.rss", "fs:proc:smaps_rollup", |resolved| {
            Ok(footprint::Prepare::new(
                Metric::Rss,
                Source::SmapsRollup,
                resolved.request.mode,
            ))
        }),
        Factory::new("mem.rss", "fs:proc:statm", |resolved| {
            Ok(footprint::Prepare::new(
                Metric::Rss,
                Source::Statm,
                resolved.request.mode,
            ))
        }),
        Factory::new("mem.pss", "fs:proc:smaps_rollup", |resolved| {
            Ok(footprint::Prepare::new(
                Metric::Pss,
                Source::SmapsRollup,
                resolved.request.mode,
            ))
        }),
        Factory::new("mem.pss", "fs:proc:smaps", |resolved| {
            Ok(footprint::Prepare::new(
                Metric::Pss,
                Source::Smaps,
                resolved.request.mode,
            ))
        }),
        Factory::new("mem.uss", "fs:proc:smaps_rollup", |resolved| {
            Ok(footprint::Prepare::new(
                Metric::Uss,
                Source::SmapsRollup,
                resolved.request.mode,
            ))
        }),
        Factory::new("mem.uss", "fs:proc:smaps", |resolved| {
            Ok(footprint::Prepare::new(
                Metric::Uss,
                Source::Smaps,
                resolved.request.mode,
            ))
        }),
        Factory::new("mem.faults", "pmu:sw", |resolved| {
            Ok(Box::new(page_faults::Prepare(resolved.request.mode)))
        }),
        Factory::new("mem.heap.alloc", "preload", |resolved| {
            Ok(Box::new(heap::HeapPrepared::new(
                Which::Alloc,
                resolved.request.mode,
            )?))
        }),
        Factory::new("mem.heap.live", "preload", |resolved| {
            Ok(Box::new(heap::HeapPrepared::new(
                Which::Live,
                resolved.request.mode,
            )?))
        }),
        Factory::new("mem.heap.retained", "preload", |resolved| {
            Ok(Box::new(heap::HeapPrepared::new(
                Which::Retained,
                resolved.request.mode,
            )?))
        }),
    ]
}
