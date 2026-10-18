//! Collectors for the `mem` family.

mod allocations;
mod cadence;
mod cgroup;
mod footprint;
pub mod heap;
mod ibs;
mod numa;
mod oom;
mod page_faults;
mod pressure;
mod slab;
mod stall;
mod swap;
mod swapin;
mod system;
#[cfg(test)]
mod testing;
mod thp;
mod victims;
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
            Ok(
                footprint::Prepare::new(Metric::Pss, Source::Smaps, resolved.request.mode)
                    .by_mapping(
                        resolved
                            .request
                            .keys
                            .contains(&crate::request::KeyKind::Mapping),
                    ),
            )
        }),
        Factory::new("mem.uss", "fs:proc:smaps_rollup", |resolved| {
            Ok(footprint::Prepare::new(
                Metric::Uss,
                Source::SmapsRollup,
                resolved.request.mode,
            ))
        }),
        Factory::new("mem.uss", "fs:proc:smaps", |resolved| {
            Ok(
                footprint::Prepare::new(Metric::Uss, Source::Smaps, resolved.request.mode)
                    .by_mapping(
                        resolved
                            .request
                            .keys
                            .contains(&crate::request::KeyKind::Mapping),
                    ),
            )
        }),
        Factory::new("mem.faults", "pmu:sw", |resolved| {
            Ok(Box::new(page_faults::Prepare(resolved.request.mode)))
        })
        .weighing_its_samples(),
        Factory::new("mem.heap.alloc", "preload", |resolved| {
            Ok(Box::new(heap::HeapPrepared::new(
                Which::Alloc,
                resolved.request.mode,
            )?))
        }),
        Factory::new("mem.heap.alloc", "bpf:uprobe", |resolved| {
            Ok(Box::new(heap::UprobePrepare {
                mode: resolved.request.mode,
                emits: resolved.request.emit.clone(),
                target: resolved.request.target.clone(),
            }))
        }),
        Factory::new("mem.heap.live", "bpf:uprobe", |resolved| {
            Ok(Box::new(heap::UprobeLivePrepare {
                which: Which::Live,
                mode: resolved.request.mode,
                target: resolved.request.target.clone(),
            }))
        }),
        Factory::new("mem.heap.retained", "bpf:uprobe", |resolved| {
            Ok(Box::new(heap::UprobeLivePrepare {
                which: Which::Retained,
                mode: resolved.request.mode,
                target: resolved.request.target.clone(),
            }))
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
        Factory::new("mem.swap", "perf:kprobe", |resolved| {
            Ok(Box::new(swapin::Prepare::new(resolved.request.mode)?))
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
        Factory::new("mem.oom", "fs:proc", |resolved| {
            Ok(Box::new(oom::Prepare::new(
                oom::Scope::Machine,
                resolved.request.mode,
            )?))
        }),
        Factory::new("mem.oom", "fs:cgroup", |resolved| {
            Ok(Box::new(oom::Prepare::new(
                oom::Scope::Cgroup,
                resolved.request.mode,
            )?))
        }),
        Factory::new("mem.oom", "bpf:raw_tp", |resolved| {
            Ok(Box::new(victims::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("mem.slab", "fs:proc", |resolved| {
            Ok(Box::new(slab::Prepare::new(resolved.request.mode)?))
        }),
        Factory::new("mem.system", "fs:proc", |resolved| {
            Ok(Box::new(system::Prepare::new(resolved.request.mode)?))
        }),
        Factory::new("mem.numa", "fs:sys", |resolved| {
            Ok(Box::new(system::NodePrepare::new(resolved.request.mode)?))
        }),
        Factory::new("mem.wss", "fs:proc:clear_refs", |resolved| {
            Ok(Box::new(wss::Prepare::new(resolved.request.mode)?))
        }),
        // One unit, two questions: which loads waited and for how long, and
        // which lines two cores took from each other. `ibs.rs` says what each
        // keeps of the ops the unit tags.
        Factory::new("mem.latency", "pmu:ibs", |resolved| {
            Ok(Box::new(ibs::Prepare {
                question: ibs::Question::Latency,
                mode: resolved.request.mode,
                keys: resolved.request.keys.clone(),
                callchain: ibs_callchain(resolved)?,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("mem.sharing", "pmu:ibs", |resolved| {
            Ok(Box::new(ibs::Prepare {
                question: ibs::Question::Sharing,
                mode: resolved.request.mode,
                keys: resolved.request.keys.clone(),
                callchain: ibs_callchain(resolved)?,
                filter: resolved.request.filter.clone(),
            }))
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

/// Stacks cost a word per frame on every tagged op, so they are taken only
/// when the request asks for them: `--emit stack`, or a `--stack` mode.
fn ibs_callchain(
    resolved: &crate::resolve::ResolvedRequest,
) -> Result<crate::perf::Callchain, String> {
    let request = &resolved.request;
    if request.stack.is_none() && !request.emit.contains(&crate::request::Emit::Stack) {
        return Ok(crate::perf::Callchain::Off);
    }
    // The IBS sources walk frame pointers and nothing else: their samples are
    // symbolized by an unwinder that reads no copied stack. So `auto` means
    // `fp` here, a forced mode that needs more is refused by name, and the
    // kernel half is always asked for -- IBS is opened only where kernel
    // profiling is allowed, and its unit refuses the exclude bits.
    let choice = crate::stack::choose(request, &resolved.environment)?;
    match choice.mode {
        crate::request::StackMode::None => Ok(crate::perf::Callchain::Off),
        crate::request::StackMode::Fp => Ok(crate::perf::Callchain::BOTH_HALVES),
        other if choice.forced => Err(format!(
            "`--stack {other}` is not served by the IBS sources: their stacks are the kernel's \
             frame-pointer walk, so ask for `--stack fp`"
        )),
        _ => Ok(crate::perf::Callchain::BOTH_HALVES),
    }
}
