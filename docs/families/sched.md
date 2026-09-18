# `sched`

The family that measures the time a program spends **not** running. A thread
blocked on a lock, on a disk, or on another thread costs nothing in a cpu
profile and everything in latency, which is why this is the family most often
missing from a performance investigation that went nowhere.

The family that also owns the eBPF layer: see `docs/bpf-layer.md`.

## State

| Source | Backend | State |
|---|---|---|
| `sched.wakeup` | `bpf:raw_tp` | implemented: one record per wakeup, both ends |
| `sched.switches` | `pmu:sw` | implemented: one sample per switch, unprivileged |
| `sched.switches` | `bpf:tp` | declared, not written |
| the other eight | `bpf:*` | declared, not written; they wait on what is listed at the end of `docs/bpf-layer.md` |

## Why `sched_waking` and not `sched_wakeup`

Both fire for the same wakeup. Only the first one runs in the waker's context.

`sched_waking` is raised by `try_to_wake_up` on the cpu of the task doing the
waking, so `bpf_get_current_pid_tgid()` inside it is the waker.
`sched_wakeup` can be raised on the woken task's cpu after an inter-processor
interrupt, where the current task is whatever happened to be running there. A
waker read from `sched_wakeup` is therefore wrong precisely on cross-cpu
wakeups — which are the interesting ones, because a cross-cpu wakeup is what a
thread pool, a lock handoff and an I/O completion all look like.

This is recorded because the catalog originally required both tracepoints, and
the choice between them looks arbitrary until it is written down.

## What a wakeup is not

`sched.wakeup` reports one event per wakeup and its unit is `count`, not `ns`.
The catalog said `ns`, and that was wrong in a way worth naming: a wakeup has
no duration. The nanoseconds this family is about appear only when a wakeup is
paired with what the woken task did next —

- wakeup → on cpu is the run queue wait, which is `sched.runqlat`;
- blocked → wakeup is the blocked time, which is `sched.offcpu`.

Reporting a wakeup stream in nanoseconds would have invited exactly the
reading the tool exists to prevent: a number that looks like a latency and is a
timestamp.

## Ground truth

- **`sched.switches`** is checked against `/proc/thread-self/status`, which
  counts the same switches from the other side of the kernel. The first version
  of that test read `/proc/self/status` and compared against a constant zero:
  `self` is the thread group leader, which in a test binary is asleep while
  another thread does the work, and a perf event follows the task that opened
  it. A ground-truth test that reads the wrong task is worse than none.
- **`sched.wakeup`** is checked against an injected, known count: a program that
  blocks *n* times is woken at least *n* times, because a timer expiring is a
  wakeup and there is no other way back onto the cpu.

Both floors, never equalities. The kernel is free to add wakeups — a signal, a
page fault on a cold binary, a preemption — and a test that demanded an exact
count would be asserting that nothing else happens on the machine.

## What the first privileged run measured

The program loads, the verifier accepts it, it attaches to `sched_waking`, and
the events are the causal chain they claim to be. Running
`omn sched.wakeup --emit raw -- /bin/sh -c 'for i in 1 2 3 4 5; do sleep 0.02; done'`
produced, in order: one wakeup of the launched process by `omn` itself -- the
launch gate being released, before the `exec`, so the woken task is still named
`omn` -- then five wakeups of the shell, each by a different `sleep` child
exiting. Five blocks, five wakers, named.

And one thing worth knowing before trusting a waker.

**A waker is the task that was running when the wakeup was issued, which is not
always the task that caused it.** A wakeup issued from interrupt or softirq
context is credited to whatever was interrupted, because that is what `current`
is there. Measured, with a process that does nothing at all:

```
$ omn sched.wakeup --emit raw -- /bin/sleep 0.3
  wakee kworker/1:2   waker 772820   (the sleeping process)
  wakee rcu_preempt   waker 772820
  wakee tailscaled    waker 772820
  wakee sleep         waker 0        (the idle task: a timer expiry)
```

A process asleep for 300 ms wakes nobody. Those three are the timer and network
softirqs running on its cpu, in its context. A waker of pid 0 is the idle task,
which is what a timer expiry looks like from here.

This is not a bug in the collector and not something `sched_wakeup` would fix --
it is what the kernel knows at that point. It is stated as a caveat on the
source, so it reaches the user beside their own numbers rather than living here.
Telling the two apart needs the interrupt state at the time of the wakeup, which
on x86 is a per-cpu `preempt_count` rather than a field of `task_struct`, so it
needs `bpf_this_cpu_ptr` and a ksym lookup the layer does not have yet. Until
then the waker is evidence, not proof.

