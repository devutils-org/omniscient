//! Collectors for the `mem` family.

mod allocations;
mod cadence;
mod cgroup;
mod footprint;
pub mod heap;
mod numa;
mod page_faults;
mod pressure;
mod stall;
mod swap;
#[cfg(test)]
mod testing;
mod thp;
mod wss;

use super::Factory;
use crate::bpf::{Allocator, Stall};
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
        Factory::new("mem.cgroup", "fs:cgroup", |resolved| {
            Ok(Box::new(cgroup::Prepare(resolved.request.mode)))
        }),
        Factory::new("mem.pressure", "fs:proc", |resolved| {
            Ok(Box::new(pressure::Prepare::new(
                pressure::Scope::Machine,
                resolved.request.mode,
            )?))
        }),
        Factory::new("mem.pressure", "fs:cgroup", |resolved| {
            Ok(Box::new(pressure::Prepare::new(
                pressure::Scope::Cgroup,
                resolved.request.mode,
            )?))
        }),
        Factory::new("mem.swap", "fs:proc", |resolved| {
            Ok(Box::new(swap::Prepare::new(
                swap::Scope::Machine,
                resolved.request.mode,
            )?))
        }),
        Factory::new("mem.swap", "fs:cgroup", |resolved| {
            Ok(Box::new(swap::Prepare::new(
                swap::Scope::Cgroup,
                resolved.request.mode,
            )?))
        }),
        Factory::new("mem.thp", "fs:sys", |resolved| {
            Ok(Box::new(thp::Prepare::new(
                thp::Source::Sys,
                resolved.request.mode,
            )?))
        }),
        Factory::new("mem.thp", "fs:proc", |resolved| {
            Ok(Box::new(thp::Prepare::new(
                thp::Source::Proc,
                resolved.request.mode,
            )?))
        }),
        Factory::new("mem.numa", "fs:proc", |resolved| {
            Ok(Box::new(numa::Prepare(resolved.request.mode)))
        }),
        Factory::new("mem.wss", "fs:proc:clear_refs", |resolved| {
            Ok(Box::new(wss::Prepare::new(resolved.request.mode)?))
        }),
        // The four that go through the kernel rather than through a file. Each
        // carries the request's own keys and predicate, because the resolver
        // credits a `bpf` backend with filtering in the kernel and then leaves
        // its events alone; `allocations.rs` says what that credit costs.
        Factory::new("mem.page_alloc", "bpf:raw_tp", |resolved| {
            Ok(Box::new(allocations::Prepare {
                allocator: Allocator::Pages,
                mode: resolved.request.mode,
                keys: resolved.request.keys.clone(),
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("mem.kmalloc", "bpf:raw_tp", |resolved| {
            Ok(Box::new(allocations::Prepare {
                allocator: Allocator::Slab,
                mode: resolved.request.mode,
                keys: resolved.request.keys.clone(),
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("mem.reclaim", "bpf:raw_tp", |resolved| {
            Ok(Box::new(stall::Prepare {
                stall: Stall::Reclaim,
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("mem.compaction", "bpf:raw_tp", |resolved| {
            Ok(Box::new(stall::Prepare {
                stall: Stall::Compaction,
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
    ]
}
