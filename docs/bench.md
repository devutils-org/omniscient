# The bench — `ROADMAP.md` phase 8

Stream X of the tenth wave. This file is the handoff and the deliverable: what
the harness is, what each part was checked against, what it measured on this
machine while three other streams were building on it, what it found wrong
with the tool, and **the one command for the real measurement**, which is
run once, alone, after the merge.

## The one command

After the tenth wave is merged, on a machine nothing else is using:

```console
make release && sudo setcap cap_bpf,cap_perfmon+ep target/release/omn && scripts/bench.sh
```

The `setcap` is there because `make` can only grant the capabilities without a
password to the path the sudoers line names, which is the debug binary. The
bench measures the release build: a debug build's collectors are unoptimised
Rust, and the preload shim is too, so their price is not the tool's.
`scripts/bench.sh` refuses to start on a release binary without the
capabilities rather than produce a table missing every eBPF and per-cpu perf
row.

It runs, in order, and stops at the first refusal:

| step | what | behind the gate | takes |
|---|---|---|---|
| `gate` | the variance gate on the machine as it is | it *is* the gate | ~40 s |
| `counts` | three forced backends against exact counts | no: a count is not a duration | ~3 s |
| `resolution` | the 0.5–50 µs sweep | no: paired against the workload's own clock | ~5 s |
| `observer` | the metric's ground truth, then the collectors | the collectors' half | ~2 min |
| `backends` | the cost table and the resolver's verdicts | yes | 15–30 min |

Each writes its document under `target/bench/<step>-<timestamp>/`: `table.md`
and `table.json` for the backends (the JSON carries every repetition and the
environment), `resolution.md`, `observer.md`. **The coordinator pastes the
backends table into this file** under "The table", replacing the paragraph
that says it has not been measured.

`scripts/bench.sh <step>` runs one step; `scripts/bench.sh loaded` runs the
gate's refusal test; `scripts/bench.sh --smoke` runs everything with three
repetitions to check the mechanics -- see "Smoke runs" for why that can never
be mistaken for a measurement.

## What is where

| Path | What |
|---|---|
| `examples/bench/workload.c` | the reference workloads, one C program with a mode per workload; each measures itself and writes what it did to `BENCH_REPORT` |
| `examples/bench/harness.rs` | statistics, the gate, the schedule, eviction, the runners, the plan, the verdict, the resolution matching; included by every `tests/bench_*.rs` through `#[path]` |
| `tests/bench_harness.rs` | the cheap tests (19, in `make check`) and the two gate measurements (`#[ignore]`) |
| `tests/bench_backends.rs` | the cost table and the exact counts (`#[ignore]`) |
| `tests/bench_resolution.rs` | the sweep (`#[ignore]`) |
| `tests/bench_observer.rs` | the observer effect (`#[ignore]`) |
| `tests/bench_capabilities.rs` | the environment capture and the `dynamic-linking` probe, through the binary (3, in `make check`) |
| `scripts/bench.sh` | the entry point |
| `src/capabilities.rs` | the prefetcher capture, and the `dynamic-linking` and `hwmon` probes |

`examples/bench/harness.rs` is not an example target: Cargo only discovers
`examples/*.rs` and `examples/*/main.rs`. It sits there because an
integration test is its own crate and the code belongs to none of them, and
because `examples/bench/**` is this stream's.

## The harness, as the roadmap lists it

### Reference workloads

| name | invocation | what it is | its own ground truth |
|---|---|---|---|
| launch | `spawn 200` | forks and execs 200 children in turn | exactly 200 execs, of a hard link only this process names |
| compute | `compute 100000000` | a xorshift loop, no memory traffic | iterations |
| memory | `memory 256 4000000` | a dependent pointer chase over one random cycle of 256 MiB, THP chosen explicitly | steps |
| syscall | `syscall 200000` | getppid | calls, and the syscall number from the kernel's header |
| io | `io <256 MiB file>` | the file dropped from the page cache, then read | bytes, reads, and `mincore`'s count of pages still cached before the read |
| contended | `contended 4 1000000` | four threads, one mutex | the counter, which a broken mutex would lose increments from |

Plus the ones the later steps need: `async`, `async-malloc`, `async-serial`
(the observer effect) and `pairs-signal`, `pairs-malloc`, `pairs-fault` (the
sweep). Every timed section is measured by the workload on `CLOCK_MONOTONIC`,
the tool's own clock domain, so the harness never times a workload through
the thing being measured. `BENCH_CPUS` pins it, from the last cpu down: away
from cpu 0, one core per thread.

**The sizes were measured, not chosen.** The first ones -- one launch, a
64 MiB read, 4 × 100 000 lock round trips, a 64 MiB chase -- varied far too
much on a machine with a load average of 0.76, before the other streams had
started:

```
  WORKLOAD      N   MEDIAN        IQR           CV       GATE
  launch        20   839.77us      103.81us       12.28 %  REFUSE
  compute       20   146.24ms      945.62us        0.40 %  pass
  memory        20   128.44ms      3.01ms          2.15 %  REFUSE
  syscall       20   28.32ms       164.68us        0.49 %  pass
  io            20   22.55ms       1.16ms          6.69 %  REFUSE
  contended     20   13.72ms       612.56us        5.89 %  REFUSE
```

A single launch at 12 % is not something any environment control fixes: it is
what starting a process costs, run to run. Longer samples -- 200 launches, a
256 MiB read, a million round trips per thread -- brought `launch`, `io` and
`contended` to 1.1–1.5 % *with the other streams building* (load about 2).
The memory chase did not come down with THP pinned either way
(`MADV_NOHUGEPAGE` or `MADV_HUGEPAGE`: 2.2 to 4.1 %), which says its variance
here is DRAM shared with three concurrent builds rather than page size. The
workload still chooses THP explicitly for its own buffer, because under
`always` it is otherwise the kernel's choice run by run. Whether all six pass
on a quiet machine is exactly what the real run's first step will say.

### Warmup, round-robin, median and IQR

Two warmup runs per method are discarded. Then **every repetition runs every
method once**, rotated so each method holds each position equally often: a
drift over the run lands on every method alike, and none always runs first
after the eviction. The table reports the median and the IQR (quartiles by
linear interpolation, numpy's default, so any number can be reproduced from
`table.json`). The mean and standard deviation exist for one reason: the gate
is written as a coefficient of variation.

### The variance gate

**Rewritten after the tenth wave, because it could not pass a quiet machine.**
Run alone on the development machine -- load average 1, and then with boost
off -- the original gate refused five times out of five, and never on the same
workloads twice: `io` at 1.1 to 3.3 %, `contended` at 1.0 to 1.8 %, `launch`
and `syscall` either side of 1 %. A cold read from a disk and four threads on
one mutex are noisy run to run on any machine, and no environment control a
reader can apply brings them under 1 %. The gate was asking the wrong
question of them.

**The question is whether a median resolves 1 %**, because the table compares
medians of many runs, not single runs. A median of `n` runs has a standard
error of about `1.2533 * stddev / sqrt(n)`, the difference of two has
`sqrt(2)` times that, and two of those is a 95 % bound: the smallest overhead
a workload can resolve is `3.545 * CV / sqrt(n)` (`Stats::resolution`). The
roadmap's aim -- resolve a 1-3 % overhead -- is that bound at 1 % or under.

So the gate now, per workload:

- **admits** it when its baseline's median resolves 1 %;
- otherwise gives it the repetitions its spread needs, round-robin among the
  workloads that need more, up to **200**, re-reading the spread once they are
  in -- and the backend comparison measures that workload with the same count;
- **excludes it by name** when 200 are not enough, with the resolution it did
  reach and the count it would have needed, and every source measured against
  it goes under "not measured here" with that sentence. The rest are measured.

It **refuses the run** only when nothing can be measured: no baseline, fewer
than twenty repetitions anywhere (a smoke run), every workload excluded, a
governor other than `performance` (read, never set -- boundary 5), or **the
rest of the machine busy**.

**That last one is new, and it is what the old gate could never see.** Its own
ground truth, `the_gate_refuses_a_machine_loaded_on_purpose`, had stopped
refusing: with a hog on every cpu, `memory`'s median moved from 366 to 428 ms
(+17 %) and its spread stayed at 0.74 % -- a steady load slows every run alike
and hides from any spread, the CV's as much as the median's. What it cannot
hide is the busy time of the cpus no workload is pinned to, read from
`/proc/stat` over the baseline: **0.35 cpus** for the harness and a quiet
desktop together, **13.1** with the hogs. The gate refuses over 2.

Measured on the development machine after the change, alone, boost off:

```
  WORKLOAD      N   MEDIAN        IQR           CV       RESOLVES   GATE
  launch        20   128.05ms      1.57ms          0.76 %    0.60 %   pass
  compute       20   164.12ms      507.79us        0.29 %    0.23 %   pass
  memory        20   365.63ms      1.58ms          0.53 %    0.42 %   pass
  syscall       20   32.05ms       280.02us        0.91 %    0.72 %   pass
  io           115   70.28ms       1.83ms          2.63 %    0.87 %   pass
  contended     20   140.68ms      1.63ms          0.99 %    0.79 %   pass
  the unpinned cpus were 0.35 cpus busy meanwhile
```

and with the hogs, refused on 13.1 busy cpus with every spread passing. The
cheap tests check the resolution against its closed form, the repetitions a
spread needs from both sides, an excluded workload carrying its numbers while
the rest are admitted, a run with nothing left refused, and a busy machine
refused whatever the spread.

Boost and THP are still recorded rather than refused; turning boost off moved
nothing that decided the gate here.

### Environment control

What the bench controls, what it reads, and what it cannot see:

| roadmap item | here |
|---|---|
| `performance` governor | **read**, refused if not; never set |
| locked clocks | **read** (`cpufreq/boost`, which is on here) and recorded |
| frozen THP | the memory workload chooses THP for its own buffer; the machine's mode is recorded |
| affinity | **controlled**: the workload pins itself |
| throttled runs invalidated | the kernel's `thermal_throttle` counters are read around every run and a run they moved in is dropped and counted -- **but this AMD part publishes none**, so here a throttled run cannot be told from one that was not, and the environment record says so in those words |
| LLC evicted between repetitions | **controlled**: twice the largest cache the kernel describes (2 × 16 MiB), written a line at a time -- a read would not evict a victim L3 like Zen's |
| each NUMA domain swept | one node here; the number is recorded |
| a cold page cache | `POSIX_FADV_DONTNEED` on a file made clean by `fsync`, checked by `mincore` before each read -- no root, no `drop_caches` |
| prefetcher state | **captured, not controlled**: see below |