## Traps

- A switch count is a symptom. A *voluntary* switch is a task blocking on
  something; an *involuntary* one is the scheduler taking the cpu away. They
  call for opposite responses, and `pmu:sw` counts both as one. Only the eBPF
  backend, reading `prev_state` at `sched_switch`, can tell them apart — which
  is why `bpf:tp` is ahead of `pmu:sw` in the chain even though `pmu:sw` is free
  and always available.
- `PERF_COUNT_SW_CONTEXT_SWITCHES` counts switches *out*. Ten blocking waits are
  ten events, not twenty.
- The perf event does not follow children: `inherit` cannot be combined with a
  per-task ring buffer (`src/perf.rs` states this). Observing a shell that
  forks measures the shell.
- `sched.offcpu` with stacks is expensive on switch-heavy workloads and must
  aggregate in the kernel — stack id to counter — rather than stream raw events.
  It is not written yet for that reason, not for lack of a tracepoint.

## What this family carries, and what it does not

`sched.wakeup` declares the `task` and `cgroup` keys. The `bpf:raw_tp` backend
carries `task` only. The cgroup of a *woken* task is not the cgroup of the
waker, so `bpf_get_current_cgroup_id()` would answer a different question than
the one the key names; it will be read from the woken `task_struct` when a
correlation actually needs it, and until then the key is absent rather than
approximated.

## Needs from the shared layer

- **G5, `hist` and `topk`.** `sched.runqlat` and `sched.offcpu` are histogram
  sources; both are declared with `emits: [hist, ...]` and neither can land
  usefully while `hist` does not exist.
- **G6, stack capture.** `sched.offcpu` without stacks tells you how long, not
  where, which the catalog already states as its degradation. The `bpf:tp:stacks`
  backend needs `bpf_get_stackid`, a stack map, and symbolization.
- **G8, perf counting mode.** `sched.switches --mode count` is refused today
  with that reason: a counting request should open no ring and read the counter
  at the end, and sampling every switch to add the samples up would be a
  different, more expensive measurement wearing the cheap one's name.
- **`join`: the `wakeup` match kind.** The join engine implements interval
  matching only, and refuses `wakeup` by name. `sched.wakeup` exists to feed
  it; until the match kind lands, the events are correct and uncorrelated.

## Found here, belongs elsewhere

Two findings from this thread's runs that are not this family's to fix.

**`perf::tests::a_page_fault_sample_carries_the_address_that_faulted` is flaky,
and the collector is not at fault.** It touches 64 pages of a fresh `Vec` and
demands 64 faults. Run alone it gets them; run inside the full suite it can get
two, and reports no loss, which reads as a silent shortfall. Measured here:

```
first allocation:  64 faults
second allocation: 64 faults
third allocation:   0 faults
```

The allocator hands back pages that are already resident, so touching them
faults nothing. The test's premise -- that touching a page faults -- holds for
fresh memory only, and a suite that has already allocated and freed leaves the
arena warm. A fix belongs with the `mem` thread or with `src/perf.rs`: map the
block with `mmap(MAP_ANONYMOUS)` so the pages are guaranteed fresh, or check
the sample count against the minor-fault delta in `/proc/thread-self/stat`
instead of against a constant. This is invariant 5 in miniature -- the
surprising number was right and the expectation was wrong.

**A throttled perf event loses samples silently.** `PERF_RECORD_THROTTLE` says
the kernel decided an event costs too much and sampled less of it, up to
`kernel.perf_event_max_sample_rate`. `SampledEvent` decodes the record and
`src/collect/mem/page_faults.rs` ignores it, so a throttled run looks complete.
`sched.switches` counts it into `Losses::dropped` with a reason naming the
sysctl; the shared layer would do better to expose `SampledEvent::throttled()`
so every perf-based collector gets it, and `Losses` to carry a `throttled`
count of its own, since the number of lost events in a throttled period is not
known.

## Shared files this thread had to touch

Listed so the coordinator can see them at a glance; both are one-liners that a
family collector cannot avoid.

- `src/lib.rs`: `pub mod bpf;`.
- `src/capabilities.rs`: the `bpf` and `raw_tracepoint:<name>` probe classes,
  which are G4 and belong to the layer this thread owns. Both delegate to
  `src/bpf/`; no probing logic was added to `capabilities.rs` itself.
