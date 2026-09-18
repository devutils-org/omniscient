# `cpu`

Fourteen sources, thirteen of which are `perf_event_open` events. This note
records what is implemented, what was measured to check it, and what the family
still needs from the shared layer.

## What runs today

Ten of the fourteen sources. The first seven are counters, read once when the
window closes; the last three are files, read at every tick.

| Source | Backend | Mode | The number |
|---|---|---|---|
| `cpu.cycles` | `pmu` | count | unhalted cycles |
| `cpu.instructions` | `pmu` | count | retired instructions |
| `cpu.ipc` | `pmu` | count | instructions over cycles, from one counter group |
| `cpu.branch_misses` | `pmu` | count | mispredicted branches |
| `cpu.cache_misses` | `pmu` | count | last-level cache misses |
| `cpu.context_switches` | `pmu:sw` | count | switches away from the task |
| `cpu.migrations` | `pmu:sw` | count | moves between cpus |
| `cpu.util` | `fs:proc` | poll, snapshot | the machine's time by state, from `/proc/stat` |
| `cpu.pressure` | `fs:proc` | poll, snapshot | time spent waiting for a cpu, from PSI |
| `cpu.throttle` | `fs:cgroup` | poll, snapshot | time a cgroup was stopped by its quota |

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

## How the files are read

The three polled sources read counters that only grow, so every number they
report is a difference. Three decisions follow from that.

**The baseline is taken when the window opens**, in `start`, not on the first
tick. Otherwise the first point of a timeline would describe the machine's
uptime and the rest would describe a second, plotted on the same axis.

**Every event says which window it covers.** A single read of `/proc/stat` can
only speak about the time since boot, and a single read of PSI can only offer
the kernel's own ten-second average. Both are honest answers; presenting either
as "the last second" would not be. The attribute is `window`, and it is on every
event.

**`busy` excludes `iowait`.** A cpu waiting for a disk is not doing work, and
counting it as busy is how a machine stalled on IO comes to look saturated. The
state is still reported on its own, beside the others, which all sum to one.

`cpu.throttle` reports nothing at all when the cgroup has no `cpu.max`: the
kernel writes `nr_throttled` and `throttled_usec` only once a quota exists, and
their absence means the cgroup *cannot* be throttled, which is a different
statement from "was not". The run says so as a degradation rather than
reporting a zero that reads like a measurement.

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

The polled sources are checked against conditions this machine is made to
satisfy, not against their own arithmetic:

- `cpu.util` — half the cpus are made to spin for a known time, and the
  reported busy share has to reach that fraction of the machine. The
  independent quantity is the work the test does, measured by a clock.
- `cpu.pressure` — twice as many spinning threads as there are cpus guarantees
  a run queue, and the stall counter has to move. It is also the check that the
  counter is not simply always zero.
- `cpu.throttle` — the test creates a cgroup, caps it at a tenth of a cpu, puts
  a spinning process inside it and reads the counters back. Each 100 ms period
  then holds about 90 ms of stopped time, which is a floor the reading cannot
  fall below. Where the cpu controller is not delegated to the user, the test
  says so and stops rather than passing vacuously.

## A test that proved nothing

Worth recording, because it is the failure this family's own invariants exist to
prevent, found in its own tests. The check that `cpu.ipc` publishes both of its
terms ran through the binary, asked for `--emit raw`, and skipped whenever the
command failed — on the assumption that a failure means a machine that cannot
count. The command had been failing since it was written: `cpu.ipc` did not
declare `raw`, so the request was *refused*, and the test reported success for
several runs without ever checking anything.

A machine that cannot serve a request and a request that cannot mean anything
are different answers, and only the first is a reason to stop quietly. The CLI
tests now tell them apart by the message and fail loudly on the second.

## Traps

- **Counting is not cheap sampling.** `-m sample` on these sources is refused
  by name, with the mechanism it would need. A count returned under a sample
  request would be a different measurement wearing the same label.
