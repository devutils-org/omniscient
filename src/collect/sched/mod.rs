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
mod interrupts;
mod irq;
mod lock;
mod migrations;
mod mutex;
mod mutex_usdt;
mod numa_balance;
mod offcpu;
mod runqlat;
mod runqueue;
mod slices;
mod switch_count;
mod switches;
mod wakeup;

use super::Factory;
use crate::bpf::Whose;
use crate::request::Target;

/// Whether an event passes the filter with `kind` naming what the row says
/// it is -- a hard or soft interrupt, a spin or a mutex wait -- rather than
/// the source.
///
/// The filter layer reads a field named `kind` as the event's own kind
/// before it looks at the attributes, so `kind == "hard"` on `sched.irq`
/// selected nothing while every row carried `kind` = `hard`
/// (`docs/fixes/w22-bpftrace.md`, gap 2). The predicate is asked of the row
/// with its kind set to the attribute for the length of the question.
pub(crate) fn matches_as(
    predicate: &crate::filter::FilterExpr,
    event: &crate::event::Event,
    kind: &str,
) -> bool {
    let mut asked = event.clone();
    kind.clone_into(&mut asked.kind);
    crate::filter::matches(predicate, &asked)
}

/// Each process's parent, for the `ppid` the family's rows carry.
///
/// A launched or named process's descendants are told by the kernel: the
/// fork program writes each child's parent as it is born, and keeps it until
/// a drain has read it after the child is gone (`bpf::Descendants::parents`).
/// Any other process is looked up once in `/proc/<tgid>/stat`, which a process
/// already gone does not answer: such a row has no `ppid` rather than a
/// wrong one.
#[derive(Debug, Default)]
pub(crate) struct Parents {
    known: std::collections::BTreeMap<u32, Option<u32>>,
}

impl Parents {
    /// Takes what the kernel said, which wins over an earlier `/proc` read.
    pub(crate) fn absorb(&mut self, pairs: impl IntoIterator<Item = (u32, u32)>) {
        for (tgid, parent) in pairs {
            self.known.insert(tgid, Some(parent));
        }
    }

    /// The parent of `tgid`, if it is known or still in `/proc`.
    pub(crate) fn of(&mut self, tgid: u32) -> Option<u32> {
        if tgid == 0 {
            return None;
        }
        *self.known.entry(tgid).or_insert_with(|| proc_parent(tgid))
    }
}

