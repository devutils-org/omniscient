//! Collectors for the `sched` family.
//!
//! The family that measures the time a program spends *not* running, which is
//! invisible to every cpu profiler: a thread blocked on a lock, on a disk, or
//! on another thread costs nothing in a cpu profile and everything in latency.
//!
//! Two backends are here, and the gap between them is the family's story.
//! `sched.wakeup` goes through the eBPF layer and records both ends of every
//! wakeup, which is what lets blocked time be attributed to whoever ended it.
//! `sched.switches` goes through `perf_event_open`, needs no privileges, and
//! can only say that a switch happened.

mod switches;
mod wakeup;

use super::Factory;

/// The collectors this family provides.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("sched.wakeup", "bpf:raw_tp", |resolved| {
            Ok(Box::new(wakeup::Prepare(resolved.request.mode)))
        }),
        Factory::new("sched.switches", "pmu:sw", |resolved| {
            Ok(Box::new(switches::Prepare(resolved.request.mode)))
        }),
    ]
}