### The prefetcher state and `dynamic-linking` (`src/capabilities.rs`)

`Environment` has a `prefetchers` field, in every resolved file, with three
answers for the reason `Probe` has three: `read` (the register, each
prefetcher by name on or off, and whether every cpu agreed), `unreadable` (the
register exists and this process may not read it -- root or `CAP_SYS_RAWIO`,
which the tool does not ask for), and `unknown` (no register this tool knows).
The registers are Intel's `0x1a4` for family 6 and AMD's `PrefetchControl`
(`0xc0000108`) from family 19h, with the bit layouts the vendors document; a
set bit disables. This machine is a Ryzen 9 3900X, family 17h, which
documents no such register:

```json
{"state": "unknown", "reason": "AMD family 0x17 documents no prefetcher-control register (PrefetchControl starts with family 0x19), so the state of this cpu's prefetchers cannot be read"}
```

The ground-truth test checks the capture against the kernel's own answer --
`faccessat` on `/dev/cpu/0/msr` and the cpu's identity -- so a state that
could not be read can never come back as a reading.

Stream S's request is answered as asked: `dynamic-linking` is available
exactly when the shim is where the binary will look for it, and missing with
the path otherwise. `sched.mutex` and `gpu.api` no longer print "unverified
requirement" on every run, and a `cargo build` without `--workspace` shows as
a `missing` naming the file instead of a `?`. Checked through the binary,
against the file on disk and against `OMN_PRELOAD_LIB` pointing nowhere.

One more, found by the bench's plan rather than asked for: `power.temp`
requires `hwmon`, which no probe answered, so a source this machine serves
read `?` on every run. It is available now exactly when some hwmon chip
publishes a temperature this process can read -- checked against a
temperature read from sysfs directly, as an integer. Together:

```
before  101 sources: 90 usable here, 7 unverified, 4 out of reach on this machine.
after   101 sources: 93 usable here, 4 unverified, 4 out of reach on this machine.
```

**For `STATE.md`**, which is the coordinator's: the "90 usable" there becomes
93 with this branch.

## What the steps measured

### Counts: the rows measure something

Three forced backends against counts the workloads know, run on the loaded
machine because a count does not depend on the load:

```
proc.syscall via bpf:raw_tp:  the workload did 200000, the tool counted 200000, lost 0
proc.exec via bpf:raw_tp:     the workload did 200,    the tool counted 200,    lost 0
mem.heap.alloc via preload:   the workload did 20000,  the tool counted 20000,  lost 1
```

Two of the three needed a workaround, and both are defects filed below: the
`proc.syscall` filter has to name the syscall rather than number it, and the
`proc.exec` count is taken by the harness from each event's `filename`,
because the tool does not apply the filter it accepts. The first attempt at
the exec count also showed why the children exec a per-process hard link:
filtering on `/proc/self/exe` counted 266 of 200, because other programs on a
shared machine exec it too.

### Effective resolution

Pairs of marker events X apart, X from 0.5 to 50 µs, forty pairs per gap. The
workload stamps each event immediately before it happens; the tool's gap is
compared with the gap the pair actually had. A gap **resolves** when every
pair is seen as two events, at least 95 % of them in order and apart, with the
median |tool gap − own gap| under half the gap. The effective resolution is
the smallest gap above which everything resolves.

This step does not stand behind the gate, deliberately: load stretches the
gaps, and both clocks see the stretched gap. It measured, on the loaded
machine, with the debug build:

| source | backend | effective resolution | median error | median bias |
|---|---|---|---|---|
| `mem.heap.alloc` | `preload` | **0.5 µs** or better | 70–135 ns | −70 to −100 ns |
| `mem.faults` | `pmu:sw` (trace) | **0.5 µs** or better | 150–215 ns | −160 to −215 ns |
| `proc.signals` | `bpf:raw_tp` | **1 µs** | 320–580 ns | −355 to −465 ns |

At 0.5 µs `proc.signals` sees every pair as two ordered events and misses the
criterion only on the error, 400–435 ns on a 500 ns gap. The error is flat
across the whole sweep and it is all bias: the tool's gap is about 400 ns
*shorter* than the workload's at every gap, in both runs. That is the
signature of the two signals taking different times from the stamp to the
tracepoint, not of a timestamp that cannot tell them apart: the first of each
pair is sent right after a 2 ms sleep, cold, and the second from a hot path.
All three backends show it, with the same sign and a size that is the
backend's own: about 80 ns through the shim, 170 ns through a perf sample,
400 ns through a raw tracepoint -- which reads as the length of each path
from the event to its timestamp, paid once cold and once hot. So the 1 µs is a
floor set by that asymmetry as much as by the backend's clock, and a sweep
that warmed the path before the first event of each pair would likely move
it down. Nothing was lost at any gap.

Two limits, stated rather than hidden. The sweep covers the backends that
timestamp every event; a kernel-aggregated backend (`drained: true`) stamps a
bucket, and its resolution is its drain window, which the catalog already
says. And 0.5 µs is the bottom of the roadmap's range, so "0.5 µs or better"
is all this sweep can say about the two that pass it.

### The observer effect

The ratio `elapsed(4 threads) / elapsed(1 thread)` at equal work per thread:
near 1 when nothing serializes the threads, near 4 or more when something
makes them take turns. **The metric's ground truth** is the same workload with
a global mutex around each operation, serialized on purpose:

```
4 threads over 1, at equal work per thread:
  independent 1.01 [IQR 0.01], serialized on purpose 7.31 [IQR 0.10]
```

A separation of seven, so this half needs no gate. The collectors' half --
`proc.syscall` on the syscall sequence, the preload shim on the allocation
sequence, each as the index traced ratio / bare ratio -- does, because the
effect it looks for is a few percent; it is in the real run.

### The table

**Measured** after the tenth wave's merge, alone on the development machine
(Zen 2, 24 cpus, boost off, governor `performance`), with the release build
carrying its capabilities: every workload admitted by the gate, `io` at 121
repetitions and `contended` at 26, 94 (source, backend) pairs in 55 minutes.
The heap sources were missing from this run -- the gate knew only the six
reference workloads and excluded the `malloc` one it had no baseline for, which
is fixed -- and their rows are from a rerun right after it, below the table.

**What it says, before the table.** The two columns disagree more than anything
else, and the disagreement is the finding:

- **A collector's own cpu is not its cost when the kernel runs it.**
  `proc.syscall` costs 0.42 % of the target's cpu in the tool and **+296 %**
  in the target's own timed section: a program on every syscall entry and exit
  runs in the target's context, and is charged to it. `io.fs` (+7.5 %),
  `io.syscall` (+3.8 %) and `sched.futex` (+5.8 %) are the same shape, smaller.
  The dilation column is the one to read for an in-kernel collector; the
  resolver's cost bands were written from the other.
- **The preload shim costs its target a hundred and fifty times on an
  allocation-bound program.** The heap rerun: a million allocations that take
  7 ms bare take about 1.1 s under any of the three heap sources, **+15 400 to
  +15 700 %**, including the two that only poll -- the cost is the interposed
  call and its record, whatever the emit. That is about 1.1 us an allocation,
  where finding 6 measured the shim at 132 ns in isolation, and
  `mem.heap.alloc` also held 2.6 GiB and counted 6.6 million records lost.
- **The preload shim does not survive a mutex storm.** `sched.mutex` on the
  contended workload -- four threads, a million lock round trips each --
  dilates it **+1043 %**, spends 1.6 s of cpu, holds **3.9 GiB**, and counts
  54.9 million records lost. The loss is surfaced, as it must be; the memory
  and the dilation are not acceptable for a backend the resolver picks first.
  The observer step says why: under the shim four allocating threads scale
  **1.96** where they scaled 1.01 bare -- every record is one `write(2)` to one
  pipe, which serialises the threads that make them.
- **Attach can dominate a short run.** `io.fsync` and `net.tcp.connect` saw no
  event and spent 64 and 68 ms: the fentry trampoline rebuild the eighth wave
  measured, paid once per run and charged to a 70 ms workload.
- **A poll is not free everywhere.** `gpu.power` through NVML costs 34 ms per
  run, 21 % of the target, and `gpu.freq` 15 ms; `mem.numa`, `mem.pss`,
  `mem.rss` and `mem.uss` through smaps are 30 to 43 ms each at 100 ms.
- **Tracing a contended program can make it faster.** `sched.lock` (−21 %),
  `sched.offcpu` (−11 %) and `sched.runqlat` (−9 %) shorten the contended
  workload: a program on the context switch changes the lock's convoy. That is
  the observer effect the roadmap's sixth task names, measured, and it is a
  reason to read a lock measurement beside the run it perturbed.
- **Unexplained, and filed:** `runtime.jit` reports 360 losses on a run that
  produced no event.

**The resolver's choices, confirmed:** the seven sources with two served
backends -- `mem.pressure`, `mem.pss`, `mem.rss`, `mem.swap`, `mem.thp`,
`mem.uss`, `power.energy` -- each kept its chosen backend: the alternative is
either not clearly cheaper or cheaper by giving up what the catalog says it
degrades (`mem.rss` through `statm` is a hundred times cheaper and has no
per-VMA breakdown).

#
#### The heap rows, from the rerun

*Measured through the pipe. The shim has written into a shared ring since
(finding 47): the same workload runs in about 90 ms instead of 1.1 s, and these
rows want one more rerun.*

```
  malloc        40   7.05ms        180.56us        1.64 %    0.92 %   pass
```

