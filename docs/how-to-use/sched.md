# `sched`: why a task is not running

Lazy default: `omn sched -- ./prog` (`sched.switches` counted; `--cost med`
adds `sched.offcpu`, `--cost high` with stacks).

A task that is not on a cpu is either **runnable** (waiting for a cpu: run
queue latency) or **blocked** (waiting for something: I/O, a lock, a timer, a
peer). The two have different sources and different cures.

## Which source for which question

| Question | Source | Mode |
|---|---|---|
| How long is it blocked, and where (stack)? | `sched.offcpu` | `trace -e hist` or `-e stack` |
| How long does it wait for a cpu once woken? | `sched.runqlat` | `trace -e hist` |
| Who wakes whom? | `sched.wakeup` | `trace -e raw` |
| How often is it switched out? | `sched.switches` | `count` |
| Every time slice: run, queue wait, time off | `sched.slices` | `trace -e raw` (`perf sched timehist`) |
| Kernel lock contention | `sched.lock` | `trace -e topk` |
| Userspace futex waits | `sched.futex` | `trace -e hist` |
| pthread mutex contention, with holder | `sched.mutex` | `trace -e topk` |
| Migrations between cores | `sched.migrations` | `count` |
| Time in interrupt handlers | `sched.irq` | `trace -e topk` |
| Interrupt counts per cpu and line | `sched.interrupts` | `poll` (`mpstat -I`) |
| Runnable and blocked task counts, load average | `sched.runqueue` | `poll@1s` (vmstat's `r` and `b`) |
| NUMA balancing decisions | `sched.numa_balance` | `trace` |

And when you want the whole picture of one task: `omn latency -- ./prog`
splits its wall clock into on-cpu, runnable and blocked, names what
dominates, and follows a wait into the task that ran during it
(`docs/triage.md`).

## Examples

```console
omn trace sched.offcpu -m trace -e hist,summary -- ./prog
omn trace sched.offcpu -m trace -e stack --format folded -o out -- ./prog
omn trace sched.runqlat -m trace -e hist --system -d 10s
omn trace sched.wakeup -m trace -e raw -- ./prog
omn trace sched.mutex -m trace -e topk:10 -- ./prog
omn trace sched.futex -m trace -e hist -- ./prog
omn trace sched.runqueue -m poll@1s -e timeline --system -d 1m
omn latency -- ./prog
```

## Targets

- Most tracing sources here take a process target **through eBPF only**
  (`bpf_only`) and **follow every process it starts**: a program on
  `sched_process_fork` adds each child before it runs, so a build's compilers
  are in a `make` run.
- `sched.futex` and `sched.mutex` are per process (no `--system`).
- `sched.mutex` through the preload shim needs a program the tool launches; a
  running process is read through glibc's USDT probes instead.
- `sched.irq`, `sched.interrupts` take `--cpu` or `--system`;
  `sched.runqueue` and `sched.numa_balance` the machine.

## Reading the numbers

- **Blocked, not preempted.** `sched.offcpu` counts a task that left because it
  waited; a task the scheduler took the cpu away from is still runnable, and
  that wait is `sched.runqlat`'s.
- A block is credited in full to the interval in which it **ended**, so a
  window's `total_ns` can exceed the window. A task still blocked at the end is
  in no total, and the run counts how many.
- A wakeup is not a latency: `sched.wakeup` says who made a task runnable;
  `waking -> on cpu` is `sched.runqlat`.
- A switch count is a symptom, not a cause.

## Cost, and the observer effect

- `sched.offcpu`, `sched.runqlat`, `sched.lock` put a program on every context
  switch. **On a contended program they shorten what they measure** (the bench
  measured 9 to 21 % faster): a lock measurement describes the run it
  perturbed.
- `sched.offcpu` with stacks is `high`; without (`bpf:raw_tp`) `med`; the
  totals are kept in the kernel, keyed by task, state, stack and bucket.
- `sched.mutex` is `extreme` through uprobes (about 2 us a lock, it serializes
  hot locks); prefer the preload backend, chosen first for a launch.
