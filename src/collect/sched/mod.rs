//! Collectors for the `sched` family.
//!
//! The family that measures the time a program spends *not* running, which is
//! invisible to every cpu profiler: a thread blocked on a lock, on a disk, or
//! on another thread costs nothing in a cpu profile and everything in latency.
//!
//! Three backends are here, and the gaps between them are the family's story.
//! `sched.wakeup` goes through the eBPF layer and records both ends of every
//! wakeup, which is what lets blocked time be attributed to whoever ended it.
//! `sched.offcpu` goes through the same layer and counts in the kernel rather
//! than streaming, because a switch-heavy workload produces more events than
//! the measurement is worth. `sched.switches` goes through `perf_event_open`,
//! needs no privileges, and can only say that a switch happened.
//!
//! `sched.mutex` is the one that is not in the kernel at all: the preload shim
//! the heap sources use, pointed at `pthread_mutex_lock`, so it sees the
//! uncontended acquisitions that never reach a futex.

mod futex;
mod irq;
mod lock;
mod migrations;
mod mutex;
mod mutex_usdt;
mod numa_balance;
mod offcpu;
mod runqlat;
mod switches;
mod wakeup;

use super::Factory;
use crate::bpf::Whose;
use crate::request::Target;

/// Whose tasks one of this family's programs narrows to, or the refusal.
///
/// Every eBPF collector here compares a task in the kernel, and until the
/// thirteenth wave each wrote its own target match: six of them refused a
/// cgroup, and the ones that took `--tid` compared it against the thread
/// group. `why` is the collector's own sentence for what it attributes per
/// task, which the refusal ends with, followed by what does work.
pub(crate) fn whose(target: &Target, why: &str) -> Result<Whose, String> {
    Whose::of(target).map_err(|reason| {
        format!(
            "{reason}, and {why}; name a process with --pid, a thread with --tid, a cgroup with \
             --cgroup, or use --system to watch the whole machine"
        )
    })
}

/// The collectors this family provides.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("sched.wakeup", "bpf:raw_tp", |resolved| {
            Ok(Box::new(wakeup::Prepare {
                mode: resolved.request.mode,
                // Applied per drained wakeup; see `wakeup::Prepare`.
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("sched.offcpu", "bpf:raw_tp", |resolved| {
            Ok(Box::new(offcpu::Prepare {
                mode: resolved.request.mode,
                // Symbolising costs a read of every process's mappings, and a
                // machine-wide run meets every process; the emit layer keeps
                // the table only when `stack` was asked for.
                wants_stacks: resolved.request.emit.contains(&crate::request::Emit::Stack),
                // Applied per drained row; see `offcpu::Prepare`.
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("sched.runqlat", "bpf:raw_tp", |resolved| {
            Ok(Box::new(runqlat::Prepare {
                mode: resolved.request.mode,
                // Carried into the collector because the resolver credits every
                // `bpf` backend with filtering in the kernel and then leaves
                // its events alone; see `runqlat::Prepare`.
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("sched.migrations", "bpf:raw_tp", |resolved| {
            Ok(Box::new(migrations::Prepare {
                mode: resolved.request.mode,
                // Applied per drained row; see `migrations::Prepare`.
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("sched.lock", "bpf:raw_tp", |resolved| {
            Ok(Box::new(lock::Prepare {
                mode: resolved.request.mode,
                // Symbolising costs a read of every waiting process's mappings,
                // and a machine-wide run meets every process; the emit layer
                // keeps the table only when `stack` was asked for.
                wants_stacks: resolved.request.emit.contains(&crate::request::Emit::Stack),
                // Carried into the collector because the resolver credits every
                // `bpf` backend with filtering in the kernel and then leaves
                // its events alone; see `lock::Prepare`.
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("sched.futex", "bpf:raw_tp", |resolved| {
            Ok(Box::new(futex::Prepare {
                mode: resolved.request.mode,
                wants_stacks: resolved.request.emit.contains(&crate::request::Emit::Stack),
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("sched.irq", "bpf:raw_tp", |resolved| {
            Ok(Box::new(irq::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("sched.numa_balance", "bpf:raw_tp", |resolved| {
            Ok(Box::new(numa_balance::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("sched.mutex", "preload", |resolved| {
            mutex::prepare(&resolved.request.emit)
        }),
        Factory::new("sched.mutex", "bpf:usdt", |resolved| {
            Ok(Box::new(mutex_usdt::Prepare {
                target: resolved.request.target.clone(),
                filter: resolved.request.filter.clone(),
                emits: resolved.request.emit.clone(),
            }))
        }),
        Factory::new("sched.switches", "pmu:sw", |resolved| {
            match resolved.request.mode {
                // Counted, not sampled: the same software counter
                // `cpu.context_switches` reads, one inherited counter per
                // thread read at the end, with no ring and no second party.
                crate::request::Mode::Count => Ok(Box::new(super::cpu::counters::Prepare::new(
                    &super::cpu::counters::SCHED_SWITCHES,
                    crate::request::Mode::Count,
                )?)),
                mode => Ok(Box::new(switches::Prepare(mode))),
            }
        }),
    ]
}
