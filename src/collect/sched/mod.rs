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
//! the measurement is worth. `sched.switches` goes through `perf_event_open`
//! first, and can only say that a switch happened; the `sched_switch` program
//! behind it counts a cgroup's, and says which were voluntary.
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
mod switch_count;
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
        Factory::new("sched.switches", "bpf:raw_tp", |resolved| {
            Ok(Box::new(switch_count::Prepare {
                mode: resolved.request.mode,
                // Applied per drained row; see `migrations::Prepare`.
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("sched.switches", "pmu:sw", |resolved| {
            kernel_mode_counted()?;
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

/// Whether this process may count what happens in kernel mode, asked the way
/// `pmu:sw` counts: one context-switch counter on this process.
///
/// A context switch is kernel mode and nothing else. Where
/// `perf_event_paranoid` is 2 -- the kernel's default, and the CI's second
/// runner -- an unprivileged process may open a counter for user mode only,
/// the counter layer reopens it that way and notes that kernel-mode events are
/// not in the count, and for this event that is every event: the runner's
/// `sched.switches -m count` read 0 for a program whose threads counted 406
/// switches of their own. A sampled run was refused at start with `EACCES`
/// instead. Both are refused here, before anything is launched, with the
/// reason, because a zero that is really "not allowed to look" is the wrong
/// number this project is built against.
fn kernel_mode_counted() -> Result<(), String> {
    let counter = crate::perf::Counter::software(
        "context_switches",
        perf_event_open_sys::bindings::PERF_COUNT_SW_CONTEXT_SWITCHES,
    );
    let paranoid = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| "unreadable".to_owned());
    match crate::perf::CounterGroup::open(&[counter], crate::perf::Scope::Process(0)) {
        Ok(group) if !group.kernel_excluded() => Ok(()),
        Ok(_) => Err(format!(
            "`pmu:sw` counts context switches, which happen only in kernel mode, and this \
             process may count its own user mode only (`perf_event_paranoid` is {paranoid}): \
             every count would be zero. Lower it to 1, grant CAP_PERFMON, or give the binary \
             CAP_BPF and CAP_PERFMON for the `bpf:*` backends ahead of this one"
        )),
        Err(error) => Err(format!(
            "`pmu:sw` cannot open a context-switch counter on this process ({error}; \
             `perf_event_paranoid` is {paranoid})"
        )),
    }
}
