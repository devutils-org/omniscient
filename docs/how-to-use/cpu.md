# `cpu`: where the cpu time goes, and how well the core runs it

Lazy default: `omn cpu -- ./prog` (a 99 Hz profile with stacks; `--cost med`
997 Hz, `--cost high` adds DWARF unwinding).

## Which source for which question

| Question | Source | Mode | Notes |
|---|---|---|---|
| Which functions burn the cpu? | `cpu.profile` | `sample@997hz -e stack` | the on-cpu time profile; `--format folded` or `speedscope` for a flame graph |
| How much cpu did it actually use? | `cpu.oncpu_time` | `count` (`perf stat`'s task-clock), `poll`, `trace` | per thread through eBPF, the process total through procfs |
| How many instructions, cycles, IPC? | `cpu.instructions`, `cpu.cycles`, `cpu.ipc` | `count` | `cpu.ipc` reads both in one counter group; read it beside `cpu.instructions`, never alone |
| Is branch prediction the problem? | `cpu.branch_misses`, `cpu.branches` | `count` | the ratio is misses over branches, same window |
| Front-end, back-end, bad speculation? | `cpu.topdown` | `count -e raw` | Intel from Icelake, AMD from Zen 4 only; `summary` follows `retiring` |
| Is it cache-bound? | `cpu.cache_misses` (and `mem.cache_*`) | `count` | a miss count is not a miss rate |
| Is the machine (or a core) busy? | `cpu.util` | `poll@1s` | `--system`, `--cpu`, `--cgroup` only: per state (user, sys, iowait, steal) |
| Is it waiting for a cpu? | `cpu.pressure` | `poll@1s` | PSI; `sched.runqlat` says how long each wait was |
| Was my container throttled? | `cpu.throttle` | `poll` | `--cgroup` only; a throttled run is not comparable, discard it |
| How often is it switched out, moved? | `cpu.context_switches`, `cpu.migrations` | `count` | says how often, not how long: that is `sched.offcpu` |
| Exhaustive control flow | `cpu.flow` | `trace` | Intel PT / CoreSight; decoding is not built |

Any counting source samples too: `cpu.cycles -m sample@997hz -e stack` is
`perf record`'s default, a **cycles** profile (a share of cycles, not of
time). `cpu.profile` is the **time** profile.

## Examples

```console
omn trace cpu.profile -m sample@997hz -e stack -- ./prog
omn trace cpu.profile -m sample@997hz -e stack --format folded -o out -- ./prog
omn trace cpu.profile -m sample@997hz -k object -e topk -- ./prog   # perf report --sort dso
omn trace cpu.instructions,cpu.cycles,cpu.ipc -m count -e summary -- ./prog
omn trace cpu.oncpu_time -m count -e summary -- make -j8
omn trace cpu.util -m poll@1s -e timeline --system -d 30s
omn trace cpu.throttle -m poll@1s -e summary --cgroup /sys/fs/cgroup/my.slice -d 1m
```

## Targets

Everything per-task takes a launch, `--pid`, `--tid`, `--cgroup`, `--cpu` or
`--system`. `cpu.util` and `cpu.pressure` describe a machine, a core or a
cgroup, never a process. `cpu.throttle` is a cgroup's alone.

## Backends and cost

- The hardware counters (`pmu`) are `low` and need a PMU. **A virtual machine
  often exposes none**: then `cpu.instructions`, `cpu.ipc`, `cpu.branches`,
  `cpu.cache_misses` and `cpu.topdown` are out of reach, and `cpu.cycles` and
  `cpu.profile` fall back to `pmu:sw`, the cpu clock (time, not cycles: blind
  to frequency changes). The run says `via pmu:sw` and `degraded:`.
- Per-cpu counting and `--system` need `CAP_PERFMON` (or
  `perf_event_paranoid` <= 0).
- `cpu.oncpu_time` through `bpf:raw_tp` is per thread in the scheduler's own
  nanoseconds; through `fs:proc` it is the process alone at tick resolution,
  and the children a launched program starts are not in it.

## Stacks

`--stack auto` (the default) picks the unwinding mode by machine **and**
target: `fp+leaf` for a binary built with frame pointers, `dwarf` for one
without (8 KiB copied per sample, `high`), `fp` system-wide. The run's notes
say which and why. `--stack dwarf` forces it. `--symbols debuginfod` names a
stripped binary's frames from `DEBUGINFOD_URLS`.

`-k object` keys each sample by the file its leaf was in (`libc.so.6`,
`[kernel]`, `?` where no mapping covered it): `perf report --sort dso`. It
is what to compare across machines where one library keeps its symbols and
another is stripped, since a named function and an unnamed address of the
same file land in the same row (pair `stripped-library`, 0.597 against perf's
0.597).

At `perf_event_paranoid` 2 without `CAP_PERFMON` the stacks are user-only,
and the run says so.

## Traps

- Counters are frequency-dependent: lock the governor or two runs are not
  comparable. Disabling SMT or pinning moves `cpu.instructions` by 10 to 30 %.
- High IPC can be a spin loop doing nothing.
- A time-based profile at a high rate includes its own interrupts.
- `omn diff before/ after/` subtracts two profiles and names the function that
  changed, among the ones whose name is the same code in both runs; addresses
  with no stable name (a withheld kernel's, a JIT's) are counted apart. For a
  confidence across processes, give each side several runs (`-r 3 -o DIR`, or
  `--before`/`--after`): one run's sample counts say how precisely it was
  sampled, not how much two runs differ. A profile's counts are on-cpu time,
  not wall clock: validate a speed-up on the wall clock without the profiler.
