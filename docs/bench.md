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

**Not measured yet, on purpose.** The gate refused every time it was asked on
this machine during the wave, and a table from a noisy machine is worse than
none. The coordinator replaces this paragraph with `table.md` from the real
run.

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

## Exit criteria of phase 8

- [ ] *A cost/resolution/loss table measured for every (source, backend).* The
  harness produces it; it has not been run on a quiet machine. **The one
  command above.**
- [ ] *The resolver's choices are confirmed by measurement, or corrected.* The
  verdict is in the table; same.
- [x] *The agent's own cost (CPU, RSS) is measured separately from the dilation
  of the observed process.* Collector cpu from `cost.json`, the tool's peak
  RSS polled from `/proc/<pid>/status`, and the dilation measured by the
  workload itself -- three columns, three sources.

## Index rows

None. This stream adds no session: the bench is driven by `scripts/bench.sh`,
and nothing under `examples/bench-*.yml` was needed. The session-index test is
unaffected by this branch.