| source | backend | resolver | mode | target | workload | collector cpu | % of target | dilation | omn peak RSS | events | losses | dropped (throttled) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| mem.heap.alloc | preload | chosen | trace | launch | malloc | 1.113s [5.19ms] | 103.12 % [0.14 %] | +15661.19 % [+79.79 %] | 2655.3 MiB | 1834999 | 6588124 | 0 |
| mem.heap.live | preload | chosen | poll@100ms | launch | malloc | 183.27ms [1.82ms] | 17.26 % [0.18 %] | +15383.46 % [+91.97 %] | 10.4 MiB | 13 | 0 | 0 |
| mem.heap.retained | preload | chosen | poll@100ms | launch | malloc | 247.15ms [2.81ms] | 23.31 % [0.33 %] | +15384.35 % [+105.14 %] | 25.3 MiB | 12 | 0 | 0 |

### Backend comparison

Measured with /home/moha/repos/omniscient/target/release/omn (release build, carrying CAP_BPF and CAP_PERFMON), at least 20 repetitions round-robin (more for a workload whose spread needed them to resolve 1 %, as the gate table says) after 2 warmup runs, the LLC evicted before each. Collector cpu and dilation are median [IQR]. A poll is measured at 100 ms and a sample at 997 Hz.

```
  WORKLOAD      N   MEDIAN        IQR           CV       RESOLVES   GATE
  launch        20   124.40ms      2.22ms          1.01 %    0.80 %   pass
  compute       20   164.29ms      380.82us        0.26 %    0.21 %   pass
  memory        20   367.02ms      1.99ms          0.46 %    0.36 %   pass
  syscall       20   32.01ms       266.74us        0.85 %    0.67 %   pass
  io           121   70.70ms       2.35ms          2.63 %    0.85 %   pass
  contended     26   140.84ms      2.49ms          1.41 %    0.98 %   pass
```

