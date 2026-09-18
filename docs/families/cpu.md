# `cpu`

Fourteen sources, thirteen of which are `perf_event_open` events. This note
records what is implemented, what was measured to check it, and what the family
still needs from the shared layer.

## What runs today

All of these are `count` mode: the counter is opened over the target, and read
once, when the window closes.

| Source | Backend | The number |
|---|---|---|
| `cpu.cycles` | `pmu` | unhalted cycles |
| `cpu.instructions` | `pmu` | retired instructions |
| `cpu.ipc` | `pmu` | instructions over cycles, from one counter group |
| `cpu.branch_misses` | `pmu` | mispredicted branches |
| `cpu.cache_misses` | `pmu` | last-level cache misses |
| `cpu.context_switches` | `pmu:sw` | switches away from the task |
| `cpu.migrations` | `pmu:sw` | moves between cpus |

```console
$ omn cpu.ipc -m count -e summary -- ./prog
work per cycle  via pmu  (cpu.ipc)
  1 events over 814.76ms, stopped because the launched program exited with exit status 0
  no loss: every read succeeded and no tick ran late

  summary
    task:352616    n=1     min 2.110  mean 2.110  max 2.110  last 2.110
```

`examples/cpu-counters.yml` runs three of them over one program in one window.

## How the counters are read

Four decisions decide whether these numbers mean what they say.

**One group per source.** The counters of a source are opened as a group, so
the kernel schedules them onto the hardware together or not at all. `cpu.ipc`
is the reason: a ratio of two counters that were live in different slices of
time is a ratio of two different windows and describes neither.

**Read after stopping.** The leader's `DISABLE` freezes every member at once,
so two counters read a microsecond apart still describe the same window. That
also avoids `PERF_FORMAT_GROUP`, which several kernels refuse for an inherited
event.

**Inherited.** A counting event needs no ring buffer, and that is what lets it
be inherited: the count follows the threads and children the target starts. A
program that does its work in a thread pool is counted rather than reported as
having done almost nothing. The sampling path in `src/perf.rs` cannot do this —
the kernel refuses to map a ring for an inherited per-task event — which is why
`mem.faults` states that it sees the target task only.

**Multiplexing is reported, never hidden.** When more counters are open than
the hardware has, the kernel time-shares them and returns what was counted
while they were on. The scaled value is an extrapolation, so the event carries
`<counter>_raw` beside it and the run says so in its output. A machine with six
counters and sixteen open groups produces exactly this, and the test
`a_time_shared_counter_reports_that_its_value_is_scaled` exercises it.

**User mode only is a different measurement.** Where `perf_event_paranoid`
forbids counting kernel mode, the group is reopened without it and the run
reports `this machine would only open ... for user mode`. It is not a lesser
version of the same number: it excludes every system call the program makes.

## Ground truth

A counter cannot be checked against another tool — that would only prove the
two agree — so it is checked against a loop whose retired-instruction count is
known by construction: `dec` plus `jnz`, two instructions per iteration.

Fifty million iterations, so a hundred million instructions, on the development
machine (Ryzen 9 3900X, `performance` governor, SMT on, `perf_event_paranoid`
at -1):

```
expected   100 000 000
counted    100 124 761  100 088 924  100 105 842  100 102 860  100 110 800
```

Which is +0.105 % high and a coefficient of variation of 0.012 %. The excess is
the harness's own instructions between the enable and the disable, plus the
interrupts handled in this task's context, which are counted because kernel
mode is counted here. The catalog claims `determinism: high` for
`cpu.instructions`; 0.012 % is what that claim is worth on this machine, and
the test fails above 1 % because at that point the environment, not the code,
is what needs fixing.

`cpu.context_switches` is checked against the kernel's own counter:
`voluntary_ctxt_switches` plus `nonvoluntary_ctxt_switches` in
`/proc/thread-self/status`. Fifty sleeps produced fifty switches on both sides,
exactly. Note `thread-self` rather than `self`: a test runs on a thread cargo
started, and the process leader's counters describe a thread that is idle.

The inheritance claim has its own test: the work is done in a thread started
after the window opened, and the count has to include it.

## Traps

- **Counting is not cheap sampling.** `-m sample` on these sources is refused
  by name, with the mechanism it would need. A count returned under a sample
  request would be a different measurement wearing the same label.
- **The counter's own window is on the event.** `enabled_ns` and `running_ns`
  are attributes, and they are the honest denominator for a rate: the wall
  clock the run reports also contains the time between the last tick and the
  moment the program was noticed to be gone (up to one 10 ms tick).
- **`cpu.instructions` is stable, never exact.** Interrupt skid, speculation
  and SMT all move it. No test here demands an exact count.
- **SMT and the governor move counters by tens of percent.** Both are in the
  resolved file next to the numbers, which is what makes two runs comparable —
  or visibly not.
- **A ratio hides its terms.** `cpu.ipc` publishes `instructions` and `cycles`
  on the same event, so the ratio can be checked rather than believed.

## Not implemented yet, and why

- `cpu.profile`, and `sample` mode generally, need the sampling path and stack
  capture (G6 in `BRIEF.md`). Sampling a hardware counter without stacks would
  produce addresses nobody can read.
- `cpu.topdown` needs `pmu:slots` and `pmu:topdown-*`, which are raw event
  encodings per microarchitecture. `src/capabilities.rs` answers **unverified**
  for them today, which is correct: nothing has been checked. Hard-coding one
  vendor's event numbers is the bug this family is most likely to ship.
- `cpu.flow` (Intel PT, CoreSight) needs the AUX buffer, and the decoder is a
  separate deliverable. Capture without decoding is still worth having, but it
  is not worth half a decoder.
- `cpu.oncpu_time` is `bpf:tp` first, and the eBPF layer is Wave 2.
- `cpu.util`, `cpu.pressure` and `cpu.throttle` are procfs and cgroup polls
  that read whole-machine or whole-cgroup files. They are blocked on the runner,
  see below.
- `cpu.cycles`'s `pmu:sw` fallback counts CPU time, not cycles. It is a
  different quantity in a different unit, and it is not wired up rather than
  returning nanoseconds under a counter's name.

## Needs from the shared layer

1. **Per-cpu counting.** `Prepared::attach` now takes the target whole, so a
   system or cgroup target reaches a collector. A counter still cannot serve
   one: counting every cpu means one event per cpu -- `Scope::Cpu` in
   `src/perf.rs`, which nothing uses yet -- and the privileges for it. The
   refusal names that today rather than counting one process and calling it the
   machine.
2. **An exit code for "valid, but this mechanism is not built".** A mode a
   collector cannot serve comes back as `CollectError::Start`, which exits 1
   like a genuine failure. `NoCollector` already exits 3, and the sample-mode
   refusal belongs in the same class.
3. **A rate over the counter's own window.** `emit::rate` divides by the run's
   wall clock. A counted source knows exactly how long it was enabled, and that
   number is already on the event as `enabled_ns`.
4. **Stack capture** (G6) and the `hist` and `topk` emits (G5), for
   `cpu.profile` and for the `sample` mode of every counter here.
5. **A probe class for raw PMU event encodings**, for `cpu.topdown` and for the
   uncore events `mem` needs. Until then those requirements are unverified,
   which is the correct answer but not a useful one.
