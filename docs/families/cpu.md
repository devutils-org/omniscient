# `cpu`

Fourteen sources, ten of which are `perf_event_open` events. This note
records what is implemented, what was measured to check it, and what the family
still needs from the shared layer.

## What runs today

Thirteen of the fourteen sources; `cpu.flow` is the one left. The first eight
are counters, read once when the window closes; `cpu.profile` samples; the last
four are files, read at every tick.

| Source | Backend | Mode | The number |
|---|---|---|---|
| `cpu.cycles` | `pmu` | count, poll | unhalted cycles |
| `cpu.instructions` | `pmu` | count, poll | retired instructions |
| `cpu.ipc` | `pmu` | count, poll | instructions over cycles, from one counter group |
| `cpu.branch_misses` | `pmu` | count, poll | mispredicted branches |
| `cpu.cache_misses` | `pmu` | count, poll | last-level cache misses |
| `cpu.context_switches` | `pmu:sw` | count, poll | switches away from the task |
| `cpu.migrations` | `pmu:sw` | count, poll | moves between cpus |
| `cpu.topdown` | `pmu` | count | where the issue slots went: retiring, bad speculation, front end, back end |
| `cpu.profile` | `pmu:sw` | sample | where the samples landed, with call stacks |
| `cpu.oncpu_time` | `fs:proc` | poll | a task's cpu time, from `/proc/<pid>/stat`, at tick granularity |
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

**Targets.** Every counter, `cpu.topdown`, `cpu.profile` and `mem.faults` serve
a process, a thread, a launch, a cpu set, the machine and, since the thirteenth
wave, **a cgroup**: one event per cpu that the kernel filters to the cgroup's
tasks (`PERF_FLAG_PID_CGROUP`). A count is reported as the cgroup's sum over
the cpus; a sample is keyed on its task, its cpu and the cgroup. The ground
truth is a child inside the cgroup doing one unit of work while its parent does
ten outside it, and the run counts the one and none of the ten. A container is
refused (`container: no`), because nothing resolves a container to its cgroup
yet, and the refusal says so: *"a container has to be resolved to its cgroup
first"*, with `--cgroup` as what to pass.

**`poll` on the counters** reads each counter at every tick instead of once at
the end, so a counted source has a timeline.

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

- `cpu.profile` samples through `pmu:sw` only. Its `pmu` backend, sampling
  the hardware cycle counter, has no collector.
- `cpu.topdown` is written and collectable, and out of reach on the development
  machine. Its five events, `slots` and the four `topdown-*`, are read from what
  the kernel publishes under `/sys/bus/event_source/devices/cpu/events`, which
  is how "map per CPU family, never hard-code event names" is honoured, and the
  probe answers `missing` by name on a machine that does not publish them. The
  method needs a pipeline that counts issue slots -- Intel from Icelake, AMD
  from Zen 4 -- and the development machine is a Zen 2, so the half of the
  ground-truth test that runs here is the refusal.
- `cpu.flow` (Intel PT, CoreSight) needs the AUX buffer, and the decoder is a
  separate deliverable. Capture without decoding is still worth having, but it
  is not worth half a decoder.
- `cpu.oncpu_time` is served by its `fs:proc` backend, in `poll`, at the
  kernel's tick granularity. Its `bpf:raw_tp` and `bpf:tp` backends, and with
  them `trace`, have no collector yet.
- `cpu.util`'s cgroup backend, and `cpu.pressure`'s, read the same quantities
  inside one cgroup. They are a small addition once a cgroup target resolves to
  a directory, which `cpu.throttle` already does.
- `cpu.cycles`'s `pmu:sw` fallback counts CPU time, not cycles. It is a
  different quantity in a different unit, and it is not wired up rather than
  returning nanoseconds under a counter's name.

## Needs from the shared layer

1. **Per-cpu counting is built, and what it costs is a privilege.**
   `perf::PerCpuGroups` opens one group per cpu, `collect/cpu/percpu.rs`
   reports one event each, keyed on `KeyValue::Cpu`, and `--system` is every
   online cpu. Counting a cpu counts other users' work, so the refusal names
   `perf_event_paranoid` or `CAP_PERFMON`, and a per-task count is never
   substituted for one. (The `cpu` correlation key this list used to ask for
   exists too: `cpu.util --cpu` produces one event per core keyed on it.)
2. **An exit code for "valid, but this mechanism is not built".** A mode a
   collector cannot serve comes back as `CollectError::Start`, which exits 1
   like a genuine failure. `NoCollector` already exits 3, and the sample-mode
   refusal belongs in the same class.
3. **A rate over the counter's own window.** `emit::rate` divides by the run's
   wall clock. A counted source knows exactly how long it was enabled, and that
   number is already on the event as `enabled_ns`.
4. **Stack capture** (G6) and the `hist` and `topk` emits (G5), for
   `cpu.profile` and for the `sample` mode of every counter here.
5. **A probe class for raw PMU event encodings**, for the uncore events `mem`
   needs. Until then those requirements are unverified,
   which is the correct answer but not a useful one.