| source | backend | resolver | mode | target | workload | collector cpu | % of target | dilation | omn peak RSS | events | losses | dropped (throttled) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| cpu.branch_misses | pmu | chosen | count | launch | compute | 270.00us [11.00us] | 0.17 % [0.01 %] | -0.06 % [+0.32 %] | 7.2 MiB | 1 | 0 | 0 |
| cpu.cache_misses | pmu | chosen | count | launch | compute | 264.00us [66.50us] | 0.17 % [0.04 %] | -0.08 % [+0.26 %] | 7.3 MiB | 1 | 0 | 0 |
| cpu.context_switches | pmu:sw | chosen | count | launch | compute | 305.00us [62.75us] | 0.19 % [0.04 %] | -0.12 % [+0.20 %] | 7.3 MiB | 1 | 0 | 0 |
| cpu.cycles | pmu | chosen | count | launch | compute | 265.00us [62.75us] | 0.17 % [0.04 %] | +0.04 % [+0.07 %] | 7.3 MiB | 1 | 0 | 0 |
| cpu.instructions | pmu | chosen | count | launch | compute | 261.00us [53.75us] | 0.16 % [0.03 %] | -0.02 % [+0.20 %] | 7.3 MiB | 1 | 0 | 0 |
| cpu.ipc | pmu | chosen | count | launch | compute | 274.00us [27.75us] | 0.17 % [0.02 %] | -0.05 % [+0.16 %] | 7.2 MiB | 1 | 0 | 0 |
| cpu.migrations | pmu:sw | chosen | count | launch | compute | 297.50us [47.50us] | 0.19 % [0.03 %] | +0.11 % [+0.27 %] | 7.3 MiB | 1 | 0 | 0 |
| cpu.oncpu_time | fs:proc | chosen | poll@100ms | launch | compute | 109.00us [5.50us] | 0.07 % [0.00 %] | -0.10 % [+0.34 %] | 7.1 MiB | 2 | 0 | 0 |
| cpu.pressure | fs:proc | chosen | poll@100ms | system | compute | 361.50us [23.00us] | 0.15 % [0.04 %] | - | 7.0 MiB | 11 | 0 | 0 |
| cpu.profile | pmu:sw | chosen | sample@997hz | launch | compute | 982.00us [63.00us] | 0.61 % [0.04 %] | +0.23 % [+0.21 %] | 46.4 MiB | 164 | 0 | 0 |
| cpu.util | fs:proc | chosen | poll@100ms | system | compute | 924.00us [69.75us] | 0.36 % [0.09 %] | - | 7.2 MiB | 10 | 0 | 0 |
| gpu.api | preload | chosen | trace | launch | compute | 236.50us [42.25us] | 0.15 % [0.03 %] | -0.03 % [+0.37 %] | 7.3 MiB | 0 | 0 | 0 |
| gpu.engine_busy | vendor:nvml | chosen | count | launch | compute | 6.00ms [1.22ms] | 3.75 % [0.77 %] | -0.01 % [+0.20 %] | 32.1 MiB | 2 | 0 | 0 |
| gpu.freq | vendor:nvml | chosen | poll@100ms | launch | compute | 14.82ms [2.22ms] | 9.27 % [1.39 %] | -0.03 % [+0.16 %] | 32.2 MiB | 2 | 0 | 0 |
| gpu.mem | vendor:nvml | chosen | poll@100ms | launch | compute | 2.21ms [229.50us] | 1.38 % [0.14 %] | +0.09 % [+0.29 %] | 32.2 MiB | 2 | 0 | 0 |
| gpu.power | vendor:nvml | chosen | poll@100ms | launch | compute | 33.76ms [1.80ms] | 21.10 % [1.13 %] | -0.62 % [+0.17 %] | 32.4 MiB | 2 | 0 | 0 |
| gpu.temp | vendor:nvml | chosen | poll@100ms | launch | compute | 630.50us [181.50us] | 0.39 % [0.11 %] | -0.03 % [+0.21 %] | 32.1 MiB | 2 | 0 | 0 |
| io.bytes | fs:proc | chosen | count | system | io | 142.00us [22.00us] | 0.07 % [0.05 %] | - | 7.0 MiB | 2 | 0 | 0 |
| io.fs | bpf:fentry | chosen | count | launch | io | 298.00us [21.00us] | 0.69 % [0.05 %] | +7.50 % [+1.66 %] | 47.5 MiB | 11 | 0 | 0 |
| io.fsync | bpf:fentry | chosen | trace | launch | io | 63.81ms [244.00us] | 156.76 % [4.57 %] | +6.47 % [+1.22 %] | 47.3 MiB | 0 | 0 | 0 |
| io.inflight | fs:proc | chosen | poll@100ms | system | io | 765.00us [41.00us] | 0.42 % [0.25 %] | - | 7.0 MiB | 22 | 0 | 0 |
| io.iops | fs:proc | chosen | count | system | io | 142.00us [20.00us] | 0.07 % [0.05 %] | - | 7.0 MiB | 2 | 0 | 0 |
| io.latency | bpf:raw_tp | chosen | count | launch | io | 1.53ms [28.00us] | 3.70 % [0.14 %] | +0.36 % [+1.25 %] | 30.2 MiB | 1 | 0 | 0 |
| io.merges | fs:proc | chosen | count | system | io | 141.00us [20.00us] | 0.07 % [0.04 %] | - | 7.0 MiB | 2 | 0 | 0 |
| io.pagecache | bpf:raw_tp | chosen | count | launch | io | 260.00us [14.00us] | 0.61 % [0.04 %] | +0.87 % [+1.11 %] | 29.8 MiB | 5 | 0 | 0 |
| io.pressure | fs:proc | chosen | poll@100ms | system | io | 374.00us [20.00us] | 0.21 % [0.13 %] | - | 7.0 MiB | 10 | 0 | 0 |
| io.queue | bpf:raw_tp | chosen | trace | system | io | 5.08ms [226.00us] | 2.79 % [1.76 %] | - | 38.0 MiB | 2048 | 0 | 0 |
| io.readahead | bpf:raw_tp | chosen | count | launch | io | 264.00us [15.00us] | 0.61 % [0.04 %] | +0.90 % [+1.76 %] | 29.7 MiB | 5 | 0 | 0 |
| io.size | bpf:raw_tp | chosen | trace | launch | io | 2.61ms [81.00us] | 6.18 % [0.26 %] | -0.00 % [+1.36 %] | 36.2 MiB | 2048 | 0 | 0 |
| io.syscall | bpf:fentry | chosen | count | launch | io | 322.00us [22.00us] | 0.75 % [0.05 %] | +3.82 % [+1.58 %] | 47.5 MiB | 11 | 0 | 0 |
| io.writeback | bpf:raw_tp | chosen | count | system | io | 664.00us [162.00us] | 0.35 % [0.26 %] | - | 29.7 MiB | 1 | 0 | 0 |
| mem.cache_l1 | pmu | chosen | count | launch | memory | 1.28ms [351.50us] | 0.14 % [0.04 %] | -0.15 % [+0.30 %] | 7.3 MiB | 1 | 0 | 0 |
| mem.cache_llc | pmu:generic | chosen | count | launch | memory | 1.29ms [185.75us] | 0.14 % [0.02 %] | -0.02 % [+0.42 %] | 7.3 MiB | 1 | 0 | 0 |
| mem.cache_tlb | pmu | chosen | count | launch | memory | 1.28ms [426.25us] | 0.14 % [0.05 %] | -0.07 % [+0.24 %] | 7.3 MiB | 1 | 0 | 0 |
| mem.cgroup | fs:cgroup | chosen | poll@100ms | launch | memory | 853.00us [87.25us] | 0.09 % [0.01 %] | -0.14 % [+0.24 %] | 7.1 MiB | 10 | 0 | 0 |
| mem.compaction | bpf:raw_tp | chosen | count | system | memory | 593.00us [194.75us] | 0.07 % [0.03 %] | - | - | 0 | 0 | 0 |
| mem.faults | pmu:sw | chosen | trace | launch | memory | 41.86ms [654.75us] | 4.12 % [0.07 %] | -0.10 % [+0.27 %] | 94.7 MiB | 65606 | 0 | 0 |
| mem.kmalloc | bpf:raw_tp | chosen | count | system | memory | 1.44ms [329.75us] | 0.17 % [0.04 %] | - | - | 76 | 2 | 0 |
| mem.latency | pmu:ibs | chosen | sample@997hz | launch | memory | 1.46ms [279.00us] | 0.15 % [0.03 %] | +0.60 % [+0.26 %] | 7.4 MiB | 128 | 0 | 0 |
| mem.numa | fs:proc | chosen | poll@100ms | launch | memory | 42.92ms [5.38ms] | 4.55 % [0.55 %] | -0.32 % [+0.26 %] | 7.2 MiB | 10 | 0 | 0 |
| mem.page_alloc | bpf:raw_tp | chosen | count | system | memory | 672.00us [525.75us] | 0.08 % [0.04 %] | - | - | 13 | 3 | 0 |
| mem.pressure | fs:proc |  | poll@100ms | system | memory | 382.50us [44.50us] | 0.04 % [0.01 %] | - | - | 10 | 0 | 0 |
| mem.pressure | fs:cgroup | chosen | poll@100ms | launch | memory | 395.00us [69.50us] | 0.04 % [0.01 %] | +0.01 % [+0.23 %] | 7.1 MiB | 9 | 0 | 0 |
| mem.pss | fs:proc:smaps_rollup | chosen | poll@100ms | launch | memory | 30.18ms [4.04ms] | 3.20 % [0.41 %] | -0.19 % [+0.29 %] | 7.2 MiB | 10 | 0 | 0 |
| mem.pss | fs:proc:smaps |  | poll@100ms | launch | memory | 33.36ms [7.09ms] | 3.54 % [0.72 %] | -0.12 % [+0.35 %] | 7.1 MiB | 10 | 0 | 0 |
| mem.reclaim | bpf:raw_tp | chosen | count | launch | memory | 510.50us [36.25us] | 0.05 % [0.00 %] | -0.05 % [+0.31 %] | 29.4 MiB | 0 | 0 | 0 |
| mem.rss | fs:proc:smaps_rollup | chosen | poll@100ms | launch | memory | 31.37ms [3.91ms] | 3.33 % [0.35 %] | -0.28 % [+0.42 %] | 7.2 MiB | 10 | 0 | 0 |
| mem.rss | fs:proc:statm |  | poll@100ms | launch | memory | 330.50us [283.75us] | 0.04 % [0.03 %] | -0.17 % [+0.34 %] | 7.2 MiB | 10 | 0 | 0 |
| mem.sharing | pmu:ibs | chosen | sample@997hz | launch | memory | 1.45ms [173.50us] | 0.15 % [0.02 %] | +0.53 % [+0.35 %] | 7.3 MiB | 8 | 0 | 0 |
| mem.swap | fs:proc |  | poll@100ms | system | memory | 1.26ms [21.50us] | 0.15 % [0.01 %] | - | - | 10 | 0 | 0 |
| mem.swap | fs:cgroup | chosen | poll@100ms | launch | memory | 647.50us [31.75us] | 0.07 % [0.00 %] | -0.08 % [+0.35 %] | 7.2 MiB | 9 | 0 | 0 |
| mem.thp | fs:sys | chosen | poll@100ms | system | memory | 965.00us [224.25us] | 0.12 % [0.01 %] | - | - | 10 | 0 | 0 |
| mem.thp | fs:proc |  | poll@100ms | system | memory | 1.26ms [23.00us] | 0.15 % [0.05 %] | - | - | 10 | 0 | 0 |
| mem.uss | fs:proc:smaps_rollup | chosen | poll@100ms | launch | memory | 32.14ms [5.10ms] | 3.41 % [0.51 %] | -0.26 % [+0.49 %] | 7.2 MiB | 10 | 0 | 0 |
| mem.uss | fs:proc:smaps |  | poll@100ms | launch | memory | 31.85ms [4.43ms] | 3.38 % [0.45 %] | -0.29 % [+0.23 %] | 7.2 MiB | 10 | 0 | 0 |
| mem.wss | fs:proc:clear_refs | chosen | poll@100ms | launch | memory | 62.23ms [9.02ms] | 6.38 % [0.91 %] | +2.71 % [+0.25 %] | 7.3 MiB | 10 | 0 | 0 |
| net.backlog | fs:proc | chosen | count | system | compute | 668.00us [32.00us] | 0.25 % [0.15 %] | - | 7.0 MiB | 2 | 0 | 0 |
| net.bytes | fs:proc | chosen | count | system | compute | 210.00us [14.50us] | 0.08 % [0.04 %] | - | 7.0 MiB | 14 | 0 | 0 |
| net.capture | af_packet | chosen | trace | system | compute | 479.50us [162.00us] | 0.20 % [0.15 %] | - | 8.0 MiB | 5 | 0 | 0 |
| net.drops | bpf:raw_tp | chosen | count | system | compute | 1.15ms [147.50us] | 0.47 % [0.18 %] | - | 30.3 MiB | 0 | 0 | 0 |
| net.iface | fs:proc | chosen | count | system | compute | 255.00us [31.75us] | 0.10 % [0.05 %] | - | 7.0 MiB | 14 | 0 | 0 |
| net.packets | fs:proc | chosen | count | system | compute | 209.00us [24.00us] | 0.08 % [0.02 %] | - | 7.0 MiB | 14 | 0 | 0 |
| net.qdisc | bpf:raw_tp | chosen | trace | system | compute | 743.00us [141.75us] | 0.27 % [0.12 %] | - | 29.4 MiB | 0 | 0 | 0 |
| net.socket | bpf:fentry | chosen | count | launch | compute | 288.00us [12.75us] | 0.18 % [0.01 %] | +0.05 % [+0.24 %] | 47.2 MiB | 0 | 0 | 0 |
| net.softirq | bpf:raw_tp | chosen | count | system | compute | 726.00us [76.50us] | 0.28 % [0.10 %] | - | 29.5 MiB | 4 | 0 | 0 |
| net.tcp.connect | bpf:fentry | chosen | trace | launch | compute | 67.68ms [434.00us] | 42.21 % [0.28 %] | +0.12 % [+0.19 %] | 47.4 MiB | 0 | 0 | 0 |
| net.tcp.lifetime | bpf:raw_tp | chosen | trace | system | compute | 952.00us [146.00us] | 0.38 % [0.18 %] | - | 29.9 MiB | 0 | 0 | 0 |
| net.tcp.retransmit | bpf:raw_tp | chosen | count | system | compute | 780.00us [159.75us] | 0.30 % [0.18 %] | - | 30.0 MiB | 0 | 0 | 0 |
| net.tcp.rtt | netlink | chosen | trace | launch | compute | 5.06ms [45.75us] | 3.16 % [0.03 %] | -0.11 % [+0.22 %] | 7.0 MiB | 0 | 0 | 0 |
| power.cstate | fs:sys | chosen | poll@100ms | system | compute | 22.25ms [454.00us] | 8.09 % [4.02 %] | - | 7.3 MiB | 240 | 0 | 0 |
| power.energy | pmu | chosen | count | system | compute | 86.50us [4.25us] | 0.03 % [0.01 %] | - | 7.0 MiB | 1 | 0 | 0 |
| power.energy | fs:sys |  | count | system | compute | 121.00us [21.00us] | 0.04 % [0.02 %] | - | 7.0 MiB | 1 | 0 | 0 |
| power.freq | fs:sys | chosen | poll@100ms | system | compute | 2.90ms [126.50us] | 1.15 % [0.25 %] | - | 7.1 MiB | 264 | 0 | 0 |
| power.temp | fs:sys | chosen | poll@100ms | system | compute | 1.13ms [33.75us] | 0.44 % [0.10 %] | - | 7.0 MiB | 11 | 0 | 0 |
| proc.exec | bpf:raw_tp | chosen | trace | system | launch | 1.33ms [134.00us] | 0.55 % [0.28 %] | - | 30.2 MiB | 201 | 0 | 0 |
| proc.exit | bpf:raw_tp | chosen | trace | system | launch | 1.37ms [109.50us] | 0.60 % [0.27 %] | - | 30.2 MiB | 209 | 0 | 0 |
| proc.fd | fs:proc | chosen | poll@100ms | launch | syscall | 183.00us [10.25us] | 0.57 % [0.04 %] | -0.16 % [+0.86 %] | 7.2 MiB | 1 | 0 | 0 |
| proc.signals | bpf:raw_tp | chosen | count | launch | syscall | 214.00us [6.25us] | 0.66 % [0.02 %] | -0.10 % [+0.53 %] | 30.0 MiB | 1 | 0 | 0 |
| proc.syscall | bpf:raw_tp | chosen | count | launch | syscall | 521.00us [63.25us] | 0.42 % [0.05 %] | +296.29 % [+3.12 %] | 29.4 MiB | 39 | 0 | 0 |
| proc.threads | fs:proc | chosen | poll@100ms | launch | syscall | 168.50us [11.25us] | 0.52 % [0.03 %] | +0.04 % [+0.97 %] | 7.1 MiB | 1 | 0 | 0 |
| runtime.jit | fs:jit-map | chosen | trace | launch | compute | 266.50us [62.00us] | 0.17 % [0.04 %] | +0.08 % [+0.11 %] | 7.2 MiB | 0 | 360 | 0 |
| sched.futex | bpf:raw_tp | chosen | trace | launch | contended | 508.50us [41.75us] | 0.09 % [0.01 %] | +5.84 % [+1.09 %] | 29.6 MiB | 58 | 2520 | 0 |
| sched.irq | bpf:raw_tp | chosen | count | system | contended | 768.00us [143.75us] | 0.14 % [0.04 %] | - | 29.5 MiB | 11 | 0 | 0 |
| sched.lock | bpf:raw_tp | chosen | trace | launch | contended | 1.06ms [68.50us] | 0.24 % [0.02 %] | -20.91 % [+2.06 %] | 29.6 MiB | 99 | 14 | 0 |
| sched.migrations | bpf:raw_tp | chosen | count | launch | contended | 207.50us [17.25us] | 0.05 % [0.01 %] | -1.89 % [+2.41 %] | 29.4 MiB | 6 | 0 | 0 |
| sched.mutex | preload | chosen | trace | launch | contended | 1.613s [8.48ms] | 27.11 % [0.10 %] | +1042.56 % [+4.83 %] | 3924.5 MiB | 1887524 | 54923080 | 0 |
| sched.numa_balance | bpf:raw_tp | chosen | count | system | contended | 457.00us [52.00us] | 0.08 % [0.02 %] | - | 29.5 MiB | 0 | 0 | 0 |
| sched.offcpu | bpf:raw_tp | chosen | trace | launch | contended | 340.00us [15.75us] | 0.08 % [0.00 %] | -10.99 % [+2.02 %] | 29.7 MiB | 13 | 0 | 0 |
| sched.runqlat | bpf:raw_tp | chosen | trace | launch | contended | 423.50us [22.00us] | 0.10 % [0.00 %] | -9.40 % [+1.51 %] | 29.6 MiB | 33 | 0 | 0 |
| sched.switches | pmu:sw | chosen | trace | launch | contended | 256.50us [39.00us] | 0.06 % [0.01 %] | -1.17 % [+1.74 %] | 7.2 MiB | 6 | 0 | 0 |
| sched.wakeup | bpf:raw_tp | chosen | trace | launch | contended | 14.93ms [915.00us] | 3.24 % [0.18 %] | -1.07 % [+1.35 %] | 73.5 MiB | 25192 | 0 | 0 |