/// The fourth field of `/proc/<tgid>/stat`, read past the `comm` in
/// parentheses, which may itself hold spaces and parentheses.
fn proc_parent(tgid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{tgid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

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

/// Reads the mappings of every process a target's waits can come from, every
/// tick, and drops the symbolizers of the ones that have left.
///
/// `followed` is the target and the processes it started that the kernel
/// follows now. A symbolizer snapshots `/proc/<pid>/maps` when it is built,
/// and a launched program and its children are each read at the wrong moment
/// by any single read: before they exec, or after they exit. A process that
/// has left the set keeps its symbolizer one tick longer, so the rows of its
/// last drain still resolve, and is dropped at the next: a build starts
/// thousands of short-lived processes, and each symbolizer holds the parsed
/// symbol tables of every file its process mapped.
pub(crate) fn refresh_followed_symbols(
    symbolizers: &mut std::collections::HashMap<i32, crate::stack::Symbolizer>,
    leaving: &mut Vec<i32>,
    followed: &[u32],
) {
    let kept = |tgid: &i32| followed.contains(&(*tgid as u32));
    for tgid in std::mem::take(leaving) {
        if !kept(&tgid) {
            symbolizers.remove(&tgid);
        }
    }
    *leaving = symbolizers
        .keys()
        .copied()
        .filter(|tgid| !kept(tgid))
        .collect();
    for &tgid in followed {
        match symbolizers.entry(tgid as i32) {
            std::collections::hash_map::Entry::Occupied(mut held) => {
                held.get_mut().refresh();
            }
            std::collections::hash_map::Entry::Vacant(empty) => {
                empty.insert(crate::stack::Symbolizer::for_process(tgid as i32));
            }
        }
    }
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
                debuginfod: resolved.request.symbols
                    == Some(crate::stack::unwind::SymbolSource::Debuginfod),
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
        Factory::new("sched.interrupts", "fs:proc", |resolved| {
            Ok(Box::new(interrupts::Prepare(resolved.request.mode)))
        }),
        Factory::new("sched.slices", "bpf:raw_tp", |resolved| {
            Ok(Box::new(slices::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("sched.runqueue", "fs:proc", |_| {
            Ok(Box::new(runqueue::Prepare))
        }),
        Factory::new("sched.switches", "bpf:raw_tp", |resolved| {
            Ok(Box::new(switch_count::Prepare {
                mode: resolved.request.mode,
                // Applied per drained row; see `migrations::Prepare`.
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("sched.switches", "pmu:sw", |resolved| {
            let prepared: Box<dyn super::Prepared> = match resolved.request.mode {
                // Counted, not sampled: the same software counter
                // `cpu.context_switches` reads, one inherited counter per
                // thread read at the end, with no ring and no second party.
                crate::request::Mode::Count => Box::new(super::cpu::counters::Prepare::new(
                    &super::cpu::counters::SCHED_SWITCHES,
                    crate::request::Mode::Count,
                )?),
                mode => Box::new(switches::Prepare(mode)),
            };
            Ok(Box::new(KernelModeCounted {
                inner: prepared,
                wall_window: resolved.request.mode == crate::request::Mode::Count,
            }))
        }),
    ]
}

/// A `pmu:sw` collector that is attached only where this process may count
/// kernel mode.
///
/// Asked at the attach, not in the factory: a factory's refusal is classified
/// as this build's gap (exit 3, *"the request itself is valid"* on a build
/// that has it), and this one is the machine's -- a paranoia level, or a
/// capability the binary does not carry. At the attach nothing of the target
/// has run yet: a launched program is still held at its gate.
struct KernelModeCounted {
    inner: Box<dyn super::Prepared>,
    /// Whether a task target's counted window is reported over the wall
    /// clock rather than the counter's enabled time ([`WallWindow`]).
    wall_window: bool,
}

impl super::Prepared for KernelModeCounted {
    fn launch_environment(&self) -> Vec<(String, String)> {
        self.inner.launch_environment()
    }

    fn share_launch(&mut self, earlier: &mut dyn super::Prepared) -> bool {
        self.inner.share_launch(earlier)
    }

    fn shareable(&mut self) -> Option<&mut dyn std::any::Any> {
        self.inner.shareable()
    }

    fn requires_launch(&self) -> Option<&'static str> {
        self.inner.requires_launch()
    }

    fn follows_only_later_threads(&self) -> Option<&'static str> {
        self.inner.follows_only_later_threads()
    }

    fn inside(&self) -> Option<Box<dyn super::InsideAccount>> {
        self.inner.inside()
    }

    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn super::Collector>, String> {
        kernel_mode_counted()?;
        let follows_tasks = !matches!(target, Target::System | Target::Cpu(_));
        let collector = self.inner.attach(target)?;
        Ok(if self.wall_window && follows_tasks {
            Box::new(WallWindow(collector))
        } else {
            collector
        })
    }
}

/// A counted `sched.switches` over a task, whose rate is per second of the
/// run rather than per second the task was on a cpu.
///
/// A per-task counter's enabled time advances only while the task is on a
/// cpu, and the results layer divides a counted series by `enabled_ns` when
/// every event carries one. For cycles that is the right denominator; for
/// switches it is not: a task that sleeps 1000 times in a second is on a cpu
/// for a few milliseconds, and `pidstat -w`'s 1000/s came out as 60 000/s
/// (`docs/fixes/w22-sysstat.md`, defect 1). The time is kept, renamed
/// `oncpu_ns`, which is what it is; the rate falls back to the run's window.
struct WallWindow(Box<dyn super::Collector>);

impl WallWindow {
    fn rename(mut events: Vec<crate::event::Event>) -> Vec<crate::event::Event> {
        for event in &mut events {
            for attr in &mut event.attrs {
                if attr.name == "enabled_ns" {
                    attr.name = "oncpu_ns".to_owned();
                }
            }
        }
        events
    }
}

impl super::Collector for WallWindow {
    fn id(&self) -> &crate::event::CollectorId {
        self.0.id()
    }
    fn value_attr(&self) -> &'static str {
        self.0.value_attr()
    }
    fn start(&mut self) -> Result<(), String> {
        self.0.start()
    }
    fn sample(&mut self, seq: u64) -> Result<Vec<crate::event::Event>, super::SampleError> {
        self.0.sample(seq).map(Self::rename)
    }
    fn stop(&mut self) -> Vec<crate::event::Event> {
        Self::rename(self.0.stop())
    }
    fn tick(&self) -> Option<std::time::Duration> {
        self.0.tick()
    }
    fn losses(&self) -> super::Losses {
        self.0.losses()
    }
    fn losses_by_ring(&self) -> Vec<super::RingLoss> {
        self.0.losses_by_ring()
    }
    fn facts(&self) -> Vec<String> {
        self.0.facts()
    }
    fn row_stamp(&self) -> Option<super::RowStamp> {
        self.0.row_stamp()
    }
    fn failure(&self) -> Option<String> {
        self.0.failure()
    }
    fn fields(&self) -> crate::emit::Fields {
        self.0.fields()
    }
    fn provenance(&self) -> crate::stack::Provenance {
        self.0.provenance()
    }
    fn stacks(&self) -> std::collections::BTreeMap<u64, Vec<String>> {
        self.0.stacks()
    }
    fn notes(&self) -> Vec<String> {
        self.0.notes()
    }
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
/// instead. Both are refused here, before the target runs, with the reason,
/// because a zero that is really "not allowed to look" is the wrong number
/// this project is built against.
///
/// The way out named is the one that makes this backend count. The `bpf:*`
/// backends of the chain sit behind `pmu:sw`, serve `count` only, and need
/// `CAP_PERFMON` as well, so granting it is what reaches them too -- and once
/// it is granted, `pmu:sw` counts kernel mode itself and they are not reached.
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
             every count would be zero. Lower it to 1, or give the binary CAP_PERFMON \
             (`make` does, on a development checkout)"
        )),
        Err(error) => Err(format!(
            "`pmu:sw` cannot open a context-switch counter on this process ({error}; \
             `perf_event_paranoid` is {paranoid})"
        )),
    }
}