- **The counter's own window is on the event.** `enabled_ns` and `running_ns`
  are attributes, and they are the honest denominator for a rate: the wall
  clock the run reports also contains the time between the last tick and the
  moment the program was noticed to be gone (up to one 10 ms tick). `--emit raw`
  is how to see them, and how to see a ratio's two terms:

  ```console
  $ omn cpu.ipc -m count -e raw -- ./prog
    raw
      14428414156587  cpu.ipc  task:581350  ipc=2.062 instructions=1218299767 cycles=590811062 enabled_ns=137602206 running_ns=137602206
  ```

  This needed a change to the mode and emit matrix, which had `count`
  producing only `rate` and `summary`. A counted window **is** one event, so
  `raw` is the emit that shows it; without it a derived metric's terms exist on
  the event and reach nobody. The seven counting sources now declare `raw`, and
  `cpu.ipc` consequently has two legitimate emits, so it no longer defaults to
  one — which is the catalog's own defaulting rule, not an exception to it.
- **`cpu.instructions` is stable, never exact.** Interrupt skid, speculation
  and SMT all move it. No test here demands an exact count.
- **SMT and the governor move counters by tens of percent.** Both are in the
  resolved file next to the numbers, which is what makes two runs comparable —
  or visibly not.
- **A ratio hides its terms.** `cpu.ipc` publishes `instructions` and `cycles`
  on the same event, so the ratio can be checked rather than believed.

## Not implemented yet, and why

- `cpu.profile`, and `sample` mode generally, need the sampling path and stack
  capture (see `STATE.md`). Sampling a hardware counter without stacks would
  produce addresses nobody can read.
- `cpu.topdown` needs `pmu:slots` and `pmu:topdown-*`, which are raw event
  encodings per microarchitecture. `src/capabilities.rs` answers **unverified**
  for them today, which is correct: nothing has been checked. Hard-coding one
  vendor's event numbers is the bug this family is most likely to ship.
- `cpu.flow` (Intel PT, CoreSight) needs the AUX buffer, and the decoder is a
  separate deliverable. Capture without decoding is still worth having, but it
  is not worth half a decoder.
- `cpu.oncpu_time` is `bpf:tp` first, and the eBPF layer is Wave 2.
- `cpu.util`'s cgroup backend, and `cpu.pressure`'s, read the same quantities
  inside one cgroup. They are a small addition once a cgroup target resolves to
  a directory, which `cpu.throttle` already does.
- `cpu.cycles`'s `pmu:sw` fallback counts CPU time, not cycles. It is a
  different quantity in a different unit, and it is not wired up rather than
  returning nanoseconds under a counter's name.

## Needs from the shared layer

1. **A `cpu` correlation key.** `/proc/stat` has a line per cpu and the catalog
   accepts a `--cpu` target for `cpu.util`, but the events would all land in one
   series: the key vocabulary has `task`, `proc`, `file`, `sock`, `blk`, `mem`,
   `gpu` and `cgroup`, and nothing for a cpu. A mean across cpus describes no
   cpu, so the target is refused with that reason instead. The same gap makes a
   machine-wide series print as `<unkeyed>`.
2. **Per-cpu counting.** `Prepared::attach` now takes the target whole, so a
   system or cgroup target reaches a collector. A counter still cannot serve
   one: counting every cpu means one event per cpu -- `Scope::Cpu` in
   `src/perf.rs`, which nothing uses yet -- and the privileges for it. The
   refusal names that today rather than counting one process and calling it the
   machine.
3. **An exit code for "valid, but this mechanism is not built".** A mode a
   collector cannot serve comes back as `CollectError::Start`, which exits 1
   like a genuine failure. `NoCollector` already exits 3, and the sample-mode
   refusal belongs in the same class.
4. **A rate over the counter's own window.** `emit::rate` divides by the run's
   wall clock. A counted source knows exactly how long it was enabled, and that
   number is already on the event as `enabled_ns`.
5. **Stack capture** (G6) and the `hist` and `topk` emits (G5), for
   `cpu.profile` and for the `sample` mode of every counter here.
6. **A probe class for raw PMU event encodings**, for `cpu.topdown` and for the
   uncore events `mem` needs. Until then those requirements are unverified,
   which is the correct answer but not a useful one.