#### The resolver's choices

| source | verdict |
|---|---|
| mem.pressure | confirmed: `fs:proc` was measured as poll@100ms, system against poll@100ms, launch, and is not compared |
| mem.pss | confirmed: `fs:proc:smaps` is not clearly cheaper |
| mem.rss | confirmed: `fs:proc:statm` is cheaper and degrades: resident pages only: no per-VMA breakdown, and no PSS or USS to cross-check against |
| mem.swap | confirmed: `fs:proc` was measured as poll@100ms, system against poll@100ms, launch, and is not compared |
| mem.thp | confirmed: `fs:proc` is not clearly cheaper |
| mem.uss | confirmed: `fs:proc:smaps` is not clearly cheaper |
| power.energy | confirmed: `fs:sys` is not clearly cheaper |

#### Not measured here

| source | backend | why |
|---|---|---|
| cpu.cycles | pmu:sw | no collector is written for it |
| cpu.topdown | pmu | error: forced past `pmu:slots`, which probes missing: this machine publishes no `slots` performance unit under /sys/bus/event_source/devices |
| cpu.profile | pmu | no collector is written for it |
| cpu.oncpu_time | bpf:raw_tp | no collector is written for it |
| cpu.oncpu_time | bpf:tp | no collector is written for it |
| cpu.oncpu_time | fs:cgroup | no collector is written for it |
| cpu.util | fs:cgroup | no collector is written for it |
| cpu.pressure | fs:cgroup | no collector is written for it |
| cpu.throttle | fs:cgroup | follows only a cgroup or a container, and the bench launches a program or watches the machine |
| cpu.flow | pmu:aux:intel_pt | no collector is written for it |
| cpu.flow | pmu:aux:cs_etm | no collector is written for it |
| mem.faults | bpf:raw_tp | no collector is written for it |
| mem.faults | bpf:tp | no collector is written for it |
| mem.page_alloc | bpf:tp | no collector is written for it |
| mem.kmalloc | bpf:tp | no collector is written for it |
| mem.heap.alloc | bpf:usdt | no collector is written for it |
| mem.heap.alloc | bpf:uprobe | no collector is written for it |
| mem.heap.live | fs:proc | no collector is written for it |
| mem.cache_llc | pmu | error: forced past `pmu:LLC-load-misses`, which probes missing: `LLC-load-misses` is not available on this cpu |
| mem.latency | pmu:pebs | no collector is written for it |
| mem.sharing | pmu:pebs | no collector is written for it |
| mem.bandwidth | pmu:uncore | no collector is written for it |
| mem.bandwidth | pmu:rdt | no collector is written for it |
| mem.numa | pmu | no collector is written for it |
| mem.wss | fs:sys:damon | no collector is written for it |
| mem.wss | fs:sys:page_idle | no collector is written for it |
| mem.reclaim | bpf:tp | no collector is written for it |
| mem.compaction | bpf:tp | no collector is written for it |
| io.latency | bpf:tp | no collector is written for it |
| io.latency | fs:tracefs | no collector is written for it |
| io.latency | fs:proc | no collector is written for it |
| io.queue | bpf:tp | no collector is written for it |
| io.bytes | bpf:raw_tp | no collector is written for it |
| io.bytes | bpf:tp | no collector is written for it |
| io.bytes | fs:cgroup | no collector is written for it |
| io.iops | bpf:raw_tp | no collector is written for it |
| io.iops | bpf:tp | no collector is written for it |
| io.size | bpf:tp | no collector is written for it |
| io.merges | bpf:raw_tp | no collector is written for it |
| io.merges | bpf:tp | no collector is written for it |
| io.syscall | bpf:raw_tp | no collector is written for it |
| io.syscall | bpf:tp | no collector is written for it |
| io.fs | bpf:kprobe | no collector is written for it |
| io.pagecache | bpf:tp | no collector is written for it |
| io.readahead | bpf:tp | no collector is written for it |
| io.readahead | bpf:fentry | no collector is written for it |
| io.writeback | bpf:tp | no collector is written for it |
| io.fsync | bpf:kprobe | no collector is written for it |
| io.pressure | fs:cgroup | no collector is written for it |
| net.tcp.connect | bpf:kprobe | no collector is written for it |
| net.tcp.lifetime | bpf:tp | no collector is written for it |
| net.tcp.rtt | bpf:fentry | no collector is written for it |
| net.tcp.retransmit | bpf:tp | no collector is written for it |
| net.drops | bpf:tp | no collector is written for it |
| net.bytes | netlink | no collector is written for it |
| net.bytes | bpf:fentry | no collector is written for it |
| net.packets | netlink | no collector is written for it |
| net.socket | bpf:kprobe | no collector is written for it |
| net.qdisc | bpf:tp | no collector is written for it |
| net.softirq | bpf:tp | no collector is written for it |
| net.hwts | so_timestamping | no collector is written for it |
| net.hwts | netlink:ethtool | error: forced past `ethtool`, which probes unknown: no probe is written for `ethtool` yet |
| net.xdp | bpf | follows only a cgroup or a container, and the bench launches a program or watches the machine |
| net.iface | netlink | no collector is written for it |
| net.request | bpf:usdt | error: cannot start collecting `net.request`: /home/moha/repos/omniscient/target/bench/workload declares no request: `net.request` follows a request from a `request__start(id)` probe to a `request__done(id)` probe the program fires itself, any provider, and this one carries no USDT probe at all. Nothing is inferred from the connection instead -- see `docs/scenarios/vertical.md` |
| net.request | bpf:usdt:threads | error: cannot start collecting `net.request`: /home/moha/repos/omniscient/target/bench/workload declares no request: `net.request` follows a request from a `request__start(id)` probe to a `request__done(id)` probe the program fires itself, any provider, and this one carries no USDT probe at all. Nothing is inferred from the connection instead -- see `docs/scenarios/vertical.md` |
| sched.wakeup | bpf:tp | no collector is written for it |
| sched.runqlat | bpf:tp | no collector is written for it |
| sched.offcpu | bpf:tp:stacks | no collector is written for it |
| sched.offcpu | bpf:tp | no collector is written for it |
| sched.switches | bpf:raw_tp | no collector is written for it |
| sched.switches | bpf:tp | no collector is written for it |
| sched.migrations | bpf:tp | no collector is written for it |
| sched.lock | bpf:tp | no collector is written for it |
| sched.lock | bpf:kprobe | no collector is written for it |
| sched.futex | bpf:tp | no collector is written for it |
| sched.mutex | bpf:usdt | error: [sched.mutex #1] backend `bpf:usdt` of `sched.mutex` cannot attribute per launch |
| sched.mutex | bpf:uprobe | no collector is written for it |
| sched.irq | bpf:tp | no collector is written for it |
| sched.numa_balance | bpf:tp | no collector is written for it |
| power.freq | bpf:raw_tp | no collector is written for it |
| power.freq | bpf:tp | no collector is written for it |
| power.freq | pmu | no collector is written for it |
| power.cstate | bpf:raw_tp | no collector is written for it |
| power.cstate | bpf:tp | no collector is written for it |
| power.throttle | bpf:raw_tp | no collector is written for it |
| power.throttle | bpf:tp | no collector is written for it |
| power.throttle | fs:sys | error: forced past `/sys/devices/system/cpu/cpu0/thermal_throttle`, which probes missing: /sys/devices/system/cpu/cpu0/thermal_throttle does not exist |
| gpu.job | bpf:tp | error: forced past `tracepoint:gpu_scheduler:drm_run_job`, which probes missing: this kernel defines no `gpu_scheduler:drm_run_job` tracepoint: /sys/kernel/tracing/events/gpu_scheduler/drm_run_job/id does not exist |
| gpu.engine_busy | pmu:i915 | no collector is written for it |
| gpu.engine_busy | pmu:xe | no collector is written for it |
| gpu.engine_busy | pmu:amdgpu | no collector is written for it |
| gpu.freq | fs:sys:amdgpu | no collector is written for it |
| gpu.freq | fs:sys:i915 | no collector is written for it |
| gpu.power | fs:sys:amdgpu | no collector is written for it |
| gpu.temp | fs:sys:amdgpu | no collector is written for it |
| gpu.mem | fs:sys:amdgpu | no collector is written for it |
| gpu.kernel | vendor:cupti | no collector is written for it |
| gpu.kernel | vendor:rocprof | no collector is written for it |
| gpu.kernel | vendor:l0 | no collector is written for it |
| gpu.counters | vendor:ncu | no collector is written for it |
| gpu.counters | vendor:rocprof | no collector is written for it |
| proc.syscall | bpf:tp | no collector is written for it |
| proc.exec | bpf:tp | no collector is written for it |
| proc.exit | bpf:tp | no collector is written for it |
| proc.signals | bpf:tp | no collector is written for it |
| runtime.gc | bpf:usdt | error: cannot start collecting `runtime.gc`: `runtime.gc` found no probe to attach to: the target carries none of python:gc__start, node:gc__start, hotspot:gc__begin, ruby:gc__mark__begin, ruby:gc__sweep__begin. A runtime built without its probes publishes nothing, which is not the same as a runtime that did nothing. CPython and node need to be built --with-dtrace, and HotSpot with DTrace support. What was read: |
| runtime.jit | bpf:usdt | no collector is written for it |
| runtime.alloc | bpf:usdt | error: cannot start collecting `runtime.alloc`: `runtime.alloc` found no probe to attach to: the target carries none of hotspot:object__alloc. A runtime built without its probes publishes nothing, which is not the same as a runtime that did nothing. HotSpot fires object__alloc only under -XX:+ExtendedDTraceProbes. What was read: |
| runtime.custom | bpf:usdt | error: cannot start collecting `runtime.custom`: `runtime.custom` is the probe you name, and this request names none. Select one through the filter, by `provider` and `probe` -- for example --filter "provider == 'libc' && probe == 'setjmp'". What the target carries: |
| runtime.custom | bpf:uprobe | no collector is written for it |
| mem.heap.alloc | every backend | the gate excluded its workload: `malloc` its workload has no baseline |
| mem.heap.live | every backend | the gate excluded its workload: `malloc` its workload has no baseline |
| mem.heap.retained | every backend | the gate excluded its workload: `malloc` its workload has no baseline |


What the smoke runs established about the mechanics, without quoting a
number: the plan enumerates every (source, backend) this build can serve here,
chooses for each a mode, emit and target the tool accepts **and that one trial
run confirms**, prefers the chosen backend's setting for the others so two
backends of a source are compared alike, and lists everything else with the
tool's own reason under "Not measured here". A source followed per launch is
priced as a share of the program's cpu, with the dilation of the program's
own timed section beside it; a system-wide one is given a one-second window,
with the workload run inside it after a 250 ms attach margin, and has no
dilation column.

Three things the smoke runs taught the harness, each now enforced by it:

- **The binary can lose its capabilities mid-run.** Formatting
  `src/capabilities.rs` during a smoke run made the next `cargo test` relink
  `target/debug/omn`, and every eBPF row after that point failed as if the
  machine had changed -- `CLAUDE.md`'s warning, met live. `run_traced` checks
  the file capabilities before every run (a `getxattr`) and stops by name if
  they are gone. The real run measures the release binary, which no
  `cargo test` relinks.
- **Which binary.** `make` grants the capabilities to the debug binary only,
  so with a release build beside it the harness takes whichever carries them;
  `OMN_BENCH_BIN` overrides, and `scripts/bench.sh` insists on the release
  build for a real run.
- **"Chosen" means chosen and collectable.** A resolved file can name a
  backend with no collector -- a launch of `io.bytes` resolves to an
  unimplemented `bpf:raw_tp`, and the run is then refused -- so the row the
  table marks `chosen` is the resolver's pick among settings a run would use.

The verdict on the resolver is mechanical and conservative, because the chain
is ordered by what answers the question best, not by price
(`docs/contributing.md`): a cheaper fallback that degrades the answer does not
overturn the choice. A choice is **corrected** only by a backend measured
under the same mode and target whose median cost is below the chosen one's
first quartile, which loses no more, and which degrades nothing.

## What the bench found wrong with the tool

Filed under "the tenth wave, stream X" in `docs/open-findings.md`, each with
the command that reproduces it:

1. **`proc.syscall --filter 'syscall == 110'` drops every call it pushed down.**
   The number reaches the eBPF program -- only getppid is timed -- and then a
   userspace pass compares the same predicate with the `syscall` attribute,
   which is the name. 200 000 calls, an empty summary, and a degradation line
   saying the predicate "removed 200000 and kept 0". By name it counts
   200 000 of 200 000.
2. **`proc.exec` accepts a filter on `filename` or `comm` and applies
   neither**, silently. A filter naming `/nonexistent/path` reports every exec
   on the machine.
3. **`--backend force:` walks past a backend's missing probes.**
   `mem.cache_llc` on `pmu` probes missing here (Zen 2 publishes no
   `LLC-loads`), the forced resolved file carries the two `missing` probes,
   and the run is accepted, launches its target and fails at attach -- finding
   15's shape, reached through the pin.
4. **`mem.faults` declares `count`, the validator accepts it, and the
   collector refuses it after the launch.**

Three of the four are the silent degradation the invariants rule out; the
bench works around each and names the workaround where it is.

## Smoke runs

`scripts/bench.sh --smoke` sets `OMN_BENCH_REPETITIONS=3`. Under twenty the
gate refuses by construction, so a smoke run records the refusal instead of
obeying it and goes on, writes into `target/bench/smoke-*`, and opens every
document with **NOT A MEASUREMENT**. When the release binary has no
capabilities it measures the debug one, and the first line of every document
says `DEBUG build`. It exists to check the mechanics on a machine where the
real thing would refuse, which during this wave was every machine.
`OMN_BENCH_ONLY=<prefix>` restricts the backend comparison to sources with
that prefix.

## Tests

In `make check` (22, all cheap, none measures the machine): the statistics
against numpy's quantiles and a closed-form CV; the gate's refusals; the
round-robin order; the verdict on synthetic rows, including that rows
measured under different settings are not compared; the resolution criterion
and pair matching; the workloads counting what they were asked to do and
reading from a cold cache; the evictor's size; and the capture and the probe
through the binary. Plus six unit tests in `src/capabilities.rs`: the
prefetcher bits against the vendors' layouts, the register per vendor and
family, the cpu identity, the capture against the kernel's own answer, the
`dynamic-linking` probe and the `hwmon` probe.

Since the eighteenth wave, five more in `tests/bench_bands.rs`: the catalog's
cost bands against the record of the last table (below, "The bands, corrected
to the second table").

`#[ignore]`, through `scripts/bench.sh`: the gate, twice; the counts; the
sweep; the observer effect, twice; the table.

**The suite on this branch**, one `make check` and one
`cargo test --workspace --no-fail-fast`, on the loaded machine: everything
passes but two tests, neither in this stream's files and both already on
record. `aggregation::two_emits_of_one_request_describe_the_same_events` is
the float compared exactly (the ninth wave's request to whoever owns
`tests/aggregation.rs`); it fails on `main` too, every run.
`scenario_cpu::a_profile_renders_in_every_format_that_carries_stacks` got 33
samples of a 200 ms spin at a load average near 9 and passes alone
(`docs/open-findings.md`, its paragraph at the profile's sample floor).
Clippy and fmt are clean.

### The band column, fifteenth wave

The table test measured three columns and asserted nothing about them: a row
whose every repetition failed printed `-` in each cell and the run passed.
Each row is now held to the cost band its backend declares
(`harness::judge_band`, `Cost::worst_case_percent`), on the larger of the
share of the target's cpu `cost.json` reports and the dilation the workload
measured of itself -- the second is where an in-kernel collector's cost is,
which is the correction the tenth wave's table made to the bands. Three
outcomes, in a `band` column of `table.md` and `table.json`:

- **within**: measured, and under the band's ceiling;
- **over**: measured, and over it. The row is flagged, listed under "Over the
  band the catalog declares" and said at the end; it does not fail the run,
  because a band the machine does not keep is a catalog claim to correct, not
  a table to throw away;
- **missing**: no repetition completed, none reported its collector cpu, or
  there is neither a share nor a dilation to hold to a band. The table is
  wrong, and the test fails naming the row.

The losses column is a count and always present; `missing` is what says it was
counted over something. Still `#[ignore]`: a quiet machine, through
`scripts/bench.sh`. The rule is in `make check` on synthetic rows
(`bench_harness::a_row_is_held_to_its_band_on_the_larger_of_its_share_and_its_dilation`,
`a_row_with_a_column_missing_is_refused_rather_than_judged`); the second table,
on a quiet machine, is what exercises it.

### The table that fails, and the guardrail, sixteenth wave

The table test now holds the whole table, not only each row
(`harness::table_failures`): it fails when it measured nothing, when a planned
(source, backend) pair is neither a row nor listed as excluded by the gate,
when a row was served by another backend than the one it forced, and when a
row is missing a column. A row over its band is still flagged and not failed.
Held on synthetic tables in `make check`
(`bench_harness::a_wrong_backends_table_fails_and_a_row_over_its_band_alone_does_not`).
The second table is `scripts/bench.sh backends` (or the whole sequence), on a
quiet machine, and is not run by a stream.

**The thermal guardrail stays a direct read** (`harness::throttle_count`), and
is not a run of `power.throttle` through the tool. The guardrail wraps the bare
runs as well as the traced ones, and the bare run is the baseline every
dilation is measured against: an `omn` beside it would make it a traced run,
and the dilation column a comparison of two traced runs. Read before and after
a repetition, outside the timed region, it costs the workload nothing; and a
guardrail measured by the tool under test would pass whatever defect the tool
had in that source. What it reads is what `power.throttle`'s `fs:sys` backend
reads, `thermal_throttle/{core,package}_throttle_count`; on a cpu that
publishes none (AMD, this machine) the report says a throttled run cannot be
told from one that was not.

## The bands, corrected to the second table, eighteenth wave

The second table (`scripts/bench.sh`, before the seventeenth wave, 68 minutes,
documents under `target/bench/backends-1791030430/` on the development
machine: release build with its capabilities, boost on, load 2.6) flagged 46
(source, backend) pairs over the band the catalog declared. Each is now either
corrected to the band it measured -- 43 pairs -- or stands with its reason
written beside it -- 2 pairs. The 46th, `mem.swap` through `fs:cgroup`, was
over only on a dilation the workload cannot resolve, and the rule below reads
it as within.

**The rule a row is judged by.** A row's cost is the larger of two columns:
the share of the target's cpu the tool reported, and the dilation the workload
measured of itself, which is where an in-kernel collector's cost is
(finding 47). That is `harness::judge_band`'s rule, with one change: **a
dilation no larger than what the gate says the workload resolves reads as
zero**, because the bench cannot tell it from zero. A negative dilation is
zero too. Without this, `mem.swap` through `fs:cgroup` -- a cgroup file read
that cannot reach into the target's timed section -- was "over `free`" on
+0.21 % of a workload that resolves 0.39 %. The harness's own column does not
apply the floor yet (a request in `docs/fixes/w18-bench.md`).

**What was corrected, and what it says.**

- **The tool's floor is above `free`.** Every hardware perf counter in
  `count` mode costs about 0.13 to 0.20 % of a target's cpu -- about 240 us on `compute`'s
  145 ms, 1.1 to 1.2 ms on `memory`'s 355 ms, so it is a rate, not a fixed
  opening cost -- and so do the system-wide `/proc` and `/sys` polls, the two
  `proc` raw tracepoints over the `launch` workload, the per-process polls
  on the 28 ms `syscall` workload and the perf-map reader. `free`'s ceiling is 0.1 %
  (`Cost::worst_case_percent`); those 27 pairs are `low`. What stays `free`
  is what measured under it: `cpu.oncpu_time`, `cpu.pressure`, `io.bytes`,
  `io.iops`, `io.merges`, `mem.cgroup`, `mem.pressure` (both), `mem.rss`
  through `statm`, `mem.swap` (both), `net.bytes`, `net.hwts`, `net.packets`,
  `power.energy` and `sched.switches`.
- **A poll is priced by what it reads.** `smaps` and `smaps_rollup` walk every
  mapping of a 300 MB process each 100 ms: `mem.pss`, `mem.rss`, `mem.uss` are
  3.2 to 3.5 %, and `mem.numa` 4.9 % -- `med`. `power.cstate` reads every
  idle state of 24 cpus: 7.8 %, `high`. NVML is not one price: `gpu.temp`
  0.5 %, `gpu.mem` 1.5 %, `gpu.engine_busy` 4.2 %, `gpu.power` 12.4 % and
  `gpu.freq` 14.6 % -- `low`, `med`, `med`, `high`, `high`.
- **The raw-tracepoint ring has a price per event.** `io.size` drains 2048
  records in 2.5 ms of a 38 ms target, 6.5 %: `high`. `io.queue` 2.9 %, `med`.
  `io.latency` in `count` -- the in-kernel histogram -- 3.4 % of the bench's
  `io` workload: `med`. That workload is far from `null_blk`'s 3 M IOPS, where
  the histogram's in-target cost was 12 to 24 % before stream io's work on it;
  whether `med` holds there is the coordinator's `null_blk` run to say.
- **`cpu.profile` through `pmu:sw`**: 4.9 % at 997 Hz, eight times the tenth
  wave's 0.61 %: `med`. The band says what it costs; stream profile is making
  it cost less, and a third table is what lowers the band again.
- **`net.tcp.rtt` through `netlink`** dumps the socket table on each read,
  3.2 % with no connection to report: `med`.

**What stands, and why.** `io.fsync` (`high`) and `net.tcp.connect` (`low`)
through `bpf:fentry` measured 142 % and 39 % of their targets' cpu, and saw no
event. Both spent about 55 ms -- `io.fsync` on `io`, `net.tcp.connect` on
`compute`, two targets of different length -- which is the fentry trampoline
rebuild the eighth wave measured, paid once at attach. A fixed cost is not a
rate and has no band: on a 10 s target it is 0.5 %. What the target pays, its
dilation, is +6.3 % and zero, inside each declared band. Both rows are held to
that: the test fails if their dilation leaves the band, or if their share
comes back under it and the reason is no longer needed. The fixed cost is
still the tool's, and still counted by `cost.json` against `--budget`: a short
run of either under a tight budget stops. That is filed, not hidden.

**An equivalent backend is not cheaper than what it stands in for.** A
backend declared `equivalent` is the same measurement, and `bpf:tp` beside
`bpf:raw_tp` is the same tracepoint and the same collector. Raising only the
measured `bpf:raw_tp` left its sibling a band below, and `--budget` degrades
onto what is below and declares less: `omn io.size -m trace -e hist --system
--budget 1%` promised to move onto `bpf:tp` (`low`) and would have kept paying
about 6.5 %. So no equivalent backend declares a cheaper band than one above
it in the chain serving a target it serves, and a test holds it. Seven lines
the table did not measure moved with it: `bpf:tp` to `med` for `io.latency`
and `io.queue`, to `high` for `io.size`, to `low` for `proc.exec` and
`proc.exit`; `power.freq` through `pmu` (aperf/mperf) to `low`, under
`fs:sys`'s; `net.tcp.rtt` through `bpf:fentry` to `med`, under `netlink`'s --
that one is a different mechanism, a program on every received segment, and
unmeasured: what a budget may promise for it is not a saving nobody measured.
`io.pressure`'s `fs:cgroup` stays `free` under `fs:proc`'s `low`: the two
serve disjoint targets, so no budget moves one request from one to the other.
`mem.cache_llc` through `pmu` moved to `low` too, beside its measured
`pmu:generic`: the same counters, read the same way.

**Not corrected.** Any other pair the table did not measure keeps its band.
Two count-mode counters stay `free` with nothing measured to move them:
`mem.bandwidth` through `pmu:uncore`, one system-wide uncore counter for which
no collector is written yet, and `power.energy` through `pmu` (RAPL), which
measured 0.03 %, under `free`. `sched.switches` through `pmu:sw` measured 0.06 % on
`contended` and stays `free`; it is the software counter the floor above does
not reach. The bands lowered nowhere: a band above its measurement may be the
source's own case, which the bench's seven workloads do not all reach.

**Held by a test.** `tests/bench_bands.rs` reads the record below, in `make
check`, and fails when the catalog declares another band than a row records
(so a cost line cannot change without its row), when a row's measurement is
over its band without a reason, when a reason is empty or no longer applies,
when a standing row's dilation leaves its band, when a row says `corrected`
without a change of band or anything but `within`, `corrected` or `stands ...:
<reason>`, when the record holds another number of rows than its
`bands:begin` marker's `rows=`, and when an equivalent backend declares less
than one above it. Each of those is shown failing on a synthetic row. A new table is pasted here --
the medians of `table.md`'s `% of target` and `dilation` columns, and the
gate's `RESOLVES` -- and the test names every band it puts outside.

<!-- bands:begin backends-1791030430 rows=95 -->
| workload | resolves |
|---|---|
| launch | 0.86 % |
| compute | 0.23 % |
| memory | 0.39 % |
| syscall | 0.84 % |
| io | 0.85 % |
| contended | 0.96 % |
| malloc | 0.89 % |

| source | backend | mode | target | workload | % of target | dilation | was | declared | verdict |
|---|---|---|---|---|---|---|---|---|---|
| cpu.branch_misses | pmu | count | launch | compute | 0.17 % | -0.40 % | free | low | corrected |
| cpu.cache_misses | pmu | count | launch | compute | 0.17 % | -0.41 % | free | low | corrected |
| cpu.context_switches | pmu:sw | count | launch | compute | 0.20 % | -0.12 % | free | low | corrected |
| cpu.cycles | pmu | count | launch | compute | 0.17 % | -0.11 % | free | low | corrected |
| cpu.cycles | pmu:sw | count | launch | compute | 0.17 % | -0.01 % | free | low | corrected |
| cpu.instructions | pmu | count | launch | compute | 0.17 % | +0.08 % | free | low | corrected |
| cpu.ipc | pmu | count | launch | compute | 0.15 % | -0.19 % | free | low | corrected |
| cpu.migrations | pmu:sw | count | launch | compute | 0.20 % | +0.02 % | free | low | corrected |
| cpu.oncpu_time | fs:proc | poll@100ms | launch | compute | 0.08 % | -0.06 % | free | free | within |
| cpu.pressure | fs:proc | poll@100ms | system | compute | 0.08 % | - | free | free | within |
| cpu.profile | pmu:sw | sample@997hz | launch | compute | 4.94 % | +0.34 % | low | med | corrected |
| cpu.util | fs:proc | poll@100ms | system | compute | 0.22 % | - | free | low | corrected |
| gpu.api | preload | trace | launch | compute | 0.14 % | +0.16 % | low | low | within |
| gpu.engine_busy | vendor:nvml | count | launch | compute | 4.20 % | +0.11 % | low | med | corrected |
| gpu.freq | vendor:nvml | poll@100ms | launch | compute | 14.55 % | -0.03 % | free | high | corrected |
| gpu.mem | vendor:nvml | poll@100ms | launch | compute | 1.48 % | +0.12 % | free | med | corrected |
| gpu.power | vendor:nvml | count | launch | compute | 12.39 % | +0.34 % | free | high | corrected |
| gpu.temp | vendor:nvml | poll@100ms | launch | compute | 0.52 % | -0.16 % | free | low | corrected |
| io.bytes | fs:proc | count | system | io | 0.05 % | - | free | free | within |
| io.fs | bpf:fentry | count | launch | io | 0.70 % | +5.53 % | high | high | within |
| io.fsync | bpf:fentry | trace | launch | io | 142.13 % | +6.31 % | high | high | stands on its dilation: the share is the fentry attach, about 55 ms a run whatever the target (`net.tcp.connect` pays the same on `compute`), over a run that saw no event |
| io.inflight | fs:proc | poll@100ms | system | io | 0.30 % | - | free | low | corrected |
| io.iops | fs:proc | count | system | io | 0.08 % | - | free | free | within |
| io.latency | bpf:raw_tp | count | launch | io | 3.44 % | -0.31 % | low | med | corrected |
| io.merges | fs:proc | count | system | io | 0.08 % | - | free | free | within |
| io.pagecache | bpf:raw_tp | count | launch | io | 0.72 % | +0.72 % | med | med | within |
| io.pressure | fs:proc | poll@100ms | system | io | 0.21 % | - | free | low | corrected |
| io.queue | bpf:raw_tp | trace | system | io | 2.85 % | - | low | med | corrected |
| io.readahead | bpf:raw_tp | count | launch | io | 0.72 % | +0.30 % | low | low | within |
| io.size | bpf:raw_tp | trace | launch | io | 6.45 % | -0.44 % | low | high | corrected |
| io.syscall | bpf:fentry | count | launch | io | 0.72 % | +2.30 % | med | med | within |
| io.writeback | bpf:raw_tp | count | system | io | 0.26 % | - | low | low | within |
| mem.cache_l1 | pmu | count | launch | memory | 0.13 % | +0.17 % | free | low | corrected |
| mem.cache_llc | pmu:generic | count | launch | memory | 0.12 % | +0.41 % | free | low | corrected |
| mem.cache_tlb | pmu | count | launch | memory | 0.12 % | +0.20 % | free | low | corrected |
| mem.cgroup | fs:cgroup | poll@100ms | launch | memory | 0.10 % | +0.09 % | free | free | within |
| mem.compaction | bpf:raw_tp | count | system | memory | 0.09 % | - | low | low | within |
| mem.faults | pmu:sw | count | launch | memory | 0.13 % | +0.19 % | free | low | corrected |
| mem.heap.alloc | preload | trace | launch | malloc | 1483.21 % | +1261.49 % | extreme | extreme | within |
| mem.heap.live | preload | poll@100ms | launch | malloc | 732.32 % | +1643.40 % | extreme | extreme | within |
| mem.heap.retained | preload | poll@100ms | launch | malloc | 923.29 % | +1545.40 % | extreme | extreme | within |
| mem.kmalloc | bpf:raw_tp | count | system | memory | 0.28 % | - | med | med | within |
| mem.latency | pmu:ibs | sample@997hz | launch | memory | 0.14 % | +0.41 % | med | med | within |
| mem.numa | fs:proc | poll@100ms | launch | memory | 4.86 % | +0.12 % | low | med | corrected |
| mem.page_alloc | bpf:raw_tp | count | system | memory | 0.12 % | - | med | med | within |
| mem.pressure | fs:proc | poll@100ms | system | memory | 0.04 % | - | free | free | within |
| mem.pressure | fs:cgroup | poll@100ms | launch | memory | 0.05 % | +0.03 % | free | free | within |
| mem.pss | fs:proc:smaps_rollup | poll@100ms | launch | memory | 3.53 % | +0.14 % | free | med | corrected |
| mem.pss | fs:proc:smaps | poll@100ms | launch | memory | 3.45 % | +0.16 % | low | med | corrected |
| mem.reclaim | bpf:raw_tp | count | launch | memory | 0.07 % | +0.19 % | low | low | within |
| mem.rss | fs:proc:smaps_rollup | poll@100ms | launch | memory | 3.24 % | -0.22 % | free | med | corrected |
| mem.rss | fs:proc:statm | poll@100ms | launch | memory | 0.04 % | -0.11 % | free | free | within |
| mem.sharing | pmu:ibs | sample@997hz | launch | memory | 0.15 % | +0.50 % | med | med | within |
| mem.swap | fs:proc | count | system | memory | 0.03 % | - | free | free | within |
| mem.swap | fs:cgroup | count | launch | memory | 0.02 % | +0.21 % | free | free | within |
| mem.thp | fs:sys | poll@100ms | system | memory | 0.21 % | - | free | low | corrected |
| mem.thp | fs:proc | poll@100ms | system | memory | 0.15 % | - | free | low | corrected |
| mem.uss | fs:proc:smaps_rollup | poll@100ms | launch | memory | 3.53 % | -0.11 % | free | med | corrected |
| mem.uss | fs:proc:smaps | poll@100ms | launch | memory | 3.53 % | -0.14 % | low | med | corrected |
| mem.wss | fs:proc:clear_refs | poll@100ms | launch | memory | 6.39 % | +3.01 % | high | high | within |
| net.backlog | fs:proc | count | system | compute | 0.17 % | - | free | low | corrected |
| net.bytes | fs:proc | count | system | compute | 0.06 % | - | free | free | within |
| net.drops | bpf:raw_tp | count | system | compute | 0.22 % | - | low | low | within |
| net.hwts | netlink:ethtool | poll@100ms | system | compute | 0.05 % | - | free | free | within |
| net.iface | fs:proc | count | system | compute | 0.15 % | - | free | low | corrected |
| net.packets | fs:proc | count | system | compute | 0.08 % | - | free | free | within |
| net.qdisc | bpf:raw_tp | trace | system | compute | 0.28 % | - | low | low | within |
| net.socket | bpf:fentry | count | launch | compute | 0.18 % | +0.45 % | low | low | within |
| net.softirq | bpf:raw_tp | count | system | compute | 0.20 % | - | med | med | within |
| net.tcp.connect | bpf:fentry | trace | launch | compute | 39.46 % | -0.01 % | low | low | stands on its dilation: the share is the fentry attach, about 55 ms a run whatever the target (`io.fsync` pays the same on `io`), over a run that saw no event |
| net.tcp.lifetime | bpf:raw_tp | trace | system | compute | 0.29 % | - | low | low | within |
| net.tcp.retransmit | bpf:raw_tp | count | system | compute | 0.30 % | - | low | low | within |
| net.tcp.rtt | netlink | trace | launch | compute | 3.21 % | +0.17 % | free | med | corrected |
| power.cstate | fs:sys | poll@100ms | system | compute | 7.77 % | - | free | high | corrected |
| power.energy | pmu | count | system | compute | 0.03 % | - | free | free | within |
| power.freq | fs:sys | poll@100ms | system | compute | 0.91 % | - | free | low | corrected |
| power.temp | fs:sys | poll@100ms | system | compute | 0.33 % | - | free | low | corrected |
| proc.exec | bpf:raw_tp | trace | system | launch | 0.46 % | - | free | low | corrected |
| proc.exit | bpf:raw_tp | trace | system | launch | 0.44 % | - | free | low | corrected |
| proc.fd | fs:proc | poll@100ms | launch | syscall | 0.68 % | -0.12 % | free | low | corrected |
| proc.signals | bpf:raw_tp | count | launch | syscall | 0.71 % | -0.35 % | low | low | within |
| proc.syscall | bpf:raw_tp | count | launch | syscall | 0.46 % | +319.80 % | extreme | extreme | within |
| proc.threads | fs:proc | poll@100ms | launch | syscall | 0.64 % | -0.09 % | free | low | corrected |
| runtime.jit | fs:jit-map | trace | launch | compute | 0.17 % | +0.05 % | free | low | corrected |
| sched.futex | bpf:raw_tp | trace | launch | contended | 0.11 % | -9.56 % | high | high | within |
| sched.irq | bpf:raw_tp | count | system | contended | 0.35 % | - | med | med | within |
| sched.lock | bpf:raw_tp | trace | launch | contended | 0.25 % | -31.47 % | med | med | within |
| sched.migrations | bpf:raw_tp | count | launch | contended | 0.05 % | -1.03 % | low | low | within |
| sched.mutex | preload | trace | launch | contended | 31.82 % | +640.02 % | extreme | extreme | within |
| sched.numa_balance | bpf:raw_tp | count | system | contended | 0.07 % | - | low | low | within |
| sched.offcpu | bpf:raw_tp | trace | launch | contended | 0.11 % | -11.32 % | med | med | within |
| sched.runqlat | bpf:raw_tp | trace | launch | contended | 0.11 % | -6.90 % | med | med | within |
| sched.switches | pmu:sw | count | launch | contended | 0.06 % | -9.83 % | free | free | within |
| sched.switches | bpf:raw_tp | count | launch | contended | 0.05 % | -3.58 % | low | low | within |
| sched.wakeup | bpf:raw_tp | trace | launch | contended | 3.65 % | +0.38 % | med | med | within |
<!-- bands:end -->

## The third table, after the eighteenth wave

Run by the coordinator alone on 2026-10-04 at `29b1705`
(`target/bench/backends-1791084966/` on the development machine: release
build with its capabilities, 67 minutes, `bench_backends` green). The gate
resolved launch 0.76 %, compute 0.18 %, memory 0.20 %, syscall 0.60 %, io
0.86 %, contended 0.86 %, malloc 0.93 %; the same 95 pairs as the second
table. What moved: `cpu.profile` 4.69 % (4.94), `io.latency` 3.63 % (3.44),
`io.inflight` 0.83 % (0.30), `net.tcp.rtt` through `netlink` 5.64 % (3.21),
the heap rows down (`mem.heap.live` 493 % from 732, `mem.heap.retained` 749 %
from 923), `sched.migrations`' dilation +1.53 % (-1.03). `io.fsync` and
`net.tcp.connect` through `bpf:fentry` read 158 % and 44 %, dilation inside
their bands: their `stands` reason holds.

Pasted into the record above, `tests/bench_bands.rs` names six bands outside:

| source | backend | workload | measured | declared |
|---|---|---|---|---|
| cpu.pressure | fs:proc | compute | 0.15 % | free |
| mem.cgroup | fs:cgroup | memory | 0.11 % | free |
| net.hwts | netlink:ethtool | compute | 0.11 % | free |
| net.tcp.rtt | netlink | compute | 5.64 % | med |
| power.freq | fs:sys | compute | 1.19 % | low |
| sched.migrations | bpf:raw_tp | contended | +1.53 % dilation | low |

Correcting them is a change to `catalog/*.yaml`, which the run that measured
them does not make, so the record still holds the second table and the
test stays green on it. Four of the six are within a few hundredths of a
point of their ceiling, `free`'s 0.1 % included; `net.tcp.rtt` and
`sched.migrations` moved by more than noise. Filed
(`docs/open-findings.md`, "The eighteenth wave, coordinator, after the
merge"): the bands to correct, or a `stands` with its reason, with the third
table pasted in their place.

## Exit criteria of phase 8

- [x] *A cost/resolution/loss table measured for every (source, backend).* 94
  pairs, every one this build serves here, under "The table"; the three heap
  sources from the rerun below it.
- [x] *The resolver's choices are confirmed by measurement, or corrected.* The
  seven sources with a second served backend: all confirmed. What the table
  corrects instead is the cost bands the resolver reads, for in-kernel
  collectors and for the shim under a lock storm -- filed.
- [x] *The agent's own cost (CPU, RSS) is measured separately from the dilation
  of the observed process.* Collector cpu from `cost.json`, the tool's peak
  RSS polled from `/proc/<pid>/status`, and the dilation measured by the
  workload itself -- three columns, three sources.

## Index rows

None. This stream adds no session: the bench is driven by `scripts/bench.sh`,
and nothing under `examples/bench-*.yml` was needed. The session-index test is
unaffected by this branch.
